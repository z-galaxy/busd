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
    /// activation passed it, or one of those of the clients. Otherwise, the bus says why it doesn't
    /// hand over, and cleans up as [`Bus::cleanup`] does, only logging a failure to, as busd does
    /// without systemd.
    ///
    /// The bus hands over as [`Bus::hand_over`] does, and stores what it hands over, for
    /// [`Bus::for_systemd`] to take over from on the restart: its state, with the records of the
    /// connections of the clients that fit, then the socket that it listens on, then the sockets of
    /// those clients, each under a name of its own, as many as the store takes so, and then the
    /// other file descriptors of their connections, in the queues of sockets ([`FdQueue`]), which
    /// take them however many there are, each as it is, as when a client sent the same file
    /// descriptor twice. systemd passes the next instance what is stored under the soft limit of
    /// open file descriptors that busd started with, `start_fd_limit`, `None` for no limit, unless
    /// the limit of the unit changed meanwhile, which bounds the sockets that go on their own. The
    /// next instance then takes the other file descriptors out of the queues once it raised that
    /// limit to the hard one, as busd does as it starts, so all of them have to fit under
    /// `fd_limit`, the limit that busd raised its own to, with the file descriptor of the process
    /// of each client, which the next instance opens to look the credentials of the client up. The
    /// clients that don't fit get disconnected as the bus goes, with a warning. It fails if the bus
    /// takes longer than 30 s to hand over, or if storing fails, which disconnects the clients as
    /// the bus goes.
    pub async fn hand_over_to_systemd(
        self,
        notifier: &Notifier,
        start_fd_limit: Option<u64>,
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

        handover.store(notifier, max, start_fd_limit)
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
    /// descriptors of the connection, where the state says they are among those that were stored:
    /// the sockets that were stored on their own, and the file descriptors that the queues of the
    /// stored sockets hold, as [`take_queued`] takes them out. One whose connection can't be
    /// restored, as when a file descriptor of it is missing, is gone: the next instance cleans up
    /// after it as after any client that goes. Without the state, or if it can't be decoded, all of
    /// the file descriptors close, which disconnects the clients.
    fn from_stored(stored: Vec<(String, OwnedFd)>) -> Option<Self> {
        let StoredFds {
            state,
            listener,
            sockets,
            queues,
        } = StoredFds::new(stored);
        let Some(state) = state else {
            if !sockets.is_empty() || !queues.is_empty() || listener.is_some() {
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
        client_fds.extend(sockets);
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
    /// did, and the file descriptors of the connections of the clients, as
    /// [`Handover::store_clients`] stores them, for a next instance that starts with
    /// `start_fd_limit` as its soft limit of open file descriptors, if any. Then wait for systemd
    /// to have dealt with them.
    fn store(mut self, notifier: &Notifier, max: usize, start_fd_limit: Option<u64>) -> Result<()> {
        self.store_state(notifier)?;
        let listener = usize::from(self.listener.is_some());
        if let Some(listener) = &self.listener {
            notifier.store(LISTENER, &[listener.as_fd()])?;
        }
        let stored = listener + self.store_clients(notifier, max, start_fd_limit)?;
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
    /// hand-over, in the store that `notifier` reaches, which takes `max` file descriptors, for a
    /// next instance that starts with `start_fd_limit` as its soft limit of open file descriptors,
    /// if any, and tell how many that stores.
    ///
    /// The sockets of the first clients go on their own, each under [`CLIENT`] and its index, as
    /// many as [`direct`] lets, and the other file descriptors in the queues of sockets
    /// ([`FdQueue`]), under [`CLIENTS`], as many sockets as that takes and as the store has room
    /// for. The clients that don't fit whole, as when the store has no room for another socket,
    /// are left out, with a warning: the next instance cleans up after them, as after clients that
    /// went.
    fn store_clients(
        &self,
        notifier: &Notifier,
        max: usize,
        start_fd_limit: Option<u64>,
    ) -> Result<usize> {
        let listener = self.listener.is_some();
        let sockets = direct(&self.clients, listener, max, start_fd_limit, |client| {
            client.connection.fds().len()
        });
        let mut queued = Vec::new();
        let mut index = 0;
        for (n, client) in self.clients.iter().enumerate() {
            for (place, fd) in client.connection.fds().enumerate() {
                if n < sockets && place == 0 {
                    notifier.store(&format!("{CLIENT}{index}"), &[fd])?;
                } else {
                    queued.push((index, fd));
                }
                index += 1;
            }
        }
        let runs = runs(queued);
        // `direct` keeps room in the store for a queue at least.
        let room = max
            .saturating_sub(1 + usize::from(listener) + sockets)
            .min(QUEUES);
        let all_runs = runs.iter().map(|(first, fds)| (*first, &fds[..]));
        let (stored_runs, failure) = queue(all_runs, room, |socket| {
            Ok(notifier.store(CLIENTS, &[socket.as_fd()])?)
        })?;
        if let Some(&(lost, _)) = runs.get(stored_runs) {
            // A client goes whole, or not at all: one that has a file descriptor from the first
            // that wasn't queued on doesn't.
            let mut first = 0;
            let handed = self
                .clients
                .iter()
                .enumerate()
                .filter(|(n, client)| {
                    let end = first + client.connection.fds().len() as u64;
                    let queued_from = first + u64::from(*n < sockets);
                    first = end;

                    queued_from == end || end <= lost
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
        let queued: usize = runs[..stored_runs].iter().map(|(_, fds)| fds.len()).sum();

        Ok(sockets + queued)
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
    /// The sockets of the clients that were stored on their own, under [`CLIENT`] and the index of
    /// each, with that index.
    sockets: Vec<(u64, OwnedFd)>,
    /// The sockets that hold the other file descriptors of the connections of the clients in their
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
                _ => match name
                    .strip_prefix(CLIENT)
                    .and_then(|index| index.parse().ok())
                {
                    Some(index) => fds.sockets.push((index, fd)),
                    None => warn!("Removing `{name}`, which busd doesn't know, from the store"),
                },
            }
        }

        fds
    }
}

/// How many of the first `clients` have their sockets stored on their own, each under a name of
/// its own, rather than in the queue of a socket: as many as there is room for in the store,
/// which takes `max` file descriptors, after the state, the socket that the bus listens on, if it
/// stores one, as `listener` tells, and a socket of a queue; as fit among the file descriptors that
/// systemd passes the next instance, which starts with `start_fd_limit` as its soft limit of open
/// file descriptors, if any, with room to spare; and as have room for their names in
/// `$LISTEN_FDNAMES`, with those of all else that systemd passes, of which Linux takes
/// [`MAX_ARG_STRLEN`] bytes at most.
///
/// Each client takes as many file descriptors as `fds` tells, which the index of the socket of the
/// next one follows, and its name with it. The store keeps one file descriptor of each open file
/// description, and the socket of a client is the one file descriptor of a connection that busd
/// knows to be of no other's, so only those go on their own. As many go so as can, since the
/// kernel counts those in the queues as in flight for the user that busd runs as, until the next
/// instance takes them out, and a process of that user without `CAP_SYS_RESOURCE` or
/// `CAP_SYS_ADMIN` can't send file descriptors over unix sockets while more of them are in flight
/// than its soft limit of open files.
fn direct<C, F>(
    clients: &[C],
    listener: bool,
    max: usize,
    start_fd_limit: Option<u64>,
    fds: F,
) -> usize
where
    F: Fn(&C) -> usize,
{
    let in_store = max.saturating_sub(1 + usize::from(listener) + 1);
    // The state, the socket to listen on, from the store or from the socket activation, and the
    // queues.
    let passed = start_fd_limit
        .and_then(|limit| usize::try_from(limit).ok())
        .map_or(usize::MAX, |limit| {
            limit.saturating_sub(FIRST_PASSED_FD + 2 + QUEUES + SPARE_FDS)
        });
    // The names of the rest are those of busd, but for that of the socket of the socket
    // activation, which its unit gives it.
    let listener_name = match listener {
        true => LISTENER.len(),
        false => systemd::MAX_NAME_LEN,
    };
    let others = STATE.len() + 1 + listener_name + 1 + QUEUES * (CLIENTS.len() + 1);
    // `LISTEN_FDNAMES=`, and the 0 that ends a string.
    let mut names = MAX_ARG_STRLEN.saturating_sub("LISTEN_FDNAMES=".len() + 1 + others);
    let mut index = 0_u64;

    clients
        .iter()
        .take(in_store.min(passed))
        .take_while(|client| {
            let digits = index.checked_ilog10().map_or(1, |log| log as usize + 1);
            index += fds(client) as u64;
            let Some(left) = names.checked_sub(CLIENT.len() + digits + 1) else {
                return false;
            };
            names = left;

            true
        })
        .count()
}

/// The runs of consecutive indices of `fds`, each file descriptor with its index, in order:
/// [`MAX_FDS`](systemd::MAX_FDS) at most each, with the index of its first, as a queue takes them,
/// in a datagram each.
fn runs<'f, I>(fds: I) -> Vec<(u64, Vec<BorrowedFd<'f>>)>
where
    I: IntoIterator<Item = (u64, BorrowedFd<'f>)>,
{
    let mut runs: Vec<(u64, Vec<_>)> = Vec::new();
    for (index, fd) in fds {
        match runs.last_mut() {
            Some((first, run))
                if run.len() < systemd::MAX_FDS && *first + run.len() as u64 == index =>
            {
                run.push(fd)
            }
            _ => runs.push((index, vec![fd])),
        }
    }

    runs
}

/// Queue the file descriptors of `runs`, each with the index of its first, in order, each run as
/// one datagram, in the queues of sockets ([`FdQueue`]), as many as that takes, `room` at most, and
/// hand each one that took any datagram to `store`, once it took all that it takes. Tell how many
/// of the runs that queued, and why not all of them, if not: queueing failed, or else, the queues
/// of `room` sockets took no more.
fn queue<'c, 'f, C, S>(runs: C, room: usize, mut store: S) -> Result<(usize, Option<io::Error>)>
where
    'f: 'c,
    C: IntoIterator<Item = (u64, &'c [BorrowedFd<'f>])>,
    S: FnMut(OwnedFd) -> Result<()>,
{
    let mut runs = runs.into_iter().peekable();
    let mut queued = 0;
    for _ in 0..room {
        if runs.peek().is_none() {
            break;
        }
        let queue = FdQueue::new()?;
        let mut took = false;
        let mut failure = None;
        while let Some(&(first, fds)) = runs.peek() {
            match queue.push(first, fds) {
                Ok(true) => queued += 1,
                // The next queue takes the rest.
                Ok(false) => break,
                Err(e) => {
                    failure = Some(e);

                    break;
                }
            }
            took = true;
            runs.next();
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
/// The prefix of the names that the sockets of the clients are stored under on their own, which
/// the index of each follows.
const CLIENT: &str = "busd-client-";
/// The name that the sockets that hold the other file descriptors of the connections of the
/// clients in their queues are stored under, however many of them there are.
const CLIENTS: &str = "busd-clients";
/// What the names that busd stores what it hands over of its clients under start with: those of
/// [`CLIENT`] and [`CLIENTS`] do, as do those that other versions of busd may store it under.
const CLIENT_PREFIX: &str = "busd-client";
/// The most sockets of queues that busd stores. They take some 70000 file descriptors each, with
/// the default size of their send buffer, so more than a service has open by default at most.
const QUEUES: usize = 8;
/// The longest string of the environment that Linux takes, `MAX_ARG_STRLEN`: 32 pages, of 4 KiB at
/// least.
const MAX_ARG_STRLEN: usize = 32 * 4096;
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
/// How many file descriptors the next instance has to spare, under its limits of open file
/// descriptors, besides those of its clients and those that systemd passes it: for those that it
/// opens of its own as it starts, before it raises its soft limit, and as it takes over.
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
            "busd-client-42",
            "busd-client-3-0",
            "busd-client-12-34-5-6",
        ] {
            assert!(is_busds(name), "{name:?}");
        }
        for name in ["busd.socket", "busd-session.socket", "busd-states", "busd"] {
            assert!(!is_busds(name), "{name:?}");
        }
    }

    /// What busd stored goes back to what it was stored as, as its name tells: the socket of a
    /// client that was stored on its own, with its index among the file descriptors of the clients.
    /// A name that busd doesn't know, as one that a version of busd that named them otherwise
    /// stored, is among those that busd stored, to be removed from the store, rather than taken
    /// for the socket of socket activation, and what was stored under it closes.
    #[test]
    fn stored_names_tell_what_each_is() {
        let names = [
            STATE,
            LISTENER,
            CLIENTS,
            "busd-client-3",
            "busd-client-4-0",
            "busd-client-x",
        ];
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
        let indexes: Vec<_> = fds.sockets.iter().map(|(index, _)| *index).collect();
        assert_eq!(indexes, [3]);
    }

    /// The sockets of the clients go on their own, rather than in a queue, as many as the store has
    /// room for, as systemd passes the next instance under the soft limit of open files that it
    /// starts with, and as have room for their names in `$LISTEN_FDNAMES`.
    #[test]
    fn sockets_go_on_their_own_as_far_as_they_fit() {
        let fds = |&fds: &usize| fds;
        let ones = [1; 10];

        assert_eq!(direct(&ones, true, 100, None, fds), 10);
        // The store keeps room for the state, the socket that the bus listens on, if it stores
        // that, and a queue.
        assert_eq!(direct(&ones, true, 8, None, fds), 5);
        assert_eq!(direct(&ones, false, 8, None, fds), 6);
        let limit = (FIRST_PASSED_FD + 2 + QUEUES + SPARE_FDS + 4) as u64;
        assert_eq!(direct(&ones, true, 100, Some(limit), fds), 4);

        // With those of all else that systemd passes the next instance, the socket of the socket
        // activation and the queues included, the names fit, and one more wouldn't.
        let many = vec![1; 100_000];
        let fitting = direct(&many, false, usize::MAX, None, fds);
        let names: usize = (0..fitting)
            .map(|index| format!("{CLIENT}{index}").len() + 1)
            .sum();
        let others = STATE.len() + systemd::MAX_NAME_LEN + QUEUES * CLIENTS.len() + 2 + QUEUES;
        let len = "LISTEN_FDNAMES=".len() + names + others + 1;
        assert!(len <= MAX_ARG_STRLEN, "{len}");
        assert!(len + format!("{CLIENT}{fitting}").len() + 1 > MAX_ARG_STRLEN);
        // The index of the socket of a client, which its name has, follows the file descriptors of
        // the clients before it.
        assert!(direct(&vec![3; 100_000], false, usize::MAX, None, fds) < fitting);
    }

    /// The file descriptors that go in the queues go in datagrams of consecutive indices, which a
    /// datagram has room for.
    #[test]
    fn runs_are_of_consecutive_indices() {
        let fd = null();
        let lens = |runs: Vec<(u64, Vec<BorrowedFd<'_>>)>| -> Vec<_> {
            runs.iter()
                .map(|(first, fds)| (*first, fds.len()))
                .collect()
        };

        let indexes = [0, 1, 2, 4, 5, 9];
        let consecutive = runs(indexes.map(|index| (index, fd.as_fd())));
        assert_eq!(lens(consecutive), [(0, 3), (4, 2), (9, 1)]);
        let max = systemd::MAX_FDS as u64;
        let long = runs((0..=max).map(|index| (index, fd.as_fd())));
        assert_eq!(lens(long), [(0, systemd::MAX_FDS), (max, 1)]);
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
        let (mut first, mut last) = (Some((0, first)), Some((2, last)));
        let none = &fds[..0];
        let runs = iter::from_fn(|| match stored.borrow().is_empty() {
            true => Some(first.take().unwrap_or((2, none))),
            false => last.take(),
        });
        let (_, failure) = queue(runs, 3, |socket| {
            stored.borrow_mut().push(socket);

            Ok(())
        })
        .unwrap();
        assert!(failure.is_none(), "{failure:?}");
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
        let runs = iter::once((0, &fds[..])).chain(iter::repeat((1, &fds[..0])));
        let (queued, failure) = queue(runs, 1, |socket| {
            stored.push(socket);

            Ok(())
        })
        .unwrap();
        assert!(failure.is_none(), "{failure:?}");
        assert!(queued > 1, "{queued}");
        assert_eq!(stored.len(), 1);
        let taken = take_queued(stored);
        assert_eq!(taken.into_keys().collect::<Vec<_>>(), [0]);

        // A store without room takes none.
        let (queued, failure) = queue([(0, &fds[..])], 0, |_| unreachable!()).unwrap();
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
