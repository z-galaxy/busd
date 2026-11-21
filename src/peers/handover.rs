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
use serde::{Deserialize, Serialize};
use zbus::{
    connection::Handover,
    names::{OwnedUniqueName, OwnedWellKnownName},
    Connection, MessageStream, Type,
};
use zruntime::{spawn, SharedRuntime};

use super::{Peers, Routing, Served};
use crate::{
    debug,
    match_rules::MatchRules,
    name_registry::{NameOwner, NameRegistry},
    peer::{unique_name, Caller, Client, HandedOver, Monitor, Peer, Recipient, Stream},
    warn,
};

impl Peers {
    /// Pause the bus, for it to hand over: have the task that serves each client, peer or
    /// monitor, take no more of its messages, once it is done with those that it took, and give
    /// the stream of the messages back, and wait for that. The clients that connect from now on
    /// pause as soon as they are connected, and those in the middle of their handshakes have
    /// `timeout` to be done with them.
    ///
    /// The bus goes on answering the calls to it that its clients sent before they paused, while
    /// the task of each client queues the calls to the bus that it took. [`Peers::settle`] waits
    /// for the bus to have answered them. The calls that the tasks didn't take stay with the
    /// connections of their clients, and go to the next instance with those connections.
    ///
    /// From now on, no send to a client waits for the client to take what the bus sends it. What a
    /// client doesn't take goes along with the hand-over of its connection, and the next instance
    /// writes it to the client first. The bus also stops writing to a client what the previous
    /// instance left unwritten, if that write is still under way ([`Client::flush`]). Otherwise,
    /// the sends to the client and the hand-over of its connection would wait for that write. So a
    /// client that doesn't read holds up no hand-over. Once the bus is paused, it takes no more
    /// clients: if a client's handshake is still under way by then, the bus disconnects it at
    /// once, as [`Peers::add`] says.
    pub(crate) async fn pause(&self, timeout: Duration) -> Result<()> {
        let runtime = SharedRuntime::current()?;
        let deadline = Instant::now() + timeout;
        {
            let mut routing = self.routing_mut().await;
            routing.handing_over = true;
            for client in routing.clients_mut() {
                client.pause();
            }
        }
        loop {
            if Instant::now() < deadline {
                // The listener comes first, so that a handshake that ends right after the check
                // isn't missed.
                let handshake_ended = self.inner.chores.listen();
                if self.inner.chores.handshakes() > 0 {
                    // Until the deadline at most, past which the handshakes still under way hold
                    // the pause up no more.
                    let _ = runtime.timeout_at(deadline, handshake_ended).await;

                    continue;
                }
            }
            // The listener comes first, so that a client that pauses, or goes, right after the
            // check isn't missed.
            let paused_changed = self.inner.paused_changed.listen();
            {
                // Under the lock that adding a client takes, so that none comes in between.
                let mut routing = self.routing_mut().await;
                if paused(&mut routing) {
                    routing.sealed = true;
                    self.inner.sealed.notify(usize::MAX);

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
                let listener = self.inner.sealed.listen();
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

    /// Wait for the bus to settle: to have no chores under way, such as its clean-up after a
    /// client that went, or calls to the bus that it has yet to answer.
    ///
    /// A hand-over waits for this once the bus is paused, so the bus only has its own work to
    /// finish, as nothing new comes in for it to do, but for what the clients whose connections
    /// ended meanwhile sent before that, which it takes, as [`Peers::serve_gone`] says: those that
    /// went, and those that the bus disconnected, as they didn't take what it sent them in time, or
    /// as a send to them failed, as one to a client that went does.
    ///
    /// Each call to the bus is a chore from the moment its caller's task queues it until the bus
    /// has sent the reply and the signals that follow the reply. So the bus answers every call that
    /// it took before it settles. Those are the calls under way, the calls in the queue, and the
    /// calls that clients' tasks wait to queue while the queue is full. It doesn't wait for the
    /// clients to take what it sends them.
    pub(crate) async fn settle(&self) {
        loop {
            let started = self.inner.chores.done().await;
            // Each client that went is served on as a chore, so the bus isn't settled until then.
            self.serve_gone().await;
            if self.inner.chores.settled_since(started) {
                return;
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
            for peer in routing.peers.values_mut() {
                let state = PeerState {
                    id: peer.client().id() as u64,
                    greeted: peer.greeted(),
                    match_rules: peer.client().match_rules().clone(),
                    pending_calls: peer
                        .pending_calls()
                        .records()
                        .map(|(caller_id, caller, serial)| (caller_id as u64, caller, serial.get()))
                        .collect(),
                };
                clients.push(Outgoing::new(peer.client_mut(), ClientState::Peer(state)));
            }
            for monitor in routing.monitors.values_mut() {
                let client = monitor.client_mut();
                let state = MonitorState {
                    id: client.id() as u64,
                    match_rules: client.match_rules().clone(),
                };
                clients.push(Outgoing::new(client, ClientState::Monitor(state)));
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
        // The bus takes no more calls.
        self.inner.itself.recipient.hand_over();
        let mut routing = self.routing_mut().await;
        for client in routing.clients_mut() {
            client.recipient().hand_over();
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
    pub(crate) async fn restore(
        &self,
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
            let unique_name = unique_name(id);
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
            match Peer::handed_over(client, send_timeout, &self.inner.users).await {
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
            match Peer::handed_over(client, send_timeout, &self.inner.users).await {
                Ok((peer, stream)) => {
                    let monitor = peer.become_monitor(state.match_rules);
                    restored_monitors.push((monitor, stream.into_inner()));
                }
                Err(e) => {
                    let unique_name = unique_name(id);
                    warn!("Disconnecting the monitor `{unique_name}`, as serving it failed: {e}")
                }
            }
        }
        {
            let mut routing = self.routing_mut().await;
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
                    .release_all(unique_name.inner().clone());
            }
            // The calls that wait on the peers, of the callers that are back too: the bus would
            // forget the calls of the others as it cleans up after them.
            let callers: HashMap<usize, Arc<Caller>> = routing
                .peers
                .values()
                .map(|peer| (peer.client().id(), peer.caller().clone()))
                .chain(
                    restored
                        .iter()
                        .map(|(peer, _, _)| (peer.client().id(), peer.caller().clone())),
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
                peer.client_mut().flush();
                self.insert(&mut routing, peer, stream);
            }
            for (mut monitor, stream) in restored_monitors {
                monitor.client_mut().flush();
                self.insert_monitor(&mut routing, monitor, stream);
            }
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
    fn insert_monitor(&self, routing: &mut Routing, monitor: Monitor, stream: MessageStream) {
        let unique_name = monitor.client().unique_name().clone();
        let listener = monitor.client().listen_cancellation();
        routing.monitors.insert(unique_name.clone(), monitor);
        spawn(self.clone().serve_monitor(stream, listener, unique_name)).detach();
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
    async fn serve_gone(&self) {
        let mut guard = self.routing_mut().await;
        let routing = &mut *guard;
        for peer in routing.peers.values_mut() {
            let Some(stream) = gone(peer.client_mut()) else {
                continue;
            };
            let stream = Stream::for_peer(peer, stream);
            let serving = self.clone().serve_peer(
                stream,
                peer.client().listen_cancellation(),
                Served::new(peer),
            );
            self.spawn_chore(serving);
        }
        for monitor in routing.monitors.values_mut() {
            let Some(stream) = gone(monitor.client_mut()) else {
                continue;
            };
            let client = monitor.client();
            let serving = self.clone().serve_monitor(
                stream,
                client.listen_cancellation(),
                client.unique_name().clone(),
            );
            self.spawn_chore(serving);
        }
    }

    /// Run `future` in a task of its own, as a chore of the bus, until it is done.
    #[track_caller]
    fn spawn_chore<F>(&self, future: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let chore = self.chore();
        spawn(async move {
            let _chore = chore;

            future.await
        })
        .detach();
    }
}

/// What a bus hands over of a client, besides what the state of the bus holds of it: its
/// connection.
#[derive(Debug)]
pub struct ClientHandover {
    /// The number of the client, which its unique name carries.
    pub id: usize,
    /// The hand-over of the connection of the client: its socket, with what zbus read of the
    /// client and the bus didn't take, and the file descriptors that came with that, which the
    /// next instance passes on.
    pub connection: Handover,
}

/// The peers, monitors and names of a bus, as it hands them over.
#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) peers: Vec<PeerState>,
    pub(crate) monitors: Vec<MonitorState>,
    pub(crate) names: Vec<NameState>,
}

/// A peer, as the bus hands it over.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct PeerState {
    /// The number of the peer, which its unique name carries.
    pub(crate) id: u64,
    /// Whether the peer called `Hello`.
    pub(crate) greeted: bool,
    pub(crate) match_rules: MatchRules,
    /// The calls to the peer that wait for its reply: the number of the caller, its unique name,
    /// and the serial of the call.
    pub(crate) pending_calls: Vec<(u64, OwnedUniqueName, u32)>,
}

/// A monitor, as the bus hands it over.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct MonitorState {
    /// The number of the peer that became the monitor.
    pub(crate) id: u64,
    pub(crate) match_rules: MatchRules,
}

/// A well-known name, as the bus hands it over: its owner first, then those that queue for it, in
/// order, each with whether it lets another take the name over from it, and whether it doesn't
/// queue for the name once that happens.
#[derive(Debug, Deserialize, Serialize, Type)]
pub(crate) struct NameState {
    pub(crate) name: OwnedWellKnownName,
    pub(crate) owners: Vec<(OwnedUniqueName, bool, bool)>,
}

/// Whether the task that served each peer of `routing`, and each monitor, gave the stream of its
/// messages back, as the bus pauses.
fn paused(routing: &mut Routing) -> bool {
    routing
        .clients_mut()
        .all(|client| client.paused().is_some())
}

/// The message stream of `client`, if the client's connection ended while the bus paused and the
/// task that served the client gave the stream back.
fn gone(client: &mut Client) -> Option<MessageStream> {
    if !client.conn().is_closed() {
        return None;
    }

    client.paused().take()
}

/// A client that the bus is handing over. See [`Peers::snapshot`].
struct Outgoing {
    /// The number of the client.
    id: usize,
    conn: Connection,
    recipient: Recipient,
    /// The stream of the messages of the client, which the task that served it gave back.
    stream: Option<MessageStream>,
    state: ClientState,
}

impl Outgoing {
    /// `client` as the bus hands it over, with `state`, the part of the bus's state that concerns
    /// the client.
    fn new(client: &mut Client, state: ClientState) -> Self {
        Self {
            id: client.id(),
            conn: client.conn().clone(),
            recipient: client.recipient().clone(),
            stream: client.paused().take(),
            state,
        }
    }

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
mod tests;
