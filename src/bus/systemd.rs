//! A bus that systemd runs: on the socket of its socket activation, and handed over to its next
//! instance through its file descriptor store.

use std::{
    collections::{BTreeSet, HashMap},
    io,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
    str::FromStr,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rustix::pipe::{pipe_with, PipeFlags};
use zbus::{
    address::{transport::UnixSocket, Transport},
    connection, Address,
};
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
    /// Create a bus that systemd runs, out of the file descriptors that systemd passed as it
    /// started the process, `fds`, with their names, as [`systemd::take_listen_fds`] gives them,
    /// and the service manager that `notifier` reaches, if any.
    ///
    /// The bus is the next instance of the one that handed over through the file descriptor store,
    /// as [`Bus::hand_over_to_systemd`] does, if one did, on the socket of the socket activation,
    /// if any. Taking over may fail, as when the previous instance encoded its state otherwise: the
    /// clients that were handed over get disconnected then, and the bus starts as if nothing was
    /// handed over. Without a hand-over, the bus listens on the socket of the socket activation, if
    /// systemd passed one, or else at `address`, as [`Bus::for_address`] does. busd listens on one
    /// socket, so this fails if systemd passed more than one socket of socket activation. All that
    /// was stored is removed from the store first, so that whatever comes of it, the instances that
    /// come after don't find it again.
    ///
    /// The next instance listens where the previous one did, whatever `address` says: it warns if
    /// they differ, unless it listens on the socket of the socket activation, which it does
    /// whatever `address` says. With a service manager to hand over to, the bus keeps a few file
    /// descriptors open from the start, before it serves anyone, to give up for those that handing
    /// over opens, however many file descriptors its clients take by then.
    pub async fn for_systemd(
        mut fds: Vec<(String, OwnedFd)>,
        notifier: Option<&Notifier>,
        address: Option<&str>,
    ) -> Result<Self> {
        let reserve = match notifier {
            Some(_) => Reserve::new()
                .inspect_err(|e| warn!("Failed to reserve file descriptors to hand over with: {e}"))
                .ok(),
            None => None,
        };
        let activation = Self::take_activation(&mut fds)?;
        if let Some(notifier) = notifier {
            // A name may come with more than one file descriptor.
            let names: BTreeSet<_> = fds.iter().map(|(name, _)| name).collect();
            for name in names {
                if let Err(e) = notifier.remove(name) {
                    warn!("Failed to remove `{name}` from the store: {e}");
                }
            }
        }
        let mut bus = Self::out_of_stored(fds, activation, notifier, address).await?;
        bus.reserve = reserve;

        Ok(bus)
    }

    /// Hand the bus over to its next instance, through the file descriptor store of systemd, which
    /// `notifier` reaches, if systemd keeps one that takes the state of the bus and one more file
    /// descriptor, as `$FDSTORE` tells: the socket that the bus listens on, unless socket
    /// activation passed it, or a socket that holds the file descriptors of the clients. Otherwise,
    /// the bus says why it doesn't hand over, and cleans up as [`Bus::cleanup`] does, only logging
    /// a failure to, as busd does without systemd.
    ///
    /// The bus hands over as [`Bus::hand_over`] does, and stores what it hands over, for
    /// [`Bus::for_systemd`] to take over from on the restart: its state, with the records of the
    /// connections of the clients that fit, then the socket that it listens on, and then the file
    /// descriptors of the connections of those clients, in the order of the hand-over, in the
    /// queues of sockets ([`FdQueue`]), as many sockets as that takes. So the store takes them,
    /// however many there are, each as it is, as when a client sent the same file descriptor twice,
    /// rather than once for each open file description. The next instance takes them out of the
    /// sockets once it raised its soft limit of open file descriptors to the hard one, as busd does
    /// as it starts, so they have to fit under `fd_limit`, the soft limit of busd, which it raised
    /// to the hard one as it started, `None` for no limit, unless the limit of the unit changed
    /// meanwhile, with the file descriptor of the process of each client, which the next instance
    /// opens to look the credentials of the client up. The clients that don't fit get disconnected
    /// as the bus goes, with a warning. It fails if the bus takes longer than 30 s to hand over, or
    /// if storing fails, which disconnects the clients as the bus goes.
    pub async fn hand_over_to_systemd(
        self,
        notifier: &Notifier,
        fd_limit: Option<u64>,
    ) -> Result<()> {
        let Some(max) = Self::fd_store_max() else {
            if let Err(e) = self.cleanup().await {
                error!("Failed to clean up: {e}");
            }

            return Ok(());
        };
        info!("Handing over to the next instance of the bus..");
        let mut handover = self.hand_over_within_reserve().await?;
        handover.fit(max, fd_limit);

        handover.store(notifier, max)
    }

    /// How many file descriptors the file descriptor store of systemd takes, `$FDSTORE`, if that's
    /// enough to hand over: the state of the bus, and one more. It says why it isn't otherwise.
    fn fd_store_max() -> Option<usize> {
        match systemd::fd_store_max() {
            Some(max) if max >= MIN_STORE => Some(max),
            Some(max) => {
                warn!(
                    "Not handing over to the next instance of the bus, as the file descriptor \
                     store takes {max}, which is too few: raise `FileDescriptorStoreMax=` of the \
                     service."
                );

                None
            }
            None => {
                info!(
                    "Not handing over to the next instance of the bus, as systemd keeps no file \
                     descriptor store for it: `$FDSTORE` isn't set."
                );

                None
            }
        }
    }

    /// Take the socket of the socket activation, if systemd passed one, out of the file descriptors
    /// that systemd passed, `fds`, with their names, which leaves those that busd stored.
    ///
    /// Those that busd stored have names of busd's own, as [`is_busds`] tells. The others are the
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

    /// The next instance of the bus that handed over what busd stored, `stored`, with the names
    /// that it stored each under, on `activation`, the socket of the socket activation, if any.
    ///
    /// Without a hand-over to take over, or if taking over fails, it is a bus on `activation`, or
    /// else at `address`, as [`Bus::on_listener_or_at`] creates. Without `activation`, that bus
    /// binds a socket in place of the one that a previous instance listened on, if it stored one,
    /// once the service manager that `notifier` reaches let go of it.
    async fn out_of_stored(
        stored: Vec<(String, OwnedFd)>,
        activation: Option<OwnedFd>,
        notifier: Option<&Notifier>,
        address: Option<&str>,
    ) -> Result<Self> {
        let stored_listener = stored.iter().any(|(name, _)| name == LISTENER);
        let replacing = notifier.filter(|_| stored_listener);
        let Some(handover) = Handover::from_stored(stored) else {
            return Self::on_listener_or_at(activation, replacing, address).await;
        };
        let clients = handover.clients.len();
        let activated = activation.is_some();
        let fallback = activation.as_ref().map(OwnedFd::try_clone).transpose()?;
        match Self::take_over(handover, activation, SEND_TIMEOUT).await {
            Ok(bus) => {
                info!("Took {clients} clients over from the previous instance of the bus.");
                if !activated {
                    bus.warn_if_moved(address);
                }

                Ok(bus)
            }
            Err(e) => {
                error!(
                    "Failed to take over from the previous instance of the bus, which disconnects \
                     its {clients} clients: {e:#}"
                );

                Self::on_listener_or_at(fallback, replacing, address).await
            }
        }
    }

    /// A bus on `listener`, if given one, as [`Bus::for_listener`] creates, or else at `address`,
    /// as [`Bus::for_address`] does, once the service manager that `replacing` reaches, if any, let
    /// go of the socket that a previous instance of the bus listened on and stored.
    ///
    /// The socket goes once systemd closes its copy too, as it gets to its removal, which the bus
    /// waits for, so that it finds the socket stale, and binds one in its place.
    async fn on_listener_or_at(
        listener: Option<OwnedFd>,
        replacing: Option<&Notifier>,
        address: Option<&str>,
    ) -> Result<Self> {
        if let Some(listener) = listener {
            return Self::for_listener(listener).await;
        }
        if let Some(notifier) = replacing {
            if let Err(e) = notifier.barrier(BARRIER_TIMEOUT) {
                warn!("Failed to wait for systemd to let go of the stored socket: {e}");
            }
        }

        Self::for_address(address).await
    }

    /// Warn if the bus, which took over from its previous instance, listens on another socket file
    /// than at `address`, the one that busd is to listen at, as when that changed: busd only gets
    /// there once it starts afresh.
    fn warn_if_moved(&self, address: Option<&str>) {
        let Some(configured) = address.and_then(|address| Address::from_str(address).ok()) else {
            return;
        };
        let (Transport::Unix(unix), Transport::Unix(configured)) =
            (self.address().transport(), configured.transport())
        else {
            return;
        };
        let (UnixSocket::File(path), UnixSocket::File(configured)) =
            (unix.path(), configured.path())
        else {
            return;
        };
        if path != configured {
            warn!(
                "Listening at `{}`, as the previous instance of the bus did, rather than at `{}`: \
                 busd only gets there once it starts afresh, after a stop.",
                path.display(),
                configured.display(),
            );
        }
    }

    /// Hand the bus over, as [`Bus::hand_over`] does, within [`HAND_OVER_TIMEOUT`], with the file
    /// descriptors that it reserved given up for those that handing over opens: one for the
    /// duplicate of the socket that the bus listens on, which the bus makes ahead of those of its
    /// clients, and the others for the state, the sockets that hold the file descriptors of the
    /// clients, and the barrier, which storing opens in turn, once the bus is handed over.
    async fn hand_over_within_reserve(mut self) -> Result<Handover> {
        // The first is given up right away, and the others as this returns.
        let (_, _storing) = self
            .reserve
            .take()
            .map(|reserve| (reserve.listener, reserve.storing))
            .unzip();

        SharedRuntime::current()?
            .timeout(HAND_OVER_TIMEOUT, self.hand_over())
            .await
            .context("The bus took too long to hand over")?
    }
}

impl Handover {
    /// What the previous instance of the bus handed over, out of what it stored, `stored`, with the
    /// names that it stored each under, if it stored its state, and the state can be decoded.
    ///
    /// Each client of the state gets its connection back from the record of its connection, where
    /// the state says it is in the memfd of the state, read in place, and from the file
    /// descriptors of the connection, where the state says they are among those that the queues of
    /// the stored sockets hold, as [`take_queued`] takes them out. One whose connection can't be
    /// restored, as when a file descriptor of it is missing, is gone: the next instance cleans up
    /// after it as after any client that goes. Without the state, or if it can't be decoded, all of
    /// the file descriptors close, which disconnects the clients.
    fn from_stored(stored: Vec<(String, OwnedFd)>) -> Option<Self> {
        let StoredFds {
            state,
            listener,
            queues,
        } = StoredFds::new(stored);
        let Some(state) = state else {
            if !queues.is_empty() || listener.is_some() {
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
            // The bus left a client that has no file descriptors out of the hand-over, as each one
            // that it hands over has a socket.
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

        Some(Self {
            state,
            listener,
            clients,
        })
    }

    /// Keep the clients that fit, in order, as [`fit`] does.
    fn fit(&mut self, max: usize, fd_limit: Option<u64>) {
        let listener = self.listener.is_some();

        fit(&mut self.clients, listener, max, fd_limit, |client| {
            client.connection.fds().len()
        });
    }

    /// Store the hand-over in the file descriptor store of systemd, which `notifier` reaches, and
    /// which takes `max` file descriptors: the state, the socket that the bus listened on, if it
    /// did, and the sockets that hold the file descriptors of the connections of the clients, under
    /// [`STATE`], [`LISTENER`] and [`CLIENTS`]. Then wait for systemd to have dealt with them.
    fn store(mut self, notifier: &Notifier, max: usize) -> Result<()> {
        self.store_state(notifier)?;
        let listener = usize::from(self.listener.is_some());
        if let Some(listener) = &self.listener {
            notifier.store(LISTENER, &[listener.as_fd()])?;
        }
        let room = max.saturating_sub(1 + listener);
        let stored = listener + self.store_clients(notifier, room)?;
        // The service manager attributes what the bus sent to it by its process, which goes next.
        notifier.barrier(BARRIER_TIMEOUT)?;
        debug!("Stored the state of the bus and {stored} file descriptors for its next instance");

        Ok(())
    }

    /// Store the state of the bus, with where the record of the connection of each client that it
    /// hands over is, and where the file descriptors of the connection are, and the records, in
    /// one memfd that closes as this returns: the sockets that hold the file descriptors of the
    /// clients take its place among the open file descriptors.
    ///
    /// The memfd holds the state, then the records, in the order of the clients in the state, each
    /// where the state says, at an offset that is a multiple of 8. zbus saves each record in its
    /// place in the memfd, which is mapped into memory, so that the bytes of the connections are
    /// copied once, and the next instance restores the connections from there. The file
    /// descriptors of the connections are queued in the order of the hand-over, as
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

    /// Store the file descriptors of the connections of the clients, in the order of the
    /// hand-over, in the queues of sockets ([`FdQueue`]), under [`CLIENTS`], as many sockets as
    /// that takes, and as the store has `room` for, and tell how many file descriptors that
    /// stores. The clients that don't fit whole, as when the store has no room for another socket,
    /// are left out, with a warning: the next instance cleans up after them, as after clients that
    /// went.
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

/// The file descriptors that busd stored, by what it stored each as, which the name that it stored
/// each under tells.
#[derive(Debug, Default)]
struct StoredFds {
    /// The memfd that holds the state of the bus, under [`STATE`].
    state: Option<OwnedFd>,
    /// The socket that the bus listened on, under [`LISTENER`].
    listener: Option<OwnedFd>,
    /// The sockets that hold the file descriptors of the connections of the clients in their
    /// queues, under [`CLIENTS`].
    queues: Vec<OwnedFd>,
}

impl StoredFds {
    /// The file descriptors that busd stored, `stored`, with the names that it stored each under.
    ///
    /// Those under names that busd doesn't store any under close, as ones that a version of busd
    /// that named them otherwise stored.
    fn new(stored: Vec<(String, OwnedFd)>) -> Self {
        let mut fds = Self::default();
        for (name, fd) in stored {
            match name.as_str() {
                STATE => fds.state = Some(fd),
                LISTENER => fds.listener = Some(fd),
                CLIENTS => fds.queues.push(fd),
                _ => warn!("Removing `{name}`, which busd doesn't know, from the store"),
            }
        }

        fds
    }
}

/// Queue the file descriptors of `chunks`, in order, each chunk as one datagram, in the queues of
/// sockets ([`FdQueue`]), as many as that takes, `room` at most, and hand each one that took any
/// datagram to `store`, once it took all that it takes. Tell how many file descriptors that queued,
/// and why not all of them, if not: queueing failed, or else, the queues of `room` sockets took no
/// more.
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

/// The file descriptors of the connections of the clients, out of the queues of the sockets that
/// hold them, `queues`, as [`systemd::take_queued`] takes them, by the index that each was queued
/// as, whatever the order of the sockets. Those that the process can't have close, as do those of a
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

/// Take `count` file descriptors out of `fds`, from the index `first` on, if they are all there.
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

/// Keep the `clients` that fit, in order: in the file descriptor store, which takes `max` file
/// descriptors, if it has room for a socket that holds those of the clients, after the state and
/// the socket that the bus listens on, if it stores one, as `listener` tells, and among the file
/// descriptors that the next instance can have open, as many as `fd_limit` takes, if anything
/// limits them, with room to spare. busd warns if any don't.
///
/// Each client takes the file descriptors of its connection, as many as `fds` tells: its socket and
/// those that came with the messages that the bus didn't take, and in the next instance the file
/// descriptor of its process too, which the next instance opens to look the credentials of the
/// client up. A socket of the store holds tens of thousands of file descriptors, and the store
/// takes them in as many sockets as it has room for, which storing sees to. One client that
/// doesn't fit is left out whole, as the next instance couldn't serve it without all of them, while
/// those after it, of fewer file descriptors, may still fit.
fn fit<C, F>(clients: &mut Vec<C>, listener: bool, max: usize, fd_limit: Option<u64>, fds: F)
where
    F: Fn(&C) -> usize,
{
    let all = clients.len();
    let in_store = max > 1 + usize::from(listener);
    let limit = fd_limit.and_then(|limit| usize::try_from(limit).ok());
    match (in_store, limit) {
        (false, _) => clients.clear(),
        (true, Some(limit)) => {
            // The next instance has the state, the socket to listen on, from the store or from the
            // socket activation, and the sockets of the file descriptors of the clients open as it
            // takes those out.
            let mut room = limit.saturating_sub(FIRST_PASSED_FD + 3 + SPARE_FDS);
            clients.retain(|client| {
                let Some(left) = room.checked_sub(fds(client) + 1) else {
                    return false;
                };
                room = left;

                true
            });
        }
        (true, None) => (),
    }
    let fit = clients.len();
    if fit < all {
        let why = match limit {
            Some(limit) if in_store => format!(
                "the next instance takes their file descriptors under its limit of open file \
                 descriptors, {limit} as busd has it: raise the hard limit of `LimitNOFILE=`"
            ),
            _ => format!("the file descriptor store takes {max}: raise `FileDescriptorStoreMax=`"),
        };
        warn!("Handing over {fit} of the {all} clients, as {why} of the service.");
    }
}

/// File descriptors that a bus that systemd runs keeps open from when it starts, to give up for
/// those that handing over opens, as [`Bus::hand_over_within_reserve`] does: however many clients
/// the bus serves by then, and however many file descriptors that takes, it has those to spare.
#[derive(Debug)]
pub(super) struct Reserve {
    /// For the duplicate of the socket that the bus listens on, before it duplicates those of its
    /// clients.
    listener: OwnedFd,
    /// For what storing opens once the bus is handed over, in turn, each closed before the next
    /// opens: the state, the two ends of each socket that holds file descriptors of the clients,
    /// and the two ends of the pipe of the barrier.
    storing: [OwnedFd; 3],
}

impl Reserve {
    /// Four file descriptors to spare: the ends of two pipes.
    fn new() -> Result<Self> {
        let (listener, storing) = pipe_with(PipeFlags::CLOEXEC)?;
        let (more, still_more) = pipe_with(PipeFlags::CLOEXEC)?;

        Ok(Self {
            listener,
            storing: [storing, more, still_more],
        })
    }
}

/// Whether `name` is one that busd stores file descriptors under, though maybe not as this version
/// of busd does, as for one that a version of busd that named them otherwise stored.
fn is_busds(name: &str) -> bool {
    name == STATE || name == LISTENER || name.starts_with(CLIENT_PREFIX)
}

/// The name that the state of the bus is stored under.
const STATE: &str = "busd-state";
/// The name that the socket that the bus listens on is stored under.
const LISTENER: &str = "busd-listener";
/// The name that the sockets that hold the file descriptors of the connections of the clients are
/// stored under, however many of them there are.
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
/// The fewest file descriptors that a store has to take for busd to hand over: the state of the bus
/// and one more.
const MIN_STORE: usize = 2;
/// The first file descriptor that systemd passes, `SD_LISTEN_FDS_START`.
const FIRST_PASSED_FD: usize = 3;
/// How many file descriptors the next instance has to spare, under its limit of open file
/// descriptors, besides those of its clients and those that systemd passes it: for those that it
/// opens of its own as it starts and takes over, and for more sockets of the file descriptors of
/// the clients than one, if it takes them in more.
const SPARE_FDS: usize = 32;

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, fs::File, iter};

    use rustix::io::{read, write};

    use super::*;

    /// busd tells the names that it stores file descriptors under from those of the sockets of
    /// socket activation, which have the names of their units, even those of the names that
    /// another version of busd may store them under.
    #[test]
    fn busds_names() {
        for name in [
            STATE,
            LISTENER,
            CLIENTS,
            "busd-client-3-0",
            "busd-client-12-34-5-6",
        ] {
            assert!(is_busds(name), "{name:?}");
        }
        for name in ["busd.socket", "busd-session.socket", "busd-states", "busd"] {
            assert!(!is_busds(name), "{name:?}");
        }
    }

    /// A name of a client that busd doesn't know, as one that a version of busd that named them
    /// otherwise stored, is among those that busd stored, to be removed from the store, rather
    /// than taken for the socket of socket activation, and what was stored under it closes.
    #[test]
    fn unknown_client_names_are_removed() {
        let names = [STATE, LISTENER, CLIENTS, "busd-client-3", "busd-client-4-0"];
        let mut passed = names
            .iter()
            .map(|name| (name.to_string(), null()))
            .collect();

        assert!(Bus::take_activation(&mut passed).unwrap().is_none());
        let stored: Vec<_> = passed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(stored, names);
        let fds = StoredFds::new(passed);
        assert!(fds.state.is_some() && fds.listener.is_some());
        assert_eq!(fds.queues.len(), 1);
    }

    /// The file descriptors of the clients go on in the next queue once one is full, and come back
    /// by the index that each was queued as, whatever the order of the queues, each as it went, a
    /// duplicate of another one included.
    #[test]
    fn queued_fds_come_back_by_their_index() {
        let pipes: Vec<_> = (0..3)
            .map(|_| pipe_with(PipeFlags::CLOEXEC).unwrap())
            .collect();
        // The last is a duplicate of the first, which the store would keep once.
        let mut fds: Vec<_> = pipes.iter().map(|(_, write)| write.as_fd()).collect();
        fds.push(fds[0]);
        let stored = RefCell::new(Vec::new());

        // Datagrams without file descriptors fill the first queue, between two that have some.
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
            "the file descriptors went in other than two queues"
        );

        stored.reverse();
        let mut taken = take_queued(stored);
        // What goes into each comes out of the pipe that it was queued of.
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

    /// The file descriptors go in no more queues than the store has room for: those of the
    /// datagrams that fit in them are queued, and the others aren't.
    #[test]
    fn queued_fds_fill_the_room_of_the_store() {
        let fd = null();
        let fds = [fd.as_fd()];
        let mut stored = Vec::new();

        // One datagram with a file descriptor, then ones without, more than any queue takes.
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

    /// A client gets the file descriptors at its place, if they are all there, and none otherwise,
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

    /// A client takes the file descriptors of its connection, its socket and those that came with
    /// the messages that the bus didn't take, and that of its process in the next instance, and one
    /// that doesn't fit is left out whole, while those after it may still fit.
    #[test]
    fn clients_fit_whole() {
        // The numbers of the clients, and how many file descriptors each takes.
        let clients: Vec<_> = [1, 3, 1, 2].into_iter().enumerate().collect();
        let ids =
            |clients: &[(usize, usize)]| -> Vec<_> { clients.iter().map(|&(id, _)| id).collect() };

        // The next instance has room for 5 file descriptors of clients: those of the first and the
        // third, with those of their processes.
        let limit = (FIRST_PASSED_FD + 3 + SPARE_FDS + 5) as u64;
        let mut fitting = clients.clone();
        fit(&mut fitting, false, MIN_STORE, Some(limit), |&(_, fds)| fds);
        assert_eq!(ids(&fitting), [0, 2]);

        // Without a limit, all of them fit, in a store that has room for a socket.
        let mut fitting = clients.clone();
        fit(&mut fitting, false, MIN_STORE, None, |&(_, fds)| fds);
        assert_eq!(ids(&fitting), [0, 1, 2, 3]);

        // A store that takes the state and the socket that the bus listens on alone takes none.
        let mut fitting = clients;
        fit(&mut fitting, true, MIN_STORE, None, |&(_, fds)| fds);
        assert!(fitting.is_empty());
    }

    /// A file descriptor of no consequence.
    fn null() -> OwnedFd {
        File::open("/dev/null").unwrap().into()
    }
}
