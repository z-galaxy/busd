//! The `shared` workload: many callers that share a connection, then as many clients.
//!
//! It runs in two phases, and prints the line of each as it ends, with how long it took, in
//! seconds, as `single_s` or `multi_s`:
//!
//! 1. `single`: one client connection that `CLIENTS` callers share, as many tasks of one program
//!    would, each making `CALLS` calls, with `PAYLOAD_LEN` bytes of payload, through the bus, to
//!    the method that one service serves. The phase times the calls, from just before the callers
//!    start, once the service and the client are connected, until the last of them is done.
//! 2. `multi`: as many client connections, each calling the method of a service of its own `CALLS`
//!    times, as those of `calls` do. The phase times what those of `calls` do: from just before its
//!    first service connects until its last service is gone, connecting included.
//!
//! Each service, and each client of the second phase, runs on a thread of its own, with a
//! connection and a single-threaded runtime of its own, as a process of its own would, so that
//! neither holds the other up. The callers of the first phase run on the thread that runs the
//! workload, and `crate::shared` says how the replies to their calls get to them. A service has no
//! well-known name: its clients call it at the unique name of its connection.
//!
//! Each service and each client of the second phase has five files open, its socket and those of
//! its runtime, so the phase needs room for about ten for each pair within the limit on open files
//! (`ulimit -n`), which is often 1024: the client raises the limit as far as it can first.

use std::{
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use futures_util::future::try_join_all;
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
use zbus::{connection, names::BusName};
use zruntime::mpmc;

use super::{calls::hammer, print_elapsed, serve};
use crate::{calls::Result, shared::SharedConnection};

/// Runs the client of `shared` against the bus at `address`.
pub(super) fn run(address: &str) -> Result<()> {
    let data = vec![0u8; PAYLOAD_LEN];

    let elapsed = shared(address, &data)?;
    print_elapsed("single", elapsed)?;

    raise_open_files_limit();
    let names = vec![None; CLIENTS];
    let before = Instant::now();
    hammer(address, &names, CALLS, data.as_slice())?;
    print_elapsed("multi", before.elapsed())?;

    Ok(())
}

/// Runs a service, and on this thread, a client connection that `CLIENTS` callers share, each
/// calling the service `CALLS` times with `data` as the byte array, and returns how long the calls
/// took.
///
/// That is from just before the callers start, once the service and the client are connected,
/// until the last of them is done.
fn shared(address: &str, data: &[u8]) -> Result<Duration> {
    thread::scope(|scope| {
        // Nothing is sent on this channel: dropping the sender, whichever way the scope is left,
        // closes it, which is what stops the service.
        let (stop, stopped) = mpmc::unbounded::<()>();
        let (ready, readiness) = mpsc::channel();
        let service = scope.spawn(move || serve(address, None, ready, stopped));
        let destination = readiness
            .recv()
            .map_err(|_| "the service ended before it was ready")??;

        let elapsed = zruntime::block_on(share(address, destination, data))?;
        drop(stop);
        service.join().map_err(|_| "the service panicked")??;

        Ok(elapsed)
    })
}

/// Connects a client to the bus at `address`, and has `CLIENTS` callers on its connection call the
/// service at `destination` at once, `CALLS` times each, with `data` as the byte array, and returns
/// how long the calls took.
///
/// That is from just before the callers start until the last of them is done.
async fn share(address: &str, destination: BusName<'static>, data: &[u8]) -> Result<Duration> {
    let connection = connection::Builder::address(address).build().await?;
    let connection = SharedConnection::new(connection, destination, CLIENTS);
    let before = Instant::now();
    try_join_all((0..CLIENTS).map(|_| caller(&connection, data))).await?;

    Ok(before.elapsed())
}

/// One of the callers that share `connection`: calls its service `CALLS` times, one call after the
/// other, with `data` as the byte array.
async fn caller(connection: &SharedConnection, data: &[u8]) -> Result<()> {
    for _ in 0..CALLS {
        connection.call(data).await?;
    }

    Ok(())
}

/// Raises the soft limit on the files that the process can have open to the hard limit, for the
/// pairs of the second phase: they have about 1000 open, ten for each, which leaves next to no room
/// under the soft limit of 1024 that many systems set.
///
/// If it cannot, it says so on the standard error, and leaves the limit as it is: the phase then
/// fails if the limit is too low for it.
fn raise_open_files_limit() {
    let limit = getrlimit(Resource::Nofile);
    let raised = Rlimit {
        current: limit.maximum,
        maximum: limit.maximum,
    };
    if let Err(e) = setrlimit(Resource::Nofile, raised) {
        let show = |limit: Option<u64>| limit.map_or("unlimited".to_owned(), |n| n.to_string());
        let (current, maximum) = (show(limit.current), show(limit.maximum));
        eprintln!("could not raise the limit on open files from {current} to {maximum}: {e}");
    }
}

/// How many callers share the connection of the first phase, and how many services and clients the
/// second one runs.
const CLIENTS: usize = 100;

/// How many calls each caller and each client makes.
const CALLS: usize = 1000;

/// How many bytes of payload each call carries.
const PAYLOAD_LEN: usize = 16 * 1024;
