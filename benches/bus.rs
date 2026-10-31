//! What the bus costs to route the traffic of its clients: method calls, signals and connecting,
//! with the workloads of `benches/compare`, which compares busd with other brokers, as rounds of
//! traffic that CodSpeed can time on every change.
//!
//! The bus runs in this process, on a thread of its own, as busd runs it: the connections of all
//! its clients on one runtime. The clients and the services run on threads of their own too, each
//! with a runtime of its own, and connect to the bus over unix sockets, as they would from
//! processes of their own. Every id connects them before it is timed, and then times rounds of
//! traffic on those connections, each round from the clients being told to start it until the last
//! of them is done with it: what the bus costs to take messages in, check them, set their sender
//! and pass them on, rather than what the clients cost to connect, which `connection/hello` times
//! on its own. Each client builds its call once, and sends it again and again, and the emitter its
//! signals, the same in every round, which it hands to its socket in as few writes as they fit in,
//! so that a round times the bus, rather than the building of what goes through it. A round that
//! takes a minute ends the process, as one in which the bus lost a message would never end.
//!
//! The messages leave the sender out, as the clients of libdbus, GLib and sd-bus do, so that the
//! bus has to set it (see `common/calls.rs`).
//!
//! - `method-call/1-client`: a client calls a service at its well-known name, with 16 KiB of data,
//!   one call after the other; `method-call/1-client-64B` makes those calls with 64 bytes, for the
//!   round trip of a call through the bus rather than the copying of its data.
//! - `method-call/20-clients`: 20 such clients and services at once.
//! - `method-call/100-clients`: 100 at once, each service at the unique name of its connection.
//! - `method-call/100-callers-1-connection`: 100 callers that share one client connection call one
//!   service, so that the calls come in, and the replies go out, many at a time on one socket.
//! - `dicts/1-client` and `dicts/20-clients`: the calls of `method-call/1-client` and
//!   `method-call/20-clients`, with a dict of dicts of 100 variants, `a{sa{sv}}`, for their body
//!   (see `common/dicts.rs`), which costs the bus far more to check than a byte array.
//! - `signal/burst-to-1`: a burst of signals with 1 KiB of data each, to one subscriber.
//! - `signal/broadcast-to-20`: a broadcast of such signals to 20 subscribers, so that the bus
//!   delivers 20 times as many signals as it is sent.
//! - `connection/hello`: a client connects, authenticates and says `Hello`, and goes again.
//!
//! The ids keep the numbers of clients of `benches/compare`, but not its threads: where that has a
//! thread for each client and each service, here up to `THREADS` threads share them, so that every
//! thread of an id, the bus's included, has a CPU of its own on the machine that CodSpeed runs
//! them on, of 16 cores. Each thread is pinned to its CPU (see `common/cpus.rs`).

use std::{
    future::Future,
    num::NonZeroUsize,
    pin::pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use busd::bus::Bus;
use calls::{Caller, DICTS_METHOD};
use cpus::{Cpu, Cpus};
use crew::{AbortOnPanic, Crew, Member, Rounds};
use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, Criterion, Throughput,
};
use futures_util::future::{join_all, select, try_join_all, Either};
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
use shared::SharedConnection;
use zbus::{connection, names::BusName, Message};
use zruntime::{mpmc, SharedRuntime};

#[path = "common/calls.rs"]
mod calls;
#[path = "common/cpus.rs"]
mod cpus;
#[path = "common/crew.rs"]
mod crew;
#[path = "common/dicts.rs"]
mod dicts;
#[path = "common/shared.rs"]
mod shared;
#[path = "common/signals.rs"]
mod signals;

criterion_group!(benches, method_calls, dicts_calls, signals, connections);
criterion_main!(benches);

/// Benchmarks the method calls with a byte array: the ids of `method-call/`.
fn method_calls(c: &mut Criterion) {
    raise_open_files_limit();
    let mut group = c.benchmark_group("method-call");
    let data = Body::Bytes(vec![0u8; 16 * 1024]);
    pairs(
        &mut group,
        "1-client",
        1,
        ServiceNames::WellKnown,
        50,
        &data,
    );
    let small = Body::Bytes(vec![0u8; 64]);
    pairs(
        &mut group,
        "1-client-64B",
        1,
        ServiceNames::WellKnown,
        100,
        &small,
    );
    pairs(
        &mut group,
        "20-clients",
        20,
        ServiceNames::WellKnown,
        10,
        &data,
    );
    pairs(
        &mut group,
        "100-clients",
        100,
        ServiceNames::Unique,
        5,
        &data,
    );
    shared_connection(&mut group, "100-callers-1-connection", 100, 5);
    group.finish();
}

/// Benchmarks the method calls with a dict of dicts of variants: the ids of `dicts/`.
fn dicts_calls(c: &mut Criterion) {
    let mut group = c.benchmark_group("dicts");
    let dicts = Body::Dicts(dicts::dicts());
    pairs(
        &mut group,
        "1-client",
        1,
        ServiceNames::WellKnown,
        50,
        &dicts,
    );
    pairs(
        &mut group,
        "20-clients",
        20,
        ServiceNames::WellKnown,
        10,
        &dicts,
    );
    group.finish();
}

/// Benchmarks the signals: `signal/burst-to-1` and `signal/broadcast-to-20`.
fn signals(c: &mut Criterion) {
    let mut group = c.benchmark_group("signal");
    broadcast(&mut group, "burst-to-1", "Burst", 1, 1000);
    broadcast(&mut group, "broadcast-to-20", "Broadcast", 20, 100);
    group.finish();
}

/// Benchmarks connecting to the bus: `connection/hello`.
fn connections(c: &mut Criterion) {
    let mut group = c.benchmark_group("connection");
    let mut seats = Seats::new();
    let bus = BusThread::start(seats.take());
    let _pinned = seats.take().map(Cpu::pin);
    group.throughput(Throughput::Elements(1));
    group.bench_function("hello", |b| {
        b.iter_custom(|iters| {
            zruntime::block_on(async {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    let started = Instant::now();
                    let connection = connection::Builder::address(bus.address.as_str())
                        .build()
                        .await
                        .expect("a client connects");
                    total += started.elapsed();
                    // The bus lets the client go while the next connects, as it would with clients
                    // that come and go one after the other.
                    drop(connection);
                }

                total
            })
        })
    });
    group.finish();
}

/// Raises the soft limit of open files of the process to `OPEN_FILES`, or to the hard limit where
/// that is lower, as `method-call/100-clients` keeps about 410 of them open: 200 connections, the
/// bus's end of each, and the runtimes of the threads. That is more than the 256 that macOS allows
/// by default.
fn raise_open_files_limit() {
    let limit = getrlimit(Resource::Nofile);
    let wanted = limit
        .maximum
        .map_or(OPEN_FILES, |maximum| maximum.min(OPEN_FILES));
    // `None` is no limit.
    if limit.current.is_none_or(|current| current >= wanted) {
        return;
    }
    let raised = Rlimit {
        current: Some(wanted),
        maximum: limit.maximum,
    };
    if let Err(e) = setrlimit(Resource::Nofile, raised) {
        eprintln!("Cannot raise the limit of open files to {wanted}: {e}");
    }
}

/// How many open files `raise_open_files_limit` raises the limit to, at most.
const OPEN_FILES: u64 = 4096;

/// Benchmarks `clients` clients that each call a service of its own `per_round` times a round, one
/// call after the other, with `body`, all at once.
///
/// The services are at the names that `names` says, and the clients call them there.
fn pairs(
    group: &mut BenchmarkGroup<'_, WallTime>,
    id: &str,
    clients: usize,
    names: ServiceNames,
    per_round: usize,
    body: &Body,
) {
    let mut seats = Seats::new();
    let bus = BusThread::start(seats.take());
    let services = Services::start(&bus.address, &mut seats, names.of(id, clients));
    let members = shares(services.names.clone())
        .map(|destinations| {
            let address = bus.address.clone();
            let body = body.clone();
            Box::new(move |mut rounds: Rounds| {
                zruntime::block_on(async move {
                    let connecting = destinations.into_iter().map(|destination| async {
                        let call = body.call(&destination)?;
                        let mut caller = Caller::connect(&address, destination).await?;
                        // A first call, untimed, has the service answer before the first round.
                        body.first_call(&mut caller).await?;

                        calls::Result::Ok((caller, call))
                    });
                    let mut callers = try_join_all(connecting).await.expect("the clients connect");
                    while rounds.next() {
                        let calling = callers.iter_mut().map(|(caller, call)| async move {
                            for _ in 0..per_round {
                                caller.send(call).await?;
                            }

                            calls::Result::Ok(())
                        });
                        within_limit(try_join_all(calling))
                            .await
                            .expect("every call succeeds");
                    }
                })
            }) as Member
        })
        .collect::<Vec<_>>();
    let count = members.len();
    let crew = Crew::spawn(members, seats.take_many(count), seats.take());

    group.throughput(Throughput::Elements((clients * per_round) as u64));
    group.bench_function(id, |b| b.iter_custom(|iters| crew.time(iters)));
    crew.join();
}

/// Benchmarks `callers` callers that share a client connection, which each call one service
/// `per_round` times a round, one call after the other, with 16 KiB of data, all at once.
fn shared_connection(
    group: &mut BenchmarkGroup<'_, WallTime>,
    id: &str,
    callers: usize,
    per_round: usize,
) {
    let mut seats = Seats::new();
    let bus = BusThread::start(seats.take());
    let services = Services::start(&bus.address, &mut seats, vec![None]);
    let destination = services.names[0].clone();
    let address = bus.address.clone();
    let member = Box::new(move |mut rounds: Rounds| {
        zruntime::block_on(async move {
            let connection = connection::Builder::address(address.as_str())
                .build()
                .await
                .expect("the client connects");
            let shared = SharedConnection::new(connection, destination, callers);
            let data = vec![0u8; 16 * 1024];
            while rounds.next() {
                let calling = (0..callers).map(|_| async {
                    for _ in 0..per_round {
                        shared.call(&data).await?;
                    }

                    calls::Result::Ok(())
                });
                within_limit(try_join_all(calling))
                    .await
                    .expect("every call succeeds");
            }
        })
    }) as Member;
    let crew = Crew::spawn(vec![member], seats.take_many(1), seats.take());

    group.throughput(Throughput::Elements((callers * per_round) as u64));
    group.bench_function(id, |b| b.iter_custom(|iters| crew.time(iters)));
    crew.join();
}

/// Benchmarks an emitter that broadcasts `signals` signals of `member` a round, with 1 KiB of data
/// each, back to back, to `subscribers` subscribers, each of which receives all of them.
fn broadcast(
    group: &mut BenchmarkGroup<'_, WallTime>,
    id: &str,
    member: &str,
    subscribers: usize,
    signals: u64,
) {
    let mut seats = Seats::new();
    let bus = BusThread::start(seats.take());
    let rule = signals::rule(member).expect("a match rule of the signals");
    let queue = NonZeroUsize::new(signals as usize + signals::QUEUE_SPARE).expect("room");
    let mut members: Vec<Member> = (0..subscribers)
        .collect::<Vec<_>>()
        .chunks(subscribers.div_ceil(THREADS))
        .map(|share| {
            let address = bus.address.clone();
            let rule = rule.clone();
            let share = share.len();
            Box::new(move |mut rounds: Rounds| {
                zruntime::block_on(async move {
                    let subscribing =
                        (0..share).map(|_| signals::subscribe(&address, rule.clone(), queue));
                    let mut streams = try_join_all(subscribing).await.expect("they subscribe");
                    while rounds.next() {
                        let receiving = streams
                            .iter_mut()
                            .map(|stream| signals::receive(stream, 0..signals, PAYLOAD_LEN));
                        within_limit(try_join_all(receiving))
                            .await
                            .expect("every signal arrives in order");
                    }
                })
            }) as Member
        })
        .collect();
    let address = bus.address.clone();
    let member = member.to_owned();
    members.push(Box::new(move |mut rounds: Rounds| {
        zruntime::block_on(async move {
            let emitter = connection::Builder::address(address.as_str())
                .build()
                .await
                .expect("the emitter connects");
            let data = vec![0u8; PAYLOAD_LEN];
            let burst = (0..signals)
                .map(|seq| signals::signal(&member, seq, &data))
                .collect::<calls::Result<Vec<_>>>()
                .expect("the signals of a round");
            while rounds.next() {
                within_limit(emitter.send_all(&burst))
                    .await
                    .expect("every signal is sent");
            }
        })
    }));
    let count = members.len();
    let crew = Crew::spawn(members, seats.take_many(count), seats.take());

    group.throughput(Throughput::Elements(subscribers as u64 * signals));
    group.bench_function(id, |b| b.iter_custom(|iters| crew.time(iters)));
    crew.join();
}

/// How many bytes of data each signal carries.
const PAYLOAD_LEN: usize = 1024;

/// What the clients of an id call their services with.
#[derive(Clone)]
enum Body {
    /// A byte array of `METHOD`.
    Bytes(Vec<u8>),
    /// The dicts of `DICTS_METHOD`.
    Dicts(dicts::Dicts),
}

impl Body {
    /// The call of the service at `destination` with this, which its caller sends again and again,
    /// so that a round times what the bus costs to route it, rather than what building it costs
    /// the client.
    fn call(&self, destination: &BusName<'_>) -> calls::Result<Message> {
        match self {
            Self::Bytes(data) => calls::call_message(destination, data),
            Self::Dicts(dicts) => calls::method_call(destination, DICTS_METHOD, dicts),
        }
    }

    /// Has `caller` make a call with this, built for it.
    async fn first_call(&self, caller: &mut Caller) -> calls::Result<()> {
        match self {
            Self::Bytes(data) => caller.call(data).await,
            Self::Dicts(dicts) => caller.call_with(DICTS_METHOD, dicts).await,
        }
    }
}

/// The names that the services of an id are called at.
#[derive(Clone, Copy)]
enum ServiceNames {
    /// A well-known name of its own each, which the bus looks the service up by for each call.
    WellKnown,
    /// The unique name of each service's connection.
    Unique,
}

impl ServiceNames {
    /// The well-known names, if any, of `count` services of the id `id`.
    fn of(self, id: &str, count: usize) -> Vec<Option<String>> {
        (0..count)
            .map(|i| match self {
                Self::WellKnown => Some(format!("org.dbus2.benchmark.Bus.{}.S{i}", id_name(id))),
                Self::Unique => None,
            })
            .collect()
    }
}

/// `id` as an element of a well-known name, which takes letters, digits, `_` and `-` alone, and
/// no digit first.
fn id_name(id: &str) -> String {
    let name: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();

    format!("I{name}")
}

/// `items` in at most `THREADS` shares, of as equal sizes as they can be, in their order.
fn shares<T>(items: Vec<T>) -> impl Iterator<Item = Vec<T>> {
    let size = items.len().div_ceil(THREADS);
    let mut items = items.into_iter();

    std::iter::from_fn(move || {
        let share: Vec<_> = items.by_ref().take(size).collect();
        (!share.is_empty()).then_some(share)
    })
}

/// How many threads at most the clients of an id share, and the services, and the subscribers.
///
/// With the bus and the thread that times the rounds, that leaves every thread of an id a CPU of
/// its own on 16 of them, emitter of the signals included. The thread of zruntime's pool for
/// blocking work, which a connection that the bus takes in hands the look-up of the client's
/// groups to, is started from the bus's thread, and so shares its CPU.
const THREADS: usize = 4;

/// Waits for `round`, what a member of a crew does in a round, for `ROUND_LIMIT` at most.
///
/// # Panics
///
/// If the round takes longer: a bus that lost a message would leave the member waiting for it, and
/// the benchmark, and whatever runs it, with it, for good. The panic ends the process instead,
/// saying where the round got stuck.
async fn within_limit<F>(round: F) -> F::Output
where
    F: Future,
{
    let runtime = SharedRuntime::current().expect("the runtime of the thread of the member");

    runtime
        .timeout(ROUND_LIMIT, round)
        .await
        .expect("the round ends within the limit")
}

/// How long a round gets, far longer than any takes, before `within_limit` gives up on it.
const ROUND_LIMIT: Duration = Duration::from_secs(60);

/// The CPUs of the threads of an id, handed out in order: the bus's first.
struct Seats {
    /// The CPUs that the seats are taken from, in order.
    cpus: Cpus,
    /// The index of the next of them to hand out.
    next: usize,
    /// Whether it was said that there are fewer CPUs than threads.
    warned: bool,
}

impl Seats {
    /// Seats from the first of the CPUs that the calling thread may run on.
    fn new() -> Self {
        Self {
            cpus: Cpus::allowed(),
            next: 0,
            warned: false,
        }
    }

    /// The next CPU, if there is one left: a thread past the last is left unpinned, which is said
    /// once, as its numbers then have more of the scheduler's doing in them.
    fn take(&mut self) -> Option<Cpu> {
        let cpu = self.cpus.get(self.next);
        if cpu.is_none() && !self.warned {
            eprintln!(
                "Fewer CPUs than threads: the threads past the {}th are not pinned.",
                self.next
            );
            self.warned = true;
        }
        self.next += 1;

        cpu
    }

    /// The next `count` CPUs, as `take` hands them out.
    fn take_many(&mut self, count: usize) -> Vec<Option<Cpu>> {
        (0..count).map(|_| self.take()).collect()
    }
}

/// A bus, on a thread of its own, that serves the connections of an id until it is dropped.
struct BusThread {
    /// The address that the clients connect to.
    address: String,
    /// What stops the bus, by closing the channel once it is dropped.
    stop: Option<mpmc::Sender<()>>,
    /// The thread of the bus, joined once this is dropped.
    thread: Option<JoinHandle<()>>,
}

impl BusThread {
    /// Starts a bus on a thread pinned to `cpu`, and waits until it listens, on a socket of its
    /// own in the directory for temporary files.
    fn start(cpu: Option<Cpu>) -> Self {
        static STARTED: AtomicUsize = AtomicUsize::new(0);
        let n = STARTED.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("busd-bench-{}-{n}", std::process::id()));
        // A benchmark that failed ended its process at once, without removing the socket of its
        // bus, which would keep a later process that gets the same pid from listening there.
        let _ = std::fs::remove_file(&path);
        let address = format!("unix:path={}", path.display());
        let (stop, stopped) = mpmc::unbounded::<()>();
        let (listening, listens) = mpsc::channel();
        let bus_address = address.clone();
        let thread = thread::spawn(move || {
            let _abort = AbortOnPanic;
            let _pinned = cpu.map(Cpu::pin);
            zruntime::block_on(async move {
                let mut bus = Bus::for_address(Some(&bus_address)).await.expect("a bus");
                listening
                    .send(())
                    .expect("the thread that started the bus waits for it");
                {
                    let running = pin!(bus.run());
                    // Nothing is sent on the channel: this completes once it is closed.
                    let stopped = pin!(stopped.recv());
                    if let Either::Left((ran, _)) = select(running, stopped).await {
                        panic!("the bus stopped before it was asked to: {ran:?}");
                    }
                }
                bus.cleanup()
                    .await
                    .expect("the socket of the bus is removed");
            });
        });
        listens.recv().expect("the bus listens");

        Self {
            address,
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}

impl Drop for BusThread {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .expect("the thread of the bus aborts the process on a panic");
        }
    }
}

/// The services of an id, on up to `THREADS` threads of their own, which serve their methods until
/// this is dropped.
struct Services {
    /// The names that the services are called at, in the order of the names they were started
    /// with.
    names: Vec<BusName<'static>>,
    /// What stops the services, by closing the channel once it is dropped.
    stop: Option<mpmc::Sender<()>>,
    /// The threads of the services, joined once this is dropped.
    threads: Vec<JoinHandle<()>>,
}

impl Services {
    /// Starts a service at each of `names`, or at the unique name of its connection where there is
    /// none, on threads pinned to CPUs of `seats`, and waits until all of them are ready to be
    /// called.
    fn start(address: &str, seats: &mut Seats, names: Vec<Option<String>>) -> Self {
        let (stop, stopped) = mpmc::unbounded::<()>();
        let mut readiness = Vec::new();
        let threads = shares(names)
            .map(|names| {
                let cpu = seats.take();
                let address = address.to_owned();
                let stopped = stopped.clone();
                let (ready, ready_at) = mpsc::channel();
                readiness.push(ready_at);
                thread::spawn(move || {
                    let _abort = AbortOnPanic;
                    let _pinned = cpu.map(Cpu::pin);
                    zruntime::block_on(serve(&address, names, ready, stopped));
                })
            })
            .collect();
        let names = readiness
            .into_iter()
            .flat_map(|ready_at| ready_at.recv().expect("the services connect"))
            .collect();

        Self {
            names,
            stop: Some(stop),
            threads,
        }
    }
}

impl Drop for Services {
    fn drop(&mut self) {
        drop(self.stop.take());
        for thread in self.threads.drain(..) {
            thread
                .join()
                .expect("the threads of the services abort the process on a panic");
        }
    }
}

/// Connects a service at each of `names`, says on `ready` the names that they are called at, and
/// serves all of them until `stopped` is closed.
async fn serve(
    address: &str,
    names: Vec<Option<String>>,
    ready: mpsc::Sender<Vec<BusName<'static>>>,
    stopped: mpmc::Receiver<()>,
) {
    let connecting = names
        .iter()
        .map(|name| calls::service(address, name.as_deref()));
    let services = try_join_all(connecting)
        .await
        .expect("the services connect");
    let called_at = services
        .iter()
        .zip(&names)
        .map(|((connection, _), name)| match name {
            Some(name) => BusName::try_from(name.clone()).expect("a well-known name"),
            None => {
                let unique = connection
                    .unique_name()
                    .expect("a connection to a bus has one");
                BusName::from(unique).into_owned()
            }
        })
        .collect();
    ready
        .send(called_at)
        .expect("the thread that starts the services waits for them");

    // Each fails at once on an error of its own, so that what went wrong is said, rather than what
    // its client ran into for want of a service.
    let serving = services.into_iter().map(|(connection, incoming)| {
        let stopped = stopped.clone();
        async move {
            calls::serve(&connection, incoming, stopped)
                .await
                .expect("a service serves until it is stopped");
        }
    });
    join_all(serving).await;
}
