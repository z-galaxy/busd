//! A bus that systemd runs: on the socket of its socket activation, and handed over to its next
//! instance through its file descriptor store.
//!
//! # A restart through the store
//!
//! An instance of busd that stops hands its state and the connections of its clients over to the
//! next instance through the file descriptor store that systemd keeps for the service across its
//! restarts, the store. The clients notice nothing but a pause: the next instance serves them on
//! the same connections, with the same names, match rules and calls that wait for replies, and
//! passes on what was on its way through the bus.
//!
//! A hand-over takes file descriptors (FDs) of many kinds, which these docs name so:
//!
//! - The **listening socket** is the socket that the bus accepts its clients on. That of socket
//!   activation is systemd's, and one that the bus binds itself is its own.
//! - The **client socket** of a client is the end of its connection that the bus has.
//! - The **client-sent FDs** of a client are those that it sent with its messages, or ahead of
//!   them, which the bus read and didn't take yet.
//! - The **outgoing FDs** of a client are those of the messages that the bus is sending it and
//!   didn't write to it yet.
//! - The **connection FDs** of a client are its client socket, its client-sent FDs and its outgoing
//!   FDs, in that order, as [`connection::Handover::fds`] gives them. Those of all the clients
//!   follow each other, in the order of the hand-over, each with its index among them, from 0 on.
//! - The **state memfd** holds the state of the bus, and the record that zbus saves of each
//!   connection.
//! - A **queue socket** holds connection FDs in its queue, in datagrams ([`FdQueue`]).
//! - The **stored FDs** are those that the bus puts in the store: the state memfd and the queue
//!   sockets.
//! - The **passed FDs** are those that systemd passes the next instance as it starts it: the
//!   listening socket of the socket activation and the stored FDs, with their names in
//!   `$LISTEN_FDNAMES`.
//! - The **process FD** of a client is the pidfd of its process, which the next instance opens to
//!   look the credentials of the client up.
//!
//! The bus hands over under socket activation alone, as [Socket activation](#socket-activation)
//! tells. On `SIGTERM`, it pauses its clients, finishes what it took, and hands their connections
//! over ([`Bus::hand_over`]). It stores the state memfd and the queue sockets, which hold the
//! connection FDs, and it exits once systemd dealt with all of them
//! ([`Bus::hand_over_to_systemd`]). systemd starts the next instance with the passed FDs, which it
//! takes over from ([`Bus::for_systemd`]):
#![doc = simple_mermaid::mermaid!("systemd/restart.mmd")]
//!
//! The store keeps each stored FD under a name of busd's own, which tells the next instance what
//! it is, as systemd passes them in an order of its own:
//!
//! | Name           | Stored FD         | What it holds               |
//! |----------------|-------------------|-----------------------------|
//! | `busd-state`   | The state memfd   | The state, then the records |
//! | `busd-clients` | Each queue socket | Connection FDs              |
//!
//! The state tells, for each client, where the record of its connection is in the state memfd,
//! with its offset and its length, and where its connection FDs are, with the index of the first
//! and how many. With clients 1, 2 and 4, say, the first of which has a client-sent FD and an
//! outgoing FD:
//!
//! | Connection FD | What it is     | Of client | In the state (first, how many) |
//! |---------------|----------------|-----------|--------------------------------|
//! | 0             | Client socket  | 1         | 0, 3                           |
//! | 1             | Client-sent FD | 1         |                                |
//! | 2             | Outgoing FD    | 1         |                                |
//! | 3             | Client socket  | 2         | 3, 1                           |
//! | 4             | Client socket  | 4         | 4, 1                           |
//!
//! A queue socket holds the connection FDs in datagrams of up to 253, each with the index of its
//! first connection FD, so that it doesn't matter either which queue socket holds which: the first
//! datagram has the index 0 and connection FDs 0 to 252, the next one 253 and connection FDs 253
//! to 505, and so on, and the next queue socket, if any, goes on where the previous one ends. Each
//! takes some 70000 connection FDs, with the default size of its send buffer, so a store of 16
//! takes almost a million.
//!
//! The next instance takes them out once it raised its soft limit of open FDs to the hard one, so
//! the connection FDs and the process FDs of the clients have to fit under that, which [`fit`] sees
//! to: the clients that don't fit get disconnected as the bus goes. A client-sent FD that is of the
//! same open file description as another, as one that a client sent twice, goes as it is, unlike
//! in the store, which keeps one FD of each.
//!
//! # Socket activation
//!
//! The hand-over takes socket activation, which makes the listening socket systemd's: systemd
//! passes it to every instance, among the passed FDs, under the name of its socket unit, and keeps
//! it open while no instance runs, with the clients that connect meanwhile in its backlog. The bus
//! listens on it whatever its address says.
//!
//! Without socket activation, the bus binds a listening socket of its own, which nothing keeps for
//! the next instance, so it doesn't hand over: it disconnects its clients as it goes, and removes
//! the file of its listening socket, as busd does without systemd. Nor does the next instance take
//! over without socket activation, as when its socket unit went meanwhile: it drops what was handed
//! over, which disconnects the clients, and binds a listening socket of its own.

use std::{
    collections::{BTreeSet, HashMap},
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rustix::{
    io::fcntl_dupfd_cloexec,
    pipe::{pipe_with, PipeFlags},
};
use zbus::connection;
use zruntime::SharedRuntime;

use super::{
    handover::{State, Stored},
    Bus, ClientHandover, Handover,
};
use crate::{
    debug, error, info,
    peer::SEND_TIMEOUT,
    systemd::{self, FdQueue, Notifier},
    warn,
};

impl Bus {
    /// Create a bus that systemd runs, out of the passed FDs, `fds`, with their names, as
    /// [`systemd::take_listen_fds`] gives them, and the service manager that `notifier` reaches,
    /// if any.
    ///
    /// The bus listens on the listening socket of the socket activation, if systemd passed one,
    /// whatever `address` says, and takes over there from the instance that handed over through
    /// the store, as [`Bus::hand_over_to_systemd`] does, if one did. Taking over may fail, as when
    /// the previous instance encoded its state otherwise: the clients that were handed over get
    /// disconnected then, and the bus starts as if nothing was handed over. Without socket
    /// activation, the bus listens at `address`, as [`Bus::for_address`] does, and drops what was
    /// handed over, if anything, which disconnects the clients. busd listens on one socket, so
    /// this fails if systemd passed more than one listening socket of socket activation. All the
    /// stored FDs are removed from the store first, so that whatever comes of it, the instances
    /// that come after don't find them again.
    ///
    /// With a service manager to hand over to, and the listening socket of the socket activation,
    /// the bus keeps a few FDs open from the start, before it serves anyone, to give up for those
    /// that storing its hand-over opens, however many client sockets and client-sent FDs its
    /// clients take by then.
    pub async fn for_systemd(
        mut fds: Vec<(String, OwnedFd)>,
        notifier: Option<&Notifier>,
        address: Option<&str>,
    ) -> Result<Self> {
        let activation = Self::take_activation(&mut fds)?;
        let reserve = match (notifier, &activation) {
            (Some(_), Some(_)) => Reserve::new()
                .inspect_err(|e| warn!("Failed to reserve file descriptors to hand over with: {e}"))
                .ok(),
            _ => None,
        };
        if let Some(notifier) = notifier {
            // More than one stored FD may come under a name, as the queue sockets do.
            let names: BTreeSet<_> = fds.iter().map(|(name, _)| name).collect();
            for name in names {
                if let Err(e) = notifier.remove(name) {
                    warn!("Failed to remove `{name}` from the store: {e}");
                }
            }
        }
        let Some(activation) = activation else {
            if !fds.is_empty() {
                warn!(
                    "Dropping what the previous instance of the bus handed over, which disconnects \
                     its clients, as systemd passed no socket of socket activation to take over on"
                );
            }

            return Self::for_address(address).await;
        };
        let mut bus = Self::out_of_stored(fds, activation).await?;
        bus.reserve = reserve;

        Ok(bus)
    }

    /// Hand the bus over to its next instance, through the store of systemd, which `notifier`
    /// reaches, if the bus listens on the listening socket of the socket activation, which systemd
    /// keeps for the next instance, and systemd keeps a store that takes the state memfd and a
    /// queue socket, as `$FDSTORE` tells. Otherwise, the bus says why it doesn't hand over, and
    /// cleans up as [`Bus::cleanup`] does, only logging a failure to, as busd does without systemd.
    ///
    /// The bus hands over as [`Bus::hand_over`] does, and stores what it hands over, for
    /// [`Bus::for_systemd`] to take over from on the restart: the state memfd, with the records of
    /// the connections of the clients that fit, and then the connection FDs of those clients, in
    /// the order of the hand-over, in queue sockets ([`FdQueue`]), as many as that takes. So the
    /// store takes the connection FDs, however many there are, each as it is, as two client-sent
    /// FDs of one open file description, which the store would keep once. The next instance takes
    /// them out of the queue sockets once it raised its soft limit of open FDs to the hard one, as
    /// busd does as it starts, so they have to fit under `fd_limit`, the soft limit that busd
    /// raised its own to as it started, `None` for no limit, unless the limit of the unit changed
    /// meanwhile, with the process FD of each client. The clients that don't fit get disconnected
    /// as the bus goes, with a warning. It fails if the bus takes longer than 30 s to hand over,
    /// or if storing fails, which disconnects the clients as the bus goes.
    pub async fn hand_over_to_systemd(
        self,
        notifier: &Notifier,
        fd_limit: Option<u64>,
    ) -> Result<()> {
        let Some(max) = self.store_max() else {
            if let Err(e) = self.cleanup().await {
                error!("Failed to clean up: {e}");
            }

            return Ok(());
        };
        info!("Handing over to the next instance of the bus..");
        let mut handover = self.hand_over_within_reserve().await?;
        handover.fit(fd_limit);

        handover.store(notifier, max)
    }

    /// How many stored FDs the store of systemd takes, `$FDSTORE`, if the bus can hand over
    /// through it: the store takes the state memfd and a queue socket, and the bus listens on the
    /// listening socket of the socket activation, which systemd keeps for the next instance,
    /// rather than on one of its own. It says why the bus can't otherwise.
    fn store_max(&self) -> Option<usize> {
        let max = match systemd::fd_store_max() {
            Some(max) if max >= MIN_STORE => max,
            Some(max) => {
                warn!(
                    "Not handing over to the next instance of the bus, as the file descriptor \
                     store takes {max}, which is too few: raise `FileDescriptorStoreMax=` of the \
                     service."
                );

                return None;
            }
            None => {
                info!(
                    "Not handing over to the next instance of the bus, as systemd keeps no file \
                     descriptor store for it: `$FDSTORE` isn't set."
                );

                return None;
            }
        };
        if self.bound {
            warn!(
                "Not handing over to the next instance of the bus, as it listens on a socket of \
                 its own rather than on one of socket activation, which systemd would keep for \
                 the next instance: add a socket unit for the service."
            );

            return None;
        }

        Some(max)
    }

    /// Take the listening socket of the socket activation, if systemd passed one, out of the passed
    /// FDs, `fds`, with their names, which leaves the stored FDs.
    ///
    /// The stored FDs have names of busd's own, as [`is_busds`] tells. The others are listening
    /// sockets of socket activation, which have the names of their units by default, such as
    /// `busd.socket`. busd listens on one, so this fails if there is more than one.
    fn take_activation(fds: &mut Vec<(String, OwnedFd)>) -> Result<Option<OwnedFd>> {
        let mut sockets = fds.extract_if(.., |(name, _)| !is_busds(name));
        let activation = sockets.next().map(|(_, fd)| fd);
        if sockets.next().is_some() {
            bail!("systemd passed more than one socket to listen on: busd listens on one.");
        }

        Ok(activation)
    }

    /// The next instance of the bus that handed over the stored FDs, `stored`, with the names that
    /// it stored each under, on `activation`, the listening socket of the socket activation.
    ///
    /// Without a hand-over to take over, or if taking over fails, it is a bus on `activation`, as
    /// [`Bus::for_listener`] creates.
    async fn out_of_stored(stored: Vec<(String, OwnedFd)>, activation: OwnedFd) -> Result<Self> {
        let Some(handover) = Handover::from_stored(stored) else {
            return Self::for_listener(activation).await;
        };
        let clients = handover.clients.len();
        let fallback = activation.try_clone()?;
        match Self::take_over(handover, Some(activation), SEND_TIMEOUT).await {
            Ok(bus) => {
                info!("Took {clients} clients over from the previous instance of the bus.");

                Ok(bus)
            }
            Err(e) => {
                error!(
                    "Failed to take over from the previous instance of the bus, which disconnects \
                     its {clients} clients: {e:#}"
                );

                Self::for_listener(fallback).await
            }
        }
    }

    /// Hand the bus over, as [`Bus::hand_over`] does, within [`HAND_OVER_TIMEOUT`], with the FDs
    /// that it reserved ([`Reserve`]) given up as this returns, for those that storing opens in
    /// turn, once the bus is handed over: the state memfd, the queue sockets and the pipe of the
    /// barrier.
    async fn hand_over_within_reserve(mut self) -> Result<Handover> {
        let _storing = self.reserve.take().map(|reserve| reserve.storing);

        SharedRuntime::current()?
            .timeout(HAND_OVER_TIMEOUT, self.hand_over())
            .await
            .context("The bus took too long to hand over")?
    }
}

impl Handover {
    /// What the previous instance of the bus handed over, out of the stored FDs, `stored`, with the
    /// names that it stored each under, if it stored its state memfd, and the state can be decoded.
    ///
    /// Each client of the state gets its connection back from the record of its connection, where
    /// the state says it is in the state memfd, read in place, and from its connection FDs, where
    /// the state says they are among those that the queue sockets hold, as [`take_queued`] takes
    /// them out. One whose connection can't be restored, as when one of its connection FDs is
    /// missing, is gone: the next instance cleans up after it as after any client that goes.
    /// Without the state memfd, or if its state can't be decoded, all of the stored FDs close, and
    /// the connection FDs in the queue sockets with them, which disconnects the clients.
    fn from_stored(stored: Vec<(String, OwnedFd)>) -> Option<Self> {
        let StoredFds { state, queues } = StoredFds::new(stored);
        let Some(state) = state else {
            if !queues.is_empty() {
                warn!("Dropping the sockets that were stored without the state of the bus");
            }

            return None;
        };
        let stored = systemd::view_memfd(state.as_fd())
            .context("Failed to read the state of the bus that was stored")
            .and_then(|bytes| Ok((State::decode(&bytes)?, bytes)));
        let (state, bytes) = match stored {
            Ok(stored) => stored,
            Err(e) => {
                error!(
                    "Failed to take over from the previous instance of the bus, which disconnects \
                     its clients: {e:#}"
                );

                return None;
            }
        };
        let mut client_fds = take_queued(queues);
        let mut clients = Vec::new();
        let mut missing = 0;
        for (id, Stored { record, fds }) in state.stored() {
            // The bus left a client that has no connection FDs out of the hand-over, as each one
            // that it hands over has a client socket.
            if fds.1 == 0 {
                continue;
            }
            let Ok(id) = usize::try_from(id) else {
                continue;
            };
            let Some(fds) = take_fds(&mut client_fds, fds) else {
                missing += 1;

                continue;
            };
            let restored = bytes_at(&bytes, record)
                .context("The record of its connection is beyond the end of the stored state")
                .and_then(|record| Ok(connection::Handover::restore(record, fds)?));
            match restored {
                Ok(connection) => clients.push(ClientHandover { id, connection }),
                Err(e) => {
                    warn!("Leaving `:busd.{id}` out, as its connection can't be restored: {e}")
                }
            }
        }
        if missing > 0 {
            warn!(
                "Leaving {missing} clients out, as file descriptors of their connections are \
                 missing from the store"
            );
        }
        if !client_fds.is_empty() {
            warn!(
                "Closing {} file descriptors of clients that the state of the bus doesn't have",
                client_fds.len()
            );
        }
        // The connections hold what they need of their records, and the memfd is unmapped.
        drop(bytes);

        // The listening socket is that of the socket activation, which systemd passes on its own.
        Some(Self {
            state,
            listener: None,
            clients,
        })
    }

    /// Keep the clients that fit, in order, as [`fit`] does.
    fn fit(&mut self, fd_limit: Option<u64>) {
        fit(&mut self.clients, fd_limit, |client| {
            client.connection.fds().len()
        });
    }

    /// Store the hand-over in the store of systemd, which `notifier` reaches, and which takes `max`
    /// stored FDs: the state memfd and the queue sockets, which hold the connection FDs, under
    /// [`STATE`] and [`CLIENTS`]. Then wait for systemd to have dealt with them.
    fn store(mut self, notifier: &Notifier, max: usize) -> Result<()> {
        self.store_state(notifier)?;
        let queued = self.store_clients(notifier, max.saturating_sub(1))?;
        // The service manager attributes what the bus sent to it by its process, which goes next.
        notifier.barrier(BARRIER_TIMEOUT)?;
        debug!(
            "Stored the state of the bus and {queued} file descriptors of its clients for its \
             next instance"
        );

        Ok(())
    }

    /// Store the state of the bus, with where the record of the connection of each client that it
    /// hands over is, and where its connection FDs are, and the records, in the state memfd, which
    /// closes as this returns: the queue sockets take its place among the FDs that the bus has
    /// open.
    ///
    /// The state memfd holds the state, then the records, in the order of the clients in the
    /// state, each where the state says, at an offset that is a multiple of 8. zbus saves each
    /// record in its place in the memfd, which is mapped into memory, so that the bytes of the
    /// connections are copied once, and the next instance restores the connections from there.
    /// The connection FDs are queued in the order of the hand-over, as
    /// [`Handover::store_clients`] queues them: those of each client after those of the clients
    /// before it.
    fn store_state(&mut self, notifier: &Notifier) -> Result<()> {
        let mut queued = 0;
        let connections: HashMap<_, _> = self
            .clients
            .iter()
            .map(|client| {
                let fds = client.connection.fds().len() as u64;
                let first = queued;
                queued += fds;

                (client.id as u64, (&client.connection, (first, fds)))
            })
            .collect();
        // The state takes as many bytes wherever the records are.
        let mut end = self.state.encoded_len()?.next_multiple_of(8);
        let mut records = Vec::with_capacity(connections.len());
        for (id, stored) in self.state.stored_mut() {
            let Some(&(connection, fds)) = connections.get(&id) else {
                continue;
            };
            // A multiple of 8.
            let len = connection
                .saved_len()
                .expect("the clients of the bus are on unix sockets");
            *stored = Stored {
                record: (end as u64, len as u64),
                fds,
            };
            records.push((end..end + len, connection));
            end += len;
        }
        let state = self.state.encode()?;
        let memfd = systemd::memfd_filled(STATE, end, |memfd| -> Result<()> {
            memfd[..state.len()].copy_from_slice(&state);
            for (place, connection) in records {
                connection.save(&mut memfd[place])?;
            }

            Ok(())
        })?;
        notifier.store(STATE, &[memfd.as_fd()])?;

        Ok(())
    }

    /// Store the connection FDs, in the order of the hand-over, in queue sockets ([`FdQueue`]),
    /// under [`CLIENTS`], as many queue sockets as that takes, and as the store has `room` for, and
    /// tell how many connection FDs that stores. The clients whose connection FDs don't fit whole,
    /// as when the store has no room for another queue socket, are left out, with a warning: the
    /// next instance cleans up after them, as after clients that went.
    fn store_clients(&self, notifier: &Notifier, room: usize) -> Result<usize> {
        let fds: Vec<_> = self
            .clients
            .iter()
            .flat_map(|client| client.connection.fds())
            .collect();
        let (queued, failure) = queue(fds.chunks(systemd::MAX_FDS), room, |socket| {
            Ok(notifier.store(CLIENTS, &[socket.as_fd()])?)
        })?;
        if queued < fds.len() {
            let mut end = 0;
            let handed = self
                .clients
                .iter()
                .take_while(|client| {
                    end += client.connection.fds().len();

                    end <= queued
                })
                .count();
            let all = self.clients.len();
            let why = match failure {
                Some(e) => format!("queueing their file descriptors failed: {e}"),
                None => "the file descriptor store takes no more sockets of their file \
                         descriptors: raise `FileDescriptorStoreMax=` of the service"
                    .to_owned(),
            };
            warn!("Handing over {handed} of the {all} clients, as {why}.");
        }

        Ok(queued)
    }
}

/// The stored FDs, by what each is, which the name that busd stored it under tells.
#[derive(Debug, Default)]
struct StoredFds {
    /// The state memfd, under [`STATE`].
    state: Option<OwnedFd>,
    /// The queue sockets, which hold the connection FDs, under [`CLIENTS`].
    queues: Vec<OwnedFd>,
}

impl StoredFds {
    /// The stored FDs, `stored`, with the names that busd stored each under.
    ///
    /// Those under names that busd doesn't store any under close, as ones that a version of busd
    /// that named them otherwise stored.
    fn new(stored: Vec<(String, OwnedFd)>) -> Self {
        let mut fds = Self::default();
        for (name, fd) in stored {
            match name.as_str() {
                STATE => fds.state = Some(fd),
                CLIENTS => fds.queues.push(fd),
                _ => warn!("Removing `{name}`, which busd doesn't know, from the store"),
            }
        }

        fds
    }
}

/// Queue the connection FDs of `chunks`, in order, each chunk as one datagram, in queue sockets
/// ([`FdQueue`]), as many as that takes, `room` at most, and hand each one that took any datagram
/// to `store`, once it took all that it takes. Tell how many connection FDs that queued, and why
/// not all of them, if not: queueing failed, or else, `room` queue sockets took no more.
fn queue<'c, 'f, C, S>(chunks: C, room: usize, mut store: S) -> Result<(usize, Option<io::Error>)>
where
    'f: 'c,
    C: IntoIterator<Item = &'c [BorrowedFd<'f>]>,
    S: FnMut(OwnedFd) -> Result<()>,
{
    let mut chunks = chunks.into_iter().peekable();
    let mut queued = 0;
    for _ in 0..room {
        if chunks.peek().is_none() {
            break;
        }
        let queue = FdQueue::new()?;
        let mut took = false;
        let mut failure = None;
        while let Some(chunk) = chunks.peek() {
            match queue.push(queued as u64, chunk) {
                Ok(true) => queued += chunk.len(),
                // The next queue takes the rest.
                Ok(false) => break,
                Err(e) => {
                    failure = Some(e);

                    break;
                }
            }
            took = true;
            chunks.next();
        }
        if took {
            store(queue.into_socket())?;
        } else if failure.is_none() {
            // Any other queue would take none either.
            failure = Some(io::Error::other("a socket took none of them"));
        }
        if failure.is_some() {
            return Ok((queued, failure));
        }
    }

    Ok((queued, None))
}

/// The connection FDs, out of the queue sockets that hold them, `queues`, as
/// [`systemd::take_queued`] takes them, by the index that each was queued as, whatever the order
/// of the queue sockets. Those that the process can't have open close, as do those of a queue
/// socket that fails to give them, with a warning: the clients that they are of go without them.
fn take_queued(queues: Vec<OwnedFd>) -> HashMap<u64, OwnedFd> {
    let mut fds = HashMap::new();
    let mut complete = true;
    for queue in queues {
        let taken = systemd::take_queued(queue.as_fd(), |index, taken, all| {
            for (n, fd) in (0..).zip(taken) {
                fds.insert(index.wrapping_add(n), fd);
            }
            complete &= all;
        });
        if let Err(e) = taken {
            warn!("Failed to take file descriptors of the clients out of the store: {e}");
        }
    }
    if !complete {
        warn!(
            "Failed to take file descriptors of the clients out of the store, as when the process \
             has too many open: raise the hard limit of `LimitNOFILE=` of the service."
        );
    }

    fds
}

/// Take `count` connection FDs out of `fds`, from the index `first` on, if they are all there.
///
/// Those that are there go either way, so that those of a client that lacks one close with it.
fn take_fds(fds: &mut HashMap<u64, OwnedFd>, (first, count): (u64, u64)) -> Option<Vec<OwnedFd>> {
    // More than there are can't all be there, and aren't looked for one by one.
    if count > fds.len() as u64 {
        return None;
    }
    let end = first.checked_add(count)?;
    let taken: Vec<_> = (first..end).map(|index| fds.remove(&index)).collect();

    taken.into_iter().collect()
}

/// The bytes at `place` of `bytes`: from an offset, and as many as a length says, if they are all
/// there.
fn bytes_at(bytes: &[u8], (offset, len): (u64, u64)) -> Option<&[u8]> {
    let offset = usize::try_from(offset).ok()?;
    let end = offset.checked_add(usize::try_from(len).ok()?)?;

    bytes.get(offset..end)
}

/// Keep the `clients` that fit, in order, among the FDs that the next instance can have open, as
/// many as `fd_limit` takes, if anything limits them, with room to spare. busd warns if any don't.
///
/// Each client takes its connection FDs, as many as `fds` tells: its client socket, its client-sent
/// FDs and its outgoing FDs, and in the next instance its process FD too. A queue socket holds
/// tens of thousands of connection FDs, and the store takes them in as many queue sockets as it
/// has room for, which storing sees to. One client that doesn't fit is left out whole, as the next
/// instance couldn't serve it without all of its connection FDs, while those after it, of fewer,
/// may still fit.
fn fit<C, F>(clients: &mut Vec<C>, fd_limit: Option<u64>, fds: F)
where
    F: Fn(&C) -> usize,
{
    let Some(limit) = fd_limit.and_then(|limit| usize::try_from(limit).ok()) else {
        return;
    };
    let all = clients.len();
    // The next instance has the state memfd, the listening socket of the socket activation, and a
    // queue socket open as it takes the connection FDs out.
    let mut room = limit.saturating_sub(FIRST_PASSED_FD + 3 + SPARE_FDS);
    clients.retain(|client| {
        let Some(left) = room.checked_sub(fds(client) + 1) else {
            return false;
        };
        room = left;

        true
    });
    let fit = clients.len();
    if fit < all {
        warn!(
            "Handing over {fit} of the {all} clients, as the next instance takes their file \
             descriptors under its limit of open file descriptors, {limit} as busd has it: raise \
             the hard limit of `LimitNOFILE=` of the service."
        );
    }
}

/// FDs that a bus that systemd runs, under socket activation, keeps open from when it starts, to
/// give up for those that storing its hand-over opens, as [`Bus::hand_over_within_reserve`] does:
/// however many clients the bus serves by then, and however many client sockets and client-sent FDs
/// that takes, it has those to spare.
#[derive(Debug)]
pub(super) struct Reserve {
    /// For what storing opens once the bus is handed over, in turn, each closed before the next
    /// opens: the state memfd, the two ends of each pair whose one end is a queue socket, and the
    /// two ends of the pipe of the barrier.
    storing: [OwnedFd; 3],
}

impl Reserve {
    /// Three FDs to spare: the ends of a pipe, and a duplicate of one of them.
    fn new() -> Result<Self> {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
        let duplicate = fcntl_dupfd_cloexec(&read, 0)?;

        Ok(Self {
            storing: [read, write, duplicate],
        })
    }
}

/// Whether `name` is one that busd stores FDs under, though maybe not as this version of busd does,
/// as for one that a version of busd that named them otherwise stored.
fn is_busds(name: &str) -> bool {
    name == STATE || name.starts_with(CLIENT_PREFIX)
}

/// The name that the state memfd is stored under.
const STATE: &str = "busd-state";
/// The name that the queue sockets are stored under, however many of them there are.
const CLIENTS: &str = "busd-clients";
/// What the names that busd stores what it hands over of its clients under start with: [`CLIENTS`]
/// does, as do those that other versions of busd may store it under.
const CLIENT_PREFIX: &str = "busd-client";
/// How long the bus may take to hand over, before busd gives up on it, and its clients get
/// disconnected as it goes.
///
/// The bus doesn't wait for its clients to take what it sends them as it hands over, but it gives
/// the hand-over of the connection of a client, which nothing should hold up, as long as the client
/// has to take a message, `SEND_TIMEOUT`, 10 s, before it gives up on it, as it gives a client to
/// take what the instance before it left unwritten. This is longer, with room for the rest of the
/// hand-over, and well within the 90 s that systemd gives a service to stop by default.
const HAND_OVER_TIMEOUT: Duration = Duration::from_secs(30);
/// How long busd waits for the service manager to deal with what busd sent it, before it goes on
/// anyway.
const BARRIER_TIMEOUT: Duration = Duration::from_secs(5);
/// The fewest stored FDs that the store has to take for busd to hand over: the state memfd and a
/// queue socket.
const MIN_STORE: usize = 2;
/// The first of the passed FDs, `SD_LISTEN_FDS_START`, after the standard input, output and error.
const FIRST_PASSED_FD: usize = 3;
/// How many FDs the next instance has to spare, under its limit of open FDs, besides the connection
/// FDs and the process FDs of its clients, the state memfd, its listening socket and a queue
/// socket: for the FDs that it opens of its own as it starts and takes over, and for the queue
/// sockets after the first, if the connection FDs come in more than one.
const SPARE_FDS: usize = 32;

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs::File, iter};

    use rustix::io::{read, write};

    use super::*;

    /// busd tells the names of the stored FDs from those of the listening sockets of socket
    /// activation, which have the names of their units, even the names that another version of
    /// busd may store them under.
    #[test]
    fn busds_names() {
        for name in [STATE, CLIENTS, "busd-client-3-0", "busd-client-12-34-5-6"] {
            assert!(is_busds(name), "{name:?}");
        }
        for name in ["busd.socket", "busd-session.socket", "busd-states", "busd"] {
            assert!(!is_busds(name), "{name:?}");
        }
    }

    /// A name of busd's that this version stores nothing under, as one that another version of
    /// busd stored connection FDs under, stays among the stored FDs, to be removed from the store,
    /// rather than taken for the listening socket of socket activation, and the stored FDs under it
    /// close.
    #[test]
    fn unknown_client_names_are_removed() {
        let names = [STATE, CLIENTS, "busd-client-3", "busd-client-4-0"];
        let mut passed = names
            .iter()
            .map(|name| (name.to_string(), null()))
            .collect();

        assert!(Bus::take_activation(&mut passed).unwrap().is_none());
        let stored: Vec<_> = passed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(stored, names);
        let fds = StoredFds::new(passed);
        assert!(fds.state.is_some());
        assert_eq!(fds.queues.len(), 1);
    }

    /// The connection FDs go on in the next queue socket once one is full, and come back by the
    /// index that each was queued as, whatever the order of the queue sockets, each as it went, one
    /// of the same open file description as another one included.
    #[test]
    fn queued_fds_come_back_by_their_index() {
        let pipes: Vec<_> = (0..3)
            .map(|_| pipe_with(PipeFlags::CLOEXEC).unwrap())
            .collect();
        // The last is of the same open file description as the first, which the store would keep
        // once.
        let mut fds: Vec<_> = pipes.iter().map(|(_, write)| write.as_fd()).collect();
        fds.push(fds[0]);
        let stored = RefCell::new(Vec::new());

        // Datagrams without connection FDs fill the first queue socket, between two that have
        // some.
        let (first, last) = fds.split_at(2);
        let (mut first, mut last) = (Some(first), Some(last));
        let chunks = iter::from_fn(|| match stored.borrow().is_empty() {
            true => Some(first.take().unwrap_or_default()),
            false => last.take(),
        });
        let (queued, failure) = queue(chunks, 3, |socket| {
            stored.borrow_mut().push(socket);

            Ok(())
        })
        .unwrap();
        assert!(failure.is_none(), "{failure:?}");
        assert_eq!(queued, fds.len());
        let mut stored = stored.into_inner();
        assert_eq!(
            stored.len(),
            2,
            "the connection FDs went in other than two queue sockets"
        );

        stored.reverse();
        let mut taken = take_queued(stored);
        // What goes into each connection FD comes out of the pipe that it was queued of.
        for (index, pipe) in [(0, 0), (1, 1), (2, 2), (3, 0)] {
            let fd = taken.remove(&index).unwrap();
            let byte = [index as u8];
            assert_eq!(write(&fd, &byte).unwrap(), 1);
            let mut read_byte = [0];
            assert_eq!(read(&pipes[pipe].0, &mut read_byte).unwrap(), 1);
            assert_eq!(read_byte, byte);
        }
        assert!(taken.is_empty(), "{taken:?}");
    }

    /// The connection FDs go in no more queue sockets than the store has room for: those of the
    /// datagrams that fit in them are queued, and the others aren't.
    #[test]
    fn queued_fds_fill_the_room_of_the_store() {
        let fd = null();
        let fds = [fd.as_fd()];
        let mut stored = Vec::new();

        // One datagram with a connection FD, then ones without, more than any queue socket takes.
        let chunks = iter::once(&fds[..]).chain(iter::repeat(&fds[..0]));
        let (queued, failure) = queue(chunks, 1, |socket| {
            stored.push(socket);

            Ok(())
        })
        .unwrap();
        assert!(failure.is_none(), "{failure:?}");
        assert_eq!(queued, 1);
        assert_eq!(stored.len(), 1);
        let taken = take_queued(stored);
        assert_eq!(taken.into_keys().collect::<Vec<_>>(), [0]);

        // A store without room takes none.
        let (queued, failure) = queue([&fds[..]], 0, |_| unreachable!()).unwrap();
        assert_eq!(queued, 0);
        assert!(failure.is_none());
    }

    /// A client gets the connection FDs at its place, if they are all there, and none otherwise,
    /// while those that are there go all the same.
    #[test]
    fn clients_take_their_fds() {
        let mut fds: HashMap<u64, OwnedFd> = [0, 1, 2, 4].map(|index| (index, null())).into();

        assert_eq!(take_fds(&mut fds, (0, 2)).unwrap().len(), 2);
        assert!(take_fds(&mut fds, (2, 2)).is_none(), "{fds:?}");
        assert!(!fds.contains_key(&2), "{fds:?}");
        assert!(take_fds(&mut fds, (4, u64::MAX)).is_none());
        assert!(take_fds(&mut fds, (u64::MAX, 1)).is_none());
        assert_eq!(take_fds(&mut fds, (4, 1)).unwrap().len(), 1);
        assert!(fds.is_empty(), "{fds:?}");
    }

    /// A client takes its connection FDs, which are its client socket and its client-sent FDs here,
    /// and its process FD in the next instance, and one that doesn't fit is left out whole, while
    /// those after it may still fit.
    #[test]
    fn clients_fit_whole() {
        // The numbers of the clients, and how many connection FDs each has.
        let clients: Vec<_> = [1, 3, 1, 2].into_iter().enumerate().collect();
        let ids =
            |clients: &[(usize, usize)]| -> Vec<_> { clients.iter().map(|&(id, _)| id).collect() };

        // The next instance has room for 5 FDs of clients: the connection FDs of the first and the
        // third, with their process FDs.
        let limit = (FIRST_PASSED_FD + 3 + SPARE_FDS + 5) as u64;
        let mut fitting = clients.clone();
        fit(&mut fitting, Some(limit), |&(_, fds)| fds);
        assert_eq!(ids(&fitting), [0, 2]);

        // Without a limit, all of them fit.
        let mut fitting = clients;
        fit(&mut fitting, None, |&(_, fds)| fds);
        assert_eq!(ids(&fitting), [0, 1, 2, 3]);
    }

    /// An FD of no consequence.
    fn null() -> OwnedFd {
        File::open("/dev/null").unwrap().into()
    }
}
