//! A bus that systemd runs: on the socket of its socket activation, and handed over to its next
//! instance through its file descriptor store.

use std::{
    collections::{BTreeSet, HashMap},
    fmt,
    os::fd::{AsFd, OwnedFd},
    str::FromStr,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rustix::{
    fs::fstat,
    pipe::{pipe_with, PipeFlags},
};
use zbus::{
    address::{transport::UnixSocket, Transport},
    Address,
};
use zruntime::SharedRuntime;

use super::{Bus, ClientHandover, Handover};
use crate::{
    debug, error, info,
    systemd::{self, Notifier},
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
    /// activation passed it, or that of a client. Otherwise, the bus says why it doesn't hand over,
    /// and cleans up as [`Bus::cleanup`] does, only logging a failure to, as busd does without
    /// systemd.
    ///
    /// The bus hands over as [`Bus::hand_over`] does, and stores what it hands over, for
    /// [`Bus::for_systemd`] to take over from on the restart: its state, then the socket that it
    /// listens on, and then the clients that fit, each with the file descriptors of its connection,
    /// in the order of the hand-over, as many as the store takes and as systemd passes the next
    /// instance. That instance starts with the soft limit of open file descriptors that busd
    /// started with, `fd_limit`, `None` for no limit, unless the limit of the unit changed
    /// meanwhile, and systemd fails to start it with more than that limit takes, after the socket
    /// of the socket activation, if any. The clients that don't fit get disconnected as the bus
    /// goes, with a warning. It fails if the bus takes longer than 30 s to hand over, or if storing
    /// fails, which disconnects the clients as the bus goes. systemd stores one file descriptor of
    /// each open file description, so a duplicate of one that was stored, as when a client sent the
    /// same file descriptor twice, goes as a place-holder, under a name that says which file
    /// descriptor the next instance is to duplicate in its place.
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

        handover.store(notifier)
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
    /// Those that busd stored have names of busd's own, as [`Stored`] has them. The others are the
    /// sockets of socket activation, which have the names of their units by default, such as
    /// `busd.socket`. busd listens on one, so this fails if there is more than one.
    fn take_activation(fds: &mut Vec<(String, OwnedFd)>) -> Result<Option<OwnedFd>> {
        let mut sockets = fds.extract_if(.., |(name, _)| !Stored::is_busds(name));
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
        match Self::take_over(handover, activation).await {
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
    /// clients, and the others for the state and the barrier, which storing opens once the bus is
    /// handed over.
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
    /// names that it stored each under, if it stored its state.
    ///
    /// Each client gets the file descriptors of its connection back in the order that they were
    /// stored in, its socket first, whatever the order that systemd passes them in, and the
    /// place-holder of a duplicate gets the client a duplicate of the file descriptor that it says,
    /// in its place, which may be one of another client. Those of a client whose socket is missing
    /// close, as do those under names that busd doesn't store any under, as ones that a version of
    /// busd that named them otherwise stored. Without the state, they all close.
    fn from_stored(stored: Vec<(String, OwnedFd)>) -> Option<Self> {
        let mut state = None;
        let mut listener = None;
        let mut client_fds = HashMap::<_, Vec<_>>::new();
        let mut duplicates = Vec::new();
        for (name, fd) in stored {
            match name.parse() {
                Ok(Stored::State) => match systemd::read_memfd(fd.as_fd()) {
                    Ok(bytes) => state = Some(bytes),
                    Err(e) => warn!("Failed to read the state of the bus that was stored: {e}"),
                },
                Ok(Stored::Listener) => listener = Some(fd),
                Ok(Stored::Client((id, n))) => client_fds.entry(id).or_default().push((n, fd)),
                // What was stored for a duplicate only holds its place.
                Ok(Stored::Duplicate(place, original)) => duplicates.push((place, original)),
                Err(_) => warn!("Removing `{name}`, which busd doesn't know, from the store"),
            }
        }
        Self::restore_duplicates(&mut client_fds, duplicates);
        let clients: Vec<_> = client_fds
            .into_iter()
            .filter_map(|(id, mut fds)| {
                fds.sort_unstable_by_key(|(n, _)| *n);
                if fds.first().is_none_or(|(n, _)| *n != 0) {
                    return None;
                }

                Some(ClientHandover {
                    id,
                    fds: fds.into_iter().map(|(_, fd)| fd).collect(),
                })
            })
            .collect();
        let Some(state) = state else {
            if !clients.is_empty() || listener.is_some() {
                warn!("Dropping the sockets that were stored without the state of the bus");
            }

            return None;
        };

        Some(Self {
            state,
            listener,
            clients,
        })
    }

    /// Keep the clients that fit, in order: in the file descriptor store, which takes `max` file
    /// descriptors, after the state and the socket that the bus listens on, if any, and among those
    /// that systemd passes the next instance, which starts with `fd_limit` as its soft limit of
    /// open file descriptors, if any, with room to spare. busd warns if any don't.
    ///
    /// Each client takes the file descriptors of its connection, its socket and those that came
    /// with the messages that the bus didn't take. One that doesn't fit is left out whole, as the
    /// next instance couldn't serve it without all of them, while those after it, of fewer file
    /// descriptors, may still fit.
    fn fit(&mut self, max: usize, fd_limit: Option<u64>) {
        // The next instance gets the state and the socket to listen on, from the store or from the
        // socket activation, and then the clients.
        let in_store = max.saturating_sub(1 + usize::from(self.listener.is_some()));
        let under_limit = fd_limit
            .and_then(|limit| usize::try_from(limit).ok())
            .map_or(usize::MAX, |limit| {
                limit.saturating_sub(FIRST_PASSED_FD + 2 + SPARE_FDS)
            });
        let clients = self.clients.len();
        let mut room = in_store.min(under_limit);
        self.clients.retain(|client| {
            let Some(left) = room.checked_sub(client.fds.len()) else {
                return false;
            };
            room = left;

            true
        });
        let fit = self.clients.len();
        if fit < clients {
            let why = match fd_limit {
                Some(limit) if under_limit < in_store => format!(
                    "systemd only passes the next instance as many file descriptors as its soft \
                     limit of open file descriptors takes, {limit} as busd started: raise that of \
                     `LimitNOFILE=`"
                ),
                _ => format!(
                    "the file descriptor store takes {max}: raise `FileDescriptorStoreMax=`"
                ),
            };
            warn!("Handing over {fit} of the {clients} clients, as {why} of the service.");
        }
    }

    /// Store the hand-over in the file descriptor store of systemd, which `notifier` reaches, under
    /// the names that [`Stored`] has: the state, the socket that the bus listened on, if it did,
    /// and the file descriptors of the connections of the clients, a place-holder for each
    /// duplicate. Then wait for systemd to have dealt with them.
    fn store(&self, notifier: &Notifier) -> Result<()> {
        self.store_state(notifier)?;
        if let Some(listener) = &self.listener {
            notifier.store(&Stored::Listener.to_string(), &[listener.as_fd()])?;
        }
        let stored = usize::from(self.listener.is_some()) + self.store_clients(notifier)?;
        // The service manager attributes what the bus sent to it by its process, which goes next.
        notifier.barrier(BARRIER_TIMEOUT)?;
        debug!("Stored the state of the bus and {stored} file descriptors for its next instance");

        Ok(())
    }

    /// Store the state of the bus, in a memfd that closes as this returns: the place-holders of the
    /// duplicates take its place among the open file descriptors.
    fn store_state(&self, notifier: &Notifier) -> Result<()> {
        let name = Stored::State.to_string();
        let state = systemd::memfd(&name, &self.state)?;
        notifier.store(&name, &[state.as_fd()])?;

        Ok(())
    }

    /// Store the file descriptors of the connections of the clients, each under the place that it
    /// has, a place-holder for each duplicate of one before it, and tell how many.
    fn store_clients(&self, notifier: &Notifier) -> Result<usize> {
        let duplicates = self.duplicates();
        let mut stored = 0;
        for client in &self.clients {
            for (n, fd) in client.fds.iter().enumerate() {
                let place = (client.id, n);
                let Some(&original) = duplicates.get(&place) else {
                    notifier.store(&Stored::Client(place).to_string(), &[fd.as_fd()])?;

                    continue;
                };
                let name = Stored::Duplicate(place, original).to_string();
                let place_holder = systemd::memfd(&name, &[])?;
                notifier.store(&name, &[place_holder.as_fd()])?;
            }
            stored += client.fds.len();
        }

        Ok(stored)
    }

    /// The duplicates among the file descriptors of the connections of the clients: where each one
    /// is, and where the file descriptor that it duplicates is.
    ///
    /// A duplicate is of the same open file description as a file descriptor before it, which
    /// systemd stores alone. Only file descriptors of the same file can be of the same open file
    /// description, so only those are compared, and only the kernel tells whether they are
    /// ([`systemd::same_file`]). Where it doesn't, they are stored as they are, rather than one
    /// taken for a duplicate of the other, whose place-holder would get a duplicate of the other in
    /// its place, sharing its offset. systemd may then take them for the same and store the first
    /// alone, as it guesses that two file descriptors of one file with the same flags are: the
    /// client lacks the other then, and the next instance fails to serve it, and cleans up after
    /// it, as after a client that went.
    fn duplicates(&self) -> HashMap<Place, Place> {
        let mut by_file = HashMap::<_, Vec<_>>::new();
        let mut duplicates = HashMap::new();
        for client in &self.clients {
            for (n, fd) in client.fds.iter().enumerate() {
                // A file descriptor whose file can't be told is stored all the same.
                let Ok(stat) = fstat(fd) else {
                    continue;
                };
                let originals: &mut Vec<_> = by_file.entry((stat.st_dev, stat.st_ino)).or_default();
                let original = originals.iter().find(|(_, original)| {
                    systemd::same_file(fd.as_fd(), *original).is_ok_and(|same| same)
                });
                match original {
                    Some((place, _)) => {
                        duplicates.insert((client.id, n), *place);
                    }
                    None => originals.push(((client.id, n), fd.as_fd())),
                }
            }
        }

        duplicates
    }

    /// Give each of `duplicates`, at its place among the file descriptors of the connections of the
    /// clients, `client_fds`, by the number of the client and then by the place of each, a
    /// duplicate of the file descriptor at the place that it says. One whose file descriptor is
    /// missing goes without, as one of a client that went would.
    fn restore_duplicates(
        client_fds: &mut HashMap<usize, Vec<(usize, OwnedFd)>>,
        duplicates: Vec<(Place, Place)>,
    ) {
        for ((id, n), original) in duplicates {
            let (original_id, original_n) = original;
            let fd = client_fds
                .get(&original_id)
                .and_then(|fds| fds.iter().find(|(m, _)| *m == original_n))
                .map(|(_, fd)| fd.try_clone());
            match fd {
                Some(Ok(fd)) => client_fds.entry(id).or_default().push((n, fd)),
                Some(Err(e)) => warn!(
                    "Failed to duplicate `{}` for the client numbered {id}: {e}",
                    Stored::Client(original),
                ),
                None => (),
            }
        }
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
    /// For the state, and the two ends of the pipe of the barrier, after.
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

/// What busd stores a file descriptor as, which the name that it stores the file descriptor under
/// tells.
#[derive(Debug, PartialEq)]
enum Stored {
    /// The state of the bus, under `busd-state`.
    State,
    /// The socket that the bus listens on, unless systemd's socket activation passed it, under
    /// `busd-listener`.
    Listener,
    /// A file descriptor of the connection of a client, at its place, under `busd-client-`, the
    /// number of the client, a `-`, and the number of the file descriptor among those of the
    /// connection of the client, from 0, its socket, on, in the order of the hand-over of the
    /// connection.
    Client(Place),
    /// The place-holder of a duplicate, at its place, of the file descriptor at the other place,
    /// under the name of a file descriptor at its place, a `-`, and the numbers of the other
    /// place, with a `-` between them.
    Duplicate(Place, Place),
}

impl Stored {
    /// Whether `name` is one that busd stores file descriptors under, though maybe not as this
    /// version of busd does, as for one that a version of busd that named them otherwise stored.
    fn is_busds(name: &str) -> bool {
        name == STATE || name == LISTENER || name.starts_with(CLIENT)
    }
}

impl FromStr for Stored {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> Result<Self> {
        match name {
            STATE => return Ok(Self::State),
            LISTENER => return Ok(Self::Listener),
            _ => (),
        }
        let numbers = name
            .strip_prefix(CLIENT)
            .and_then(|place| {
                place
                    .split('-')
                    .map(|number| number.parse().ok())
                    .collect::<Option<Vec<_>>>()
            })
            .with_context(|| format!("busd stores nothing under `{name}`"))?;

        match numbers[..] {
            [id, n] => Ok(Self::Client((id, n))),
            [id, n, original_id, original_n] => {
                Ok(Self::Duplicate((id, n), (original_id, original_n)))
            }
            _ => bail!("busd stores nothing under `{name}`"),
        }
    }
}

impl fmt::Display for Stored {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State => f.write_str(STATE),
            Self::Listener => f.write_str(LISTENER),
            Self::Client((id, n)) => write!(f, "{CLIENT}{id}-{n}"),
            Self::Duplicate((id, n), (original_id, original_n)) => {
                write!(f, "{CLIENT}{id}-{n}-{original_id}-{original_n}")
            }
        }
    }
}

/// Where a file descriptor is among those of the connections of the clients: the number of the
/// client, and the place of the file descriptor among those of its connection.
type Place = (usize, usize);

/// The name that the state of the bus is stored under.
const STATE: &str = "busd-state";
/// The name that the socket that the bus listens on is stored under.
const LISTENER: &str = "busd-listener";
/// The prefix of the names that the file descriptors of the connections of the clients are stored
/// under.
const CLIENT: &str = "busd-client-";
/// How long the bus may take to hand over, before busd gives up on it, and its clients get
/// disconnected as it goes.
///
/// The bus gives each client as long as it has to take a message, `SEND_TIMEOUT`, 10 s, from when
/// the hand-over starts, to take all that the bus sends it meanwhile, so a client that doesn't read
/// holds the hand-over up for no longer than that. This is longer, with room for the rest of the
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
/// How many file descriptors the next instance has to spare, under its soft limit, besides those
/// that systemd passes it, to start with if it can't raise the limit.
const SPARE_FDS: usize = 32;

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        os::fd::{AsRawFd, RawFd},
    };

    use super::*;

    /// The names of the file descriptors that busd stores tell what each is, and for those of the
    /// clients, which client each is of, and where it is among those of the connection of the
    /// client.
    #[test]
    fn stored_names() {
        for (name, stored) in [
            ("busd-state", Stored::State),
            ("busd-listener", Stored::Listener),
            ("busd-client-3-0", Stored::Client((3, 0))),
            ("busd-client-12-34", Stored::Client((12, 34))),
            ("busd-client-12-34-5-6", Stored::Duplicate((12, 34), (5, 6))),
        ] {
            assert_eq!(name.parse::<Stored>().unwrap(), stored, "{name:?}");
            assert_eq!(stored.to_string(), name);
            assert!(Stored::is_busds(name), "{name:?}");
        }
        for place in [
            "",
            "x",
            "3",
            "3-",
            "-0",
            "3-x",
            "3-fd-0",
            "3-1-2",
            "3-1-2-",
            "3-1-2-3-4",
        ] {
            let name = format!("{CLIENT}{place}");
            assert!(name.parse::<Stored>().is_err(), "{name:?}");
            assert!(Stored::is_busds(&name), "{name:?}");
        }
        for name in ["busd.socket", "busd-states", "busd-client"] {
            assert!(name.parse::<Stored>().is_err(), "{name:?}");
            assert!(!Stored::is_busds(name), "{name:?}");
        }
    }

    /// A name of a client that busd doesn't know, as one that a version of busd that named them
    /// otherwise stored, is among those that busd stored, to be removed from the store, rather
    /// than taken for the socket of socket activation, and the client is left out.
    #[test]
    fn unknown_client_names_are_removed() {
        let names = [STATE, LISTENER, "busd-client-3", "busd-client-4-fd-0"];
        let mut passed = names
            .iter()
            .map(|name| {
                let fd = match *name {
                    STATE => systemd::memfd(STATE, b"state").unwrap(),
                    _ => null(),
                };

                (name.to_string(), fd)
            })
            .collect();

        assert!(Bus::take_activation(&mut passed).unwrap().is_none());
        let stored: Vec<_> = passed.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(stored, names);
        assert!(Handover::from_stored(passed).unwrap().clients.is_empty());
    }

    /// The place-holder of a duplicate gets the client a duplicate of the file descriptor that it
    /// says, in its place, which may be one of another client.
    #[test]
    fn duplicates_go_back_to_their_clients() {
        let state = systemd::memfd(STATE, b"state").unwrap();
        let stored = vec![
            (STATE.to_owned(), state),
            (format!("{CLIENT}1-0"), null()),
            (format!("{CLIENT}1-1"), null()),
            (format!("{CLIENT}1-2-1-1"), null()),
            (format!("{CLIENT}2-0"), null()),
            (format!("{CLIENT}2-1-1-1"), null()),
        ];

        let mut clients = Handover::from_stored(stored).unwrap().clients;
        clients.sort_unstable_by_key(|client| client.id);
        let [first, second] = &clients[..] else {
            panic!("{} clients were passed", clients.len());
        };
        let original = first.fds[1].as_fd();
        let same = |fd: &OwnedFd| systemd::same_file(original, fd.as_fd()).unwrap();
        assert_eq!(
            first.fds.iter().map(same).collect::<Vec<_>>(),
            [false, true, true]
        );
        assert_eq!(
            second.fds.iter().map(same).collect::<Vec<_>>(),
            [false, true]
        );
    }

    /// A file descriptor of the same open file description as one before it, among those of the
    /// same client or of another one, is a duplicate of the first, while one of another open file
    /// description of the same file isn't.
    #[test]
    fn duplicates_are_found() {
        let shared = null();
        let duplicate = || shared.try_clone().unwrap();
        let clients = vec![
            ClientHandover {
                id: 1,
                fds: vec![null(), duplicate(), null(), duplicate()],
            },
            ClientHandover {
                id: 2,
                fds: vec![null(), duplicate()],
            },
        ];

        let duplicates = handover(clients).duplicates();
        let expected = HashMap::from([((1, 3), (1, 1)), ((2, 1), (1, 1))]);
        assert_eq!(duplicates, expected);
    }

    /// The file descriptors of the connection of a client go back to it in the order that they
    /// were stored in, its socket first, whatever the order that systemd passes them in, and those
    /// of a client whose socket is missing close.
    #[test]
    fn passed_fds_go_back_to_their_clients() {
        let state = systemd::memfd(STATE, b"state").unwrap();
        let fds = [null(), null(), null()];
        let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
        let [socket, first, second] = fds;
        let mut passed = vec![
            (format!("{CLIENT}1-2"), second),
            (STATE.to_owned(), state),
            (format!("{CLIENT}2-1"), null()),
            (format!("{CLIENT}1-0"), socket),
            (format!("{CLIENT}1-1"), first),
        ];

        assert!(Bus::take_activation(&mut passed).unwrap().is_none());
        let handover = Handover::from_stored(passed).unwrap();
        let [client] = &handover.clients[..] else {
            panic!("{} clients were passed", handover.clients.len());
        };
        assert_eq!(client.id, 1);
        let got: Vec<RawFd> = client.fds.iter().map(AsRawFd::as_raw_fd).collect();
        assert_eq!(got, raw);
    }

    /// A client takes the file descriptors of its connection, its socket and those that came with
    /// the messages that the bus didn't take, and one that doesn't fit is left out whole, while
    /// those after it may still fit.
    #[test]
    fn clients_fit_whole() {
        let clients = [1, 3, 1, 2].into_iter().enumerate().map(client).collect();
        let mut handover = handover(clients);

        // The store takes the state, and the file descriptors of 3 clients.
        handover.fit(4, None);

        let ids: Vec<_> = handover.clients.iter().map(|client| client.id).collect();
        assert_eq!(ids, [0, 2]);
    }

    /// A hand-over of `clients`, with no listener.
    fn handover(clients: Vec<ClientHandover>) -> Handover {
        Handover {
            state: Vec::new(),
            listener: None,
            clients,
        }
    }

    /// The client numbered `id`, with `fds` file descriptors of its connection.
    fn client((id, fds): (usize, usize)) -> ClientHandover {
        ClientHandover {
            id,
            fds: (0..fds).map(|_| null()).collect(),
        }
    }

    /// A file descriptor of no consequence.
    fn null() -> OwnedFd {
        File::open("/dev/null").unwrap().into()
    }
}
