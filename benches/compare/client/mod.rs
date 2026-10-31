//! The clients of the comparison: what one run of a workload does against a bus.
//!
//! The runner runs the client of a workload once for each run, as a process of its own: this
//! program, with the hidden `client` subcommand, the workload and the address of the bus of the
//! broker under test, any broker. The client connects to the bus at that address and to nothing
//! else, and needs nothing of the broker but what the D-Bus specification has every bus do.
//!
//! As each phase of the workload ends, the client prints the `PhaseLine` of the phase, a line of
//! JSON with its name and what the client measured of it, on its standard output, and nothing else
//! goes there: the runner reads each line as it comes, as `crate::protocol` says. What goes wrong
//! goes to the standard error, and the client then exits with a status other than 0: 2 if it gave
//! up on a phase that took too long.
//!
//! Each workload is that of a client of [dbus-bench], with the same connections, messages and
//! timings, on busd's own locked zbus and zruntime, which are not those of dbus-bench's clients: so
//! the results compare with each other within a run, and only roughly with dbus-bench's.
//! Each service, client and subscriber runs on a thread of its own, with a connection and a
//! single-threaded runtime of its own, as a process of its own would, so that none holds another
//! up. The messages that are timed have no sender, for the bus to set, as `crate::calls` says.
//!
//! [dbus-bench]: https://github.com/zeenix/dbus-bench

mod calls;
mod latency;
mod shared;
mod signals;

use std::{
    collections::BTreeMap,
    future::Future,
    num::NonZeroUsize,
    sync::mpsc,
    thread::{self, JoinHandle},
    time::Duration,
};

use zbus::{names::BusName, Connection, MatchRule, MessageStream};
use zruntime::mpmc;

use crate::{
    calls::{service, Result},
    protocol::PhaseLine,
    signals::subscribe,
    workload::Workload,
};

/// Runs the client of `workload` against the bus at `address`, printing a `PhaseLine` as each of
/// its phases ends.
pub fn run(workload: Workload, address: &str) -> Result<()> {
    match workload {
        Workload::Calls => calls::run_calls(address),
        Workload::Signals => signals::run(address),
        Workload::Latency => latency::run(address),
        Workload::Shared => shared::run(address),
        Workload::Dicts => calls::run_dicts(address),
    }
}

/// Prints the line of the phase `phase`, which took `elapsed`: what each phase of every workload
/// but `latency` measures, as the metric `<phase>_s`, in seconds.
fn print_elapsed(phase: &str, elapsed: Duration) -> Result<()> {
    let metrics = BTreeMap::from([(format!("{phase}_s"), elapsed.as_secs_f64())]);
    PhaseLine {
        phase: phase.to_owned(),
        metrics,
    }
    .print()?;

    Ok(())
}

/// Runs a service at each of `names`, and a client of each, `client`, which is handed the name to
/// call its service at, and returns what the clients returned, once all of them are done and all
/// the services are gone.
///
/// A service at `None` has no well-known name, and is called at the unique name of its connection.
/// Each service and each client runs on a thread of its own, with a connection and a
/// single-threaded runtime of its own, which `client` makes for the client. The clients start once
/// all the services can be called, and the services stop once the clients are done, or one of them
/// failed.
fn run_pairs<C, T>(address: &str, names: &[Option<String>], client: C) -> Result<Vec<T>>
where
    C: Fn(BusName<'static>) -> Result<T> + Sync,
    T: Send,
{
    thread::scope(|scope| {
        // Nothing is sent on this channel: dropping the sender, whichever way the scope is left,
        // closes it, which is what stops the services.
        let (stop, stopped) = mpmc::unbounded::<()>();
        let (ready, readiness) = mpsc::channel();
        let services: Vec<_> = names
            .iter()
            .map(|name| {
                let ready = ready.clone();
                let stopped = stopped.clone();
                scope.spawn(move || serve(address, name.as_deref(), ready, stopped))
            })
            .collect();
        drop(ready);
        let mut destinations = Vec::with_capacity(services.len());
        for _ in 0..services.len() {
            let destination = readiness
                .recv()
                .map_err(|_| "a service ended before it was ready")??;
            destinations.push(destination);
        }

        let clients: Vec<_> = destinations
            .into_iter()
            .map(|destination| {
                let client = &client;
                scope.spawn(move || client(destination))
            })
            .collect();
        let mut done = Vec::with_capacity(clients.len());
        for client in clients {
            done.push(client.join().map_err(|_| "a client panicked")??);
        }
        drop(stop);
        for service in services {
            service.join().map_err(|_| "a service panicked")??;
        }

        Ok(done)
    })
}

/// A service, on a thread of its own, with a connection and a single-threaded runtime of its own:
/// connects to the bus at `address`, at the well-known name `name` if there is one, says on `ready`
/// at which name to call it, or why it could not connect, and goes on replying to the calls of its
/// methods until `stopped` is closed.
fn serve(
    address: &str,
    name: Option<&str>,
    ready: mpsc::Sender<Result<BusName<'static>>>,
    stopped: mpmc::Receiver<()>,
) -> Result<()> {
    zruntime::block_on(async {
        let (connection, calls, destination) = match connect_service(address, name).await {
            Ok(service) => service,
            Err(e) => {
                // The thread that waits for the services gives up on the first that fails, with
                // its error.
                let _ = ready.send(Err(e));

                return Err("the service could not connect".into());
            }
        };
        let _ = ready.send(Ok(destination));
        // Dropped once it has said so, rather than kept while the service serves, so that the
        // channel closes once every service that did not panic has: the thread that waits for them
        // then learns that one panicked, rather than waiting for it for good.
        drop(ready);

        crate::calls::serve(&connection, calls, stopped).await
    })
}

/// Connects a service to the bus at `address`, at the well-known name `name` if there is one, and
/// makes the stream of its connection that it takes the calls off, and the name to call it at:
/// `name`, or else the unique name of its connection.
async fn connect_service(
    address: &str,
    name: Option<&str>,
) -> Result<(Connection, MessageStream, BusName<'static>)> {
    let (connection, calls) = service(address, name).await?;
    let destination = match name {
        Some(name) => BusName::try_from(name.to_owned())?,
        None => {
            let Some(unique_name) = connection.unique_name() else {
                return Err("the bus gave the service no unique name".into());
            };

            unique_name.clone().into()
        }
    };

    Ok((connection, calls, destination))
}

/// Connects `count` subscribers to the bus at `address`, each on a thread of its own, with a
/// connection and a single-threaded runtime of its own, and subscribes each to the signals that
/// `rule` matches, with room in the queue of its stream for `queue` of them; and returns their
/// threads once all of them have subscribed, and so before any signal is sent.
///
/// Each subscriber then hands its stream to `receive`, and its thread returns what that returns.
fn spawn_subscribers<R, F, T>(
    address: &str,
    rule: &MatchRule<'static>,
    queue: NonZeroUsize,
    count: usize,
    receive: R,
) -> Result<Vec<JoinHandle<Result<T>>>>
where
    R: FnOnce(MessageStream) -> F + Clone + Send + 'static,
    F: Future<Output = Result<T>>,
    T: Send + 'static,
{
    let (subscribed, subscriptions) = mpsc::channel();
    let subscribers: Vec<_> = (0..count)
        .map(|_| {
            let address = address.to_owned();
            let rule = rule.clone();
            let subscribed = subscribed.clone();
            let receive = receive.clone();
            thread::spawn(move || subscriber(&address, rule, queue, subscribed, receive))
        })
        .collect();
    drop(subscribed);
    // Each has subscribed once it says so: the bus has registered its match rule by then, as
    // subscribing waits for the bus to reply to `AddMatch`.
    for _ in 0..subscribers.len() {
        subscriptions
            .recv()
            .map_err(|_| "a subscriber ended before it subscribed")??;
    }

    Ok(subscribers)
}

/// A subscriber, on a thread of its own, with a connection and a single-threaded runtime of its
/// own: connects to the bus at `address`, subscribes to the signals that `rule` matches, says on
/// `subscribed` whether it did, and hands its stream to `receive`.
fn subscriber<R, F, T>(
    address: &str,
    rule: MatchRule<'static>,
    queue: NonZeroUsize,
    subscribed: mpsc::Sender<Result<()>>,
    receive: R,
) -> Result<T>
where
    R: FnOnce(MessageStream) -> F,
    F: Future<Output = Result<T>>,
{
    zruntime::block_on(async {
        let stream = match subscribe(address, rule, queue).await {
            Ok(stream) => stream,
            Err(e) => {
                // The thread that waits for the subscribers gives up on the first that fails, with
                // its error.
                let _ = subscribed.send(Err(e));

                return Err("the subscriber could not subscribe".into());
            }
        };
        let _ = subscribed.send(Ok(()));
        // Dropped once it has said so, as `serve` drops its sender, for the same reason.
        drop(subscribed);

        receive(stream).await
    })
}

/// Ends the process, with `TIMEOUT_EXIT_STATUS`, if it is not dropped within `PHASE_TIMEOUT`.
///
/// A bus that loses a message leaves a client or a subscriber waiting for it for good, and the main
/// thread waiting for the thread of that client or subscriber to end. No timer of a runtime could
/// cut that wait short, as it blocks the thread, but a thread of its own can. The watchdog says on
/// the standard error which phase it gave up on, as the standard output is for the lines of the
/// phases only.
struct Watchdog {
    /// Dropping this is what calls the watchdog off.
    _disarm: mpsc::Sender<()>,
}

impl Watchdog {
    /// Starts watching the phase `phase`.
    fn start(phase: &'static str) -> Self {
        let (disarm, disarmed) = mpsc::channel();
        thread::spawn(move || {
            if let Err(mpsc::RecvTimeoutError::Timeout) = disarmed.recv_timeout(PHASE_TIMEOUT) {
                eprintln!("the {phase} phase did not finish within {PHASE_TIMEOUT:?}");
                std::process::exit(TIMEOUT_EXIT_STATUS);
            }
        });

        Self { _disarm: disarm }
    }
}

/// How long a phase that has a `Watchdog` gets, connecting included, before the client gives up on
/// it.
const PHASE_TIMEOUT: Duration = Duration::from_secs(120);

/// The exit status of a client that gave up on a phase, as opposed to one that failed.
const TIMEOUT_EXIT_STATUS: i32 = 2;
