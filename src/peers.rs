use crate::{debug, trace, warn};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{
    future::{join_all, select, Either},
    stream::StreamExt,
};
use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap},
    future::poll_fn,
    num::NonZeroU32,
    ops::{Deref, DerefMut, Range},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    task::Poll,
    time::Duration,
};
use zbus::{
    connection, message,
    names::{BusName, OwnedUniqueName, UniqueName},
    Connection, Message, MessageStream, Optional, OwnedGuid, OwnedMatchRule,
};
use zruntime::{lock::RwLock, spawn, Event, EventListener};

#[cfg(target_os = "linux")]
mod handover;

use crate::{
    fdo,
    match_rules::MatchRules,
    name_registry::{NameOwnerChanged, NameRegistry},
    peer::{Caller, Invalid, Monitor, Peer, PendingCalls, Recipient, Refusal, Stream},
    policy::{Facts, Policy},
    user::{Charge, Users, MAX_OBJECTS},
};

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

    /// Add the bus itself, which serves the D-Bus API on `conn`: what the bus sends on it comes out
    /// of `msg_stream`.
    ///
    /// It fails as [`Peer::new_us`] does.
    pub async fn add_us(
        self: &Arc<Self>,
        msg_stream: zbus::MessageStream,
        conn: Connection,
    ) -> Result<()> {
        *self.bus_conn() = Some(conn);
        // The user of the process of the bus, as with dbus-broker.
        let user = self.users.user(self.uid);
        let (peer, peer_stream) = Peer::new_us(msg_stream, user).await?;

        self.insert(&mut *self.routing_mut().await, peer, peer_stream);

        Ok(())
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

    /// What the bus counts for the `GetStats` method of its `org.freedesktop.DBus.Debug.Stats`
    /// interface.
    ///
    /// This goes over all the peers, once, under the read lock of the routing.
    pub async fn counts(&self) -> Counts {
        let routing = self.routing().await;
        let mut greeted = 0;
        let mut not_greeted = 0;
        for (name, peer) in routing.peers.iter() {
            if *name == fdo::BUS_NAME {
                continue;
            }
            if peer.greeted() {
                greeted += 1;
            } else {
                not_greeted += 1;
            }
        }
        let connecting = self.chores().connecting;
        let match_rules = routing.match_rule_counts;

        Counts {
            active_connections: greeted + routing.monitors.len(),
            incomplete_connections: connecting + not_greeted,
            match_rules: match_rules.total,
            peak_match_rules: match_rules.peak_total,
            peak_match_rules_per_connection: match_rules.peak_per_peer,
            bus_names: routing.name_registry.all_names().len(),
            peak_bus_names: routing.name_registry.peak_names(),
            peak_bus_names_per_connection: routing.name_registry.peak_names_per_owner(),
        }
    }

    /// What the bus counts for the peer that `name` is the unique name of, or that owns `name`,
    /// if there is one, for the `GetConnectionStats` method of its
    /// `org.freedesktop.DBus.Debug.Stats` interface.
    ///
    /// A monitor is no peer, so there is nothing for its name. A client that has yet to say
    /// `Hello` is a peer like any other for this, as it is for `GetNameOwner`.
    pub async fn connection_counts(&self, name: &BusName<'_>) -> Option<ConnectionCounts> {
        let routing = self.routing().await;
        let unique_name = match name {
            BusName::Unique(name) => name.as_str(),
            BusName::WellKnown(name) => routing.name_registry.lookup(name)?.as_str(),
        };
        let (unique_name, peer) = routing.peers.get_key_value(unique_name)?;
        let owned = routing.name_registry.owned_by(unique_name.as_str());

        Some(ConnectionCounts {
            unique_name: unique_name.clone(),
            match_rules: peer.match_rules().len(),
            peak_match_rules: peer.peak_match_rules(),
            bus_names: owned.now,
            peak_bus_names: owned.peak,
        })
    }

    /// The match rules of each peer but the bus itself, by the unique name of the peer, for the
    /// `GetAllMatchRules` method of the `org.freedesktop.DBus.Debug.Stats` interface.
    ///
    /// A peer without match rules has an empty list, and a monitor has none, as it is no peer. A
    /// client that has yet to say `Hello` is listed like any other peer, as `ListNames` lists it.
    /// This goes over all the peers and their rules, once, under the read lock of the routing.
    pub async fn all_match_rules(&self) -> HashMap<OwnedUniqueName, Vec<OwnedMatchRule>> {
        let routing = self.routing().await;

        routing
            .peers
            .iter()
            .filter(|(name, _)| **name != fdo::BUS_NAME)
            .map(|(name, peer)| (name.clone(), peer.match_rules().iter().cloned().collect()))
            .collect()
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

    /// Note a chore of the bus, under way until the returned guard goes.
    ///
    /// A chore is work that the bus does on its own, besides forwarding the messages of its peers,
    /// and that sends messages, such as the signals that it sends once it answered a call, or its
    /// clean-up after a peer that goes. A hand-over waits for them.
    pub(crate) fn chore(self: &Arc<Self>) -> Chore {
        let mut chores = self.chores();
        chores.running += 1;
        chores.started += 1;

        Chore(self.clone())
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

    /// Serve `peer`, whose messages come out of `peer_stream`, until it goes, and then the monitor
    /// that it becomes, if it becomes one.
    async fn serve_peer(
        self: Arc<Self>,
        mut peer_stream: Stream,
        mut cancellation_listener: EventListener,
        peer: Served,
    ) -> Result<()> {
        let Served {
            unique_name,
            id,
            pending_calls,
            caller,
        } = peer;
        #[cfg(target_os = "linux")]
        let from_bus = *unique_name == fdo::BUS_NAME;
        let mut batch = Vec::new();
        let mut outbox = Outbox::default();
        // Whether the peer became a monitor, or the bus pauses to hand over, rather than the peer
        // went.
        let canceled = loop {
            let msg = match select(cancellation_listener, peer_stream.next()).await {
                Either::Left(_) => break true,
                Either::Right((None, _)) => break false,
                Either::Right((Some(msg), listener)) => {
                    cancellation_listener = listener;

                    msg
                }
            };
            // The messages that the peer sent after this one go along with it, as far as they are
            // here already.
            let mut next = Some(msg);
            let mut bytes = 0;
            let mut violation = None;
            let mut ended = false;
            while let Some(msg) = next.take() {
                match msg {
                    Ok(msg) => {
                        bytes += msg.data().len();
                        batch.push(msg);
                    }
                    Err(Invalid::Dropped(e)) => debug!("{e}"),
                    Err(invalid) => {
                        violation = Some(invalid);

                        break;
                    }
                }
                if batch.len() == MAX_BATCH || bytes >= MAX_BATCH_BYTES {
                    break;
                }
                // Polled with the waker of this task, which the stream keeps if it has nothing yet,
                // for the wait for the next batch.
                match poll_fn(|cx| Poll::Ready(peer_stream.poll_next_unpin(cx))).await {
                    Poll::Ready(Some(msg)) => next = Some(msg),
                    Poll::Ready(None) => ended = true,
                    Poll::Pending => (),
                }
            }

            // The replies to the barriers of a hand-over come back to the bus itself, which they
            // end the wait of once the messages of the bus before them are on their way too.
            #[cfg(target_os = "linux")]
            let barrier = if from_bus {
                handover::take_barriers(&mut batch)
            } else {
                None
            };
            self.forward(
                &unique_name,
                id,
                &caller,
                &pending_calls,
                &batch,
                &mut outbox,
            )
            .await?;
            batch.clear();
            #[cfg(target_os = "linux")]
            if let Some(serial) = barrier {
                self.barrier_returned(serial);
            }
            if let Some(violation) = violation {
                warn!("Disconnecting `{unique_name}`, as it sent {violation}");

                break false;
            }
            if ended {
                break false;
            }
        };
        if canceled {
            self.serve_on(peer_stream.into_inner(), unique_name, id)
                .await;

            return Ok(());
        }
        trace!("Peer `{}` disconnected", unique_name);

        // The bus sends what follows on its own, which a hand-over has to wait for.
        let _chore = self.chore();
        // Stream is done means the peer disconnected, unless the peer sent a message that gets it
        // disconnected. Remove it from the list of peers, or from that of the monitors if it became
        // one meanwhile: that, and the end of its stream, close its connection.
        {
            let mut routing = self.routing_mut().await;
            if routing.remove_peer(unique_name.as_str()).is_none() {
                // The bus cleaned up after the peer as it turned it into a monitor, and the
                // `Monitoring` interface emits the signals then.
                routing.monitors.remove(&unique_name);
                // This task may have forwarded calls that the peer sent after the one that made it
                // a monitor, which can't get their replies to it either.
                for peer in routing.peers.values() {
                    peer.pending_calls().forget(id);
                }
                // A pause waits for the monitor no more.
                #[cfg(target_os = "linux")]
                self.paused_changed.notify(usize::MAX);

                return Ok(());
            }
        }
        // A pause waits for the peer no more.
        #[cfg(target_os = "linux")]
        self.paused_changed.notify(usize::MAX);

        self.clean_up(&unique_name, id, pending_calls.take().collect())
            .await
    }

    /// Go on with the peer `unique_name`, numbered `id`, whose messages come out of `stream`, now
    /// that the task that served it was canceled: serve the monitor that it became, unless the bus
    /// pauses to hand over, which takes the stream back.
    async fn serve_on(
        self: Arc<Self>,
        stream: MessageStream,
        unique_name: OwnedUniqueName,
        id: usize,
    ) {
        let Some((stream, listener)) = self.monitor_to_serve(stream, &unique_name, id).await else {
            return;
        };

        self.serve_monitor(stream, listener, unique_name).await;
    }

    /// What the task that served the peer `unique_name`, numbered `id`, whose messages come out of
    /// `stream`, serves the monitor that the peer became with: `stream`, and the listener of the
    /// cancellation of the task, taken under the lock that a pause takes, so that the task hears of
    /// any pause that it missed. Nothing if the peer is gone, or if the bus pauses to hand over,
    /// which takes `stream` back, as it does if the peer is still one.
    async fn monitor_to_serve(
        &self,
        stream: MessageStream,
        unique_name: &OwnedUniqueName,
        id: usize,
    ) -> Option<(MessageStream, EventListener)> {
        let mut guard = self.routing_mut().await;
        let routing = &mut *guard;
        #[cfg(target_os = "linux")]
        if let Some(peer) = routing.peers.get_mut(unique_name) {
            *peer.paused() = Some(stream);
            self.paused_changed.notify(usize::MAX);

            return None;
        }
        let monitor = routing.monitors.get_mut(unique_name)?;
        // This task may have forwarded calls that the peer sent after the one that made it a
        // monitor, as the bus cleaned up after it, which can't get their replies to it either.
        for peer in routing.peers.values() {
            peer.pending_calls().forget(id);
        }
        #[cfg(target_os = "linux")]
        if routing.handing_over {
            *monitor.paused() = Some(stream);
            self.paused_changed.notify(usize::MAX);

            return None;
        }

        Some((stream, monitor.listen_cancellation()))
    }

    /// Serve the monitor `unique_name`, whose messages come out of `stream`, until it goes, or
    /// until the bus pauses to hand over, which `cancellation_listener` tells.
    ///
    /// A monitor is to send nothing, so it gets disconnected as soon as it sends anything.
    async fn serve_monitor(
        self: Arc<Self>,
        mut stream: MessageStream,
        cancellation_listener: EventListener,
        unique_name: OwnedUniqueName,
    ) {
        let msg = match select(cancellation_listener, stream.next()).await {
            Either::Left(_) => None,
            Either::Right((msg, _)) => Some(msg),
        };
        let Some(msg) = msg else {
            // The bus pauses to hand over, which takes the stream back, unless it let go of the
            // monitor.
            #[cfg(target_os = "linux")]
            if let Some(monitor) = self.routing_mut().await.monitors.get_mut(&unique_name) {
                *monitor.paused() = Some(stream);
                self.paused_changed.notify(usize::MAX);
            }

            return;
        };
        if let Some(Ok(_)) = msg {
            warn!(
                "Monitor {} sent a message, which is against the rules.",
                unique_name
            );
        }
        // Removing the monitor, and the end of its stream, close its connection.
        self.routing_mut().await.monitors.remove(&unique_name);
        // A pause waits for the monitor no more.
        #[cfg(target_os = "linux")]
        self.paused_changed.notify(usize::MAX);
        debug!("Monitor {} disconnected", unique_name);
    }

    /// Clean up after the peer `unique_name`, numbered `id`, which went, and whose calls that
    /// waited for its replies were those of `waiting`: the caller and the serial of each.
    async fn clean_up(
        &self,
        unique_name: &OwnedUniqueName,
        id: usize,
        waiting: Vec<(OwnedUniqueName, NonZeroU32)>,
    ) -> Result<()> {
        // The calls that the peer made won't get their replies to it now.
        for peer in self.routing().await.peers.values() {
            peer.pending_calls().forget(id);
        }
        let names_changes = self
            .routing_mut()
            .await
            .name_registry
            .release_all(unique_name.inner().clone())
            .await;

        self.finish_clean_up(unique_name, names_changes, waiting)
            .await
    }

    /// Finish cleaning up after the peer `unique_name`, which went, and whose names changed owners
    /// with `names_changes`, as they were released: signal the changes, and answer the calls that
    /// waited for its replies, `waiting`, with errors.
    async fn finish_clean_up(
        &self,
        unique_name: &OwnedUniqueName,
        names_changes: Vec<NameOwnerChanged>,
        waiting: Vec<(OwnedUniqueName, NonZeroU32)>,
    ) -> Result<()> {
        for changed in names_changes {
            self.notify_name_changes(changed).await?;
        }
        self.notify_name_changes(NameOwnerChanged {
            name: BusName::from(unique_name.clone()).into(),
            old_owner: Some(unique_name.clone()),
            new_owner: None,
        })
        .await?;
        // Nor will the calls that wait on the peer get its replies, so they get an error from the
        // bus instead, as with dbus-broker, rather than wait for as long as their callers do.
        for (caller, serial) in waiting {
            let error = no_reply(&caller, serial)?;
            self.send_from_bus(&error).await;
        }

        Ok(())
    }

    /// Forward `msgs`, which the peer `sender`, numbered `sender_id`, sent in this order, to their
    /// recipients, through the empty `outbox` of the task that serves `sender`. `caller` is
    /// `sender`, as the caller of its calls that wait for replies.
    ///
    /// Each recipient gets those of `msgs` that go to it in one go, in order, and all of them get
    /// theirs at once.
    ///
    /// A call that expects a reply is recorded as one that waits on its callee as it goes to the
    /// callee, unless its caller has a call of the same serial waiting there already, or the user
    /// of its caller has as many of the objects of the user of the callee as it may, in which case
    /// the bus refuses it, with an error, as dbus-broker does. A reply, or an error, of `sender`
    /// only goes on to its destination if it answers one of the calls that wait on `sender`,
    /// `pending_calls`. The calls to the bus are recorded too, so that a hand-over can wait for the
    /// bus to answer them, but the bus never refuses them, nor counts them against any user, and
    /// its own replies go on whatever they answer.
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
    async fn forward(
        &self,
        sender: &OwnedUniqueName,
        sender_id: usize,
        caller: &Arc<Caller>,
        pending_calls: &PendingCalls,
        msgs: &[Message],
        outbox: &mut Outbox,
    ) -> Result<()> {
        let from_bus = *sender == fdo::BUS_NAME;
        // The calls that the bus answers with an error of its own, and why.
        let mut rejected = Vec::new();
        // The recipients are picked under the lock, and only sent their messages without it.
        {
            let routing = self.routing().await;
            let Routing {
                peers,
                monitors,
                name_registry,
                ..
            } = &*routing;
            // The sender is a peer no more if it became a monitor meanwhile.
            let sender_peer = peers.get(sender.as_str());
            for (i, msg) in msgs.iter().enumerate() {
                if msg.message_type() == message::Type::Signal {
                    trace!("Broadcasting message: {:?}", msg);
                    if let Some(sender_peer) = sender_peer {
                        // Parsed only for the policy to look at, if it has to.
                        let mut header = None;
                        for (name, peer) in peers.iter() {
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

                            outbox.add(msgs, peer.id(), peer.recipient(), i);
                        }
                    } else {
                        trace!("Dropping {msg:?}, as `{sender}` is a peer no more");
                    }
                } else {
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
                    // A reply takes the record of the call that it answers away. One of the bus,
                    // which records the calls to it too, goes on whatever it answers.
                    let answers_none =
                        matches!(msg_type, message::Type::MethodReturn | message::Type::Error)
                            && !match (peer, header.reply_serial()) {
                                (Some((_, peer)), Some(serial)) => {
                                    pending_calls.remove(peer.id(), serial)
                                }
                                _ => false,
                            }
                            && !from_bus;
                    let primary = msg.primary_header();
                    let flags = primary.flags();
                    let is_call = msg_type == message::Type::MethodCall;
                    let expects_reply =
                        is_call && !flags.contains(message::Flags::NO_REPLY_EXPECTED);
                    // The policy has its say on a call before the call waits for its reply.
                    let denied = match (peer, sender_peer) {
                        (Some((_, peer)), Some(sender_peer)) if is_call => denial(
                            sender_peer,
                            peer,
                            || Facts::new(msg, &header),
                            name_registry,
                        ),
                        _ => None,
                    };
                    match (peer, denied) {
                        _ if answers_none => {
                            debug!("{msg:?} answers no call that waits on {sender}")
                        }
                        _ if is_call && sender_peer.is_none() => {
                            trace!("Dropping {msg:?}, as `{sender}` is a peer no more")
                        }
                        (Some(_), Some(rejection)) => {
                            if expects_reply {
                                rejected.push((msg.clone(), rejection));
                            }
                        }
                        (Some((name, peer)), None) => {
                            let refusal = if expects_reply {
                                peer.pending_calls()
                                    .add(sender_id, caller, primary.serial_num())
                                    .err()
                            } else {
                                None
                            };
                            match refusal {
                                Some(refusal) if *name != fdo::BUS_NAME => {
                                    rejected.push((msg.clone(), Rejection::from(refusal)))
                                }
                                _ => outbox.add(msgs, peer.id(), peer.recipient(), i),
                            }
                        }
                        (None, _) => {
                            debug!("no peer for destination `{destination}`");
                            // The bus starts no services, so no name is one that a service can be
                            // started for.
                            if expects_reply {
                                let rejection = if flags.contains(message::Flags::NO_AUTO_START) {
                                    Rejection::NameHasNoOwner
                                } else {
                                    Rejection::ServiceUnknown
                                };
                                rejected.push((msg.clone(), rejection));
                            }
                        }
                    }
                }
                if monitors.is_empty() {
                    continue;
                }
                trace!(
                    "Broadcasting message to {} monitors: {:?}",
                    monitors.len(),
                    msg
                );
                for (name, monitor) in monitors.iter() {
                    if !monitor.interested(msg, name_registry) {
                        trace!("Monitor {} not interested in {msg:?}", name);
                        continue;
                    }

                    outbox.add(msgs, monitor.id(), monitor.recipient(), i);
                }
            }
        }

        let dropped = outbox.send(msgs).await;
        // A call that its callee can't take, for its file descriptors, waits for no reply, and gets
        // an error from the bus instead, as with dbus-broker.
        if !dropped.is_empty() {
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
                    .remove(sender_id, primary.serial_num())
                {
                    rejected.push((msg, Rejection::UnexpectedFds));
                }
            }
        }
        for (msg, rejection) in rejected {
            let (name, description) = rejection.error();
            let error = Message::error(&msg.header(), name)
                .sender(fdo::BUS_NAME)
                .build(&description)?;
            self.send_from_bus(&error).await;
        }

        Ok(())
    }

    fn chores(&self) -> MutexGuard<'_, Chores> {
        // Nothing can panic while the lock is held.
        self.chores.lock().unwrap_or_else(PoisonError::into_inner)
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

/// What the bus counts for the `GetStats` method of its `org.freedesktop.DBus.Debug.Stats`
/// interface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The clients that called `Hello`, monitors included.
    pub active_connections: usize,
    /// The clients that are connected but did not call `Hello` yet.
    pub incomplete_connections: usize,
    /// The match rules that the peers have between them.
    pub match_rules: usize,
    /// The most match rules that the peers had between them at a time.
    pub peak_match_rules: usize,
    /// The most match rules that one peer had at a time.
    pub peak_match_rules_per_connection: usize,
    /// The well-known names that have an owner.
    pub bus_names: usize,
    /// The most well-known names that had an owner at a time.
    pub peak_bus_names: usize,
    /// The most well-known names that one peer owned at a time.
    pub peak_bus_names_per_connection: usize,
}

/// What the bus counts for one peer: see [`Peers::connection_counts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionCounts {
    /// The unique name of the peer.
    pub unique_name: OwnedUniqueName,
    /// The match rules that the peer has.
    pub match_rules: usize,
    /// The most match rules that the peer had at a time.
    pub peak_match_rules: usize,
    /// The well-known names that the peer owns.
    pub bus_names: usize,
    /// The most well-known names that the peer owned at a time.
    pub peak_bus_names: usize,
}

/// What the bus routes messages by: its peers, its monitors and the owners of its names.
///
/// They are all behind one lock, so that the bus takes one lock to pick the recipients of a batch.
#[derive(Debug, Default)]
pub struct Routing {
    /// The peers, by their unique names.
    ///
    /// They are hashed, with foldhash, so that finding the peer that a message goes to takes one
    /// hash of the name and one comparison with the name of the peer, rather than kept in a tree,
    /// which would compare the name with the keys of each node on the way, every comparison
    /// through the `:busd.` that the unique names of all clients start with.
    pub peers: HashMap<OwnedUniqueName, Peer, foldhash::fast::RandomState>,
    /// The owners of the well-known names, and those that queue for them.
    pub name_registry: NameRegistry,
    /// The monitors, by their unique names.
    monitors: BTreeMap<OwnedUniqueName, Monitor>,
    /// Whether the bus is handing over to its next instance, so that it pauses the clients that
    /// connect, and takes none of their messages.
    #[cfg(target_os = "linux")]
    handing_over: bool,
    /// Whether the bus takes no more clients, as it is done pausing those that it hands over: see
    /// [`Peers::add`].
    #[cfg(target_os = "linux")]
    sealed: bool,
    /// How many match rules the peers have, kept up to date by the methods that change them, so
    /// that counting them takes no pass over the peers.
    match_rule_counts: MatchRuleCounts,
    /// The policy of the bus, which each peer but the bus itself gets its part of as it is added.
    policy: Arc<Policy>,
}

impl Routing {
    /// Add `rule` to the match rules of the peer `name`.
    pub fn add_match_rule(
        &mut self,
        name: &UniqueName<'_>,
        rule: OwnedMatchRule,
    ) -> zbus::fdo::Result<()> {
        let peer = self
            .peers
            .get_mut(name.as_str())
            .ok_or_else(|| zbus::fdo::Error::NameHasNoOwner(format!("No such peer: {name}")))?;
        if peer.add_match_rule(rule) {
            let rules = peer.match_rules().len();
            self.match_rule_counts.add(1, rules);
        }

        Ok(())
    }

    /// Remove `rule` from the match rules of the peer `name`.
    pub fn remove_match_rule(
        &mut self,
        name: &UniqueName<'_>,
        rule: OwnedMatchRule,
    ) -> zbus::fdo::Result<()> {
        let peer = self
            .peers
            .get_mut(name.as_str())
            .ok_or_else(|| zbus::fdo::Error::NameHasNoOwner(format!("No such peer: {name}")))?;
        peer.remove_match_rule(rule)?;
        self.match_rule_counts.total = self.match_rule_counts.total.saturating_sub(1);

        Ok(())
    }

    /// Add `peer` under `name`, along with the match rules that it has already, and give it what
    /// the policy has for it, unless it is the bus itself.
    fn insert_peer(&mut self, name: OwnedUniqueName, mut peer: Peer) {
        if !peer.is_bus() {
            peer.set_policy(self.policy.for_peer(peer.user().uid(), peer.groups()));
        }
        let rules = peer.match_rules().len();
        self.match_rule_counts.add(rules, rules);
        self.peers.insert(name, peer);
    }

    /// Remove the peer `name`, along with its match rules.
    fn remove_peer(&mut self, name: &str) -> Option<Peer> {
        let peer = self.peers.remove(name)?;
        let rules = peer.match_rules().len();
        self.match_rule_counts.total = self.match_rule_counts.total.saturating_sub(rules);

        Some(peer)
    }
}

/// How many match rules the peers have between them, and the most they had.
#[derive(Clone, Copy, Debug, Default)]
struct MatchRuleCounts {
    /// The match rules that the peers have between them.
    total: usize,
    /// The most that `total` has been.
    peak_total: usize,
    /// The most match rules that one peer had at a time.
    peak_per_peer: usize,
}

impl MatchRuleCounts {
    /// Note that a peer gained `gained` match rules, and has `rules` now.
    fn add(&mut self, gained: usize, rules: usize) {
        self.total += gained;
        self.peak_total = self.peak_total.max(self.total);
        self.peak_per_peer = self.peak_per_peer.max(rules);
    }
}

/// A chore of the bus, under way until this goes: see [`Peers::chore`].
#[derive(Debug)]
pub(crate) struct Chore(Arc<Peers>);

impl Drop for Chore {
    fn drop(&mut self) {
        self.0.chores().running -= 1;
        self.0.chores_changed.notify(usize::MAX);
    }
}

/// What the task that serves a peer keeps of the peer: see [`Peers::serve_peer`].
struct Served {
    unique_name: OwnedUniqueName,
    /// The number of the peer.
    id: usize,
    /// The calls to the peer that wait for its reply.
    pending_calls: Arc<PendingCalls>,
    /// The peer, as the caller of the calls that wait for replies.
    caller: Arc<Caller>,
}

impl Served {
    /// What the task that serves `peer` keeps of it.
    fn new(peer: &Peer) -> Self {
        Self {
            unique_name: peer.unique_name().clone(),
            id: peer.id(),
            pending_calls: peer.pending_calls().clone(),
            caller: peer.caller().clone(),
        }
    }
}

/// A client whose connection the bus is setting up, until this goes.
struct Connecting<'p>(&'p Peers);

impl<'p> Connecting<'p> {
    fn new(peers: &'p Peers) -> Self {
        peers.chores().connecting += 1;

        Self(peers)
    }
}

impl Drop for Connecting<'_> {
    fn drop(&mut self) {
        self.0.chores().connecting -= 1;
        self.0.chores_changed.notify(usize::MAX);
    }
}

/// What the bus does on its own, besides forwarding the messages of its peers.
#[derive(Debug, Default)]
struct Chores {
    /// How many chores are under way.
    running: usize,
    /// How many chores have started, ever, so that a wait for the bus to settle can tell whether
    /// one started meanwhile.
    started: u64,
    /// How many clients the bus is setting the connections of up: their handshakes are under way.
    connecting: usize,
    /// The serial of the last barrier that the bus made, and of the last one whose reply came
    /// back.
    #[cfg(target_os = "linux")]
    barriers_sent: u32,
    #[cfg(target_os = "linux")]
    barrier: u32,
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
    let sender_names = name_registry.names_of(sender.unique_name());
    let receiver_names = name_registry.names_of(receiver.unique_name());
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
    let Facts {
        msg_type,
        path,
        interface,
        member,
        broadcast,
        ..
    } = facts;
    let (denied, other) = (denied.unique_name(), other.unique_name());
    let [path, interface, member] = [path, interface, member].map(|value| value.unwrap_or("none"));
    // A broadcast still goes to those that the policy lets have it, which is nothing for an
    // administrator to look at.
    if broadcast {
        trace!(
            "The policy denies `{denied}` {action} the {msg_type:?} (path {path}, interface \
             {interface}, member {member}) {preposition} `{other}`"
        );
    } else {
        warn!(
            "The policy denies `{denied}` {action} the {msg_type:?} (path {path}, interface \
             {interface}, member {member}) {preposition} `{other}`"
        );
    }

    Some(rejection)
}

/// The `NoReply` error, from the bus, to the call of `caller` with `serial`, as dbus-broker sends
/// to each call that waits on a peer that goes.
fn no_reply(caller: &OwnedUniqueName, serial: NonZeroU32) -> zbus::Result<Message> {
    // zbus only builds an error as the reply to the header of a call, and the bus doesn't keep the
    // calls themselves. A call of `caller` with `serial` has all that the error takes of it.
    let call = Message::method_call(fdo::DBus::PATH, "Call")
        .sender(caller)
        .serial(serial)
        .build(&())?;

    Message::error(&call.header(), "org.freedesktop.DBus.Error.NoReply")
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

/// How many messages of a peer the bus forwards in one batch, at most.
pub const MAX_BATCH: usize = 16;

/// How many bytes of a peer's messages a batch takes before it ends.
///
/// A batch ends with the message that brings it to this or more, so its messages before the last
/// come to less than this. The time limit of a send to a client,
/// [`SEND_TIMEOUT`](crate::peer::SEND_TIMEOUT), covers all the messages of a batch that go to the
/// client, rather than one message, so a client has to take less than this much more within the
/// time limit than if the bus sent it each message on its own. Messages of up to 1 KiB still go up
/// to [`MAX_BATCH`] in a batch.
pub const MAX_BATCH_BYTES: usize = 16 * 1024;

/// The messages of a batch that each recipient gets.
///
/// The task that serves a peer keeps its outbox from one batch to the next, as it keeps the vector
/// of the batch, so that the outbox only allocates where a batch takes more room than those before
/// it did, rather than for every batch.
#[derive(Default)]
struct Outbox {
    /// Each recipient, by its number, and its share, in the order of their first messages.
    sends: Vec<(usize, Recipient, Share)>,
    /// Where each recipient is in `sends`, by its number.
    ///
    /// The recipients of the first message come up once each, so that they need no lookup, and
    /// they only go in here once the recipients of another message are looked up among them. A
    /// batch of one message, as most are when a peer waits for each reply, needs none at all.
    index: HashMap<usize, usize, foldhash::fast::RandomState>,
}

impl Outbox {
    /// Add the message at `i` in `batch` to the share of `recipient`, whose number is `id`.
    ///
    /// The number of a peer is that of [`Peer::id`], which a monitor keeps from the peer that it
    /// was, so no two recipients share one.
    fn add(&mut self, batch: &[Message], id: usize, recipient: &Recipient, i: usize) {
        if i > 0 {
            for (at, (id, ..)) in self.sends.iter().enumerate().skip(self.index.len()) {
                self.index.insert(*id, at);
            }
            match self.index.entry(id) {
                Entry::Occupied(at) => {
                    self.sends[*at.get()].2.add(batch, i);

                    return;
                }
                Entry::Vacant(at) => {
                    at.insert(self.sends.len());
                }
            }
        }

        self.sends
            .push((id, recipient.clone(), Share::Run(i..i + 1)));
    }

    /// Send each recipient its share of `batch`, all of them at once, and empty the outbox for the
    /// next batch.
    ///
    /// Returns the messages that their recipients can't take, for their file descriptors, along
    /// with each recipient.
    ///
    /// The outbox lets go of the recipients, and the messages of their shares, as soon as they are
    /// sent, rather than with the next batch, which may be a long time coming: a recipient holds
    /// the connection of its client, which stays open for as long as it is held, and a message the
    /// file descriptors that it carries.
    async fn send(&mut self, batch: &[Message]) -> Vec<(Recipient, Message)> {
        let dropped = match &self.sends[..] {
            [] => Vec::new(),
            [(_, recipient, share)] => {
                let dropped = recipient.send(share.of(batch)).await;

                with_recipient(recipient, dropped)
            }
            sends => {
                let all = sends
                    .iter()
                    .map(|(_, recipient, share)| recipient.send(share.of(batch)));
                let all = join_all(all).await;

                sends
                    .iter()
                    .zip(all)
                    .flat_map(|((_, recipient, _), dropped)| with_recipient(recipient, dropped))
                    .collect()
            }
        };
        self.sends.clear();
        self.index.clear();

        dropped
    }
}

/// `msgs`, each with `recipient`.
fn with_recipient(recipient: &Recipient, msgs: Vec<Message>) -> Vec<(Recipient, Message)> {
    msgs.into_iter()
        .map(|msg| (recipient.clone(), msg))
        .collect()
}

/// The messages of a batch that a recipient gets.
enum Share {
    /// The messages in this range of the batch, one after the other, as most shares are.
    Run(Range<usize>),
    /// Messages that aren't all next to each other in the batch.
    Picked(Vec<Message>),
}

impl Share {
    /// Add the message at `i` in `batch`, which comes after those in the share.
    fn add(&mut self, batch: &[Message], i: usize) {
        match self {
            Self::Run(run) if run.end == i => run.end += 1,
            Self::Run(run) => {
                let mut picked = batch[run.clone()].to_vec();
                picked.push(batch[i].clone());
                *self = Self::Picked(picked);
            }
            Self::Picked(picked) => picked.push(batch[i].clone()),
        }
    }

    /// The messages of the share, out of `batch`.
    fn of<'s>(&'s self, batch: &'s [Message]) -> &'s [Message] {
        match self {
            Self::Run(run) => &batch[run.clone()],
            Self::Picked(picked) => picked,
        }
    }
}
