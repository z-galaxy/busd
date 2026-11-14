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
use zbus::{connection::Handover, names::OwnedUniqueName, Connection, MessageStream};
use zruntime::{spawn, SharedRuntime};

use super::{Peers, Routing, Served, Us};
use crate::{
    bus::handover::{ClientHandover, MonitorState, NameState, OwnHandover, PeerState, Snapshot},
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
    /// The bus itself pauses as its clients do: it passes nothing more on of what it sends, nor
    /// waits to send it, and takes no more calls once it settles ([`Peers::settle`]). What is on
    /// its way between the bus and its clients from then on stays on the connection on which the
    /// bus serves the D-Bus API, which is handed over with it ([`Peers::hand_over_us`]).
    ///
    /// No send to a client waits for the client to take what the bus sends it from now on: what a
    /// client doesn't take goes on with the hand-over of its connection, for the next instance to
    /// write to the client first. The bus stops writing a client what the instance before it left
    /// unwritten, too, if it still is ([`Peer::flush`]), which the sends to the client, and the
    /// hand-over of its connection, would wait for otherwise. So a client that doesn't read holds
    /// no hand-over up. Once the bus is paused, it takes no more clients: one whose handshake is
    /// still under way by then gets disconnected at once, as [`Peers::add`] says.
    pub(crate) async fn pause(&self, timeout: Duration) -> Result<()> {
        let runtime = SharedRuntime::current()?;
        let deadline = Instant::now() + timeout;
        {
            let mut routing = self.routing_mut().await;
            routing.handing_over = true;
            // The bus keeps what its peer doesn't take of what it sends, as the task that serves
            // that peer is to take no more of it.
            if let Some(bus) = self.bus_conn().clone() {
                let prepared = bus.prepare_hand_over();
                debug_assert!(prepared, "the connection of the bus can't be handed over");
            }
            for peer in routing.peers.values_mut() {
                peer.pause();
            }
            for monitor in routing.monitors.values_mut() {
                monitor.pause();
            }
        }
        loop {
            if Instant::now() < deadline {
                // The listener comes first, so that a handshake that ends right after the check
                // isn't missed.
                let handshake_ended = self.chores_changed.listen();
                if self.chores().connecting > 0 {
                    // Until the deadline at most, past which the handshakes still under way hold
                    // the pause up no more.
                    let _ = runtime.timeout_at(deadline, handshake_ended).await;

                    continue;
                }
            }
            // The listener comes first, so that a client that pauses, or goes, right after the
            // check isn't missed.
            let paused_changed = self.paused_changed.listen();
            {
                // Under the lock that adding a client takes, so that none comes in between.
                let mut routing = self.routing_mut().await;
                if paused(&mut routing) {
                    routing.sealed = true;
                    self.sealed.notify(usize::MAX);

                    return Ok(());
                }
            }
            paused_changed.await;
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

    /// Wait for the bus to settle: to take no more calls, leaving those that it didn't take to its
    /// next instance, to have answered those that it has under way, and to have no chores under
    /// way, so that it sends nothing more.
    ///
    /// A hand-over waits for this once the bus is paused, so the bus only has its own work to
    /// finish, as nothing new comes in for it to do, but for what the clients whose connections
    /// ended meanwhile sent before that, which it takes, as [`Peers::serve_gone`] says: those that
    /// went, and those that the bus disconnected, as they didn't take what it sent them in time, or
    /// as a send to them failed, as one to a client that went does.
    ///
    /// It doesn't wait for what is on its way between the bus and its clients: the calls to the
    /// bus that it didn't take, and what it sent and didn't pass on, go to the next instance on the
    /// connection on which the bus serves the D-Bus API ([`Peers::hand_over_us`]).
    pub(crate) async fn settle(self: &Arc<Self>) -> Result<()> {
        // Only for as long as this waits: the bus is to let go of its connection as it goes.
        let bus = self.bus_conn().clone().context("The bus isn't up")?;
        // The bus takes no more calls from now on, so the chores that its methods left, such as
        // the signals that follow a reply, are under way once it answered those under way.
        bus.object_server().prepare_hand_over().await;
        loop {
            let started = self.chores_done().await;
            // Each client that went is served on as a chore, so the bus isn't settled until then.
            self.serve_gone().await;
            let settled = {
                let chores = self.chores();

                chores.running == 0 && chores.started == started
            };
            if settled {
                return Ok(());
            }
        }
    }

    /// Hand the connection on which the bus serves the D-Bus API over, both ends of it, once the
    /// bus settled: the end on which it answers the calls to it, with those that it didn't take,
    /// and the end of its peer, with what the bus sent and the task that served that peer didn't
    /// take, each with what it took to send and the other end didn't read.
    ///
    /// The end that answers goes first, so that nothing more comes out of it by the time that the
    /// other one goes.
    pub(crate) async fn hand_over_us(&self) -> Result<OwnHandover> {
        let service = self.bus_conn().clone().context("The bus isn't up")?;
        let mut stream = self
            .routing_mut()
            .await
            .peers
            .get_mut(fdo::BUS_NAME)
            .and_then(|bus| bus.paused().take())
            .context("The bus isn't paused")?;
        let service = service
            .hand_over()
            .await
            .context("Failed to hand the connection of the bus over")?;
        let peer = stream
            .hand_over()
            .await
            .context("Failed to hand the connection of the peer of the bus over")?;

        Ok(OwnHandover { service, peer })
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
                        .map(|owner| {
                            let name = owner.unique_name().clone();

                            (name, owner.allow_replacement(), owner.do_not_queue())
                        })
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
    /// gets disconnected, as when a send to it times out. What is left of it once the bus pauses,
    /// to hand over in turn, goes on with that hand-over.
    ///
    /// The bus itself is added to its peers along with them, on the connection of `us`, unless it
    /// is among them already: the end of that connection on which it answers the calls to it is
    /// returned then. The calls to the bus that it was handed over with, and what the instance
    /// before it sent and didn't pass on, so find the peers, monitors and names all back.
    pub(crate) async fn restore(
        self: &Arc<Self>,
        snapshot: Snapshot,
        mut clients: HashMap<usize, ClientHandover>,
        send_timeout: Duration,
        us: Option<Us>,
    ) -> Result<Option<Connection>> {
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
        let service_conn = {
            let mut routing = self.routing_mut().await;
            // The object server of the bus answers the calls that the bus was handed over with,
            // and the task that serves the bus passes on what it was handed over with, only once
            // they get this lock: once all that follows is in place.
            let service_conn = match us {
                Some(us) => Some(self.insert_us(&mut routing, us).await?),
                None => None,
            };
            for NameState { name, owners } in names {
                let mut owners = owners
                    .into_iter()
                    .map(|(unique_name, replace, do_not_queue)| {
                        NameOwner::new(unique_name, replace, do_not_queue)
                    });
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
            for (mut peer, stream, records) in restored {
                for (caller_id, _, serial) in records {
                    if let Some(caller) = callers.get(&caller_id) {
                        peer.pending_calls()
                            .add_handed_over(caller_id, caller, serial);
                    }
                }
                // zbus writes what is left ahead of anything else that the bus sends the client
                // meanwhile.
                peer.flush();
                self.insert(&mut routing, peer, stream);
            }
            for (mut monitor, stream) in restored_monitors {
                monitor.flush();
                self.insert_monitor(&mut routing, monitor, stream);
            }

            service_conn
        };
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

        Ok(service_conn)
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

/// Whether the task that served each peer of `routing`, the bus itself included, and each monitor,
/// gave the stream of its messages back, as the bus pauses.
fn paused(routing: &mut Routing) -> bool {
    routing
        .peers
        .values_mut()
        .all(|peer| peer.paused().is_some())
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
            // A send to the client may have failed, or timed out, as the hand-over waited for its
            // turn to write, and the hand-over may have got the connection before the closing of
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

#[cfg(test)]
mod tests {
    use std::{
        future::poll_fn,
        io::{self, Read, Write},
        mem,
        net::Shutdown,
        num::NonZeroUsize,
        os::unix::net::UnixStream,
        sync::atomic::{AtomicBool, Ordering},
        task::Poll,
    };

    use futures_util::{future::join, FutureExt, StreamExt};
    use ntest::timeout;
    use rand::{
        distr::{Alphanumeric, SampleString},
        rng,
    };
    use zbus::{
        connection::{self, socket::ChannelHandover},
        message,
        object_server::SignalEmitter,
        MatchRule, Message, OwnedGuid,
    };
    use zoter::{Restore, Save};
    use zruntime::{unblock, Event};

    use super::*;
    use crate::{
        bus::{Bus, STOP_TIMEOUT},
        match_rules::MatchRules,
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
            let handed_over = hand_over(bus).await;
            assert!(
                handed_over.clients.iter().any(|client| client.id == 2),
                "the client that closed its end was left out"
            );

            let _next = take_over(handed_over, Carry::AsItIs).await;
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
            let handed_over = hand_over(bus).await;

            let _next = take_over(handed_over, Carry::AsItIs).await;
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

    /// The bus isn't settled while it has a call under way, whether it answers the call itself or
    /// in a task of its own, as zbus has those of the interfaces that it adds to the bus answered.
    /// It settles once the call is answered, and doesn't pass the reply on: the next instance
    /// does, once.
    #[test]
    #[timeout(30000)]
    fn bus_settles_once_the_calls_under_way_are_answered() {
        for inline in [false, true] {
            zruntime::block_on(async {
                let bus = Bus::for_address(Some(&address())).await.unwrap();
                let peers = bus.peers().clone();
                let gate = add_gated(&peers, inline).await;
                let (caller, _socket) = connect(&bus, 1).await;

                let mut calling = pin!(caller.call_method(
                    Some(fdo::BUS_NAME),
                    GATED_PATH,
                    Some(GATED_INTERFACE),
                    "Pass",
                    &0u32,
                ));
                assert!(
                    calling.as_mut().now_or_never().is_none(),
                    "the call was answered before its method returned"
                );
                gate.reached().await;
                peers.pause(STOP_TIMEOUT).await.unwrap();
                let runtime = SharedRuntime::current().unwrap();
                let mut settling = pin!(peers.settle());
                let settled = runtime
                    .timeout(Duration::from_millis(200), settling.as_mut())
                    .await;
                assert!(settled.is_err(), "the bus settled with a call under way");
                gate.open();
                settling.await.unwrap();
                let settled = runtime
                    .timeout(Duration::from_millis(200), calling.as_mut())
                    .await;
                assert!(settled.is_err(), "the bus passed the reply on once paused");

                let handed_over = hand_over(bus).await;
                let _next = take_over(handed_over, Carry::AsItIs).await;
                calling.await.unwrap();
            });
        }
    }

    /// What is on its way between the bus and its clients as the bus hands over goes to the next
    /// instance, on the connection on which the bus serves the D-Bus API, as it is or through its
    /// record, as a bus that systemd restarts stores it: the calls that the bus didn't take, as it
    /// was answering another one, and the reply and the signals that it sent once it paused, more
    /// of each than the connection holds on its way, so that its ends keep some to write. The next
    /// instance answers the calls, and passes the reply and the signals on, each once, in order.
    #[test]
    #[timeout(30000)]
    fn what_is_on_its_way_to_and_from_the_bus_goes_to_the_next_instance() {
        for carry in [Carry::AsItIs, Carry::Records] {
            zruntime::block_on(what_is_on_its_way_goes_to_the_next_instance(carry));
        }
    }

    /// Have a bus hand over with calls to it that it didn't take, and a reply and signals that it
    /// sent once it paused, and check that the next instance, which gets the connection of the
    /// bus as `carry` says, answers the calls and passes the rest on, each once, in order.
    async fn what_is_on_its_way_goes_to_the_next_instance(carry: Carry) {
        // More than the two ends of the connection of the bus hold between them.
        const MANY: u32 = 150;

        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        // The bus answers the calls of the interface itself, one at a time, as those to
        // `org.freedesktop.DBus`, so it takes no call while the method waits at the gate.
        let gate = add_gated(&peers, true).await;
        let (caller, _socket) = connect(&bus, 1).await;
        let rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .interface(GATED_INTERFACE)
            .build()
            .unwrap();
        let call_bus = |member: &'static str| {
            Message::method_call(fdo::DBus::PATH, member)
                .interface(fdo::DBus::INTERFACE)
                .destination(fdo::BUS_NAME)
        };
        caller
            .call_method(
                Some(fdo::BUS_NAME),
                fdo::DBus::PATH,
                Some(fdo::DBus::INTERFACE),
                "AddMatch",
                &rule.to_string(),
            )
            .await
            .unwrap();
        let mut messages = MessageStream::from(&caller);

        let pass = Message::method_call(GATED_PATH, "Pass")
            .interface(GATED_INTERFACE)
            .destination(fdo::BUS_NAME)
            .build(&MANY)
            .unwrap();
        caller.send(&pass).await.unwrap();
        gate.reached().await;
        // The bus forwarded the first call. The others get to the bus as those that it forwards
        // do, the first of them before it pauses, and wait for it to be done with the first call.
        let bus_peer = peers
            .routing()
            .await
            .peers
            .get(fdo::BUS_NAME)
            .map(|bus| bus.conn().clone())
            .unwrap();
        let forwarded: Vec<_> = (0..MANY)
            .map(|_| {
                call_bus("GetId")
                    .sender(caller.unique_name().unwrap())
                    .build(&())
                    .unwrap()
            })
            .collect();
        bus_peer.send(&forwarded[0]).await.unwrap();
        peers.pause(STOP_TIMEOUT).await.unwrap();
        bus_peer.send_all(&forwarded[1..]).await.unwrap();
        let (settled, ()) = join(peers.settle(), async { gate.open() }).await;
        settled.unwrap();
        let handed_over = hand_over(bus).await;
        let own = &handed_over.own;
        assert_ne!(
            own.service.unread_len(),
            0,
            "the bus took the calls that it got while it answered another"
        );
        assert_ne!(
            own.peer.unread_len(),
            0,
            "the bus passed on what it sent once it paused"
        );
        assert_ne!(
            own.service.unwritten_len(),
            0,
            "the bus wrote more than its connection holds"
        );
        assert_ne!(
            own.peer.unwritten_len(),
            0,
            "the peer of the bus wrote more than its connection holds"
        );

        let _next = take_over(handed_over, carry).await;
        // The reply to the first call and the signals of its method, from the instance that handed
        // over, and the replies to the other calls, from the next instance, in order, with nothing
        // more sent to the bus, so that it writes what the two ends kept on its own.
        let serial = |msg: &Message| msg.primary_header().serial_num();
        let mut expected: Vec<_> = [&pass].into_iter().chain(&forwarded).map(serial).collect();
        let mut answered = Vec::new();
        let mut passed = Vec::new();
        let mut collect = async |answers: usize, signals: usize| {
            while answered.len() < answers || passed.len() < signals {
                let msg = messages.next().await.unwrap().unwrap();
                match msg.message_type() {
                    message::Type::MethodReturn => {
                        answered.push(msg.header().reply_serial().unwrap());
                    }
                    message::Type::Signal => {
                        passed.push(msg.body().deserialize::<u32>().unwrap());
                    }
                    _ => (),
                }
            }
        };
        collect(expected.len(), MANY as usize).await;
        // And nothing twice: the reply to a call made now, and a signal emitted now, come right
        // after them.
        let ping = call_bus("GetId").build(&()).unwrap();
        caller.send(&ping).await.unwrap();
        caller
            .emit_signal(None::<()>, GATED_PATH, GATED_INTERFACE, "Passed", &MANY)
            .await
            .unwrap();
        expected.push(serial(&ping));
        collect(expected.len(), MANY as usize + 1).await;
        assert_eq!(answered, expected, "{carry:?}");
        assert_eq!(passed, (0..=MANY).collect::<Vec<_>>(), "{carry:?}");
    }

    /// A bus takes over from one that handed it no connection to serve the D-Bus API on, as a bus
    /// of the version before this one, which made sure that nothing was on its way on that
    /// connection as it stopped: it serves the D-Bus API on a new one, to the clients that it took
    /// over as to any other.
    #[test]
    #[timeout(30000)]
    fn bus_that_was_handed_no_connection_of_its_own_serves_on_a_new_one() {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let (client, _socket) = connect(&bus, 1).await;
            peers.pause(STOP_TIMEOUT).await.unwrap();
            peers.settle().await.unwrap();
            let handed_over = hand_over(bus).await;

            let _next = take_over(handed_over, Carry::NotAtAll).await;
            let has_owner: bool = client
                .call_method(
                    Some(fdo::BUS_NAME),
                    fdo::DBus::PATH,
                    Some(fdo::DBus::INTERFACE),
                    "NameHasOwner",
                    &client.unique_name().unwrap().as_str(),
                )
                .await
                .unwrap()
                .body()
                .deserialize()
                .unwrap();
            assert!(has_owner, "the next instance doesn't know the client");
        });
    }

    /// The connection on which a bus serves the D-Bus API gets to a next instance in another
    /// process as its two ends together, off the channel that joined them, through one record: the
    /// ends of two connections don't go together, and the two that do come back as they went.
    #[test]
    #[timeout(30000)]
    fn connection_of_the_bus_goes_as_both_of_its_ends() {
        zruntime::block_on(async {
            let paused = || async {
                let bus = Bus::for_address(Some(&address())).await.unwrap();
                let peers = bus.peers().clone();
                peers.pause(STOP_TIMEOUT).await.unwrap();
                peers.settle().await.unwrap();

                hand_over(bus).await.own
            };
            let (one, other) = (paused().await, paused().await);
            assert!(ChannelHandover::new(one.service, other.peer).is_err());

            let own = paused().await;
            let channel = ChannelHandover::new(own.service, own.peer).unwrap();
            let restored = stored(&channel);
            assert_eq!(record_of(&restored), record_of(&channel));
            let OwnHandover { service, peer } = OwnHandover::from(restored);
            assert_eq!(
                (service.unread_len(), peer.unread_len()),
                (0, 0),
                "a settled bus left something unread"
            );
        });
    }

    /// The bus is done pausing as soon as the tasks that serve its clients gave the streams of
    /// their messages back, rather than at a later look: a pause takes no longer than its clients
    /// take to pause.
    #[test]
    #[timeout(30000)]
    fn pause_ends_once_the_clients_paused() {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let _first = connect(&bus, 1).await;
            let _second = connect(&bus, 2).await;

            let mut pausing = pin!(peers.pause(STOP_TIMEOUT));
            // The tasks that serve the clients only give their streams back once they run, after
            // this yields.
            assert!(
                pausing.as_mut().now_or_never().is_none(),
                "the clients paused before their tasks ran"
            );
            until(|| {
                peers
                    .routing
                    .try_write()
                    .is_some_and(|mut routing| paused(&mut routing))
            })
            .await;
            assert!(
                matches!(pausing.now_or_never(), Some(Ok(()))),
                "the bus went on pausing once its clients paused"
            );
        });
    }

    /// A client that goes while the bus pauses, as the task that serves it removes it rather than
    /// give the stream of its messages back, holds the pause up no longer: the bus is done pausing
    /// as soon as the client is gone.
    #[test]
    #[timeout(30000)]
    fn peer_that_goes_while_the_bus_pauses_holds_it_up_no_longer() {
        zruntime::block_on(goes_while_the_bus_pauses(false));
    }

    /// As with `peer_that_goes_while_the_bus_pauses_holds_it_up_no_longer`, for a monitor.
    #[test]
    #[timeout(30000)]
    fn monitor_that_goes_while_the_bus_pauses_holds_it_up_no_longer() {
        zruntime::block_on(goes_while_the_bus_pauses(true));
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

    /// A client that the bus starts disconnecting while the hand-over of its connection waits for
    /// its turn to write, as when a send to the client that holds the turn fails, isn't handed
    /// over, even where the hand-over gets the connection before the closing of the connection
    /// does, as with a lock that goes to the first to wait for it.
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

            // The bus flags the client before the flush lets go of the connection, as it flags one
            // that a send fails to, and nothing closes the connection before the hand-over gets it.
            recipient.hand_over();
            drop(flushing);
            let (_, clients) = handing_over.await.unwrap();
            assert!(
                clients.iter().all(|client| client.id != 1),
                "the client was handed over, though the bus was disconnecting it"
            );
        });
    }

    /// Have a client of a bus, a monitor if `monitor`, go while the bus pauses, once the pause took
    /// the lock of the routing ahead of the task that serves the client, which waits for the lock
    /// to remove the client, and check that the bus is done pausing as soon as the task removed it.
    async fn goes_while_the_bus_pauses(monitor: bool) {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (client, socket) = connect(&bus, 1).await;
        let name = client.unique_name().unwrap().clone();
        if monitor {
            assert!(peers.make_monitor(&name, MatchRules::default()).await);
        }
        // The bus is done with what it sent as the client connected, and became a monitor, and,
        // once the runtime ran what was ready, the task that serves the client waits for the client
        // to send something.
        peers.chores_done().await;
        let mut yielded = false;
        until(|| mem::replace(&mut yielded, true)).await;

        {
            // The task that serves the client, which goes, waits for this to remove the client.
            let _reading = peers.routing.try_read().unwrap();
            socket.shutdown(Shutdown::Both).unwrap();
            // A writer that waits for the lock, the task, keeps new readers out.
            until(|| peers.routing.try_read().is_none()).await;
        }
        // The pause takes the lock, which is free, ahead of the task, which only runs again once
        // this yields: the pause doesn't find the client gone.
        let mut pausing = pin!(peers.pause(STOP_TIMEOUT));
        assert!(
            pausing.as_mut().now_or_never().is_none(),
            "the bus was done pausing before the client went"
        );
        until(|| {
            peers.routing.try_write().is_some_and(|routing| {
                !routing.peers.contains_key(&name) && !routing.monitors.contains_key(&name)
            })
        })
        .await;
        assert!(
            matches!(pausing.now_or_never(), Some(Ok(()))),
            "the bus went on pausing once the client went"
        );
    }

    /// A gate that the method of [`Gated`] waits at, until the test opens it.
    #[derive(Default)]
    struct Gate {
        reached: AtomicBool,
        open: AtomicBool,
        changed: Event,
    }

    impl Gate {
        /// Wait at the gate until it is open, and tell that it was reached.
        async fn pass(&self) {
            self.reached.store(true, Ordering::SeqCst);
            self.changed.notify(usize::MAX);
            self.wait_for(&self.open).await;
        }

        /// Wait for a method to have reached the gate.
        async fn reached(&self) {
            self.wait_for(&self.reached).await;
        }

        fn open(&self) {
            self.open.store(true, Ordering::SeqCst);
            self.changed.notify(usize::MAX);
        }

        async fn wait_for(&self, flag: &AtomicBool) {
            loop {
                // The listener comes first, so that a change right after the check isn't missed.
                let listener = self.changed.listen();
                if flag.load(Ordering::SeqCst) {
                    return;
                }
                listener.await;
            }
        }
    }

    /// Defines an interface whose method waits at a gate, and emits signals once it is through:
    /// in a task of its own for each call, as those of the interfaces that zbus adds to the bus
    /// run, or not, as those of the interfaces of the bus itself run, as `spawn` says.
    macro_rules! gated {
        ($name:ident, $spawn:literal) => {
            struct $name(Arc<Gate>);

            #[zbus::interface(name = "org.busd.Test.Gated", spawn = $spawn)]
            impl $name {
                async fn pass(
                    &self,
                    signals: u32,
                    #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
                ) -> zbus::fdo::Result<()> {
                    self.0.pass().await;
                    for n in 0..signals {
                        Self::passed(&emitter, n).await?;
                    }

                    Ok(())
                }

                #[zbus(signal)]
                async fn passed(emitter: &SignalEmitter<'_>, n: u32) -> zbus::Result<()>;
            }
        };
    }

    gated!(Gated, true);
    gated!(InlineGated, false);

    const GATED_PATH: &str = "/org/busd/Test/Gated";
    const GATED_INTERFACE: &str = "org.busd.Test.Gated";

    /// Add an interface whose method waits at a gate to the bus of `peers`, which answers its
    /// calls itself if `inline`, and in tasks of their own otherwise, and return the gate.
    async fn add_gated(peers: &Peers, inline: bool) -> Arc<Gate> {
        let gate = Arc::new(Gate::default());
        let service = peers.bus_conn().clone().unwrap();
        let server = service.object_server();
        // What was there before, if anything.
        let free = match inline {
            false => server
                .at(GATED_PATH, Gated(gate.clone()))
                .await
                .map(|was| was.is_none()),
            true => server
                .at(GATED_PATH, InlineGated(gate.clone()))
                .await
                .map(|was| was.is_none()),
        };
        assert!(free.unwrap(), "the bus has the interface already");

        gate
    }

    /// What a bus hands over, as [`hand_over`] takes it out of the bus.
    struct HandedOver {
        guid: OwnedGuid,
        snapshot: Snapshot,
        clients: Vec<ClientHandover>,
        own: OwnHandover,
    }

    /// How the connection on which a bus serves the D-Bus API gets to its next instance.
    #[derive(Clone, Copy, Debug)]
    enum Carry {
        /// As it is, as to a next instance in the same process.
        AsItIs,
        /// Through the record of its two ends together, as to a next instance in another process.
        Records,
        /// Not at all, as from a bus of the version before this one, which handed none over.
        NotAtAll,
    }

    /// Have `bus`, which is paused and settled, hand over, as [`Bus::hand_over`] does once it is:
    /// its own connection, then those of its clients. It lets go of its clients without
    /// disconnecting them, and of `bus` itself, as its process does as it exits.
    async fn hand_over(bus: Bus) -> HandedOver {
        let peers = bus.peers().clone();
        let own = peers.hand_over_us().await.unwrap();
        let (snapshot, clients) = peers.snapshot().await.unwrap();
        let guid = bus.guid().clone();
        peers.retire().await;

        HandedOver {
            guid,
            snapshot,
            clients,
            own,
        }
    }

    /// The peers of the next instance of the bus that handed `handed_over` over, which gets the
    /// connection on which the bus serves the D-Bus API as `carry` says, and the end of that
    /// connection on which it answers the calls to it.
    async fn take_over(handed_over: HandedOver, carry: Carry) -> (Arc<Peers>, Connection) {
        let HandedOver {
            guid,
            snapshot,
            clients,
            own,
        } = handed_over;
        let own = match carry {
            Carry::AsItIs => Some(own),
            Carry::Records => {
                let channel = ChannelHandover::new(own.service, own.peer).unwrap();

                Some(OwnHandover::from(stored(&channel)))
            }
            Carry::NotAtAll => None,
        };
        let peers = Peers::new();
        let us = Bus::us(&peers, &guid, own).await.unwrap();
        let clients = clients
            .into_iter()
            .map(|client| (client.id, client))
            .collect();
        let service = peers
            .restore(snapshot, clients, SEND_TIMEOUT, Some(us))
            .await
            .unwrap()
            .unwrap();

        (peers, service)
    }

    /// `channel`, the two ends of the connection of a bus off their channel, as zoter gives them
    /// back to a next instance in another process: restored from their record and their file
    /// descriptors.
    fn stored(channel: &ChannelHandover) -> ChannelHandover {
        let fds = channel
            .fds()
            .into_iter()
            .map(|fd| fd.try_clone_to_owned().unwrap())
            .collect();

        ChannelHandover::restore(&record_of(channel), fds).unwrap()
    }

    /// The record that `channel` saves.
    fn record_of(channel: &ChannelHandover) -> Vec<u8> {
        let mut record = vec![0; channel.saved_len().unwrap()];
        channel.save(&mut record).unwrap();

        record
    }

    /// Wait until `ready`, which is checked again each time that the runtime has run the tasks that
    /// are ready to run.
    async fn until<F>(mut ready: F)
    where
        F: FnMut() -> bool,
    {
        poll_fn(|cx| {
            if ready() {
                return Poll::Ready(());
            }
            cx.waker().wake_by_ref();

            Poll::Pending
        })
        .await
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
