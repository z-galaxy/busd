//! The runtime that the bus runs on: a zruntime `LocalRuntime`.
//!
//! The thread that runs the bus drives the runtime, and the runtime runs the bus's tasks, timers
//! and sockets.

use std::io;

use zruntime::LocalRuntime;

/// The `LocalRuntime` that the calling thread runs, for the bus to run on.
///
/// # Errors
///
/// Fails if the calling thread runs no `LocalRuntime`. The bus is created and run from inside
/// `LocalRuntime::block_on`, or from a task of that runtime.
pub(crate) fn local() -> io::Result<LocalRuntime> {
    LocalRuntime::running().ok_or_else(|| {
        io::Error::other("the bus runs on a `LocalRuntime`, and this thread runs none")
    })
}
