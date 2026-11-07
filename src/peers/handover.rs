//! The hand-over of the peers, monitors and names to the next instance of the bus.

use std::{
    collections::HashMap,
    future::Future,
    iter,
    num::NonZeroU32,
    pin::pin,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures_util::future::{join_all, select, Either};
use zbus::{
    connection::Handover, message, names::OwnedUniqueName, Connection, Message, MessageStream,
};
use zruntime::{spawn, SharedRuntime};

use super::{Peers, Routing, Served};
use crate::{
    bus::handover::{ClientHandover, MonitorState, NameState, PeerState, Snapshot},
    debug, fdo,
    name_registry::{NameOwner, NameRegistry},
    peer::{Caller, HandedOver, Monitor, Peer, Recipient, Stream},
    warn,
};

impl Peers {
    /// Pause the bus, for it to hand over: have the task that serves each client, peer or
    /// monitor, take no more of its messages, once it is done with those that it took, and give
    /// the stream of the messages back, and wait for that. The clients that connect from now on
    /// pause as soon as they are connected, and those in the middle of their handshakes have
    /// `timeout` to be done with them.
    ///
    /// No send to a client waits for the client to take what the bus sends it from now on: what a
    /// client doesn't take goes on with the hand-over of its connection, for the next instance to
    /// write to the client first. So a client that doesn't read holds no hand-over up, unless the
    /// bus is still writing to it what the instance before it left unwritten, which takes as long
    /// as the client has to take a message at most ([`Recipient::flush`]). Once the bus is paused,
    /// it takes no more clients: one whose handshake is still under way by then gets disconnected
    /// at once, as [`Peers::add`] says.
    pub(crate) async fn pause(&self, timeout: Duration) -> Result<()> {
        let runtime = SharedRuntime::current()?;
        let deadline = Instant::now() + timeout;
        {
            let mut routing = self.routing_mut().await;
            routing.handing_over = true;
            for peer in routing.peers.values().filter(|peer| !peer.is_bus()) {
                peer.pause();
            }
            for monitor in routing.monitors.values() {
                monitor.pause();
            }
        }
        loop {
            let connecting = Instant::now() < deadline && self.chores().connecting > 0;
            if !connecting {
                // Under the lock that adding a client takes, so that none comes in between.
                let mut routing = self.routing_mut().await;
                if paused(&mut routing) {
                    routing.sealed = true;
                    self.sealed.notify(usize::MAX);

                    return Ok(());
                }
            }
            runtime.sleep(SETTLE_INTERVAL).await;
        }
    }

    /// What `future` gives, unless the bus takes no more clients before then, as it is done
    /// pausing: `future` is dropped then, with all that it holds, and isn't polled again.
    pub(super) async fn unless_sealed<F>(&self, future: F) -> Option<F::Output>
    where
        F: Future,
    {
        let sealed = async {
            loop {
                // The listener comes first, so that a seal right after the check isn't missed.
                let listener = self.sealed.listen();
                if self.routing().await.sealed {
                    return;
                }
                listener.await;
            }
        };

        // The seal is polled first, so that `future` takes no step once the bus is sealed.
        match select(pin!(sealed), pin!(future)).await {
            Either::Left(_) => None,
            Either::Right((output, _)) => Some(output),
        }
    }

    /// Wait for the bus to settle: to have answered every call that it got, and to have no chores
    /// under way, with every message that the bus sent written to its recipient, or kept for the
    /// next instance to write.
    ///
    /// A hand-over waits for this once the bus is paused, so the bus only has its own work to
    /// finish, as nothing new comes in for it to do, but for what the clients whose connections
    /// ended meanwhile sent before that, which it takes, as [`Peers::serve_gone`] says: those that
    /// went, and those that the bus disconnected, as they didn't take what it sent them in time, or
    /// as a send to them failed, as one to a client that went does.
    pub(crate) async fn settle(self: &Arc<Self>) -> Result<()> {
        let runtime = SharedRuntime::current()?;
        loop {
            let started = self.chores_done().await;
            // The calls of the interfaces that zbus adds to the bus, such as
            // `org.freedesktop.DBus.Properties`, are answered by tasks of their own.
            if !self.bus_calls_answered().await {
                runtime.sleep(SETTLE_INTERVAL).await;

                continue;
            }
            self.barrier().await?;
            // Each client that went is served on as a chore, so the bus isn't settled until then.
            self.serve_gone().await;
            let settled = {
                let chores = self.chores();

                chores.running == 0 && chores.started == started
            };
            // A chore that started while the barrier was on its way may have sent messages after
            // it.
            if settled && self.bus_calls_answered().await {
                return Ok(());
            }
        }
    }

    /// The peers, monitors and names, as the bus hands them over, and what it hands over of the
    /// connection of each client.
    ///
    /// The connections of the clients are handed over all at once, each through the stream of its
    /// messages, with what the bus sent the client and the client didn't take, which nothing waits
    /// for the client to take. One whose hand-over takes longer than the time that its client has
    /// to take a message all the same is given up on, rather than hold the others up. That of a
    /// client that went since the bus settled, as it closed its end, is handed over too, with what
    /// zbus read of it, for the next instance to pass that on and clean up after the client. A
    /// client whose connection fails to be handed over, as when the bus is disconnecting it, when
    /// the connection ended on an error, when its hand-over is given up on, or when its socket
    /// fails to be duplicated, as the process has too many file descriptors open, gets
    /// disconnected: the bus leaves it out, so that the next instance cleans up after it as after a
    /// client that went.
    pub(crate) async fn snapshot(&self) -> Result<(Snapshot, Vec<ClientHandover>)> {
        let runtime = SharedRuntime::current()?;
        let mut clients = Vec::new();
        let names = {
            let mut routing = self.routing_mut().await;
            let routing = &mut *routing;
            for peer in routing.peers.values_mut().filter(|peer| !peer.is_bus()) {
                let state = PeerState {
                    id: peer.id() as u64,
                    greeted: peer.greeted(),
                    connection: Vec::new(),
                    match_rules: peer.match_rules().clone(),
                    pending_calls: peer
                        .pending_calls()
                        .records()
                        .map(|(caller_id, caller, serial)| (caller_id as u64, caller, serial.get()))
                        .collect(),
                };
                clients.push(Client {
                    id: peer.id(),
                    conn: peer.conn().clone(),
                    recipient: peer.recipient().clone(),
                    stream: peer.paused().take(),
                    state: ClientState::Peer(state),
                });
            }
            for monitor in routing.monitors.values_mut() {
                let state = MonitorState {
                    id: monitor.id() as u64,
                    connection: Vec::new(),
                    match_rules: monitor.match_rules().clone(),
                };
                clients.push(Client {
                    id: monitor.id(),
                    conn: monitor.conn().clone(),
                    recipient: monitor.recipient().clone(),
                    stream: monitor.paused().take(),
                    state: ClientState::Monitor(state),
                });
            }

            routing
                .name_registry
                .all_names()
                .iter()
                .map(|(name, entry)| NameState {
                    name: name.clone(),
                    owners: iter::once(entry.owner())
                        .chain(entry.waiting_list())
                        .map(|owner| (owner.unique_name().clone(), owner.allow_replacement()))
                        .collect(),
                })
                .collect()
        };
        let handing_over = clients.iter_mut().map(|client| client.hand_over(&runtime));
        let handovers = join_all(handing_over).await;

        let mut snapshot = Snapshot {
            peers: Vec::new(),
            monitors: Vec::new(),
            names,
        };
        let mut handed_over = Vec::new();
        for (client, connection) in clients.into_iter().zip(handovers) {
            if let Some(connection) = connection {
                handed_over.push(ClientHandover {
                    id: client.id,
                    connection,
                });
            }
            match client.state {
                ClientState::Peer(state) => snapshot.peers.push(state),
                ClientState::Monitor(state) => snapshot.monitors.push(state),
            }
        }

        Ok((snapshot, handed_over))
    }

    /// Note that the reply to the barrier with `serial` came back.
    pub(super) fn barrier_returned(&self, serial: u32) {
        {
            let mut chores = self.chores();
            chores.barrier = chores.barrier.max(serial);
        }
        self.chores_changed.notify(usize::MAX);
    }

    /// Let go of the clients without disconnecting them, as the bus handed them over: nothing more
    /// goes to them from this instance of the bus.
    pub(crate) async fn retire(&self) {
        // Nothing more comes from the bus, such as its errors to the calls that wait on the peers
        // that go now.
        self.remove_us();
        let mut routing = self.routing_mut().await;
        for peer in routing.peers.values() {
            peer.recipient().hand_over();
        }
        for monitor in routing.monitors.values() {
            monitor.recipient().hand_over();
        }
        routing.peers.clear();
        routing.monitors.clear();
        routing.name_registry = NameRegistry::default();
    }

    /// Serve the peers and monitors of `snapshot` on the connections of `clients`, and give the
    /// peers their names, as the instance of the bus that handed them over did.
    ///
    /// The peers and the monitors whose connections are missing are gone, as when the service
    /// manager didn't take their sockets, or let go of them, so the bus cleans up after them as
    /// after any peer that goes, and after those whose connections it fails to serve as well. It
    /// fails, if at all, before it serves any client.
    ///
    /// What the instance that handed a client over sent it, and the client didn't take, goes to the
    /// client first, as the client takes it: a client that doesn't take it within `send_timeout`
    /// gets disconnected, as when a send to it times out.
    pub(crate) async fn restore(
        self: &Arc<Self>,
        snapshot: Snapshot,
        mut clients: HashMap<usize, ClientHandover>,
        send_timeout: Duration,
    ) -> Result<()> {
        let Snapshot {
            peers,
            monitors,
            names,
        } = snapshot;
        // The connections start reading as they are created, but the bus only forwards what comes
        // in once all of its peers, monitors and names are back.
        let mut restored = Vec::new();
        let mut gone = Vec::new();
        for state in peers {
            let id = usize::try_from(state.id)?;
            let unique_name = OwnedUniqueName::try_from(format!(":busd.{id}"))?;
            let records = pending_call_records(state.pending_calls)?;
            let Some(client) = clients.remove(&id) else {
                debug!("Peer `{unique_name}` went while the bus handed over");
                gone.push((unique_name, records, Vec::new()));

                continue;
            };
            let client = HandedOver {
                id,
                handover: client.connection,
                greeted: state.greeted,
                match_rules: state.match_rules,
            };
            match Peer::handed_over(client, send_timeout, &self.users).await {
                Ok((peer, stream)) => restored.push((peer, stream, records)),
                Err(e) => {
                    warn!("Disconnecting `{unique_name}`, as serving it failed: {e}");
                    gone.push((unique_name, records, Vec::new()));
                }
            }
        }
        let mut restored_monitors = Vec::new();
        // Those that have what the instance that handed them over sent them to take still.
        let mut unwritten = Vec::new();
        for state in monitors {
            let id = usize::try_from(state.id)?;
            let Some(client) = clients.remove(&id) else {
                continue;
            };
            let client = HandedOver {
                id,
                handover: client.connection,
                greeted: true,
                match_rules: Default::default(),
            };
            match Peer::handed_over(client, send_timeout, &self.users).await {
                Ok((peer, stream)) => {
                    let monitor = peer.become_monitor(state.match_rules);
                    restored_monitors.push((monitor, stream.into_inner()));
                }
                Err(e) => {
                    warn!("Disconnecting the monitor `:busd.{id}`, as serving it failed: {e}")
                }
            }
        }
        {
            let mut routing = self.routing_mut().await;
            for NameState { name, owners } in names {
                let mut owners = owners
                    .into_iter()
                    .map(|(unique_name, replace)| NameOwner::new(unique_name, replace));
                let owner = owners.next().context("A name with no owner")?;
                routing.name_registry.insert(name, owner, owners);
            }
            // The names of the peers that went pass on to those that queue for them before the bus
            // serves anyone, so that no call to one of those names finds its owner gone.
            for (unique_name, _, names_changes) in &mut gone {
                *names_changes = routing
                    .name_registry
                    .release_all(unique_name.inner().clone())
                    .await;
            }
            // The calls that wait on the peers, of the callers that are back too, the bus itself
            // among them: the bus would forget the calls of the others as it cleans up after them.
            let callers: HashMap<usize, Arc<Caller>> = routing
                .peers
                .values()
                .map(|peer| (peer.id(), peer.caller().clone()))
                .chain(
                    restored
                        .iter()
                        .map(|(peer, _, _)| (peer.id(), peer.caller().clone())),
                )
                .collect();
            for (peer, stream, records) in restored {
                for (caller_id, _, serial) in records {
                    if let Some(caller) = callers.get(&caller_id) {
                        peer.pending_calls()
                            .add_handed_over(caller_id, caller, serial);
                    }
                }
                if peer.conn().unwritten_len() > 0 {
                    unwritten.push(peer.recipient().clone());
                }
                self.insert(&mut routing, peer, stream);
            }
            for (monitor, stream) in restored_monitors {
                if monitor.conn().unwritten_len() > 0 {
                    unwritten.push(monitor.recipient().clone());
                }
                self.insert_monitor(&mut routing, monitor, stream);
            }
        }
        // zbus writes what is left ahead of anything else that the bus sends the client meanwhile.
        for recipient in unwritten {
            spawn(async move { recipient.flush().await }).detach();
        }
        // The bus serves its clients from here on, so it goes on whatever comes.
        for (unique_name, records, names_changes) in gone {
            let waiting = records
                .into_iter()
                .map(|(_, caller, serial)| (caller, serial))
                .collect();
            let cleaned_up = self.finish_clean_up(&unique_name, names_changes, waiting);
            if let Err(e) = cleaned_up.await {
                warn!("Failed to clean up after `{unique_name}`: {e}");
            }
        }
        if !clients.is_empty() {
            warn!(
                "Closing {} sockets that were handed over for no client",
                clients.len()
            );
        }

        Ok(())
    }

    /// Add `monitor`, whose messages come out of `stream`, to the monitors of `routing`, and serve
    /// it until it goes.
    fn insert_monitor(
        self: &Arc<Self>,
        routing: &mut Routing,
        monitor: Monitor,
        stream: MessageStream,
    ) {
        let unique_name = monitor.unique_name().clone();
        let listener = monitor.listen_cancellation();
        routing.monitors.insert(unique_name.clone(), monitor);
        spawn(self.clone().serve_monitor(stream, listener, unique_name)).detach();
    }

    /// Wait for no chore of the bus to be under way, and how many have started by then.
    async fn chores_done(&self) -> u64 {
        loop {
            let listener = self.chores_changed.listen();
            let started = {
                let chores = self.chores();

                (chores.running == 0).then_some(chores.started)
            };
            if let Some(started) = started {
                return started;
            }
            listener.await;
        }
    }

    /// Whether the bus answered each call to it that waits for its reply.
    async fn bus_calls_answered(&self) -> bool {
        self.routing()
            .await
            .peers
            .get(fdo::BUS_NAME)
            .is_none_or(|bus| bus.pending_calls().is_empty())
    }

    /// Wait for the bus to have answered every call that it got so far, and for its peers to have
    /// got the messages that it sent until then.
    ///
    /// The bus answers the calls of its D-Bus interfaces one at a time, in the order that they come
    /// in, and sends its replies, and its signals, on one connection, which its peer passes on in
    /// that order. So the bus makes a call of its own, to itself, behind those of its peers, and
    /// waits for the reply to come back out of its peer, which `serve_peer` doesn't pass on.
    async fn barrier(&self) -> Result<()> {
        let conn = self
            .routing()
            .await
            .peers
            .get(fdo::BUS_NAME)
            .map(|bus| bus.conn().clone())
            .context("The bus isn't up")?;
        let serial = {
            let mut chores = self.chores();
            chores.barriers_sent += 1;

            NonZeroU32::new(chores.barriers_sent).context("Too many barriers")?
        };
        let call = Message::method_call(fdo::DBus::PATH, "GetId")
            .interface(fdo::DBus::INTERFACE)
            .destination(fdo::BUS_NAME)
            .sender(fdo::BUS_NAME)
            .serial(serial)
            .build(&())?;
        let mut listener = self.chores_changed.listen();
        conn.send(&call).await?;
        while self.chores().barrier < serial.get() {
            listener.await;
            listener = self.chores_changed.listen();
        }

        Ok(())
    }

    /// Serve on the clients, peers and monitors, whose connections ended while the bus paused,
    /// once the tasks that served them gave the streams of their messages back, each as a chore of
    /// its own, until its stream ends: the bus takes what zbus read of them, and cleans up after
    /// them, as after any client that goes. Those are the clients that went, and those that the bus
    /// disconnected meanwhile, once their connections are closed.
    ///
    /// zbus hands the connection of a client that closed its end over, with what it read of the
    /// client, but not one that ended on an error, nor one that the bus closed, as it does once a
    /// send to the client fails, and what zbus read of such a client would be lost otherwise. The
    /// bus closes the connection of a client that went too, if it sends the client anything before
    /// this finds it gone, as the send fails. So the bus serves the clients whose connections
    /// ended here, while their recipients are still its clients, whatever their connections ended
    /// on, and leaves only those that go after that to the hand-over.
    async fn serve_gone(self: &Arc<Self>) {
        let mut guard = self.routing_mut().await;
        let routing = &mut *guard;
        for peer in routing.peers.values_mut() {
            if !peer.conn().is_closed() {
                continue;
            }
            let Some(stream) = peer.paused().take() else {
                continue;
            };
            let stream = Stream::for_peer(peer, stream);
            let serving =
                self.clone()
                    .serve_peer(stream, peer.listen_cancellation(), Served::new(peer));
            let chore = self.chore();
            spawn(async move {
                let _chore = chore;

                serving.await
            })
            .detach();
        }
        for monitor in routing.monitors.values_mut() {
            if !monitor.conn().is_closed() {
                continue;
            }
            let Some(stream) = monitor.paused().take() else {
                continue;
            };
            let serving = self.clone().serve_monitor(
                stream,
                monitor.listen_cancellation(),
                monitor.unique_name().clone(),
            );
            let chore = self.chore();
            spawn(async move {
                let _chore = chore;

                serving.await
            })
            .detach();
        }
    }
}

/// Take the replies to the barriers of a hand-over out of `batch`, a batch of messages of the bus,
/// and the serial of the last call that they answer, if any.
///
/// The bus makes its barriers in the name of `org.freedesktop.DBus`, so they are the only replies
/// of the bus that go to the bus itself.
pub(super) fn take_barriers(batch: &mut Vec<Message>) -> Option<u32> {
    let mut last = None;
    batch.retain(|msg| {
        let header = msg.header();
        let barrier = matches!(
            msg.message_type(),
            message::Type::MethodReturn | message::Type::Error
        ) && header
            .destination()
            .is_some_and(|destination| destination.as_str() == fdo::BUS_NAME);
        if barrier {
            last = header.reply_serial().map(NonZeroU32::get).max(last);
        }

        !barrier
    });

    last
}

/// Whether the task that served each client of `routing`, peer or monitor, gave the stream of its
/// messages back, as the bus pauses.
fn paused(routing: &mut Routing) -> bool {
    routing
        .peers
        .values_mut()
        .all(|peer| peer.is_bus() || peer.paused().is_some())
        && routing
            .monitors
            .values_mut()
            .all(|monitor| monitor.paused().is_some())
}

/// A client, as the bus hands it over: see [`Peers::snapshot`].
struct Client {
    /// The number of the client.
    id: usize,
    conn: Connection,
    recipient: Recipient,
    /// The stream of the messages of the client, which the task that served it gave back.
    stream: Option<MessageStream>,
    state: ClientState,
}

impl Client {
    /// Hand the connection of the client over, through the stream of its messages, within the time
    /// that the client has to take a message: none if that fails, or takes longer.
    ///
    /// A client whose connection ended as the client closed its end, such as once it sent a last
    /// message, is handed over all the same, with what zbus read of it, which the next instance
    /// passes on before it cleans up after the client. A client that the bus is disconnecting
    /// isn't handed over. One whose connection fails to be handed over gets closed, and so does one
    /// whose hand-over takes too long.
    async fn hand_over(&mut self, runtime: &SharedRuntime) -> Option<Handover> {
        let name = self.recipient.unique_name()?;
        if self.recipient.disconnecting() {
            debug!("Leaving `{name}` out of the hand-over, as the bus is disconnecting it");

            return None;
        }
        let stream = self.stream.as_mut()?;
        let timeout = self.recipient.send_timeout()?;
        let error = match runtime.timeout(timeout, stream.hand_over()).await {
            // A send or a flush to the client that held the hand-over up may have failed, or timed
            // out, meanwhile, and the hand-over may have got the connection before the closing of
            // the connection did: dropping the hand-over closes what it holds.
            Ok(Ok(_)) if self.recipient.disconnecting() => {
                debug!("Leaving `{name}` out of the hand-over, as the bus is disconnecting it");

                return None;
            }
            Ok(Ok(handover)) => return Some(handover),
            Ok(Err(e)) => e.to_string(),
            Err(_) => "it took too long".to_owned(),
        };
        warn!("Disconnecting `{name}`, as its connection failed to be handed over: {error}");
        // The stream still has the messages that zbus read of the client and that the bus didn't
        // take, such as those that zbus read before the connection ended on an error, but the bus
        // can't forward them anymore: the connections of the clients are handed over all at once,
        // so those that the messages are for may have been handed over already. They go with the
        // stream. The connection reads nothing more, and all that is left is to close it.
        let name = name.clone();
        let conn = self.conn.clone();
        spawn(async move {
            if let Err(e) = conn.close().await {
                debug!("Failed to close the connection of `{name}`: {e}");
            }
        })
        .detach();

        None
    }
}

/// What the state of the bus holds of a client, as the bus hands it over.
enum ClientState {
    Peer(PeerState),
    Monitor(MonitorState),
}

/// The records of the calls that wait on a peer, as the bus hands them over, `records`: the number
/// of the caller of each, its unique name, and the serial of the call.
fn pending_call_records(
    records: Vec<(u64, OwnedUniqueName, u32)>,
) -> Result<Vec<(usize, OwnedUniqueName, NonZeroU32)>> {
    records
        .into_iter()
        .map(|(caller_id, caller, serial)| {
            let serial = NonZeroU32::new(serial).context("A call with no serial")?;

            Ok((usize::try_from(caller_id)?, caller, serial))
        })
        .collect()
}

/// How long a wait for the bus to pause, or to settle, waits before it checks again.
const SETTLE_INTERVAL: Duration = Duration::from_millis(1);

#[cfg(test)]
mod tests {
    use std::{
        future::poll_fn,
        io::{self, Read, Write},
        net::Shutdown,
        num::NonZeroUsize,
        os::unix::net::UnixStream,
        task::Poll,
    };

    use futures_util::{future::join, FutureExt, StreamExt};
    use ntest::timeout;
    use rand::{
        distr::{Alphanumeric, SampleString},
        rng,
    };
    use zbus::{connection, MatchRule};
    use zruntime::unblock;

    use super::*;
    use crate::{
        bus::{Bus, STOP_TIMEOUT},
        peer::SEND_TIMEOUT,
        user::Charge,
    };

    /// A client that sends a last signal and closes its end of the connection, once the bus took
    /// the last of its messages that it forwards itself, but before it hands the connection over,
    /// as zbus read the signal and the end of what the client sent by then, is handed over with
    /// the signal: the next instance passes the signal on, and then cleans up after the client.
    #[test]
    #[timeout(30000)]
    fn last_signal_of_a_client_that_closes_its_end_is_handed_over() {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let (receiver, _) = connect(&bus, 1).await;
            let rule = MatchRule::builder()
                .msg_type(message::Type::Signal)
                .build()
                .unwrap();
            let mut signals = MessageStream::for_match_rule(rule, &receiver, None)
                .await
                .unwrap();
            let (sender, socket) = connect(&bus, 2).await;
            let sender_name = sender.unique_name().unwrap().to_string();

            peers.pause(STOP_TIMEOUT).await.unwrap();
            peers.settle().await.unwrap();
            let bye = Message::signal("/org/zbus/Test", "org.zbus.Test", "Bye")
                .build(&())
                .unwrap();
            sender.send(&bye).await.unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
            let conn = peers
                .routing()
                .await
                .peers
                .get(sender_name.as_str())
                .map(|peer| peer.conn().clone())
                .unwrap();
            conn.closed().await;
            let (snapshot, clients) = peers.snapshot().await.unwrap();
            assert!(
                clients.iter().any(|client| client.id == 2),
                "the client that closed its end was left out"
            );
            retire(bus).await;

            let next = Bus::for_address(Some(&address())).await.unwrap();
            let clients = clients
                .into_iter()
                .map(|client| (client.id, client))
                .collect();
            next.peers()
                .restore(snapshot, clients, SEND_TIMEOUT)
                .await
                .unwrap();
            let mut passed_on = false;
            loop {
                let signal = signals.next().await.unwrap().unwrap();
                let header = signal.header();
                match header.member().map(|member| member.as_str()) {
                    Some("Bye") => passed_on = true,
                    Some("NameOwnerChanged") => {
                        let (name, _, new_owner): (String, String, String) =
                            signal.body().deserialize().unwrap();
                        if name == sender_name && new_owner.is_empty() {
                            break;
                        }
                    }
                    _ => (),
                }
            }
            assert!(
                passed_on,
                "the client was cleaned up after before its signal was passed on"
            );
        });
    }

    /// A client that sends a last signal and goes while the bus settles, and that a send of the
    /// bus fails to then, as it went, has its signal passed on all the same, though the bus is
    /// disconnecting it, and is cleaned up after: zbus hands over no connection that the bus
    /// closed, so the bus serves the client to the end of its stream, as any client that went.
    #[test]
    #[timeout(30000)]
    fn last_signal_of_a_client_that_a_send_fails_to_is_passed_on() {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let rule = MatchRule::builder()
                .msg_type(message::Type::Signal)
                .build()
                .unwrap();
            let (receiver, _) = connect(&bus, 1).await;
            let mut signals = MessageStream::for_match_rule(rule.clone(), &receiver, None)
                .await
                .unwrap();
            // The bus forwards the signals to `x` too, so it sends `x` the signal of `y`.
            let (x, x_socket) = connect(&bus, 2).await;
            let _x_signals = MessageStream::for_match_rule(rule, &x, None).await.unwrap();
            let (y, y_socket) = connect(&bus, 3).await;
            let x_name = x.unique_name().unwrap().to_string();
            let y_name = y.unique_name().unwrap().to_string();

            peers.pause(STOP_TIMEOUT).await.unwrap();
            let bye = Message::signal("/org/zbus/Test", "org.zbus.Test", "Bye")
                .build(&())
                .unwrap();
            x.send(&bye).await.unwrap();
            y.send(&bye).await.unwrap();
            y_socket.shutdown(Shutdown::Both).unwrap();
            let y_conn = peers
                .routing()
                .await
                .peers
                .get(y_name.as_str())
                .map(|peer| peer.conn().clone())
                .unwrap();
            y_conn.closed().await;
            // `x` goes once the bus took the stream of `y` back, to serve it on, and before the
            // chore that serves it forwards the signal of `y` to `x`: the runtime runs one task at
            // a time, and `join` polls this each time that it is polled, so this runs as soon as
            // the bus waits for that chore, before the chore runs.
            let x_goes = poll_fn(|_| {
                let taken = peers.routing.try_write().is_some_and(|mut routing| {
                    routing
                        .peers
                        .get_mut(y_name.as_str())
                        .is_none_or(|y| y.paused().is_none())
                });
                if !taken {
                    return Poll::Pending;
                }
                x_socket.shutdown(Shutdown::Both).unwrap();

                Poll::Ready(())
            });
            let (settled, ()) = join(peers.settle(), x_goes).await;
            settled.unwrap();
            let (snapshot, clients) = peers.snapshot().await.unwrap();
            retire(bus).await;

            let next = Bus::for_address(Some(&address())).await.unwrap();
            let clients = clients
                .into_iter()
                .map(|client| (client.id, client))
                .collect();
            next.peers()
                .restore(snapshot, clients, SEND_TIMEOUT)
                .await
                .unwrap();
            let mut passed_on = Vec::new();
            let mut cleaned_up = Vec::new();
            while cleaned_up.len() < 2 {
                let signal = signals.next().await.unwrap().unwrap();
                let header = signal.header();
                match header.member().map(|member| member.as_str()) {
                    Some("Bye") => passed_on.push(header.sender().unwrap().to_string()),
                    Some("NameOwnerChanged") => {
                        let (name, _, new_owner): (String, String, String) =
                            signal.body().deserialize().unwrap();
                        if (name == x_name || name == y_name) && new_owner.is_empty() {
                            assert!(
                                passed_on.contains(&name),
                                "`{name}` was cleaned up after before its signal was passed on"
                            );
                            cleaned_up.push(name);
                        }
                    }
                    _ => (),
                }
            }
        });
    }

    /// A client still in the middle of its handshake once the bus is done pausing, as it hands
    /// over, gets disconnected then, rather than once its handshake ends, which may be never: the
    /// bus lets go of its socket, and of the objects charged to its user, at once.
    #[test]
    #[timeout(30000)]
    fn a_handshake_under_way_once_the_bus_paused_is_cut_off() {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            // The client is the one object of its user.
            peers.users().set_max_objects(1);
            let (mut ours, theirs) = UnixStream::pair().unwrap();
            ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let uid = rustix::process::getuid().as_raw();
            let user = peers.users().user(uid);
            let server = connection::Builder::unix_stream(theirs);
            let adding = peers.add(bus.guid(), 1, server, SEND_TIMEOUT, uid);
            let cutting_off = async {
                // The client authenticates, but never begins.
                let mut ours = unblock(move || {
                    let uid: String = uid
                        .to_string()
                        .bytes()
                        .map(|digit| format!("{digit:02x}"))
                        .collect();
                    ours.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())
                        .unwrap();
                    let mut line = Vec::new();
                    while !line.ends_with(b"\r\n") {
                        let mut byte = [0];
                        ours.read_exact(&mut byte).unwrap();
                        line.push(byte[0]);
                    }
                    assert!(line.starts_with(b"OK "), "the bus refused the client");

                    ours
                })
                .await;
                assert!(Charge::new(&user, &user).is_none());
                peers.pause(Duration::from_millis(100)).await.unwrap();
                // A bus that still had the client would answer this, and keep the connection.
                let (answer, ended) = unblock(move || {
                    // The write fails if the bus closed the connection already.
                    let _ = ours.write_all(b"CANCEL\r\n");
                    let mut answer = Vec::new();
                    // The read ends with the connection, or else with the timeout. A connection
                    // that the bus closed with the cancel unread is reset.
                    let ended = match ours.read_to_end(&mut answer) {
                        Ok(_) => true,
                        Err(e) => e.kind() == io::ErrorKind::ConnectionReset,
                    };

                    (String::from_utf8_lossy(&answer).into_owned(), ended)
                })
                .await;
                assert!(
                    answer.is_empty(),
                    "the bus answered `{answer}` once it was done pausing"
                );
                assert!(ended, "the connection outlived the pause");
            };

            let (added, ()) = join(adding, cutting_off).await;
            added.unwrap();
            assert!(
                Charge::new(&user, &user).is_some(),
                "the client is still charged to its user"
            );
        });
    }

    /// A client that the bus starts disconnecting while the hand-over of its connection waits, as
    /// when a flush to the client times out as it holds the hand-over up, isn't handed over, even
    /// where the hand-over gets the connection before the closing of the connection does, as with
    /// a lock that goes to the first to wait for it.
    #[test]
    #[timeout(30000)]
    fn client_that_the_bus_disconnects_as_its_hand_over_waits_is_left_out() {
        const CHUNK: usize = 64 * 1024;
        const CHUNKS: usize = 16;

        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let (mut receiver, _socket) = connect(&bus, 1).await;
            let name = receiver.unique_name().unwrap().to_string();
            // The receiver takes no more of what the bus sends it once a message waits in its
            // queue.
            let _messages = MessageStream::from(&receiver);
            receiver.set_max_queued(NonZeroUsize::MIN);
            peers.pause(STOP_TIMEOUT).await.unwrap();
            peers.settle().await.unwrap();
            let (conn, recipient) = peers
                .routing()
                .await
                .peers
                .get(name.as_str())
                .map(|peer| (peer.conn().clone(), peer.recipient().clone()))
                .unwrap();
            // The connection keeps what the socket of the receiver doesn't take, and a flush holds
            // the connection as it waits for the receiver to take that.
            let chunk = "x".repeat(CHUNK);
            let chunks: Vec<_> = (0..CHUNKS)
                .map(|_| {
                    Message::signal("/org/zbus/Test", "org.zbus.Test", "Chunk")
                        .build(&chunk)
                        .unwrap()
                })
                .collect();
            recipient.send(&chunks).await;
            let mut flushing = Box::pin(conn.flush());
            assert!(
                flushing.as_mut().now_or_never().is_none(),
                "the flush didn't wait for the receiver"
            );
            let mut handing_over = Box::pin(peers.snapshot());
            assert!(
                handing_over.as_mut().now_or_never().is_none(),
                "the hand-over didn't wait for the flush"
            );

            // The bus flags the client before the flush lets go of the connection, as when the
            // flush times out, and nothing closes the connection before the hand-over gets it.
            recipient.hand_over();
            drop(flushing);
            let (_, clients) = handing_over.await.unwrap();
            assert!(
                clients.iter().all(|client| client.id != 1),
                "the client was handed over, though the bus was disconnecting it"
            );
        });
    }

    /// Let go of the clients of `bus` without disconnecting them, as it hands over, and of `bus`
    /// itself, as its process does as it exits.
    async fn retire(bus: Bus) {
        bus.peers().retire().await;
    }

    /// A client of `bus`, numbered `id`, over a socket pair, and its end of the pair.
    async fn connect(bus: &Bus, id: usize) -> (Connection, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let socket = ours.try_clone().unwrap();
        let uid = rustix::process::getuid().as_raw();
        let server = connection::Builder::unix_stream(theirs);
        let adding = bus.peers().add(bus.guid(), id, server, SEND_TIMEOUT, uid);
        let connecting = connection::Builder::unix_stream(ours).build();
        let (added, conn) = join(adding, connecting).await;
        added.unwrap();

        (conn.unwrap(), socket)
    }

    /// The address of an abstract socket that no other bus listens on.
    fn address() -> String {
        format!(
            "unix:abstract={}",
            Alphanumeric.sample_string(&mut rng(), 10)
        )
    }
}
