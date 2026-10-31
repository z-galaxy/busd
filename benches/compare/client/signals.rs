//! The `signals` workload: how fast the bus gets signals through.
//!
//! The counterpart of `calls` for signals: it broadcasts signals through the bus and times how long
//! they take to arrive, in two phases, and prints the line of each as it ends, with how long it
//! took, in seconds, as `single_s` or `multi_s`:
//!
//! 1. `single`, a burst: one connection broadcasts `BURST_SIGNALS` signals back to back, and a
//!    second one, subscribed to them, receives all of them.
//! 2. `multi`, a fan-out: one connection broadcasts `FANOUT_SIGNALS` signals back to back, and
//!    `SUBSCRIBERS` others, all subscribed to them, receive each one, so the bus delivers
//!    `SUBSCRIBERS` times as many signals as it is sent.
//!
//! In between, the connections of the fan-out are made and subscribed, which ends with a line of
//! its own, `fanout_connect`, with nothing measured: the runner notes the CPU time that the bus and
//! the client have used by the time that each line comes, so that what connecting the fan-out costs
//! them counts towards neither phase.
//!
//! Every signal carries its sequence number and `PAYLOAD_LEN` bytes of payload. A subscriber
//! checks that the numbers arrive as 0, 1, 2 and so on, so the run fails if the bus loses, repeats
//! or reorders a signal, and so it does if a D-Bus call fails. Each subscriber runs on a thread of
//! its own, with a single-threaded runtime of its own, as a process of its own would: it takes each
//! signal off its socket as soon as the bus has written it there, rather than when one thread for
//! all of them gets round to it, and the emitter has the main thread to itself, so that how long a
//! phase takes is how fast the bus gets the signals through.
//!
//! What a phase times is from just before the first signal is sent until the last subscriber has
//! received its last one. The connections are made and subscribed before that.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    ops::Range,
    thread::JoinHandle,
    time::{Duration, Instant},
};

use zbus::{connection, Connection};

use super::{print_elapsed, spawn_subscribers, Watchdog};
use crate::{
    calls::Result,
    protocol::PhaseLine,
    signals::{receive, rule, signal, QUEUE_SPARE},
};

/// Runs the client of `signals` against the bus at `address`.
pub(super) fn run(address: &str) -> Result<()> {
    zruntime::block_on(async {
        let data = vec![0u8; PAYLOAD_LEN];

        let burst = Phase::connect(address, "burst", "Burst", 1, BURST_SIGNALS).await?;
        let elapsed = burst.run(&data).await?;
        print_elapsed("single", elapsed)?;

        let fanout =
            Phase::connect(address, "fan-out", "Fanout", SUBSCRIBERS, FANOUT_SIGNALS).await?;
        PhaseLine {
            phase: "fanout_connect".to_owned(),
            metrics: BTreeMap::new(),
        }
        .print()?;

        let elapsed = fanout.run(&data).await?;
        print_elapsed("multi", elapsed)?;

        Ok(())
    })
}

/// A phase of the workload, connected and subscribed: an emitter and the subscribers that receive
/// the signals it sends.
struct Phase {
    /// The member of the signals of this phase.
    member: &'static str,
    /// How many signals the emitter sends, and each subscriber receives.
    signals: usize,
    /// The connection that sends the signals. It stays open until the phase is over, so that the
    /// bus never sees the sender go while signals from it are still on their way.
    emitter: Connection,
    /// The threads of the subscribers, each of which returns when it took its last signal.
    subscribers: Vec<JoinHandle<Result<Instant>>>,
    /// Ends the process if the phase takes too long, unless it is over by then and drops this.
    _watchdog: Watchdog,
}

impl Phase {
    /// Connects an emitter, and `subscribers` subscribers, to the bus at `address`, and subscribes
    /// the latter to the signals of `member`, of which the phase will send `signals`.
    ///
    /// `name` is the name of the phase, for the watchdog to say which phase took too long.
    async fn connect(
        address: &str,
        name: &'static str,
        member: &'static str,
        subscribers: usize,
        signals: usize,
    ) -> Result<Self> {
        let watchdog = Watchdog::start(name);
        let rule = rule(member)?;
        let queue = NonZeroUsize::new(signals + QUEUE_SPARE).expect("room for the spare signals");

        let emitter = connection::Builder::address(address).build().await?;
        let seqs = 0..signals as u64;
        let subscribers = spawn_subscribers(
            address,
            &rule,
            queue,
            subscribers,
            move |mut stream| async move {
                receive(&mut stream, seqs, PAYLOAD_LEN)
                    .await
                    .map(|()| Instant::now())
            },
        )?;

        Ok(Self {
            member,
            signals,
            emitter,
            subscribers,
            _watchdog: watchdog,
        })
    }

    /// Sends the signals while the subscribers receive them, and returns how long that took.
    async fn run(self, data: &[u8]) -> Result<Duration> {
        let before = Instant::now();
        emit(&self.emitter, self.member, 0..self.signals as u64, data).await?;
        let mut last = before;
        for subscriber in self.subscribers {
            let done = subscriber.join().map_err(|_| "a subscriber panicked")??;
            last = last.max(done);
        }

        Ok(last.duration_since(before))
    }
}

/// Broadcasts the signals of `member` with the sequence numbers of `seqs`, each with `data` as
/// payload, one after the other, each built as it is sent, as a program that emits signals would.
async fn emit(connection: &Connection, member: &str, seqs: Range<u64>, data: &[u8]) -> Result<()> {
    for seq in seqs {
        connection.send(&signal(member, seq, data)?).await?;
    }

    Ok(())
}

/// How many signals the burst sends.
///
/// A phase should take between 0.2 and 1.5 s: long enough to time to a fraction of a percent,
/// short enough to repeat many times. On a 4-vCPU Xeon VM, the burst takes about 0.35 s against
/// busd, 0.27 s against dbus-broker and 0.68 s against dbus-daemon, so half as many signals would
/// leave the fastest of them close to 0.14 s.
const BURST_SIGNALS: usize = 40_000;

/// How many signals the fan-out sends, each of which is delivered to every subscriber.
///
/// That is `SUBSCRIBERS` times as many deliveries, 120000 of them, but 15% of the signals that the
/// burst sends, which takes 0.27 to 0.34 s against the fastest brokers, and 1.0 to 1.2 s against
/// the slowest, on a 4-vCPU Xeon VM, so fewer signals would bring the fastest close to 0.2 s.
const FANOUT_SIGNALS: usize = 6_000;

/// How many subscribers the fan-out has, as many as the simultaneous clients of `calls`.
const SUBSCRIBERS: usize = 20;

/// How many bytes of payload every signal carries.
const PAYLOAD_LEN: usize = 1024;
