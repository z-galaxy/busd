//! Writing to the standard output and the standard error without panicking.
//!
//! `println!` and `eprintln!` panic when they cannot write: when the other end of a pipe is gone,
//! as `tee` is once Ctrl-C has stopped it, or the terminal is, as when SSH drops. Those are the
//! very interrupts that the comparison stops on and writes its results, so what it says along the
//! way goes through these instead, which leave out what cannot be written, and go on.

/// Writes a line to the standard error, as `eprintln!` does, but leaves it out if it cannot be
/// written.
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), $($arg)*);
    }};
}

pub(crate) use note;

/// Writes `text` to the standard output, as it is, but leaves it out if it cannot be written.
pub fn print(text: &str) {
    use std::io::Write as _;

    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
}
