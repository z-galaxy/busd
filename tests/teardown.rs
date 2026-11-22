//! What the buses of a test leave behind in its process once `busd::testing::block_on` returns.
#![cfg(all(feature = "testing", target_os = "linux"))]

use std::{fs, num::NonZeroUsize, path::PathBuf, time::Duration};

use busd::{
    bus::Bus,
    peers::Peers,
    testing::{connect, init_tracing, unix_address, with, with_bus, within, TestBus},
};
use ntest::timeout;
use zbus::{fdo::DBusProxy, message, Connection, MatchRule, Message, MessageStream};
use zruntime::SharedRuntime;

/// `busd::testing::block_on` leaves nothing of a test's bus open in the process, round after round.
///
/// That holds once the clients of the test have called the bus and gone. It holds too once the test
/// has ended while the bus was in the middle of a send to a client that doesn't read. Such a send
/// waits for the client within its time limit, on a timer and on the client's socket.
///
/// The test is the only one of its process, so that no other test opens file descriptors
/// meanwhile.
#[test]
#[timeout(60000)]
fn buses_leave_nothing_open() {
    init_tracing();
    let before = open_fds();

    for _ in 0..ROUNDS {
        busd::testing::block_on(with_bus(idle_clients));
    }
    assert_nothing_left_open(&before, "clients that called the bus and went");
    for _ in 0..ROUNDS {
        busd::testing::block_on(async {
            let bus = Bus::for_address(Some(&unix_address())).await.unwrap();
            with(bus, send_under_way).await;
        });
    }
    assert_nothing_left_open(&before, "a send to a client that doesn't read under way");
}

/// Two clients of the bus at `address`, which call it, and go as the test ends.
async fn idle_clients(address: String) -> anyhow::Result<()> {
    let first = connect(&address).await?;
    let second = connect(&address).await?;
    for client in [&first, &second] {
        DBusProxy::new(client).await?.get_id().await?;
    }

    Ok(())
}

/// Assert that the process has at most [`SLACK`] more file descriptors open than before the rounds,
/// once rounds of `what` are done.
///
/// `before` holds the FDs that the process had open before the rounds.
fn assert_nothing_left_open(before: &[PathBuf], what: &str) {
    let after = open_fds();
    assert!(
        after.len() <= before.len() + SLACK,
        "{} file descriptors were open before the rounds, and {} after those of {what}: \
         {before:?} before, {after:?} after",
        before.len(),
        after.len(),
    );
}

/// A client of `bus` that stops reading once it holds a signal, which a flood of signals from
/// another client fills up.
///
/// The test ends once the bus waits for the client to take the rest. The test then cancels the
/// flood, and the test's clients go.
async fn send_under_way(bus: TestBus) -> anyhow::Result<()> {
    let stalled = bus.connect().await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    // zbus reads no more of the socket while the stream holds a message that the test didn't take.
    let _signals = MessageStream::for_match_rule(rule, &stalled, Some(NonZeroUsize::MIN)).await?;
    let sender = bus.connect().await?;
    let _flooding = zruntime::spawn(flood(sender));
    until_stalled(bus.peers(), &stalled).await;

    Ok(())
}

/// Have `sender` emit [`FLOOD`] signals of 64 KiB, far more than the sockets of a client that
/// doesn't read take.
async fn flood(sender: Connection) -> zbus::Result<()> {
    let payload = "x".repeat(64 * 1024);
    let signal = Message::signal("/org/busd/Test", INTERFACE, "Flood").build(&payload)?;
    for _ in 0..FLOOD {
        sender.send(&signal).await?;
    }

    Ok(())
}

/// Wait until the bus of `peers` waits for `receiver` to take what it sends.
///
/// The bus is waiting once what it has left to write to the client is the same in two looks, 10 ms
/// apart, and more than nothing.
async fn until_stalled(peers: &Peers, receiver: &Connection) {
    let runtime = SharedRuntime::current().unwrap();
    within(async {
        let mut before = unwritten_len(peers, receiver);
        loop {
            runtime.sleep(Duration::from_millis(10)).await;
            let now = unwritten_len(peers, receiver);
            if now == before && now > 0 {
                return;
            }
            before = now;
        }
    })
    .await;
}

/// How many bytes the bus of `peers` has left to write to `receiver`.
fn unwritten_len(peers: &Peers, receiver: &Connection) -> usize {
    let name = receiver.unique_name().unwrap();
    let routing = peers.routing();

    routing
        .peer(name.as_str())
        .unwrap()
        .client()
        .connection()
        .unwritten_len()
}

/// The file descriptors that the process has open, by what each refers to.
fn open_fds() -> Vec<PathBuf> {
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| fs::read_link(entry.ok()?.path()).ok())
        .collect()
}

/// How many rounds of each kind the test runs.
const ROUNDS: usize = 50;
/// How many more file descriptors than before the rounds may be open after them.
///
/// They are the FDs of zruntime's process-wide runtime, which the clients run on: its poller, its
/// timer and the two ends of the pipe that wakes it. That runtime goes in its own time once the
/// clients are done with it. A bus is gone only once its clients have closed their ends. A bus
/// whose runtime outlived its round would leave twice as many FDs open. They would be those of its
/// own runtime, and, for each of the two clients of the round, its end of the client's connection
/// and the pidfd of the client's process.
const SLACK: usize = 4;
/// How many signals a flood has.
const FLOOD: usize = 64;
/// The interface of the signals of the floods.
const INTERFACE: &str = "org.busd.Teardown";
