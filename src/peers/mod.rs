use std::{
    fmt, iter,
    ops::{Deref, DerefMut},
    sync::{Arc, Weak},
    time::Duration,
};

use anyhow::{anyhow, Result};
use zbus::{connection, names::UniqueName, Message, OwnedGuid};
#[cfg(target_os = "linux")]
use zruntime::Event;
use zruntime::{
    lock::{self, RwLock},
    spawn, unblock,
};

#[cfg(target_os = "linux")]
use crate::debug;
use crate::{
    fdo,
    match_rules::MatchRules,
    peer::{Peer, Recipient, Stream},
    policy::{PeerPolicy, Policy},
    user::{Charge, Users, MAX_OBJECTS},
    warn,
};

mod chores;
mod forward;
#[cfg(target_os = "linux")]
pub(crate) mod handover;
mod outbox;
mod routing;
mod serve;
mod stats;

pub(crate) use chores::{Chore, Chores};
use forward::no_reply;
use outbox::Outbox;
pub use routing::Routing;
use serve::Served;
pub use serve::{MAX_BATCH, MAX_BATCH_BYTES};
pub use stats::{ConnectionCounts, Counts};

/// The peers of the bus, and its monitors and names.
///
/// The bus forwards the messages of each peer from the task that serves the peer, a batch after the
/// other: each batch is the messages that the peer sent by the time the bus gets to them, up to
/// [`MAX_BATCH`] of them and [`MAX_BATCH_BYTES`]. Each recipient gets its messages of a batch in
/// one go, so that its socket takes them with as few writes as it can, in the order that the peer
/// sent them, monitors included. The bus picks the recipients of the messages of a batch while it
/// holds the lock of its [`Routing`], and only sends the messages once it has let go of it, as a
/// send may wait for its recipient to read. It sends the batch to all of its recipients at once, so
/// that those that don't read hold it up for as long as the slowest of them, rather than for as
/// long as all of them together, and goes on to the next batch once each recipient has taken its
/// messages or been disconnected.
///
/// The bus isn't among its peers, but it forwards its own messages, its replies, errors and
/// signals, in the same way, in batches of the same bounds. It forwards each of them from the task
/// that makes it send the message. One such task answers the calls to the bus ([`fdo`]), and sends
/// its answer to each call in order. Another cleans up after a peer that goes, and there are
/// others. The calls to the bus go to the queue of the task that answers them, as the messages for
/// a peer go to its socket.
///
/// A handle: cheap to clone, and all clones refer to the same peers.
#[derive(Clone, Debug)]
pub struct Peers {
    inner: Arc<Inner>,
}

/// A [`Peers`] that doesn't keep the peers alive. The task that answers the calls to the bus holds
/// one, so that the task goes once the peers go.
#[derive(Clone, Debug)]
pub struct WeakPeers {
    inner: Weak<Inner>,
}

impl Peers {
    /// The peers of a bus whose GUID is `guid`, none yet, and the task that answers the calls to
    /// the bus.
    ///
    /// The task goes once the peers go.
    pub fn new(guid: OwnedGuid) -> Self {
        let users = Users::new(MAX_OBJECTS);
        let uid = rustix::process::getuid().as_raw();
        let chores = Chores::default();
        let (calls, queue) = fdo::Calls::new(chores.clone());
        // The bus is of the user of its process, as with dbus-broker.
        let itself = Itself {
            recipient: Recipient::bus(calls),
            served: Served::bus(users.user(uid)),
            policy: PeerPolicy::unrestricted(),
        };
        let peers = Self {
            inner: Arc::new(Inner {
                routing: RwLock::new(Routing::default()),
                itself,
                users,
                uid,
                reload: lock::Mutex::new(None),
                chores,
                #[cfg(target_os = "linux")]
                sealed: Event::new(),
                #[cfg(target_os = "linux")]
                paused_changed: Event::new(),
                #[cfg(all(test, target_os = "linux"))]
                hook: fdo::hook::Hook::default(),
            }),
        };
        peers.answer_calls(guid, queue);

        peers
    }

    /// A handle that doesn't keep the peers alive.
    pub fn downgrade(&self) -> WeakPeers {
        WeakPeers {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Note a chore of the bus, under way until the returned guard is dropped. See
    /// [`Chores::start`].
    pub(crate) fn chore(&self) -> Chore {
        self.inner.chores.start()
    }

    /// The users of the peers, and of the bus itself.
    pub fn users(&self) -> &Users {
        &self.inner.users
    }

    /// Have the bus go by `policy` from now on: for the clients that connect, and for the peers
    /// that it has, each of which gets what `policy` has for it.
    pub async fn set_policy(&self, policy: Arc<Policy>) {
        let mut routing = self.routing_mut().await;
        for peer in routing.peers.values_mut() {
            peer.set_policy(policy.for_peer(peer.user().uid(), peer.groups()));
        }
        routing.policy = policy;
    }

    /// Have [`Peers::reload`] set the policy that `read` returns: `read` reads the configuration of
    /// the bus again, and may block, as it runs on a thread of its own.
    pub async fn set_reload<F>(&self, read: F)
    where
        F: FnMut() -> Result<Policy> + Send + 'static,
    {
        *self.inner.reload.lock().await = Some(Reload(Box::new(read)));
    }

    /// Read the policy of the bus again, as [`Peers::set_reload`] told how to, and have the bus go
    /// by it, as [`Peers::set_policy`] does: nothing if the bus was not told how to.
    ///
    /// The policy is read on a thread of its own, so the bus goes on forwarding the messages of
    /// its peers meanwhile, and a reload waits for the one before it. If reading fails, the bus
    /// goes on by the policy that it has.
    pub async fn reload(&self) -> Option<Result<()>> {
        // Held until the bus goes by what was read, so that reloads take turns.
        let mut reload = self.inner.reload.lock().await;
        let Reload(mut read) = reload.take()?;
        let (read, policy) = unblock(move || {
            let policy = read();

            (read, policy)
        })
        .await;
        *reload = Some(Reload(read));
        let policy = match policy {
            Ok(policy) => policy,
            Err(e) => return Some(Err(e)),
        };
        self.set_policy(Arc::new(policy)).await;

        Some(Ok(()))
    }

    /// Whether the policy of the bus lets a client of `uid`, in the groups `gids`, connect.
    pub async fn may_connect(&self, uid: u32, gids: &[u32]) -> bool {
        self.routing().await.policy.may_connect(uid, gids)
    }

    /// Add a client, of the user of `uid`, which the bus gives `send_timeout` to take each message.
    ///
    /// The client is an object of its user from the start, as with dbus-broker, so the bus refuses
    /// a client beyond the quota of its user before the client authenticates.
    ///
    /// A client whose handshake is still under way once the bus is done pausing, as it hands over,
    /// gets disconnected then, before it has a name: the bus hands over the clients that it paused,
    /// and only those.
    pub async fn add(
        &self,
        guid: &OwnedGuid,
        id: usize,
        builder: connection::Builder<'_>,
        send_timeout: Duration,
        uid: u32,
    ) -> Result<()> {
        let user = self.inner.users.user(uid);
        let charge = Charge::new(&user, &user)
            .ok_or_else(|| anyhow!("the bus keeps as many objects for the user {uid} as it may"))?;
        let _connecting = self.inner.chores.connecting();
        // Perform the connection build and SASL handshake without holding the peers lock, as it
        // involves async I/O and would otherwise block all other peer operations (Hello
        // handling, etc.).
        let handshake = Peer::new(guid.clone(), id, builder, send_timeout, user, charge);
        // Dropping the handshake closes the connection, and lets go of the charge.
        #[cfg(target_os = "linux")]
        let Some(handshake) = self.unless_sealed(handshake).await
        else {
            debug!("Disconnecting a client in the middle of its handshake, as the bus hands over");

            return Ok(());
        };
        #[cfg(not(target_os = "linux"))]
        let handshake = handshake.await;
        let (peer, peer_stream) = handshake?;

        let mut routing = self.routing_mut().await;
        // The bus takes no more clients once it is done pausing, as it hands over. Dropping this
        // one closes its connection.
        #[cfg(target_os = "linux")]
        if routing.sealed {
            debug!(
                "Disconnecting `{}`, as its handshake ended too late for it to be handed over",
                peer.client().unique_name()
            );

            return Ok(());
        }
        self.insert(&mut routing, peer, peer_stream);

        Ok(())
    }

    /// What the bus routes messages by, to read.
    pub async fn routing(&self) -> impl Deref<Target = Routing> + '_ {
        self.inner.routing.read().await
    }

    /// What the bus routes messages by, to change.
    pub async fn routing_mut(&self) -> impl DerefMut<Target = Routing> + '_ {
        self.inner.routing.write().await
    }

    /// Whether the peer `name` is privileged: of root, or of the user that the bus itself runs as,
    /// as dbus-broker has it. Nothing if the bus has no such peer.
    pub async fn privileged(&self, name: &UniqueName<'_>) -> Option<bool> {
        let routing = self.routing().await;
        let uid = routing.peers.get(name.as_str())?.user().uid();

        Some(uid == 0 || uid == self.inner.uid)
    }

    pub async fn make_monitor(&self, peer_name: &UniqueName<'_>, match_rules: MatchRules) -> bool {
        let mut routing = self.routing_mut().await;
        let Some(peer) = routing.remove_peer(peer_name.as_str()) else {
            return false;
        };
        // A monitor makes no calls and answers none, as with a peer that goes. This is done here,
        // as the bus answers the call that makes the monitor, rather than by the task that served
        // the peer, which only gets to it later, so that the bus has sent all that the peer's
        // change of role takes by the time it answered.
        for other in routing.peers.values() {
            other.pending_calls().forget(peer.client().id());
        }
        let waiting = peer.pending_calls().take();
        // From now on, the task that served the peer serves the monitor, once it is done with the
        // peer's messages that it took. Turning the peer into a monitor tells the task so.
        let monitor = peer.become_monitor(match_rules);
        routing
            .monitors
            .insert(monitor.client().unique_name().clone(), monitor);
        drop(routing);
        let errors: Vec<_> = waiting
            .filter_map(|(caller, serial)| match no_reply(&caller, serial) {
                Ok(error) => Some(error),
                Err(e) => {
                    warn!("Failed to build the error for the call of {caller}: {e}");

                    None
                }
            })
            .collect();
        self.send_from_bus(&errors).await;

        true
    }

    /// Send `msgs`, from the bus itself, in order, as the bus forwards the messages of a peer.
    ///
    /// The bus sends them in batches, one after the other. Each batch holds up to [`MAX_BATCH`]
    /// messages, and ends with the message that brings it to [`MAX_BATCH_BYTES`] or more, if any.
    /// The policy has its say on each message, and the monitors get each of them.
    pub(crate) async fn send_from_bus(&self, msgs: &[Message]) {
        for batch in batches(msgs) {
            // A batch that fails to be routed may leave its messages in the outbox, so each batch
            // gets its own outbox.
            let mut outbox = Outbox::default();
            match self
                .route(&self.inner.itself.served, batch, &mut outbox)
                .await
            {
                // The bus makes no calls, which it would refuse itself.
                Ok(rejected) => debug_assert!(rejected.is_empty(), "the bus refused its own calls"),
                Err(e) => warn!("Failed to send {} messages from the bus: {e}", batch.len()),
            }
        }
    }

    /// The gates of the hook of the tests in the bus ([`fdo::hook`]).
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn hook(&self) -> &fdo::hook::Hook {
        &self.inner.hook
    }

    /// Serve `peer`, whose messages come out of `peer_stream`, and add it to the peers of
    /// `routing`.
    ///
    /// A peer added while the bus pauses to hand over pauses right away: the bus takes none of its
    /// messages.
    fn insert(&self, routing: &mut Routing, peer: Peer, peer_stream: Stream) {
        let unique_name = peer.client().unique_name().clone();
        if routing.peers.contains_key(&unique_name) {
            panic!("Unique name `{unique_name}` re-used. We're in deep trouble if this happens");
        }
        #[cfg(target_os = "linux")]
        if routing.handing_over {
            let mut peer = peer;
            let client = peer.client_mut();
            client.pause();
            *client.paused() = Some(peer_stream.into_inner());
            routing.insert_peer(unique_name, peer);

            return;
        }
        let served = Served::new(&peer);
        spawn(
            self.clone()
                .serve_peer(peer_stream, peer.client().listen_cancellation(), served),
        )
        .detach();
        routing.insert_peer(unique_name, peer);
    }
}

impl WeakPeers {
    /// The peers, or `None` if they are gone.
    pub fn upgrade(&self) -> Option<Peers> {
        self.inner.upgrade().map(|inner| Peers { inner })
    }
}

/// The state that the [`Peers`] handles share.
#[derive(Debug)]
struct Inner {
    routing: RwLock<Routing>,
    /// The bus itself, as forwarding has it.
    itself: Itself,
    /// The users of the peers, and of the bus itself.
    users: Users,
    /// The uid of the user that the bus itself runs as.
    uid: u32,
    /// How the bus reads its policy again, if it was told how: see [`Peers::set_reload`]. A reload
    /// holds it until the bus goes by what it read, so that reloads take turns.
    reload: lock::Mutex<Option<Reload>>,
    /// What the bus does on its own, besides forwarding the messages of its peers.
    chores: Chores,
    /// Notified once the bus takes no more clients, as it is done pausing: see [`Peers::add`].
    #[cfg(target_os = "linux")]
    sealed: Event,
    /// Notified as the task that serves a client gives the stream of its messages back, as the bus
    /// pauses, and as a client goes, which a pause waits for: see [`Peers::pause`].
    #[cfg(target_os = "linux")]
    paused_changed: Event,
    /// The gates of the hook of the tests in the bus.
    #[cfg(all(test, target_os = "linux"))]
    hook: fdo::hook::Hook,
}

/// The bus itself, as forwarding has it.
///
/// The bus isn't among its peers, but it is the recipient of the calls to it and the sender of its
/// own messages, as a peer is of its own.
#[derive(Debug)]
struct Itself {
    /// Where the calls to the bus go: the queue of the task that answers them.
    recipient: Recipient,
    /// The bus, as the sender of its replies, errors and signals.
    served: Served,
    /// What the policy has for the bus: it may send and receive everything.
    policy: PeerPolicy,
}

/// How the bus reads its policy again: see [`Peers::set_reload`].
struct Reload(Box<dyn FnMut() -> Result<Policy> + Send>);

impl fmt::Debug for Reload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reload")
    }
}

/// `msgs`, in order, in batches, as the bus forwards the messages of a peer.
///
/// Each batch holds up to [`MAX_BATCH`] messages, and ends with the message that brings it to
/// [`MAX_BATCH_BYTES`] or more, if any.
fn batches(msgs: &[Message]) -> impl Iterator<Item = &[Message]> {
    let mut rest = msgs;

    iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let mut bytes = 0;
        let len = rest
            .iter()
            .take(MAX_BATCH)
            .position(|msg| {
                bytes += msg.data().len();

                bytes >= MAX_BATCH_BYTES
            })
            .map_or(rest.len().min(MAX_BATCH), |last| last + 1);
        let (batch, after) = rest.split_at(len);
        rest = after;

        Some(batch)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bus sends its own messages in batches of the same bounds as those of a peer, one after
    /// the other, in order. A batch holds [`MAX_BATCH`] messages at most, and ends with the message
    /// that brings it to [`MAX_BATCH_BYTES`] or more, if any.
    #[test]
    fn bus_sends_its_messages_in_batches_of_a_peer() {
        let signal = |n: u32, len: usize| {
            Message::signal("/org/busd/Test", "org.busd.Test", "Sent")
                .sender(fdo::BUS_NAME)
                .build(&(n, "x".repeat(len)))
                .unwrap()
        };
        let lens = |msgs: &[Message]| batches(msgs).map(<[_]>::len).collect::<Vec<_>>();

        assert_eq!(lens(&[]), Vec::<usize>::new());
        let small: Vec<_> = (0..40).map(|n| signal(n, 1)).collect();
        assert_eq!(lens(&small), [16, 16, 8]);
        // Each of these takes more than a third of the bytes of a batch, so the third ends it.
        let large: Vec<_> = (0..7).map(|n| signal(n, MAX_BATCH_BYTES / 3)).collect();
        assert_eq!(lens(&large), [3, 3, 1]);
        let all: Vec<_> = batches(&large).flatten().cloned().collect();
        let serial = |msg: &Message| msg.primary_header().serial_num();
        assert!(all.iter().map(serial).eq(large.iter().map(serial)));
    }
}
