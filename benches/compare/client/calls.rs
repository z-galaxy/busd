//! The `calls` and `dicts` workloads: method calls through the bus.
//!
//! Each runs in two phases, and prints the line of each as it ends, with how long it took, in
//! seconds, as `single_s` or `multi_s`:
//!
//! 1. `single`: a service, at a well-known name of its own, and a client that calls it through that
//!    name `CALLS` times, one call after the other, so that each call and its reply go through the
//!    bus.
//! 2. `multi`: `PAIRS` such services and clients at once.
//!
//! The calls of `calls` carry a byte array of `PAYLOAD_LEN` bytes, and those of `dicts` the dict of
//! dicts of variants that `crate::dicts` makes, `a{sa{sv}}`, which costs a bus far more to check
//! against its signature: that is what `dicts` measures, and `crate::dicts` says why it costs more.
//! The service replies without reading either, so that the timings are the bus's as far as they
//! can be.
//!
//! Each service and each client runs on a thread of its own, with a connection and a
//! single-threaded runtime of its own, as a process of its own would, so that neither holds the
//! other up: a client sends its next call as soon as the reply to the last one comes, and a
//! service replies as soon as a call comes. What a phase times is from just before its first
//! service connects until its last service is gone, connecting included.

use std::time::Instant;

use zbus::names::BusName;

use super::{print_elapsed, run_pairs};
use crate::{
    calls::{Caller, Result, DICTS_METHOD},
    dicts::{dicts, Dicts},
};

/// Runs the client of `calls` against the bus at `address`.
pub(super) fn run_calls(address: &str) -> Result<()> {
    let data = vec![0u8; PAYLOAD_LEN];

    run_phases(address, "org.dbus2.benchmark", data.as_slice())
}

/// Runs the client of `dicts` against the bus at `address`.
pub(super) fn run_dicts(address: &str) -> Result<()> {
    run_phases(address, "org.dbus2.benchmark.dicts", &dicts())
}

/// Runs the two phases of `calls` or `dicts`, with `body` for the body of every call, and services
/// at well-known names that start with `prefix`.
fn run_phases<B>(address: &str, prefix: &str, body: &B) -> Result<()>
where
    B: Body + ?Sized,
{
    let names = [Some(format!("{prefix}.SingleClient"))];
    let before = Instant::now();
    hammer(address, &names, CALLS, body)?;
    print_elapsed("single", before.elapsed())?;

    let names: Vec<_> = (0..PAIRS)
        .map(|i| Some(format!("{prefix}.MultiClient{i}")))
        .collect();
    let before = Instant::now();
    hammer(address, &names, CALLS, body)?;
    print_elapsed("multi", before.elapsed())?;

    Ok(())
}

/// Runs a service at each of `names`, and a client of each that calls it `calls` times, one call
/// after the other, with `body`, and returns once all the clients are done and all the services
/// are gone.
///
/// A service at `None` has no well-known name, and is called at the unique name of its connection.
/// The clients start once all the services can be called, and the services stop once the clients
/// are done, or one of them failed.
pub(super) fn hammer<B>(
    address: &str,
    names: &[Option<String>],
    calls: usize,
    body: &B,
) -> Result<()>
where
    B: Body + ?Sized,
{
    run_pairs(address, names, |destination| {
        client(address, destination, calls, body)
    })?;

    Ok(())
}

/// A client, on a thread of its own, with a connection and a single-threaded runtime of its own:
/// connects to the bus at `address`, and calls the service at `destination` `calls` times, one call
/// after the other, with `body`.
fn client<B>(address: &str, destination: BusName<'static>, calls: usize, body: &B) -> Result<()>
where
    B: Body + ?Sized,
{
    zruntime::block_on(async {
        let mut caller = Caller::connect(address, destination).await?;
        for _ in 0..calls {
            body.call(&mut caller).await?;
        }

        Ok(())
    })
}

/// The body of the calls of a workload, which says which method they call.
///
/// It is `Sync`, as the clients of a phase, each on a thread of its own, share it.
pub(super) trait Body: Sync {
    /// Calls the service of `caller` with this for the body, and waits for the reply.
    async fn call(&self, caller: &mut Caller) -> Result<()>;
}

/// The byte array of the calls of `calls` and `shared`, to `crate::calls::METHOD`.
impl Body for [u8] {
    async fn call(&self, caller: &mut Caller) -> Result<()> {
        caller.call(self).await
    }
}

/// The dicts of the calls of `dicts`, to `DICTS_METHOD`.
impl Body for Dicts {
    async fn call(&self, caller: &mut Caller) -> Result<()> {
        caller.call_with(DICTS_METHOD, self).await
    }
}

/// How many calls each client makes.
const CALLS: usize = 1000;

/// How many services and clients the second phase runs at once.
const PAIRS: usize = 20;

/// How many bytes of payload each call of `calls` carries.
const PAYLOAD_LEN: usize = 16 * 1024;
