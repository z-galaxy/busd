//! Logging macros that forward to [`tracing`] when the `tracing` feature is enabled, and compile
//! to nothing otherwise.
//!
//! These are exported at the crate root (e.g. `busd::error!`) so both the library and the `busd`
//! binary can use them without a hard dependency on `tracing`.

#[doc(hidden)]
#[cfg(feature = "tracing")]
pub use tracing;

// The no-op shims type-check their arguments in a branch that is never taken, the way the `log`
// crate does when a level is compiled out. Nothing is evaluated at runtime, but the bindings a
// call site only uses in its log message do not become unused, so the no-`tracing` build stays
// free of warnings without any `#[allow]` or `_`-prefixed names.
#[doc(hidden)]
#[macro_export]
#[cfg(not(feature = "tracing"))]
macro_rules! __log_event {
    ($fmt:literal $(, $arg:expr)* $(,)?) => {
        if false {
            let _ = ::std::format_args!($fmt $(, $arg)*);
        }
    };
}

#[doc(hidden)]
#[macro_export]
#[cfg(feature = "tracing")]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::tracing::error!($($arg)*) };
}
#[doc(hidden)]
#[macro_export]
#[cfg(not(feature = "tracing"))]
macro_rules! error {
    ($($arg:tt)*) => { $crate::__log_event!($($arg)*) };
}

#[doc(hidden)]
#[macro_export]
#[cfg(feature = "tracing")]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::tracing::warn!($($arg)*) };
}
#[doc(hidden)]
#[macro_export]
#[cfg(not(feature = "tracing"))]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::__log_event!($($arg)*) };
}

#[doc(hidden)]
#[macro_export]
#[cfg(feature = "tracing")]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::tracing::info!($($arg)*) };
}
#[doc(hidden)]
#[macro_export]
#[cfg(not(feature = "tracing"))]
macro_rules! info {
    ($($arg:tt)*) => { $crate::__log_event!($($arg)*) };
}

#[doc(hidden)]
#[macro_export]
#[cfg(feature = "tracing")]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::tracing::debug!($($arg)*) };
}
#[doc(hidden)]
#[macro_export]
#[cfg(not(feature = "tracing"))]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::__log_event!($($arg)*) };
}

#[doc(hidden)]
#[macro_export]
#[cfg(feature = "tracing")]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log::tracing::trace!($($arg)*) };
}
#[doc(hidden)]
#[macro_export]
#[cfg(not(feature = "tracing"))]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::__log_event!($($arg)*) };
}
