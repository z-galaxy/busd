use std::{
    fmt,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, MutexGuard, PoisonError, Weak},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use zbus::{
    connection,
    names::{BusName, UniqueName},
    object_server::SignalEmitter,
    Connection, Message, MessageStream, OwnedGuid,
};
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
    name_registry::NameOwnerChanged,
    peer::{Peer, Stream},
    policy::Policy,
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

use chores::{Chore, Chores};
use forward::no_reply;
pub use routing::Routing;
use serve::Served;
pub use serve::{MAX_BATCH, MAX_BATCH_BYTES};
pub use stats::{ConnectionCounts, Counts};

/// The peers of the bus, and its monitors and names.
///
/// The bus forwards the messages of each peer, including those of the bus itself, from the task
/// that serves the peer, a batch after the other: each batch is the messages that the peer sent by
/// the time the bus gets to them, up to [`MAX_BATCH`] of them and [`MAX_BATCH_BYTES`]. Each
/// recipient gets its messages of a batch in one go, so that its socket takes them with as few
/// writes as it can, in the order that the peer sent them, monitors included. The bus picks the
/// recipients of the messages of a batch while it holds the lock of its [`Routing`], and only
/// sends the messages once it has let go of it, as a send may wait for its recipient to read. It
/// sends the batch to all of its recipients at once, so that those that don't read hold it up for
/// as long as the slowest of them, rather than for as long as all of them together, and goes on to
/// the next batch once each recipient has taken its messages or been disconnected.
///
/// A handle: cheap to clone, and all clones refer to the same peers.
#[derive(Clone, Debug)]
pub struct Peers {
    inner: Arc<Inner>,
}

/// A [`Peers`] that doesn't keep the peers alive. Objects that the peers themselves keep alive use
/// it, such as the interfaces through which the bus serves the D-Bus API.
#[derive(Clone, Debug)]
pub struct WeakPeers {
    inner: Weak<Inner>,
}

impl Peers {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                routing: RwLock::new(Routing::default()),
                bus_conn: Mutex::new(None),
                users: Users::new(MAX_OBJECTS),
                uid: rustix::process::getuid().as_raw(),
                reload: lock::Mutex::new(None),
                chores: Chores::default(),
                #[cfg(target_os = "linux")]
                sealed: Event::new(),
                #[cfg(target_os = "linux")]
                paused_changed: Event::new(),
            }),
        }
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
        for peer in routing.peers.values_mut().filter(|peer| !peer.is_bus()) {
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

    /// Add the bus itself, which serves the D-Bus API on the connection of `us`, and return the
    /// end of the connection on which it answers the calls to it.
    ///
    /// It fails as [`Peer::new_us`] does, and if that end fails to be built.
    pub async fn add_us(&self, us: Us) -> Result<Connection> {
        let mut routing = self.routing_mut().await;

        self.insert_us(&mut routing, us).await
    }

    /// Add the bus itself to the peers of `routing`, as [`Peers::add_us`] does, under the lock of
    /// `routing`, which the caller holds.
    ///
    /// The object server of the bus answers the calls that the bus was handed over with as soon as
    /// its end of the connection is built, and the bus passes what it was handed over with on as
    /// soon as it is added. The methods of the bus, and the task that serves it, wait for the lock
    /// of `routing` for that, so neither goes by anything short of what the caller puts there
    /// before it lets go of the lock.
    async fn insert_us(&self, routing: &mut Routing, us: Us) -> Result<Connection> {
        let Us { service, stream } = us;
        // The user of the process of the bus, as with dbus-broker.
        let user = self.inner.users.user(self.inner.uid);
        let (peer, peer_stream) = Peer::new_us(stream, user).await?;
        let conn = service.build().await?;
        *self.bus_conn() = Some(conn.clone());
        self.insert(routing, peer, peer_stream);
        // Each end of the connection writes what the instance before this one left unwritten
        // there, if anything, ahead of what it sends.
        #[cfg(target_os = "linux")]
        if let Some(peer) = routing.peers.get_mut(fdo::BUS_NAME) {
            peer.flush_us(&conn);
        }

        Ok(conn)
    }

    /// Let go of the connection on which the bus serves the D-Bus API, as the bus goes.
    ///
    /// What the bus sends on that connection comes back to it as the messages of its own peer, so
    /// holding on to the connection would keep the task that serves that peer, and with it the
    /// peers, from ever ending.
    pub fn remove_us(&self) {
        self.bus_conn().take();
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
        for (caller, serial) in waiting {
            match no_reply(&caller, serial) {
                Ok(error) => self.send_from_bus(&error).await,
                Err(e) => warn!("Failed to build the error for the call of {caller}: {e}"),
            }
        }

        true
    }

    /// Signal a change of a name's owner: `NameOwnerChanged` to all, then `NameLost` to the old
    /// owner, if any, and `NameAcquired` to the new owner, if any.
    ///
    /// The signals go out on the bus's connection, like its replies and its other signals, so that
    /// they all reach each recipient in the order in which the bus sends them. It stops at the
    /// first signal that fails to go out.
    pub async fn notify_name_changes(&self, changed: NameOwnerChanged) -> Result<()> {
        let bus = self.bus_conn().clone().context("The bus isn't up")?;
        let emitter = SignalEmitter::new(&bus, fdo::DBus::PATH)?;
        let name = BusName::from(changed.name());
        let old_owner = changed.old_owner().map(UniqueName::from);
        let new_owner = changed.new_owner().map(UniqueName::from);

        // First broadcast the name change signal.
        fdo::DBus::name_owner_changed(
            &emitter,
            name.clone(),
            old_owner.clone().into(),
            new_owner.clone().into(),
        )
        .await?;

        // Now unicast the appropriate signal to the old and new owners.
        if let Some(old_owner) = old_owner {
            let emitter = emitter.clone().set_destination(old_owner.into());
            fdo::DBus::name_lost(&emitter, name.clone()).await?;
        }
        if let Some(new_owner) = new_owner {
            let emitter = emitter.set_destination(new_owner.into());
            fdo::DBus::name_acquired(&emitter, name).await?;
        }

        Ok(())
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

    /// Send `msg` on the connection of the bus, as the bus, unless the bus is going.
    async fn send_from_bus(&self, msg: &Message) {
        let Some(bus) = self.bus_conn().clone() else {
            return;
        };
        if let Err(e) = bus.send(msg).await {
            warn!("Failed to send {msg:?} from the bus: {e}");
        }
    }

    fn bus_conn(&self) -> MutexGuard<'_, Option<Connection>> {
        // Nothing can panic while the lock is held.
        self.inner
            .bus_conn
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for Peers {
    fn default() -> Self {
        Self::new()
    }
}

impl WeakPeers {
    /// The peers, or `None` if they are gone.
    pub fn upgrade(&self) -> Option<Peers> {
        self.inner.upgrade().map(|inner| Peers { inner })
    }
}

/// The connection on which the bus serves the D-Bus API as a peer of its own, for the bus to be
/// added to its peers ([`Peers::add_us`]): a new one, or the one that the instance of the bus
/// before this one handed over, with what was on its way between the bus and its clients.
#[derive(Debug)]
pub struct Us {
    /// The end on which the bus answers the calls to it, yet to be built.
    pub service: connection::Builder<'static>,
    /// The stream of the messages of the other end: what the bus sends comes out of it.
    pub stream: MessageStream,
}

/// The state that the [`Peers`] handles share.
#[derive(Debug)]
struct Inner {
    routing: RwLock<Routing>,
    /// The connection on which the bus serves the D-Bus API, from when the bus is among its peers
    /// until the bus goes.
    bus_conn: Mutex<Option<Connection>>,
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
}

/// How the bus reads its policy again: see [`Peers::set_reload`].
struct Reload(Box<dyn FnMut() -> Result<Policy> + Send>);

impl fmt::Debug for Reload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reload")
    }
}
