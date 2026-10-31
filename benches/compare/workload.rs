//! The workloads that the comparison runs against each broker.

use clap::ValueEnum;

/// A workload of the comparison: what one run of the client does against one broker.
///
/// Each is a client of [dbus-bench], with the same connections, messages and timings, on busd's
/// own locked zbus and zruntime.
///
/// [dbus-bench]: https://github.com/zeenix/dbus-bench
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Workload {
    /// Method calls with 16 KiB of data each: 1000 by one client to its service, then 1000 by each
    /// of 20 clients to a service of its own, all at once.
    Calls,
    /// Signals with 1 KiB of data each: a burst of 40,000 to one subscriber, then a broadcast of
    /// 6000 to each of 20 subscribers.
    Signals,
    /// The latencies of method calls, by one client and by 20 at once, and of the signals of a
    /// broadcast to 20 subscribers.
    Latency,
    /// 100 callers that share a connection to call one service, then 100 clients, each calling a
    /// service of its own.
    Shared,
    /// The method calls of `Calls`, with a dict of dicts of 100 variants for their body.
    Dicts,
}

impl Workload {
    /// Every workload, in the order that the comparison runs them by default.
    pub const ALL: [Self; 5] = [
        Self::Calls,
        Self::Signals,
        Self::Latency,
        Self::Shared,
        Self::Dicts,
    ];

    /// The name of the workload, as the command line and the files of the results have it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::Signals => "signals",
            Self::Latency => "latency",
            Self::Shared => "shared",
            Self::Dicts => "dicts",
        }
    }
}
