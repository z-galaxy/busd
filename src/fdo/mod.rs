//! The bus itself, as its clients see it: the D-Bus API that it serves, under its name,
//! `org.freedesktop.DBus`.
//!
//! One task answers the calls to the bus, one at a time, in the order that they come in, as the
//! reference implementation of D-Bus and dbus-broker do: the calls that a client makes one after
//! the other get their replies, and have the bus send their signals, in that order. The tasks that
//! serve the clients queue the calls for it as they forward the messages of their clients, and a
//! client that calls the bus while [`QUEUED`] calls wait in the queue waits for room there, as it
//! waits for any recipient to take what it sends. The replies, errors and signals of the bus go
//! through forwarding, as the messages of a client do, so that the policy has its say on them and
//! the monitors see them.
//!
//! So a method must never wait for anything that waits for the bus to take the calls after the one
//! that it answers, such as the task of a client that waits for room in the queue, or the bus waits
//! for ever. `every_call_to_the_bus_among_calls` of the tests calls each method among many other
//! calls, to make sure.
//!
//! The bus serves the `org.freedesktop.DBus` interface, the `org.freedesktop.DBus.Monitoring` and
//! `org.freedesktop.DBus.Debug.Stats` interfaces, and the standard `Introspectable`, `Properties`
//! and `Peer` interfaces on its object, [`PATH`], and the standard ones on the objects above it,
//! `/`, `/org` and `/org/freedesktop`. `Peer` answers on any path, as the specification has it. A
//! call to any other object, or of any other interface, gets the error of an unknown object or
//! interface. A call that expects no reply gets none, not even an error: the signals that it has
//! the bus send go out all the same.

use std::num::NonZeroUsize;

use serde::Serialize;
use zbus::{
    fdo::{Error, Result},
    message::{self, Flags, Header},
    names::{MemberName, UniqueName},
    wire::{DynamicDeserialize, DynamicType},
    DBusError, Message, ObjectPath, OwnedGuid,
};
use zruntime::{mpmc, spawn};

use crate::{
    debug,
    peers::{Chore, Chores, Peers, WeakPeers},
    warn,
};

mod dbus;
pub(crate) use dbus::name_change_signals;
#[cfg(target_os = "linux")]
mod handover;
#[cfg(target_os = "linux")]
pub(crate) use handover::OnItsWay;
#[cfg(target_os = "linux")]
use handover::TakenOver;
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod hook;
mod introspectable;
mod monitoring;
mod peer;
mod properties;
mod stats;

/// The name of the bus, and of its interface.
pub const BUS_NAME: &str = "org.freedesktop.DBus";

/// The path of the object of the bus.
pub const PATH: &str = "/org/freedesktop/DBus";

/// The interface through which a peer becomes a monitor, on the object of the bus.
pub const MONITORING: &str = "org.freedesktop.DBus.Monitoring";

/// The interface of the statistics of the bus, on its object.
pub const STATS: &str = "org.freedesktop.DBus.Debug.Stats";

/// The standard interface that describes an object, on each object of the bus.
pub const INTROSPECTABLE: &str = "org.freedesktop.DBus.Introspectable";

/// The standard interface of the properties of the interfaces of an object, on each object of the
/// bus.
pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// The standard interface that answers on any path.
pub const PEER: &str = "org.freedesktop.DBus.Peer";

/// How many calls to the bus wait for it in its queue at most.
pub const QUEUED: NonZeroUsize = NonZeroUsize::new(32).unwrap();

/// A queue of the calls to the bus, whose calls are chores among `chores`: the end that the tasks
/// that serve the clients put the calls in, and the end that the bus takes them out of.
pub(crate) fn queue(chores: Chores) -> (Calls, mpmc::Receiver<Job>) {
    let (sender, receiver) = mpmc::bounded(QUEUED);

    (
        Calls {
            queue: sender,
            chores,
        },
        receiver,
    )
}

/// Answer the calls to the bus of `peers`, whose GUID is `guid`, that come out of `queue`, in a
/// task of its own, until the queue is closed or the peers go.
pub(crate) fn answer_calls(peers: &Peers, guid: OwnedGuid, queue: mpmc::Receiver<Job>) {
    let driver = Driver {
        peers: peers.downgrade(),
        guid,
        stats_serial: 0,
        connection_stats_serial: 0,
    };

    spawn(driver.run(queue)).detach();
}

/// The end of the queue of the calls to the bus that the tasks that serve the clients put the calls
/// in.
#[derive(Clone, Debug)]
pub(crate) struct Calls {
    queue: mpmc::Sender<Job>,
    chores: Chores,
}

impl Calls {
    /// Queue the method calls among `msgs` for the bus, in order, each once there is room for it.
    /// The bus takes no other message, so the others go nowhere.
    ///
    /// Each call is a chore of the bus from now on, while it waits for room too, until the bus has
    /// answered it and sent the signals that follow its reply: a hand-over waits for that.
    pub(crate) async fn queue(&self, msgs: &[Message]) {
        let calls = msgs
            .iter()
            .filter(|msg| msg.message_type() == message::Type::MethodCall);
        for msg in calls {
            let call = Call {
                msg: msg.clone(),
                _chore: self.chores.start(),
            };
            if self.queue.send(Job::Call(call)).await.is_err() {
                debug!("Dropping {msg:?}, as the bus takes no more calls");
            }
        }
    }

    /// Have the bus answer `calls`, the calls to it that an instance of busd 0.8 didn't take, as
    /// [`OnItsWay`] says, before any other call, but not before the returned gate is given a
    /// message, or dropped: call this before anyone calls the bus, and let the bus go on once it
    /// passed on what that instance sent.
    ///
    /// Answering them is a chore of the bus, until the bus is done with it.
    #[cfg(target_os = "linux")]
    pub(crate) fn take_over(&self, calls: Vec<Message>) -> mpmc::Sender<()> {
        let (gate, held) = mpmc::bounded(NonZeroUsize::MIN);
        let taken_over = TakenOver { calls, held };
        let job = Job::TakeOver(Box::new(taken_over), self.chores.start());
        if self.queue.try_send(job).is_err() {
            warn!(
                "Dropping the calls to the bus that it was handed over with, as the bus took calls \
                 ahead of them"
            );
        }

        gate
    }

    /// Take no more calls: the bus answers those that are in the queue already, and stops.
    #[cfg(target_os = "linux")]
    pub(crate) fn close(&self) {
        self.queue.close();
    }
}

/// What the bus does, in the order of its queue, each a chore of the bus until it is done.
#[derive(Debug)]
pub(crate) enum Job {
    /// Answer a call.
    Call(Call),
    /// Answer the calls to the bus that an instance of busd 0.8 didn't take: see
    /// [`Calls::take_over`]. It is boxed, as the calls that fill the queue are much smaller.
    #[cfg(target_os = "linux")]
    TakeOver(Box<TakenOver>, Chore),
}

/// A call to the bus, in its queue, which is a chore of the bus until the bus has answered it.
#[derive(Debug)]
pub(crate) struct Call {
    msg: Message,
    _chore: Chore,
}

/// The task that answers the calls to the bus, one at a time, in the order that they come out of
/// the queue.
struct Driver {
    /// The peers of the bus, which the task doesn't keep: it goes once they go.
    peers: WeakPeers,
    /// The GUID of the bus.
    guid: OwnedGuid,
    /// The number of the last call of `GetStats`.
    stats_serial: u32,
    /// The number of the last call of `GetConnectionStats` that the bus answered, which counts
    /// independently of that of `GetStats`, as with the reference implementation of D-Bus.
    connection_stats_serial: u32,
}

impl Driver {
    /// Do the jobs that come out of `queue`, until it is closed and empty, or the peers go.
    async fn run(mut self, queue: mpmc::Receiver<Job>) {
        while let Ok(job) = queue.recv().await {
            let Some(peers) = self.peers.upgrade() else {
                return;
            };
            // Each job is a chore until it is done.
            match job {
                Job::Call(Call { msg, _chore }) => self.answer(&peers, &msg).await,
                #[cfg(target_os = "linux")]
                Job::TakeOver(taken_over, _chore) => self.take_over(&peers, *taken_over).await,
            }
        }
    }

    /// Answer `call`, and send what the call has the bus send, its reply and signals, in the order
    /// that the call has them.
    async fn answer(&mut self, peers: &Peers, call: &Message) {
        let Some(msgs) = self.messages(peers, call).await else {
            return;
        };

        peers.send_from_bus(&msgs).await;
    }

    /// What the bus sends in answer to `call`, in order: the signals that come before the reply,
    /// the reply or the error, unless the call expects no reply, and the signals that follow the
    /// reply. Nothing if even the error fails to be built.
    async fn messages(&mut self, peers: &Peers, call: &Message) -> Option<Vec<Message>> {
        #[cfg(all(test, target_os = "linux"))]
        if let Some(msgs) = hook::answer(peers, call).await {
            return Some(msgs);
        }
        let header = call.header();
        let answer = self.dispatch(peers, call, &header).await;
        let expects_reply = !call
            .primary_header()
            .flags()
            .contains(Flags::NO_REPLY_EXPECTED);
        match answer {
            Ok(Answer {
                before,
                reply,
                after,
            }) => {
                let reply = expects_reply.then_some(reply);

                Some(before.into_iter().chain(reply).chain(after).collect())
            }
            Err(e) if expects_reply => match error(&header, &e) {
                Ok(error) => Some(vec![error]),
                Err(e) => {
                    warn!("Failed to build the error for {call:?}: {e}");

                    None
                }
            },
            Err(_) => Some(Vec::new()),
        }
    }

    /// What the bus sends in answer to `call`, whose header is `header`: what the method of the
    /// call answers, or the error of a call to an object, interface or method that the bus doesn't
    /// have. The checks are those of an object server of zbus, in its order.
    async fn dispatch(
        &mut self,
        peers: &Peers,
        call: &Message,
        header: &Header<'_>,
    ) -> Result<Answer> {
        let Some(path) = header.path() else {
            return Err(Error::Failed("Missing object path".to_string()));
        };
        let Some(interface) = header.interface() else {
            return Err(Error::Failed("Missing interface".to_string()));
        };
        let Some(member) = header.member() else {
            return Err(Error::Failed("Missing member".to_string()));
        };
        if *interface == PEER {
            return peer::answer(header, member);
        }
        let Some(object) = Object::at(path) else {
            return Err(Error::UnknownObject(format!("Unknown object '{path}'")));
        };

        match (interface.as_str(), object) {
            (BUS_NAME, Object::Bus) => self.dbus(peers, call, header, member).await,
            (MONITORING, Object::Bus) => monitoring::answer(peers, call, header, member).await,
            (STATS, Object::Bus) => self.stats(peers, call, header, member).await,
            (INTROSPECTABLE, _) => introspectable::answer(header, member, object),
            (PROPERTIES, _) => properties::answer(call, header, member, object),
            _ => Err(Error::UnknownInterface(format!(
                "Unknown interface '{interface}'"
            ))),
        }
    }
}

/// What the bus sends in answer to a call, in this order: the signals that come before its reply,
/// the reply, and the signals that follow the reply.
struct Answer {
    before: Vec<Message>,
    reply: Message,
    after: Vec<Message>,
}

impl Answer {
    /// The answer that is the reply to the call of `header`, with `body`, alone.
    fn reply<B>(header: &Header<'_>, body: &B) -> Result<Self>
    where
        B: Serialize + DynamicType,
    {
        Ok(Self {
            before: Vec::new(),
            reply: reply(header, body)?,
            after: Vec::new(),
        })
    }
}

/// An object of the bus: one that it serves interfaces on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Object {
    /// `/`.
    Root,
    /// `/org`.
    Org,
    /// `/org/freedesktop`.
    Freedesktop,
    /// The object of the bus, [`PATH`].
    Bus,
}

impl Object {
    /// The object at `path`, if the bus has one there.
    fn at(path: &ObjectPath<'_>) -> Option<Self> {
        match path.as_str() {
            "/" => Some(Self::Root),
            "/org" => Some(Self::Org),
            "/org/freedesktop" => Some(Self::Freedesktop),
            PATH => Some(Self::Bus),
            _ => None,
        }
    }

    /// Whether the object has `interface`: every object has the standard interfaces, and the
    /// object of the bus has the interfaces of the bus too.
    fn has(self, interface: &str) -> bool {
        [INTROSPECTABLE, PEER, PROPERTIES].contains(&interface)
            || (self == Self::Bus && [BUS_NAME, MONITORING, STATS].contains(&interface))
    }

    /// The object right below this one, if any, with the last element of its path.
    fn child(self) -> Option<(&'static str, Self)> {
        match self {
            Self::Root => Some(("org", Self::Org)),
            Self::Org => Some(("freedesktop", Self::Freedesktop)),
            Self::Freedesktop => Some(("DBus", Self::Bus)),
            Self::Bus => None,
        }
    }
}

/// The arguments of the call whose body is `body`, as a method takes them, or the error with which
/// the bus answers a call with other arguments: `InvalidArgs`, with what is wrong with them.
fn args<'b, T>(body: &'b message::Body) -> Result<T>
where
    T: DynamicDeserialize<'b>,
{
    body.deserialize()
        .map_err(|e| Error::InvalidArgs(e.to_string()))
}

/// The reply from the bus to the call of `header`, with `body`.
fn reply<B>(header: &Header<'_>, body: &B) -> Result<Message>
where
    B: Serialize + DynamicType,
{
    Message::method_return(header)
        .sender(BUS_NAME)
        .build(body)
        .map_err(failed)
}

/// The error `error` from the bus, in answer to the call of `header`.
fn error(header: &Header<'_>, error: &Error) -> zbus::Result<Message> {
    let builder = Message::error(header, error.name()).sender(BUS_NAME);

    match error.description() {
        Some(description) => builder.build(&description),
        None => builder.build(&()),
    }
}

/// Fails unless the peer `name`, which sent a call, is privileged, as [`Peers::privileged`] has
/// it: dbus-broker only lets those peers call the methods that this guards, whatever the policy of
/// the configuration says.
async fn ensure_privileged(peers: &Peers, name: &UniqueName<'_>) -> Result<()> {
    match peers.privileged(name).await {
        Some(true) => Ok(()),
        Some(false) => Err(Error::AccessDenied(NOT_PRIVILEGED.to_string())),
        None => Err(Error::NameHasNoOwner(format!("No such peer: {name}"))),
    }
}

/// The description of the error for a peer that is not privileged, worded (grammar included) as
/// dbus-broker words it.
const NOT_PRIVILEGED: &str =
    "The caller does not have the necessary privileged to call this method";

/// The error with which a call fails as the bus fails to build or send what it answers.
fn failed(e: zbus::Error) -> Error {
    Error::Failed(e.to_string())
}

/// The error of a call of `member`, which the interface that the call is of doesn't have.
fn unknown_method(member: &MemberName<'_>) -> Error {
    Error::UnknownMethod(format!("Unknown method '{member}'"))
}

#[cfg(test)]
pub(crate) mod tests;
