use crate::{debug, trace, warn};
use anyhow::{bail, Context, Result};
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
    Connection, Message, Optional, OwnedGuid,
};
use zruntime::{lock::RwLock, spawn, EventListener};

use crate::{
    fdo,
    match_rules::MatchRules,
    name_registry::{NameOwnerChanged, NameRegistry},
    peer::{Monitor, Peer, PendingCalls, Recipient, Stream, MAX_MESSAGE_FDS},
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
}

impl Peers {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            routing: RwLock::new(Routing::default()),
            bus_conn: Mutex::new(None),
        })
    }

    /// Add a client, which the bus gives `send_timeout` to take each message.
    pub async fn add(
        self: &Arc<Self>,
        guid: &OwnedGuid,
        id: usize,
        builder: connection::Builder<'_>,
        send_timeout: Duration,
    ) -> Result<()> {
        // Perform the connection build and SASL handshake without holding the peers lock, as it
        // involves async I/O and would otherwise block all other peer operations (Hello
        // handling, etc.).
        let (peer, peer_stream) = Peer::new(guid.clone(), id, builder, send_timeout).await?;
        let unique_name = peer.unique_name().clone();
        let listener = peer.listen_cancellation();
        let pending_calls = peer.pending_calls().clone();
        let id = peer.id();

        let mut routing = self.routing_mut().await;
        match routing.peers.get(&unique_name) {
            Some(peer) => panic!(
                "Unique name `{}` re-used. We're in deep trouble if this happens",
                peer.unique_name()
            ),
            None => {
                spawn(self.clone().serve_peer(
                    peer_stream,
                    listener,
                    unique_name.clone(),
                    id,
                    pending_calls,
                ))
                .detach();
                routing.peers.insert(unique_name.clone(), peer);
            }
        }

        Ok(())
    }

    /// Add the bus itself, which serves the D-Bus API on `conn`: what the bus sends on it comes out
    /// of `msg_stream`.
    pub async fn add_us(self: &Arc<Self>, msg_stream: zbus::MessageStream, conn: Connection) {
        *self.bus_conn() = Some(conn);
        let (peer, peer_stream) = Peer::new_us(msg_stream).await;
        let unique_name = peer.unique_name().clone();
        let listener = peer.listen_cancellation();
        let pending_calls = peer.pending_calls().clone();
        let id = peer.id();

        let mut routing = self.routing_mut().await;
        match routing.peers.get(&unique_name) {
            Some(peer) => panic!(
                "Unique name `{}` re-used. We're in deep trouble if this happens",
                peer.unique_name()
            ),
            None => {
                spawn(self.clone().serve_peer(
                    peer_stream,
                    listener,
                    unique_name.clone(),
                    id,
                    pending_calls,
                ))
                .detach();
                routing.peers.insert(unique_name.clone(), peer);
            }
        }
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

    pub async fn make_monitor(
        self: &Arc<Self>,
        peer_name: &UniqueName<'_>,
        match_rules: MatchRules,
    ) -> bool {
        let mut routing = self.routing_mut().await;
        let Some(peer) = routing.peers.remove(peer_name.as_str()) else {
            return false;
        };
        let monitor = peer.become_monitor(match_rules);
        let monitor_monitoring_fut = monitor.monitor();
        let unique_name = monitor.unique_name().clone();
        routing.monitors.insert(unique_name.clone(), monitor);
        drop(routing);

        let peers = self.clone();
        spawn(async move {
            monitor_monitoring_fut.await;
            peers.routing_mut().await.monitors.remove(&unique_name);
            debug!("Monitor {} disconnected", unique_name);
        })
        .detach();

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

    /// Serve the peer `unique_name`, numbered `id`, whose messages come out of `peer_stream`, and
    /// whose calls that wait for its replies are `pending_calls`, until it goes.
    async fn serve_peer(
        self: Arc<Self>,
        mut peer_stream: Stream,
        mut cancellation_listener: EventListener,
        unique_name: OwnedUniqueName,
        id: usize,
        pending_calls: Arc<PendingCalls>,
    ) -> Result<()> {
        let mut batch = Vec::new();
        let mut outbox = Outbox::default();
        loop {
            let msg = match select(cancellation_listener, peer_stream.next()).await {
                Either::Left(_) | Either::Right((None, _)) => {
                    trace!("Peer `{}` disconnected", unique_name);

                    break;
                }
                Either::Right((Some(msg), listener)) => {
                    cancellation_listener = listener;

                    msg
                }
            };
            // The messages that the peer sent after this one go along with it, as far as they are
            // here already.
            let mut next = Some(msg);
            let mut bytes = 0;
            let mut too_many_fds = false;
            let mut ended = false;
            while let Some(msg) = next.take() {
                match msg {
                    // Passing such a message on would fail, and get its recipients disconnected.
                    Ok(msg) if msg.data().fds().len() > MAX_MESSAGE_FDS => {
                        too_many_fds = true;

                        break;
                    }
                    Ok(msg) => {
                        bytes += msg.data().len();
                        batch.push(msg);
                    }
                    Err(e) => debug!("{e}"),
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

            self.forward(&unique_name, id, &pending_calls, &batch, &mut outbox)
                .await?;
            batch.clear();
            if too_many_fds {
                warn!(
                    "Disconnecting `{unique_name}`, as it sent a message with more than \
                     {MAX_MESSAGE_FDS} file descriptors"
                );

                break;
            }
            if ended {
                trace!("Peer `{}` disconnected", unique_name);

                break;
            }
        }

        // Stream is done means the peer disconnected or it became a monitor, unless the peer sent
        // a message that gets it disconnected. Remove it from the list of peers: that, and the end
        // of its stream, close its connection.
        let removed = self
            .routing_mut()
            .await
            .peers
            .remove(&unique_name)
            .is_some();
        // The calls that the peer made won't get their replies to it now.
        for peer in self.routing().await.peers.values() {
            peer.pending_calls().forget(id);
        }
        // Not being in the list means the peer was turned into a monitor. `Monitoring` iface
        // emits the signals then.
        if removed {
            let names_changes = self
                .routing_mut()
                .await
                .name_registry
                .release_all(unique_name.inner().clone())
                .await;
            for changed in names_changes {
                self.notify_name_changes(changed).await?;
            }
            self.notify_name_changes(NameOwnerChanged {
                name: BusName::from(unique_name.clone()).into(),
                old_owner: Some(unique_name.clone()),
                new_owner: None,
            })
            .await?;
        }
        // Nor will the calls that wait on the peer get its replies, so they get an error from the
        // bus instead, as with dbus-broker, rather than wait for as long as their callers do.
        for (caller, serial) in pending_calls.take() {
            let error = no_reply(&caller, serial)?;
            self.send_from_bus(&error).await;
        }

        Ok(())
    }

    /// Forward `msgs`, which the peer `sender`, numbered `sender_id`, sent in this order, to their
    /// recipients, through the empty `outbox` of the task that serves `sender`.
    ///
    /// Each recipient gets those of `msgs` that go to it in one go, in order, and all of them get
    /// theirs at once.
    ///
    /// A call that expects a reply is recorded as one that waits on its callee as it goes to the
    /// callee, unless its caller has a call of the same serial waiting there already, in which case
    /// the bus refuses it, with an error, as dbus-broker does. A reply, or an error, of `sender`
    /// only goes on if it answers one of the calls that wait on `sender`, `pending_calls`. The bus
    /// answers each call to it itself, so those aren't recorded, and its own replies go on as they
    /// are.
    async fn forward(
        &self,
        sender: &OwnedUniqueName,
        sender_id: usize,
        pending_calls: &PendingCalls,
        msgs: &[Message],
        outbox: &mut Outbox,
    ) -> Result<()> {
        let from_bus = *sender == fdo::BUS_NAME;
        // The calls that the bus refuses.
        let mut refused = Vec::new();
        // The recipients are picked under the lock, and only sent their messages without it.
        {
            let routing = self.routing().await;
            let Routing {
                peers,
                monitors,
                name_registry,
            } = &*routing;
            for (i, msg) in msgs.iter().enumerate() {
                if msg.message_type() == message::Type::Signal {
                    trace!("Broadcasting message: {:?}", msg);
                    for (name, peer) in peers.iter() {
                        if !peer.interested(msg, name_registry) {
                            trace!("Peer {} not interested in {msg:?}", name);
                            continue;
                        }

                        outbox.add(msgs, peer.id(), peer.recipient(), i);
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
                        BusName::WellKnown(name) => match name_registry.lookup(name) {
                            Some(owner) => peers.get_key_value(owner.as_str()),
                            None => {
                                warn!("unknown destination: {}", name);

                                continue;
                            }
                        },
                    };
                    let msg_type = msg.message_type();
                    if !from_bus
                        && matches!(msg_type, message::Type::MethodReturn | message::Type::Error)
                    {
                        let answers = match (peer, header.reply_serial()) {
                            (Some((_, peer)), Some(serial)) => {
                                pending_calls.remove(peer.id(), serial)
                            }
                            _ => false,
                        };
                        // Monitors don't get it either, as it doesn't go anywhere.
                        if !answers {
                            debug!(
                                "Dropping {msg:?}, as it answers no call that waits on {sender}"
                            );

                            continue;
                        }
                    }
                    match peer {
                        Some((name, peer)) => {
                            let primary = msg.primary_header();
                            if msg_type == message::Type::MethodCall
                                && !primary.flags().contains(message::Flags::NO_REPLY_EXPECTED)
                                && *name != fdo::BUS_NAME
                                && !peer.pending_calls().add(
                                    sender_id,
                                    sender,
                                    primary.serial_num(),
                                )
                            {
                                refused.push(i);

                                continue;
                            }
                            outbox.add(msgs, peer.id(), peer.recipient(), i)
                        }
                        None => debug!("no peer for destination `{destination}`"),
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

        outbox.send(msgs).await;
        for i in refused {
            let error =
                Message::error(&msgs[i].header(), "org.freedesktop.DBus.Error.AccessDenied")
                    .sender(fdo::BUS_NAME)
                    .build(&"Pending reply with that serial already exists")?;
            self.send_from_bus(&error).await;
        }

        Ok(())
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
    /// The outbox lets go of the recipients, and the messages of their shares, as soon as they are
    /// sent, rather than with the next batch, which may be a long time coming: a recipient holds
    /// the connection of its client, which stays open for as long as it is held, and a message the
    /// file descriptors that it carries.
    async fn send(&mut self, batch: &[Message]) {
        match &self.sends[..] {
            [] => (),
            [(_, recipient, share)] => recipient.send(share.of(batch)).await,
            sends => {
                let sends = sends
                    .iter()
                    .map(|(_, recipient, share)| recipient.send(share.of(batch)));
                join_all(sends).await;
            }
        }
        self.sends.clear();
        self.index.clear();
    }
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
