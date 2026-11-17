use std::num::NonZeroU32;

use anyhow::{bail, Result};
use zbus::{
    message,
    names::{BusName, OwnedUniqueName},
    Message,
};

use super::{outbox::Outbox, serve::Served, Peers, Routing};
use crate::{
    debug, fdo,
    name_registry::NameRegistry,
    peer::{Peer, Recipient, Refusal},
    policy::Facts,
    trace, warn,
};

impl Peers {
    /// Forward `msgs` from the peer represented by `sender`, preserving their order and using its
    /// task's empty `outbox`. The bus sends each message to its recipients. The task that serves
    /// the peer keeps `sender`.
    ///
    /// Each recipient gets those of `msgs` that go to it in one go, in order, and all of them get
    /// theirs at once.
    ///
    /// When a call that expects a reply goes to its callee, the bus records it as a call that waits
    /// on the callee. The bus refuses the call with an error instead, as dbus-broker does, in two
    /// cases: the caller already has a call with the same serial waiting on the callee, or the
    /// caller's user already has as many objects of the callee's user as it may. A reply or an
    /// error from `sender` goes on to its destination only if it answers one of the calls that
    /// wait on `sender`. As in dbus-broker, the bus doesn't record the calls to the bus itself: it
    /// answers them itself, so it refuses none of them, and its own replies go on whatever they
    /// answer.
    ///
    /// A call to a name that no peer has gets an error from the bus, as with dbus-broker, unless
    /// it expects no reply, and so does one with file descriptors to a callee that didn't negotiate
    /// passing them, which can't take it, rather than wait for a reply that can't come.
    ///
    /// The policy of the bus has its say on each method call and signal, for each peer that would
    /// get it, the bus itself included: first the part of the receiver, as to whether it may
    /// receive the message from `sender`, and then that of `sender`, as to whether it may send it
    /// to the receiver, each by the well-known names of the other. A call, or a signal with a
    /// destination, that the policy refuses goes nowhere, and a call that expects a reply gets an
    /// `AccessDenied` error from the bus, as with dbus-broker; a call is refused before it is
    /// recorded as one that waits. A signal without a destination goes to those of its subscribers
    /// that the policy lets have it. Replies and errors are not subject to the policy. The calls
    /// and signals of a `sender` that is a peer no more, as it became a monitor meanwhile, go
    /// nowhere.
    ///
    /// The monitors get each message, whether it goes on to its destination or not, as dbus-broker
    /// passes every message on to its monitors before it routes it.
    pub(super) async fn forward(
        &self,
        sender: &Served,
        msgs: &[Message],
        outbox: &mut Outbox,
    ) -> Result<()> {
        // The calls that the bus answers with an error of its own, and why.
        let mut rejected = Vec::new();
        // The recipients are picked under the lock, and only sent their messages without it.
        {
            let routing = self.routing().await;
            let router = Router::new(&routing, sender);
            for (i, msg) in msgs.iter().enumerate() {
                if msg.message_type() == message::Type::Signal {
                    router.signal(msgs, i, outbox);
                } else {
                    match router.unicast(msg)? {
                        Unicast::Deliver(peer) => {
                            let client = peer.client();
                            outbox.add(msgs, client.id(), client.recipient(), i);
                        }
                        Unicast::Reject(rejection) => rejected.push((msg.clone(), rejection)),
                        Unicast::Drop => (),
                    }
                }
                router.monitors(msgs, i, outbox);
            }
        }

        let dropped = outbox.send(msgs).await;
        self.reject_dropped(sender, dropped, &mut rejected).await;
        for (msg, rejection) in rejected {
            let (name, description) = rejection.error();
            let error = Message::error(&msg.header(), name)
                .sender(fdo::BUS_NAME)
                .build(&description)?;
            self.send_from_bus(&error).await;
        }

        Ok(())
    }

    /// Add the calls among `dropped` to `rejected`. `dropped` holds the messages from the peer
    /// represented by `sender` that their recipients can't take because of their file descriptors,
    /// each with its recipient.
    ///
    /// A call that its callee can't take doesn't wait for a reply. Instead, the bus answers it with
    /// an error, as dbus-broker does.
    async fn reject_dropped(
        &self,
        sender: &Served,
        dropped: Vec<(Recipient, Message)>,
        rejected: &mut Vec<(Message, Rejection)>,
    ) {
        if dropped.is_empty() {
            return;
        }
        let routing = self.routing().await;
        for (recipient, msg) in dropped {
            let primary = msg.primary_header();
            if msg.message_type() != message::Type::MethodCall
                || primary.flags().contains(message::Flags::NO_REPLY_EXPECTED)
            {
                continue;
            }
            let Some(callee) = recipient
                .unique_name()
                .and_then(|name| routing.peers.get(name))
            else {
                continue;
            };
            if callee
                .pending_calls()
                .remove(sender.id, primary.serial_num())
            {
                rejected.push((msg, Rejection::UnexpectedFds));
            }
        }
    }
}

/// Picks the recipients of one sender's messages, while the routing lock is held.
struct Router<'r> {
    routing: &'r Routing,
    /// The sender's state, as kept by the task that serves the sender.
    sender: &'r Served,
    /// The sender, or `None` if it is not a peer anymore because it became a monitor meanwhile.
    sender_peer: Option<&'r Peer>,
    /// Whether the sender is the bus itself.
    from_bus: bool,
}

impl<'r> Router<'r> {
    fn new(routing: &'r Routing, sender: &'r Served) -> Self {
        Self {
            routing,
            sender,
            sender_peer: routing.peers.get(sender.unique_name.as_str()),
            from_bus: sender.unique_name == fdo::BUS_NAME,
        }
    }

    /// Add the signal at `i` in `msgs` to the share of each peer that it goes to and that the
    /// policy lets receive it from the sender. If the sender is not a peer anymore, add the signal
    /// to no share.
    fn signal(&self, msgs: &[Message], i: usize, outbox: &mut Outbox) {
        let msg = &msgs[i];
        trace!("Broadcasting message: {:?}", msg);
        let Some(sender_peer) = self.sender_peer else {
            trace!(
                "Dropping {msg:?}, as `{}` is a peer no more",
                self.sender.unique_name
            );

            return;
        };
        let name_registry = &self.routing.name_registry;
        // Parsed only for the policy to look at, if it has to.
        let mut header = None;
        for (name, peer) in self.routing.peers.iter() {
            if !peer.interested(msg, name_registry) {
                trace!("Peer {} not interested in {msg:?}", name);
                continue;
            }
            if denial(
                sender_peer,
                peer,
                || Facts::new(msg, header.get_or_insert_with(|| msg.header())),
                name_registry,
            )
            .is_some()
            {
                continue;
            }

            let client = peer.client();
            outbox.add(msgs, client.id(), client.recipient(), i);
        }
    }

    /// Decide what becomes of `msg`, a call, a reply or an error from the sender.
    ///
    /// A reply or an error removes the record of the call that it answers. A call that goes on to
    /// its callee and expects a reply is recorded as a call that waits on the callee.
    fn unicast(&self, msg: &Message) -> Result<Unicast<'r>> {
        let Routing {
            peers,
            name_registry,
            ..
        } = self.routing;
        let sender = &self.sender.unique_name;
        let header = msg.header();
        // peer::Stream ensures a valid destination so this isn't exactly needed.
        let Some(destination) = header.destination() else {
            bail!("invalid message: {:?}", msg);
        };
        trace!(
            "Forwarding message: {:?}, destination: {}",
            msg,
            destination
        );
        let peer = match destination {
            BusName::Unique(name) => peers.get_key_value(name.as_str()),
            BusName::WellKnown(name) => name_registry
                .lookup(name)
                .and_then(|owner| peers.get_key_value(owner.as_str())),
        };
        let msg_type = msg.message_type();
        // A reply removes the record of the call that it answers. A reply from the bus goes on
        // whatever it answers, because the bus keeps no records of the calls to it.
        let answers_none = !self.from_bus
            && matches!(msg_type, message::Type::MethodReturn | message::Type::Error)
            && !match (peer, header.reply_serial()) {
                (Some((_, peer)), Some(serial)) => {
                    self.sender.pending_calls.remove(peer.client().id(), serial)
                }
                _ => false,
            };
        let primary = msg.primary_header();
        let flags = primary.flags();
        let is_call = msg_type == message::Type::MethodCall;
        let expects_reply = is_call && !flags.contains(message::Flags::NO_REPLY_EXPECTED);
        // The policy has its say on a call before the call waits for its reply.
        let denied = match (peer, self.sender_peer) {
            (Some((_, peer)), Some(sender_peer)) if is_call => denial(
                sender_peer,
                peer,
                || Facts::new(msg, &header),
                name_registry,
            ),
            _ => None,
        };

        Ok(match (peer, denied) {
            _ if answers_none => {
                debug!("{msg:?} answers no call that waits on {sender}");

                Unicast::Drop
            }
            _ if is_call && self.sender_peer.is_none() => {
                trace!("Dropping {msg:?}, as `{sender}` is a peer no more");

                Unicast::Drop
            }
            (Some(_), Some(rejection)) if expects_reply => Unicast::Reject(rejection),
            (Some(_), Some(_)) => Unicast::Drop,
            (Some((name, peer)), None) => {
                // The bus answers the calls to it itself, so they wait on no record.
                let refusal = if expects_reply && *name != fdo::BUS_NAME {
                    peer.pending_calls()
                        .add(self.sender.id, &self.sender.caller, primary.serial_num())
                        .err()
                } else {
                    None
                };
                match refusal {
                    Some(refusal) => Unicast::Reject(Rejection::from(refusal)),
                    None => Unicast::Deliver(peer),
                }
            }
            (None, _) => {
                debug!("no peer for destination `{destination}`");
                // The bus starts no services, so no service can be started for any name.
                if !expects_reply {
                    Unicast::Drop
                } else if flags.contains(message::Flags::NO_AUTO_START) {
                    Unicast::Reject(Rejection::NameHasNoOwner)
                } else {
                    Unicast::Reject(Rejection::ServiceUnknown)
                }
            }
        })
    }

    /// Add the message at `i` in `msgs` to the share of each monitor that watches it.
    fn monitors(&self, msgs: &[Message], i: usize, outbox: &mut Outbox) {
        let monitors = &self.routing.monitors;
        if monitors.is_empty() {
            return;
        }
        let msg = &msgs[i];
        trace!(
            "Broadcasting message to {} monitors: {:?}",
            monitors.len(),
            msg
        );
        for (name, monitor) in monitors.iter() {
            if !monitor.interested(msg, &self.routing.name_registry) {
                trace!("Monitor {} not interested in {msg:?}", name);
                continue;
            }

            let client = monitor.client();
            outbox.add(msgs, client.id(), client.recipient(), i);
        }
    }
}

/// What becomes of a message with a destination: a call, a reply or an error.
enum Unicast<'r> {
    /// It goes on to this peer, its destination.
    Deliver(&'r Peer),
    /// The bus answers it with an error of its own.
    Reject(Rejection),
    /// It goes nowhere, and gets no answer.
    Drop,
}

/// Why the policy keeps the message of `facts` from going from `sender` to `receiver`, if it does:
/// first the part of `receiver` is asked whether it may receive the message from `sender`, and then
/// the part of `sender` whether it may send it to `receiver`, each by the names of the other in
/// `name_registry`.
///
/// Where `sender` sends freely and `receiver` receives freely, as with the open policy, that is
/// all there is to it: the facts of the message are only looked at otherwise.
fn denial<'m, F>(
    sender: &Peer,
    receiver: &Peer,
    facts: F,
    name_registry: &NameRegistry,
) -> Option<Rejection>
where
    F: FnOnce() -> Facts<'m>,
{
    if sender.policy().sends_freely() && receiver.policy().receives_freely() {
        return None;
    }
    let facts = facts();
    let sender_names = name_registry.names_of(sender.client().unique_name());
    let receiver_names = name_registry.names_of(receiver.client().unique_name());
    let (rejection, denied, action, preposition, other) =
        if !receiver.policy().may_receive(&facts, sender_names) {
            (
                Rejection::ReceiveDenied,
                receiver,
                "receiving",
                "from",
                sender,
            )
        } else if !sender.policy().may_send(&facts, receiver_names) {
            (Rejection::SendDenied, sender, "sending", "to", receiver)
        } else {
            return None;
        };
    let (denied, other) = (denied.client().unique_name(), other.client().unique_name());
    // A broadcast still goes to those that the policy lets have it, which is nothing for an
    // administrator to look at.
    if facts.is_broadcast() {
        trace!("The policy denies `{denied}` {action} {facts} {preposition} `{other}`");
    } else {
        warn!("The policy denies `{denied}` {action} {facts} {preposition} `{other}`");
    }

    Some(rejection)
}

/// The `NoReply` error, from the bus, to the call of `caller` with `serial`, as dbus-broker sends
/// to each call that waits on a peer that goes.
pub(super) fn no_reply(caller: &OwnedUniqueName, serial: NonZeroU32) -> zbus::Result<Message> {
    // The bus doesn't keep the calls themselves. The error needs only the caller and the serial
    // of a call.
    Message::error_for_serial(caller, serial, "org.freedesktop.DBus.Error.NoReply")
        .sender(fdo::BUS_NAME)
        .build(&"Remote peer disconnected")
}

/// Why the bus answers a call with an error of its own, rather than pass the call on to its
/// callee.
#[derive(Clone, Copy, Debug)]
enum Rejection {
    /// A call of the same caller with the same serial waits on the callee already.
    SerialPending,
    /// The user of the caller has as many of the objects of the user of the callee as it may
    /// already, as [`Charge::new`] has it.
    ///
    /// [`Charge::new`]: crate::user::Charge::new
    QuotaExceeded,
    /// No peer has the destination, and no service can be started for it.
    ServiceUnknown,
    /// No peer has the destination, and the call asks for no service to be started for it.
    NameHasNoOwner,
    /// The call has file descriptors, and the callee didn't negotiate passing them.
    UnexpectedFds,
    /// The policy of the bus doesn't let the callee receive the call from the caller.
    ReceiveDenied,
    /// The policy of the bus doesn't let the caller send the call to the callee.
    SendDenied,
}

impl Rejection {
    /// The name of the error, and its description, as dbus-broker has them.
    fn error(self) -> (&'static str, &'static str) {
        match self {
            Self::SerialPending => (
                "org.freedesktop.DBus.Error.AccessDenied",
                "Pending reply with that serial already exists",
            ),
            Self::QuotaExceeded => (
                "org.freedesktop.DBus.Error.LimitsExceeded",
                "Sending user's quota exceeded",
            ),
            Self::ServiceUnknown => (
                "org.freedesktop.DBus.Error.ServiceUnknown",
                "The name is not activatable",
            ),
            Self::NameHasNoOwner => (
                "org.freedesktop.DBus.Error.NameHasNoOwner",
                "Destination does not exist",
            ),
            Self::UnexpectedFds => (
                "org.freedesktop.DBus.Error.Failed",
                "Peer does not support file descriptor passing.",
            ),
            Self::ReceiveDenied => (
                "org.freedesktop.DBus.Error.AccessDenied",
                "Receiver is not authorized to receive message",
            ),
            Self::SendDenied => (
                "org.freedesktop.DBus.Error.AccessDenied",
                "Sender is not authorized to send message",
            ),
        }
    }
}

impl From<Refusal> for Rejection {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::SerialPending => Self::SerialPending,
            Refusal::QuotaExceeded => Self::QuotaExceeded,
        }
    }
}
