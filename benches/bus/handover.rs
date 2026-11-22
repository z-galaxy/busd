//! The setup of the `handover/` benchmarks, which restart the bus. They run on Linux, the only
//! system on which the bus hands over. The setup is a bus that restarts on request, either in its
//! own process or through a fake systemd's file descriptor store, and clients that keep their idle
//! connections across its restarts.

use std::{
    io::IoSliceMut,
    mem::{self, MaybeUninit},
    os::{
        fd::OwnedFd,
        unix::net::{UnixDatagram, UnixListener},
    },
    path::{Path, PathBuf},
    pin::pin,
    rc::Rc,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use busd::{
    bus::{Bus, STORE_KEY},
    peer::SEND_TIMEOUT,
};
use futures_util::future::{select, try_join_all, Either};
use rustix::{
    cmsg_space,
    net::{recvmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags},
};
use zbus::connection;
use zoter::{testing, Passed, Store};
use zruntime::{mpmc, LocalRuntime};

use super::{socket_path, until_gone, within_limit, AbortOnPanic, Cpu};

/// How the bus restarts in the ids of `handover/`.
#[derive(Clone, Copy)]
pub(super) enum Restart {
    /// In this process: `Bus::hand_over`, then `Bus::take_over`.
    InProcess,
    /// Through the file descriptor store of a fake systemd: `Bus::hand_over_to_systemd`, then
    /// `Bus::for_systemd`.
    FdStore,
}

/// A bus, on a thread of its own, that serves the connections of an id, and restarts when asked
/// to, until this is dropped.
pub(super) struct RestartingBus {
    /// The address that the clients connect to.
    address: String,
    /// What asks the bus to restart a number of times in a row, with where to say how long that
    /// took, and stops the bus, by closing the channel once it is dropped.
    requests: Option<mpmc::Sender<(u64, mpsc::Sender<Duration>)>>,
    /// The thread of the bus, joined once this is dropped.
    thread: Option<JoinHandle<()>>,
}

impl RestartingBus {
    /// Starts a bus on a thread pinned to `cpu`, which restarts as `how` says, through a fake
    /// systemd on a thread pinned to `systemd_cpu` for [`Restart::FdStore`], and waits until it
    /// listens.
    pub(super) fn start(how: Restart, cpu: Option<Cpu>, systemd_cpu: Option<Cpu>) -> Self {
        let path = socket_path("bus");
        let address = format!("unix:path={}", path.display());
        let (requests, requested) = mpmc::unbounded::<(u64, mpsc::Sender<Duration>)>();
        let (listening, listens) = mpsc::channel();
        let bus_address = address.clone();
        let thread = thread::spawn(move || {
            let _abort = AbortOnPanic;
            let _pinned = cpu.map(Cpu::pin);
            let systemd = match how {
                Restart::InProcess => None,
                Restart::FdStore => Some(FakeSystemd::start(systemd_cpu, &path)),
            };
            let runtime = LocalRuntime::new().expect("the runtime of the bus");
            runtime.block_on(async move {
                // The store that the bus hands over through, which a bus is given as it starts.
                let mut store = systemd.as_ref().map(FakeSystemd::store);
                let bus = match &systemd {
                    None => Bus::for_address(Some(&bus_address)).await,
                    // As busd starts under systemd: on the listening socket of the socket
                    // activation.
                    Some(systemd) => {
                        let passed = Passed::new(systemd.passed(Vec::new()));
                        Bus::for_systemd(passed, store.as_mut(), Some(&bus_address), Rc::default())
                            .await
                    }
                };
                let mut bus = bus.expect("a bus");
                listening
                    .send(())
                    .expect("the thread that started the bus waits for it");
                loop {
                    let request = {
                        let running = pin!(bus.run());
                        let requested = pin!(requested.recv());
                        match select(running, requested).await {
                            Either::Left((ran, _)) => {
                                panic!("the bus stopped before it was asked to: {ran:?}")
                            }
                            Either::Right((request, _)) => request,
                        }
                    };
                    // The channel closes once the bus is to stop.
                    let Ok((times, took)) = request else {
                        break;
                    };
                    let started = Instant::now();
                    for _ in 0..times {
                        bus = match &systemd {
                            None => restart_in_process(bus).await,
                            Some(systemd) => {
                                let handing = store.take().expect("a bus that systemd runs");
                                let (next, next_store) =
                                    restart_through(systemd, bus, handing, &bus_address).await;
                                store = Some(next_store);

                                next
                            }
                        };
                    }
                    took.send(started.elapsed())
                        .expect("the thread that asked for the restarts waits for them");
                }
                // A bus on the listening socket of the fake systemd leaves its file be, for the
                // fake to remove.
                let peers = bus.peers().downgrade();
                bus.cleanup()
                    .await
                    .expect("the socket of the bus is removed");
                until_gone(peers).await;
            });
        });
        listens.recv().expect("the bus listens");

        Self {
            address,
            requests: Some(requests),
            thread: Some(thread),
        }
    }

    /// The address that the clients connect to.
    pub(super) fn address(&self) -> &str {
        &self.address
    }

    /// Has the bus restart `times` times in a row, and gives how long that took.
    pub(super) fn restart(&self, times: u64) -> Duration {
        let (took, takes) = mpsc::channel();
        self.requests
            .as_ref()
            .expect("the bus runs until this is dropped")
            .try_send((times, took))
            .expect("the bus takes requests");

        takes
            .recv()
            .expect("the bus says how long the restarts took")
    }
}

impl Drop for RestartingBus {
    fn drop(&mut self) {
        drop(self.requests.take());
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("the thread of the bus aborts the process on a panic");
        }
    }
}

/// The next instance of `bus`, which takes its clients over in this process.
async fn restart_in_process(bus: Bus) -> Bus {
    let handover = bus.hand_over().await.expect("the bus hands over");

    Bus::take_over(handover, None, SEND_TIMEOUT, Rc::default())
        .await
        .expect("the next instance takes over")
}

/// The next instance of `bus`, which hands its clients over through `store`, the store of
/// `systemd`, and the store of the next instance, which takes them over at `address`, as busd does
/// at the address that it is told.
async fn restart_through(
    systemd: &FakeSystemd,
    bus: Bus,
    store: Store,
    address: &str,
) -> (Bus, Store) {
    bus.hand_over_to_systemd(store)
        .await
        .expect("the bus hands over");
    let passed = Passed::new(systemd.passed(systemd.stored()));
    let mut store = systemd.store();
    let bus = Bus::for_systemd(passed, Some(&mut store), Some(address), Rc::default())
        .await
        .expect("the next instance starts");

    (bus, store)
}

/// A fake systemd, on a thread of its own, that keeps what a bus stores in the file descriptor
/// store, as systemd does, and passes it to the next instance of the bus, with the listening socket
/// of the socket activation, which it keeps too: what the bus stored up to the barrier that it
/// sends once it stored all.
struct FakeSystemd {
    /// What the bus stored up to each of its barriers, in turn.
    stored: mpsc::Receiver<Vec<(String, OwnedFd)>>,
    /// The path of the notification socket, which the bus sends its notifications to, removed once
    /// this is dropped.
    path: PathBuf,
    /// The listening socket of the socket activation, which the bus listens on.
    activation: UnixListener,
    /// Its path, removed once this is dropped.
    activation_path: PathBuf,
    /// The thread of the fake, joined once this is dropped.
    thread: Option<JoinHandle<()>>,
}

impl FakeSystemd {
    /// Starts a fake systemd on a thread pinned to `cpu`, with a notification socket of its own,
    /// and a listening socket of socket activation, bound at `activation_path`.
    fn start(cpu: Option<Cpu>, activation_path: &Path) -> Self {
        let activation = UnixListener::bind(activation_path).expect("the listening socket");
        let path = socket_path("systemd");
        let socket = UnixDatagram::bind(&path).expect("the notification socket");
        let (storing, stored) = mpsc::channel();
        let thread = thread::spawn(move || {
            let _abort = AbortOnPanic;
            let _pinned = cpu.map(Cpu::pin);
            let mut store = Vec::new();
            loop {
                // The file descriptors that come with a notification close at the end of the turn,
                // unless they are stored: the bus goes on from its barrier once the pipe that comes
                // with it closes, after what it stored is passed on.
                let (state, fds) = receive(&socket);
                if let Some(name) = state.strip_prefix("FDSTORE=1\nFDPOLL=0\nFDNAME=") {
                    store.extend(fds.into_iter().map(|fd| (name.to_owned(), fd)));
                } else if let Some(name) = state.strip_prefix("FDSTOREREMOVE=1\nFDNAME=") {
                    store.retain(|(stored, _)| stored != name);
                } else if state == "BARRIER=1" {
                    storing
                        .send(mem::take(&mut store))
                        .expect("the bus waits for what it stored");
                } else if state == STOP_FAKE_SYSTEMD {
                    break;
                }
            }
        });

        Self {
            stored,
            path,
            activation,
            activation_path: activation_path.to_owned(),
            thread: Some(thread),
        }
    }

    /// The store that an instance of the bus hands over through, which takes `FD_STORE_MAX` file
    /// descriptors.
    fn store(&self) -> Store {
        testing::store(&self.path, STORE_KEY, FD_STORE_MAX).expect("a store of the fake systemd")
    }

    /// What the bus stored as it handed over.
    fn stored(&self) -> Vec<(String, OwnedFd)> {
        self.stored
            .recv()
            .expect("the fake systemd passes on what the bus stored")
    }

    /// The passed FDs that the fake starts an instance of the bus with: the listening socket of the
    /// socket activation, and `stored`, what the previous instance stored, if any.
    fn passed(&self, stored: Vec<(String, OwnedFd)>) -> Vec<(String, OwnedFd)> {
        let activation = self
            .activation
            .try_clone()
            .expect("a duplicate of the listening socket");

        std::iter::once((ACTIVATION.to_owned(), activation.into()))
            .chain(stored)
            .collect()
    }
}

impl Drop for FakeSystemd {
    fn drop(&mut self) {
        // The fake waits for notifications, and ends with this one.
        UnixDatagram::unbound()
            .and_then(|socket| socket.send_to(STOP_FAKE_SYSTEMD.as_bytes(), &self.path))
            .expect("the fake systemd takes notifications");
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("the thread of the fake systemd aborts the process on a panic");
        }
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(&self.activation_path);
    }
}

/// The name of the socket unit that the fake systemd passes the listening socket under.
const ACTIVATION: &str = "busd.socket";
/// What ends the fake systemd, which systemd knows nothing of.
const STOP_FAKE_SYSTEMD: &str = "X_BENCH_STOP=1";
/// How many file descriptors the store of the fake systemd takes, as `FileDescriptorStoreMax=`
/// does for busd's service in the README.
const FD_STORE_MAX: usize = 16;

/// The next notification that `socket` gets, with the file descriptors that came with it.
fn receive(socket: &UnixDatagram) -> (String, Vec<OwnedFd>) {
    // The bus sends one file descriptor with each, and a few bytes.
    let mut state = [0; 4096];
    let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(4))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let received = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut state)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )
    .expect("a notification");
    assert!(
        !received
            .flags
            .intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC),
        "a notification of the bus is too large for the fake systemd"
    );
    let fds = control
        .drain()
        .filter_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten()
        .collect();
    let state = String::from_utf8_lossy(&state[..received.bytes]).into_owned();

    (state, fds)
}

/// Clients of a bus, on a thread of their own, that keep their connections, idle, until this is
/// dropped, and check when asked that the bus still serves each of them.
pub(super) struct IdleClients {
    /// What asks the clients to check, with where to say that they did, and lets them go, by
    /// closing the channel once it is dropped.
    checks: Option<mpmc::Sender<mpsc::Sender<()>>>,
    /// The thread of the clients, joined once this is dropped.
    thread: Option<JoinHandle<()>>,
}

impl IdleClients {
    /// Connects `count` clients to the bus at `address`, on a thread pinned to `cpu`, and waits
    /// until all of them are connected.
    pub(super) fn connect(address: &str, count: usize, cpu: Option<Cpu>) -> Self {
        let (checks, checking) = mpmc::unbounded::<mpsc::Sender<()>>();
        let (connected, connects) = mpsc::channel();
        let address = address.to_owned();
        let thread = thread::spawn(move || {
            let _abort = AbortOnPanic;
            let _pinned = cpu.map(Cpu::pin);
            zruntime::block_on(async move {
                let connecting =
                    (0..count).map(|_| connection::Builder::address(address.as_str()).build());
                let clients = try_join_all(connecting).await.expect("the clients connect");
                connected
                    .send(())
                    .expect("the thread that connects the clients waits for them");
                while let Ok(checked) = checking.recv().await {
                    // The bus answers a call only on a connection that it serves.
                    let calling = clients.iter().map(|client| {
                        client.call_method(
                            Some("org.freedesktop.DBus"),
                            "/org/freedesktop/DBus",
                            Some("org.freedesktop.DBus"),
                            "GetId",
                            &(),
                        )
                    });
                    within_limit(try_join_all(calling))
                        .await
                        .expect("the bus serves every client on");
                    checked
                        .send(())
                        .expect("the thread that asked for the check waits for it");
                }
            });
        });
        connects.recv().expect("the clients connect");

        Self {
            checks: Some(checks),
            thread: Some(thread),
        }
    }

    /// Checks that the bus still serves every client.
    pub(super) fn check(&self) {
        let (checked, checks) = mpsc::channel();
        self.checks
            .as_ref()
            .expect("the clients wait until this is dropped")
            .try_send(checked)
            .expect("the clients take checks");
        checks.recv().expect("the clients check");
    }
}

impl Drop for IdleClients {
    fn drop(&mut self) {
        drop(self.checks.take());
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("the thread of the clients aborts the process on a panic");
        }
    }
}
