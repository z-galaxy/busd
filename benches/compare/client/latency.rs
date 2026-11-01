//! The `latency` workload: the latency distributions of the bus.
//!
//! Where `calls` and `signals` time whole phases, this times every message of three phases like
//! theirs, and prints the line of each as it ends, with the distribution of those times:
//!
//! - `single_rtt`: the round trips of `SINGLE_CALLS` calls, one after the other, from a client to a
//!   service through the bus, as in `calls`, with a small payload.
//! - `multi_rtt`: the round trips of `MULTI_CALLS` calls from each of `MULTI_CLIENTS` such clients
//!   at once, each to a service of its own, with the payload of `calls`.
//! - `fanout_delivery`: how long each of `FANOUT_SIGNALS` signals, which one connection broadcasts
//!   back to back, takes to reach each of `FANOUT_SUBSCRIBERS` subscribers, as in `signals`: from
//!   just before the emitter builds it until the subscriber has taken it off its stream. The
//!   signals queue up behind each other in the bus, so the later ones wait longer. Those that a bus
//!   keeps the emitter from sending, by taking its signals off its socket slowly, wait before they
//!   are timed, though: how long the phase takes is what tells how fast a bus gets the signals
//!   through.
//!
//! Each distribution has the number of samples, the seconds that the phase took, and the mean, the
//! median (`p50`), the `p90`, the `p99`, the `p999` and the largest of the samples, in
//! microseconds, as the metrics `<phase>_count`, `<phase>_elapsed_s`, `<phase>_mean_us`,
//! `<phase>_p50_us`, `<phase>_p90_us`, `<phase>_p99_us`, `<phase>_p999_us` and `<phase>_max_us`. A
//! percentile is the nearest-rank one: the sample at the rank that the fraction of the samples ends
//! at, rounded up, counting from the smallest.
//!
//! Each service and each client of the round trips, and each subscriber of `fanout_delivery`, runs
//! on a thread of its own, with a connection and a single-threaded runtime of its own, as a process
//! of its own would, like those of `calls` and `signals`: it takes each message off its socket as
//! soon as the bus has written it there, rather than when one thread for all of them gets round to
//! it, so that how long the messages wait is the bus's doing. The emitter has the main thread to
//! itself. A phase fails on a failed call, and on a signal that is lost or arrives out of order,
//! and the client then exits without printing the line of that phase, or of those after it.

use std::{
    num::NonZeroUsize,
    time::{Duration, Instant},
};

use futures_util::TryStreamExt;
use serde_bytes::Bytes;
use zbus::{connection, names::BusName, Connection, Message, MessageStream};
use zruntime::lock::Barrier;

use super::{run_pairs, spawn_subscribers, Watchdog};
use crate::{
    calls::{Caller, Result, PATH},
    protocol::PhaseLine,
    signals::{rule, INTERFACE, QUEUE_SPARE},
};

/// Runs the client of `latency` against the bus at `address`.
pub(super) fn run(address: &str) -> Result<()> {
    single_rtt(address)?.print("single_rtt")?;
    multi_rtt(address)?.print("multi_rtt")?;
    zruntime::block_on(fanout_delivery(address))?.print("fanout_delivery")?;

    Ok(())
}

/// The phase `single_rtt`: calls from one client to its service, one after the other.
fn single_rtt(address: &str) -> Result<Stats> {
    let _watchdog = Watchdog::start("single_rtt");
    let data = vec![0u8; SINGLE_PAYLOAD_LEN];
    let names = [Some("org.dbus2.benchmark.SingleClient".to_owned())];

    round_trips(address, &names, &data, SINGLE_CALLS)
}

/// The phase `multi_rtt`: calls from many clients, each to a service of its own, at the same time.
fn multi_rtt(address: &str) -> Result<Stats> {
    let _watchdog = Watchdog::start("multi_rtt");
    let data = vec![0u8; MULTI_PAYLOAD_LEN];
    let names: Vec<_> = (0..MULTI_CLIENTS)
        .map(|i| Some(format!("org.dbus2.benchmark.MultiClient{i}")))
        .collect();

    round_trips(address, &names, &data, MULTI_CALLS)
}

/// Runs a service at each of `names`, and a client that calls it `calls` times, one call after the
/// other, with `data` as the byte array, and times the calls.
///
/// The clients start calling together, once all of them have connected and all the services own
/// their names, and the services stop once the clients are done, or one of them failed. How long
/// the phase took is from when the first client started calling until the last reply came.
fn round_trips(
    address: &str,
    names: &[Option<String>],
    data: &[u8],
    calls: usize,
) -> Result<Stats> {
    let start = Barrier::new(names.len());
    let done = run_pairs(address, names, |destination| {
        client(address, destination, data, calls, &start)
    })?;

    let started = done.iter().map(|calls| calls.started).min();
    let finished = done.iter().map(|calls| calls.finished).max();
    let (Some(started), Some(finished)) = (started, finished) else {
        return Err("there was no client".into());
    };
    let round_trips = done.into_iter().flat_map(|calls| calls.round_trips);

    Stats::new(round_trips.collect(), finished.duration_since(started))
}

/// A client of the round trips, on a thread of its own, with a connection and a single-threaded
/// runtime of its own: connects to the bus at `address`, waits at `start` until the other clients
/// have too, and calls the service at `destination` `calls` times, one call after the other, with
/// `data` as the byte array.
fn client(
    address: &str,
    destination: BusName<'static>,
    data: &[u8],
    calls: usize,
    start: &Barrier,
) -> Result<Calls> {
    zruntime::block_on(async {
        let caller = Caller::connect(address, destination).await;
        // It arrives whether it connected or not, so that the others do not wait for good for one
        // that failed.
        start.wait().await;
        let mut caller = caller?;

        let started = Instant::now();
        let mut round_trips = Vec::with_capacity(calls);
        for _ in 0..calls {
            let before = Instant::now();
            caller.call(data).await?;
            round_trips.push(nanos(before.elapsed()));
        }

        Ok(Calls {
            round_trips,
            started,
            finished: Instant::now(),
        })
    })
}

/// The calls that a client of the round trips made.
struct Calls {
    /// How long each call took, from sending it until the reply came, in nanoseconds.
    round_trips: Vec<u64>,
    /// When the client started calling.
    started: Instant,
    /// When the reply to its last call came.
    finished: Instant,
}

/// The phase `fanout_delivery`: signals from one emitter to many subscribers.
async fn fanout_delivery(address: &str) -> Result<Stats> {
    let _watchdog = Watchdog::start("fanout_delivery");
    let data = vec![0u8; FANOUT_PAYLOAD_LEN];
    let rule = rule(MEMBER)?;
    let queue = NonZeroUsize::new(FANOUT_SIGNALS + QUEUE_SPARE).expect("room for the spare");

    let emitter = connection::Builder::address(address).build().await?;
    let subscribers = spawn_subscribers(address, &rule, queue, FANOUT_SUBSCRIBERS, receive)?;

    // The signals carry the time they were sent at as a number of nanoseconds since this.
    let epoch = Instant::now();
    emit(&emitter, &data, epoch).await?;
    let mut delays = Vec::with_capacity(FANOUT_SIGNALS * FANOUT_SUBSCRIBERS);
    let mut last = epoch;
    for subscriber in subscribers {
        let received = subscriber.join().map_err(|_| "a subscriber panicked")??;
        for (seq, (sent, taken)) in received.into_iter().enumerate() {
            let Some(delay) = nanos(taken.duration_since(epoch)).checked_sub(sent) else {
                return Err(format!("signal {seq} arrived before it was sent").into());
            };
            delays.push(delay);
            last = last.max(taken);
        }
    }

    Stats::new(delays, last.duration_since(epoch))
}

/// Broadcasts the signals of `fanout_delivery` on `connection`.
///
/// Each carries its sequence number, the time at which this started to build it, in nanoseconds
/// since `epoch`, and `data` as payload, which is borrowed, rather than copied into the signal
/// first. They have no sender, for the bus to set.
async fn emit(connection: &Connection, data: &[u8], epoch: Instant) -> Result<()> {
    let payload = Bytes::new(data);
    for seq in 0..FANOUT_SIGNALS as u64 {
        let sent = nanos(epoch.elapsed());
        let msg = Message::signal(PATH, INTERFACE, MEMBER).build(&(seq, sent, payload))?;
        connection.send(&msg).await?;
    }

    Ok(())
}

/// Receives the signals of `fanout_delivery` from `stream`, checking that they arrive in order and
/// intact.
///
/// What it gives back is, for each signal, the time that it carries, in nanoseconds since the
/// emitter's epoch, and when the stream gave it up.
async fn receive(mut stream: MessageStream) -> Result<Vec<(u64, Instant)>> {
    let mut received = Vec::with_capacity(FANOUT_SIGNALS);
    for expected in 0..FANOUT_SIGNALS as u64 {
        let msg = stream.try_next().await?;
        // Taken before anything else is done with the signal, so as to count only the delivery.
        let taken = Instant::now();
        let Some(msg) = msg else {
            let count = FANOUT_SIGNALS;
            return Err(format!("the stream ended after {expected} of {count} signals").into());
        };
        let body = msg.body();
        let (seq, sent, payload): (u64, u64, &Bytes) = body.deserialize()?;

        if seq != expected {
            return Err(format!("signal {seq} arrived where signal {expected} was due").into());
        }
        if payload.len() != FANOUT_PAYLOAD_LEN {
            let len = payload.len();
            return Err(
                format!("signal {seq} carried {len} bytes, not {FANOUT_PAYLOAD_LEN}").into(),
            );
        }
        received.push((sent, taken));
    }

    Ok(received)
}

/// The nanoseconds in `duration`.
fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).expect("a duration of less than 584 years")
}

/// The distribution of the samples of a phase.
struct Stats {
    /// How many samples there are.
    count: usize,
    /// How long the phase took, in seconds.
    elapsed_s: f64,
    /// The mean of the samples, in microseconds.
    mean_us: f64,
    /// The median of the samples, in microseconds.
    p50_us: f64,
    /// The 90th percentile of the samples, in microseconds.
    p90_us: f64,
    /// The 99th percentile of the samples, in microseconds.
    p99_us: f64,
    /// The 99.9th percentile of the samples, in microseconds.
    p999_us: f64,
    /// The largest of the samples, in microseconds.
    max_us: f64,
}

impl Stats {
    /// The distribution of `samples`, in nanoseconds, of a phase that took `elapsed`.
    ///
    /// Fails if there are no samples, which have no distribution.
    fn new(mut samples: Vec<u64>, elapsed: Duration) -> Result<Self> {
        samples.sort_unstable();
        let Some(&max) = samples.last() else {
            return Err("the phase has no samples".into());
        };
        let count = samples.len();
        let total: u128 = samples.iter().map(|&sample| u128::from(sample)).sum();
        let us = |ns: u64| ns as f64 / 1000.0;

        Ok(Self {
            count,
            elapsed_s: elapsed.as_secs_f64(),
            mean_us: total as f64 / count as f64 / 1000.0,
            p50_us: us(percentile(&samples, 500)),
            p90_us: us(percentile(&samples, 900)),
            p99_us: us(percentile(&samples, 990)),
            p999_us: us(percentile(&samples, 999)),
            max_us: us(max),
        })
    }

    /// Prints the line of the phase `phase`, with each figure of the distribution as a metric named
    /// after the phase: `<phase>_count`, `<phase>_elapsed_s`, `<phase>_mean_us` and so on.
    fn print(&self, phase: &str) -> Result<()> {
        let figures = [
            ("count", self.count as f64),
            ("elapsed_s", self.elapsed_s),
            ("mean_us", self.mean_us),
            ("p50_us", self.p50_us),
            ("p90_us", self.p90_us),
            ("p99_us", self.p99_us),
            ("p999_us", self.p999_us),
            ("max_us", self.max_us),
        ];
        let metrics = figures
            .into_iter()
            .map(|(figure, value)| (format!("{phase}_{figure}"), value))
            .collect();
        PhaseLine {
            phase: phase.to_owned(),
            metrics,
        }
        .print()?;

        Ok(())
    }
}

/// The nearest-rank percentile of `sorted`, at `permille` thousandths of the way through them.
///
/// That is the sample at the rank that `permille` thousandths of their number comes to, rounded
/// up, counting from 1 for the smallest. It is worked out in whole numbers, which a fraction of
/// a float could get wrong by one at the boundaries.
fn percentile(sorted: &[u64], permille: usize) -> u64 {
    let rank = (permille * sorted.len())
        .div_ceil(1000)
        .clamp(1, sorted.len());

    sorted[rank - 1]
}

/// How many calls `single_rtt` makes.
const SINGLE_CALLS: usize = 5_000;

/// How many bytes of payload the calls of `single_rtt` carry: a small one, so that the round
/// trip is that of the bus and not of copying the payload.
const SINGLE_PAYLOAD_LEN: usize = 64;

/// How many services `multi_rtt` calls at once, as many as `calls`.
const MULTI_CLIENTS: usize = 20;

/// How many calls `multi_rtt` makes on each service.
const MULTI_CALLS: usize = 1_000;

/// How many bytes of payload the calls of `multi_rtt` carry, as many as in `calls`.
const MULTI_PAYLOAD_LEN: usize = 16 * 1024;

/// How many signals `fanout_delivery` sends.
const FANOUT_SIGNALS: usize = 2_000;

/// How many subscribers receive the signals of `fanout_delivery`.
const FANOUT_SUBSCRIBERS: usize = 20;

/// How many bytes of payload the signals of `fanout_delivery` carry, as many as in `signals`.
const FANOUT_PAYLOAD_LEN: usize = 1024;

/// The member of the signals of `fanout_delivery`.
const MEMBER: &str = "Delivery";
