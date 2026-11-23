//! What the busd tests share to run a bus and to connect clients to it.
//!
//! [`with`] and [`with_bus`] stop the bus and remove its socket file before they look at the
//! outcome of the test. A test that panics skips this clean-up and leaves the socket file behind.

use std::{
    env::temp_dir,
    future::{Future, IntoFuture},
    os::unix::net::UnixStream,
    path::PathBuf,
    pin::pin,
    sync::Once,
    time::Duration,
};

use futures_util::future::{join, select, Either};
use zbus::{connection, Address, Connection, OwnedGuid};
use zruntime::{Event, SharedRuntime, Task};

use crate::{bus::Bus, peer::SEND_TIMEOUT, peers::Peers};

/// Set up the tracing subscriber, once per process.
///
/// The tests of a file run in one process, and a subscriber can be set only once.
pub fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(crate::tracing_subscriber::init);
}

/// Run `test` against a bus of its own. `test` is given the bus's address.
///
/// The address has no GUID.
pub async fn with_bus<F, T>(test: F)
where
    F: FnOnce(String) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let address = unix_address();
    let bus = Bus::for_address(Some(&address)).await.unwrap();

    run(bus, address, |bus| test(bus.address)).await;
}

/// Run `test` against `bus`. The caller sets the bus up: its time limit, its quota and its policy.
///
/// The bus is stopped and its socket file removed before the outcome of `test` is looked at, so a
/// test that returns an error leaves nothing behind. A test that panics skips this clean-up and
/// leaves the socket file behind: dropping the bus's task doesn't remove it.
pub async fn with<F, T>(bus: Bus, test: F)
where
    F: FnOnce(TestBus) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    // The bus's address without its GUID, as a test that starts its own bus gets it.
    let address = Address::new(bus.address().transport().clone()).to_string();

    run(bus, address, test).await;
}

/// The bus as a test sees it: its address, its peers and its GUID.
#[derive(Clone, Debug)]
pub struct TestBus {
    /// The bus's listening address, without its GUID.
    address: String,
    /// The peers of the bus.
    peers: Peers,
    /// The GUID of the bus.
    guid: OwnedGuid,
}

impl TestBus {
    /// The address that clients connect to.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The bus's peers. [`TestBus::connect_as`] adds its clients to them.
    pub fn peers(&self) -> &Peers {
        &self.peers
    }

    /// Connect a client through the bus's listening socket.
    pub async fn connect(&self) -> zbus::Result<Connection> {
        connect(&self.address).await
    }

    /// Connect a client of `uid`, numbered `id`, to this bus. The client connects over a pair of
    /// sockets, not through the bus's listening socket.
    ///
    /// The client belongs to that user, whichever user runs the test.
    pub async fn connect_as(&self, id: usize, uid: u32) -> anyhow::Result<Connection> {
        let (ours, theirs) = UnixStream::pair()?;
        let adding = self.peers.add(
            &self.guid,
            id,
            connection::Builder::unix_stream(theirs),
            SEND_TIMEOUT,
            uid,
        );
        let connecting = connection::Builder::unix_stream(ours).build();
        let (added, conn) = join(adding, connecting).await;
        added?;

        Ok(conn?)
    }
}

/// A bus that runs in a task of its own until it is stopped.
pub struct Running {
    /// The event that stops the bus when it is notified.
    stop: Event,
    /// The task that runs the bus, and gives it back once it is stopped.
    task: Task<Bus, zruntime::Shared>,
}

impl Running {
    /// Run `bus` until [`Running::stop`].
    ///
    /// # Panics
    ///
    /// The task panics if the bus stops on its own.
    pub fn new(mut bus: Bus) -> Self {
        let stop = Event::new();
        let stopped = stop.listen();
        let task = zruntime::spawn(async move {
            {
                let running = pin!(bus.run());
                match select(stopped, running).await {
                    Either::Left(_) => (),
                    Either::Right((Ok(()), _)) => panic!("Bus exited unexpectedly"),
                    Either::Right((Err(e), _)) => panic!("Bus exited with an error: {e}"),
                }
            }

            bus
        });

        Self { stop, task }
    }

    /// Stop the bus and give it back, so that the caller can hand it over or look at what it
    /// leaves behind.
    pub async fn stop(self) -> Bus {
        self.stop.notify(usize::MAX);

        self.task.await.unwrap()
    }

    /// Stop the bus and remove its socket.
    pub async fn finish(self) {
        self.stop().await.cleanup().await.unwrap();
    }
}

/// Connect a client to the bus at `address`.
pub async fn connect(address: &str) -> zbus::Result<Connection> {
    connection::Builder::address(address).build().await
}

/// Await `future` for at most [`WAIT`].
///
/// # Panics
///
/// If `future` takes longer.
pub async fn within<F>(future: F) -> F::Output
where
    F: IntoFuture,
{
    SharedRuntime::current()
        .unwrap()
        .timeout(WAIT, future)
        .await
        .expect("Timed out")
}

/// How long a test waits for something that should happen at once.
pub const WAIT: Duration = Duration::from_secs(10);

/// The address of a test's own UNIX socket: `unix:path=` followed by [`socket_path`].
pub fn unix_address() -> String {
    format!("unix:path={}", socket_path().display())
}

/// A path for a test's own socket, in the directory for temporary files.
pub fn socket_path() -> PathBuf {
    let name: String = std::iter::repeat_with(fastrand::alphanumeric)
        .take(10)
        .collect();

    temp_dir().join(name)
}

/// Run `bus` while `test` runs against it at `address`, then stop the bus and clean up after it.
async fn run<F, T>(bus: Bus, address: String, test: F)
where
    F: FnOnce(TestBus) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let test_bus = TestBus {
        address,
        peers: bus.peers().clone(),
        guid: bus.guid().clone(),
    };
    let running = Running::new(bus);

    let ret = test(test_bus).await;
    running.finish().await;
    ret.unwrap();
}
