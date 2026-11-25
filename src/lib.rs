//! The code of busd, as a library that the busd binary, its tests and its benchmarks share.
//!
//! This library is internal to busd: it's only published along with the binary, and isn't meant
//! for other crates. Its API can change in any release.

// The bus keeps its state in `RefCell`s, which its tasks borrow in turn, on the one thread that
// runs them all. If a borrow is held across an `await`, a task that borrows the same cell
// meanwhile panics, unless both borrows only read it.
#![deny(clippy::await_holding_refcell_ref)]

pub mod bus;
pub mod config;
pub mod connection;
pub mod fdo;
#[doc(hidden)]
pub mod log;
pub mod match_rules;
pub mod name_registry;
pub mod peer;
pub mod peers;
pub mod policy;
mod runtime;
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub mod testing;
pub mod tracing_subscriber;
pub mod user;
