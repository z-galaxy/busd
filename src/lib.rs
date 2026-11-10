//! The code of busd, as a library that the busd binary, its tests and its benchmarks share.
//!
//! This library is internal to busd: it's only published along with the binary, and isn't meant
//! for other crates. Its API can change in any release.

pub mod bus;
pub mod config;
pub mod fdo;
#[doc(hidden)]
pub mod log;
pub mod match_rules;
pub mod name_registry;
pub mod peer;
pub mod peers;
pub mod tracing_subscriber;
pub mod user;
