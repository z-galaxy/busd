//! The hand-over of the peers, monitors and names to the next instance of the bus.

use std::{
    collections::HashMap,
    future::Future,
    iter, mem,
    num::NonZeroU32,
    pin::pin,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use futures_util::future::{select, Either};
use serde::{Deserialize, Serialize};
use zbus::{
    names::{OwnedUniqueName, OwnedWellKnownName},
    Type,
};
use zruntime::{spawn, SharedRuntime};

use super::{Peers, Routing, Served};
use crate::{
    connection::{Handover, Reader},
    debug,
    match_rules::MatchRules,
    name_registry::{NameOwner, NameRegistry},
    peer::{unique_name, Caller, Client, HandedOver, Monitor, Peer, Stream},
    warn,
};

impl Peers {
    /// Pause the bus, for it to hand over: have the task that serves each client, peer or
    /// monitor, take no more of its messages, once it is done with those that it took, and give
    /// the reader of the messages back, and wait for that. The clients that connect from now on
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
    /// instance left unwritten, if that write is still under way ([`Client::flush`]), just as it
    /// stops the sends. So a client that doesn't read holds up no hand-over. Once the bus is
    /// paused, it takes no more clients: if a client's handshake is still under way by then, the
    /// bus disconnects it at once, as [`Peers::add`] says.
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
    /// finish, as nothing new comes in for it to do, but for what the clients that went meanwhile
    /// sent before that, which it takes, as [`Peers::serve_gone`] says: those that closed their
    /// ends, and those that the bus disconnected, as they didn't take what it sent them in time, or
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

    /// Take the peers, monitors and names out of the bus as it hands them over, with the hand-over
    /// of each client's connection, and take no more calls.
    ///
    /// The connections of the clients are handed over all at once, each with the bytes that the bus
    /// read from the client and didn't take, and what the bus sent the client and the client didn't
    /// take, which nothing waits for the client to take. What a client sent and the bus didn't read
    /// is still in its socket, which goes with its connection. That of a client that went since the
    /// bus settled, as it closed its end, is handed over too, with what it sent, for the next
    /// instance to pass that on and clean up after the client. A client whose connection fails to
    /// be handed over, as when the bus is disconnecting it, or when its socket fails to be
    /// duplicated, as the process has too many file descriptors open, gets disconnected: the bus
    /// leaves it out, so that the next instance cleans up after it as after a client that went.
    ///
    /// The bus lets go of the other clients without disconnecting them: nothing more goes to them
    /// from this instance of the bus.
    pub(crate) async fn hand_over(&self) -> (Snapshot, Vec<ClientHandover>) {
        let mut routing = self.routing_mut().await;
        let routing = &mut *routing;
        let mut handed_over = Vec::new();
        let mut peers = Vec::new();
        for (_, peer) in routing.peers.drain() {
            peers.push(PeerState {
                id: peer.client().id() as u64,
                greeted: peer.greeted(),
                match_rules: peer.client().match_rules().clone(),
                pending_calls: peer
                    .pending_calls()
                    .records()
                    .map(|(caller_id, caller, serial)| (caller_id as u64, caller, serial.get()))
                    .collect(),
            });
            handed_over.push(peer.into_client());
        }
        let mut monitors = Vec::new();
        for monitor in mem::take(&mut routing.monitors).into_values() {
            let client = monitor.into_client();
            monitors.push(MonitorState {
                id: client.id() as u64,
                match_rules: client.match_rules().clone(),
            });
            handed_over.push(client);
        }
        let names = routing
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
            .collect();
        routing.name_registry = NameRegistry::default();
        // The bus takes no more calls.
        self.inner.itself.recipient.hand_over();
        let clients = handed_over
            .into_iter()
            .filter_map(|client| {
                let id = client.id();
                let connection = client.hand_over()?;

                Some(ClientHandover { id, connection })
            })
            .collect();
        let snapshot = Snapshot {
            peers,
            monitors,
            names,
        };

        (snapshot, clients)
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
        // The bus reads nothing of the clients, and so forwards nothing, until all of its peers,
        // monitors and names are back.
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
                // The connection writes what is left ahead of anything else that the bus sends the
                // client meanwhile.
                peer.client_mut().flush();
                self.insert(&mut routing, peer, stream);
            }
            for (mut monitor, reader) in restored_monitors {
                monitor.client_mut().flush();
                self.insert_monitor(&mut routing, monitor, reader);
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

    /// Add `monitor`, whose messages `reader` reads, to the monitors of `routing`, and serve it
    /// until it goes.
    fn insert_monitor(&self, routing: &mut Routing, monitor: Monitor, reader: Reader) {
        let unique_name = monitor.client().unique_name().clone();
        let listener = monitor.client().listen_cancellation();
        routing.monitors.insert(unique_name.clone(), monitor);
        spawn(self.clone().serve_monitor(reader, listener, unique_name)).detach();
    }

    /// Serve on the clients, peers and monitors, that went while the bus paused, once the tasks
    /// that served them have given the readers of their messages back.
    ///
    /// Each client is served in its own chore, until its EOF. The bus reads and takes what the
    /// client sent before it went, and cleans up after it, as after any client that goes. A client
    /// went if it closed its end or shut its writes down, or if the bus disconnected it meanwhile,
    /// as the bus does once a send to a client fails or times out. The client's socket reports that
    /// the client closed its end or shut its writes down, whatever the socket still holds to read.
    ///
    /// The bus hands over no connection that it disconnects, and what it didn't read of such a
    /// client would be lost otherwise. The bus disconnects a client that went too, if it sends the
    /// client anything before this finds it gone, as the send fails. So the bus serves the clients
    /// that went here, while their recipients are still its clients, and leaves only those that go
    /// after that to the hand-over. The next instance serves those clients until their EOF.
    async fn serve_gone(&self) {
        let mut guard = self.routing_mut().await;
        let routing = &mut *guard;
        for peer in routing.peers.values_mut() {
            let Some(reader) = peer.client_mut().take_reader_if_gone() else {
                continue;
            };
            let stream = Stream::for_peer(peer, reader);
            let serving = self.clone().serve_peer(
                stream,
                peer.client().listen_cancellation(),
                Served::new(peer),
            );
            self.spawn_chore(serving);
        }
        for monitor in routing.monitors.values_mut() {
            let Some(reader) = monitor.client_mut().take_reader_if_gone() else {
                continue;
            };
            let client = monitor.client();
            let serving = self.clone().serve_monitor(
                reader,
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
    /// The hand-over of the client's connection: its socket, the bytes that the bus read from the
    /// client and didn't take, and what is left to write to the client, with the file descriptors
    /// of both.
    ///
    /// The next instance passes those on.
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

/// Whether the task that served each peer of `routing`, and each monitor, gave the reader of its
/// messages back, as the bus pauses.
fn paused(routing: &mut Routing) -> bool {
    routing
        .clients_mut()
        .all(|client| client.paused().is_some())
}

impl Client {
    /// Take the reader of the client's messages, if the task that served the client gave it back
    /// when the bus paused, and the client went meanwhile.
    ///
    /// The client went if it closed its end of its socket or shut its writes down, or if the bus is
    /// disconnecting it.
    fn take_reader_if_gone(&mut self) -> Option<Reader> {
        let gone = self.connection().is_disconnecting()
            || self.paused().as_ref().is_some_and(Reader::is_closed);
        if !gone {
            return None;
        }

        self.paused().take()
    }
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
