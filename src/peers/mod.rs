use std::{
    fmt,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use zbus::{
    connection,
    names::{BusName, UniqueName},
    Connection, Message, MessageStream, Optional, OwnedGuid,
};
use zruntime::{
    lock::{self, RwLock},
    spawn, unblock, Event,
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

use chores::{Chores, Connecting};
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
#[derive(Debug)]
pub struct Peers {
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
    chores: Mutex<Chores>,
    /// Notified on each change of `chores`.
    chores_changed: Event,
    /// Notified once the bus takes no more clients, as it is done pausing: see [`Peers::add`].
    #[cfg(target_os = "linux")]
    sealed: Event,
    /// Notified as the task that serves a client gives the stream of its messages back, as the bus
    /// pauses, and as a client goes, which a pause waits for: see [`Peers::pause`].
    #[cfg(target_os = "linux")]
    paused_changed: Event,
}

impl Peers {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routing: RwLock::new(Routing::default()),
            bus_conn: Mutex::new(None),
            users: Users::new(MAX_OBJECTS),
            uid: rustix::process::getuid().as_raw(),
            reload: lock::Mutex::new(None),
            chores: Mutex::new(Chores::default()),
            chores_changed: Event::new(),
            #[cfg(target_os = "linux")]
            sealed: Event::new(),
            #[cfg(target_os = "linux")]
            paused_changed: Event::new(),
        })
    }

    /// The users of the peers, and of the bus itself.
    pub fn users(&self) -> &Users {
        &self.users
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
        F: Fn() -> Result<Policy> + Send + Sync + 'static,
    {
        *self.reload.lock().await = Some(Reload(Arc::new(read)));
    }

    /// Read the policy of the bus again, as [`Peers::set_reload`] told how to, and have the bus go
    /// by it, as [`Peers::set_policy`] does: nothing if the bus was not told how to.
    ///
    /// The policy is read on a thread of its own, so the bus goes on forwarding the messages of
    /// its peers meanwhile, and a reload waits for the one before it. If reading fails, the bus
    /// goes on by the policy that it has.
    pub async fn reload(&self) -> Option<Result<()>> {
        // Held until the bus goes by what was read, so that reloads take turns.
        let reload = self.reload.lock().await;
        let Reload(read) = reload.clone()?;
        let policy = match unblock(move || read()).await {
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
        self: &Arc<Self>,
        guid: &OwnedGuid,
        id: usize,
        builder: connection::Builder<'_>,
        send_timeout: Duration,
        uid: u32,
    ) -> Result<()> {
        let user = self.users.user(uid);
        let charge = Charge::new(&user, &user)
            .ok_or_else(|| anyhow!("the bus keeps as many objects for the user {uid} as it may"))?;
        let _connecting = Connecting::new(self);
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
                peer.unique_name()
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
    pub async fn add_us(self: &Arc<Self>, us: Us) -> Result<Connection> {
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
    async fn insert_us(self: &Arc<Self>, routing: &mut Routing, us: Us) -> Result<Connection> {
        let Us { service, stream } = us;
        // The user of the process of the bus, as with dbus-broker.
        let user = self.users.user(self.uid);
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
        self.routing.read().await
    }

    /// What the bus routes messages by, to change.
    pub async fn routing_mut(&self) -> impl DerefMut<Target = Routing> + '_ {
        self.routing.write().await
    }

    /// Whether the peer `name` is privileged: of root, or of the user that the bus itself runs as,
    /// as dbus-broker has it. Nothing if the bus has no such peer.
    pub async fn privileged(&self, name: &UniqueName<'_>) -> Option<bool> {
        let routing = self.routing().await;
        let uid = routing.peers.get(name.as_str())?.user().uid();

        Some(uid == 0 || uid == self.uid)
    }

    pub async fn make_monitor(
        self: &Arc<Self>,
        peer_name: &UniqueName<'_>,
        match_rules: MatchRules,
    ) -> bool {
        let mut routing = self.routing_mut().await;
        let Some(peer) = routing.remove_peer(peer_name.as_str()) else {
            return false;
        };
        // A monitor makes no calls and answers none, as with a peer that goes. This is done here,
        // as the bus answers the call that makes the monitor, rather than by the task that served
        // the peer, which only gets to it later, so that the bus has sent all that the peer's
        // change of role takes by the time it answered.
        for other in routing.peers.values() {
            other.pending_calls().forget(peer.id());
        }
        let waiting = peer.pending_calls().take();
        // The task that served the peer serves the monitor from now on, once it is done with what
        // it took of the peer: dropping the peer tells it.
        let monitor = peer.become_monitor(match_rules);
        routing
            .monitors
            .insert(monitor.unique_name().clone(), monitor);
        drop(routing);
        for (caller, serial) in waiting {
            match no_reply(&caller, serial) {
                Ok(error) => self.send_from_bus(&error).await,
                Err(e) => warn!("Failed to build the error for the call of {caller}: {e}"),
            }
        }

        true
    }

    /// Signal a change of the owner of a name.
    ///
    /// The signals go out on the connection of the bus, like its replies and its other signals,
    /// so that they all reach each recipient in the order that the bus sends them.
    pub async fn notify_name_changes(&self, name_owner_changed: NameOwnerChanged) -> Result<()> {
        let bus = self.bus_conn().clone().context("The bus isn't up")?;
        let name = BusName::from(name_owner_changed.name);
        let old_owner = name_owner_changed.old_owner.map(UniqueName::from);
        let new_owner = name_owner_changed.new_owner.map(UniqueName::from);

        // First broadcast the name change signal.
        let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameOwnerChanged")
            .sender(fdo::BUS_NAME)
            .build(&(
                &name,
                Optional::from(old_owner.clone()),
                Optional::from(new_owner.clone()),
            ))?;
        bus.send(&msg).await?;

        // Now unicast the appropriate signal to the old and new owners.
        if let Some(old_owner) = old_owner {
            let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameLost")
                .sender(fdo::BUS_NAME)
                .destination(old_owner)
                .build(&name)?;
            bus.send(&msg).await?;
        }
        if let Some(new_owner) = new_owner {
            let msg = Message::signal(fdo::DBus::PATH, fdo::DBus::INTERFACE, "NameAcquired")
                .sender(fdo::BUS_NAME)
                .destination(new_owner)
                .build(&name)?;
            bus.send(&msg).await?;
        }

        Ok(())
    }

    /// Serve `peer`, whose messages come out of `peer_stream`, and add it to the peers of
    /// `routing`.
    ///
    /// A peer added while the bus pauses to hand over pauses right away: the bus takes none of its
    /// messages.
    fn insert(self: &Arc<Self>, routing: &mut Routing, peer: Peer, peer_stream: Stream) {
        let unique_name = peer.unique_name().clone();
        if routing.peers.contains_key(&unique_name) {
            panic!("Unique name `{unique_name}` re-used. We're in deep trouble if this happens");
        }
        #[cfg(target_os = "linux")]
        if routing.handing_over {
            let mut peer = peer;
            peer.pause();
            *peer.paused() = Some(peer_stream.into_inner());
            routing.insert_peer(unique_name, peer);

            return;
        }
        let served = Served::new(&peer);
        spawn(
            self.clone()
                .serve_peer(peer_stream, peer.listen_cancellation(), served),
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
        self.bus_conn.lock().unwrap_or_else(PoisonError::into_inner)
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

/// How the bus reads its policy again: see [`Peers::set_reload`].
#[derive(Clone)]
struct Reload(Arc<dyn Fn() -> Result<Policy> + Send + Sync>);

impl fmt::Debug for Reload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reload")
    }
}
