//! The protocol of systemd that lets a service take its sockets from it.
//!
//! A service that systemd starts gets the sockets of the socket units that activate it:
//! [`take_listen_fds`] takes those. This is the protocol of [`sd_listen_fds(3)`], with nothing of
//! the bus in it.
//!
//! [`sd_listen_fds(3)`]: https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html

use std::{
    env::{self, VarError},
    io::{self, ErrorKind},
    os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd},
    process,
    str::FromStr,
};

use rustix::io::{fcntl_setfd, FdFlags};

/// The file descriptors that systemd passed to this process.
///
/// Systemd passes a service the sockets of the socket units that activate it.
///
/// It gives no file descriptors if systemd passed none to this process: if the environment doesn't
/// tell of any, or tells of those for another process. It fails if what the environment tells of
/// them makes no sense, and closes those that it took by then. The file descriptors it gives are
/// close-on-exec.
///
/// Whether it succeeds or not, it removes `$LISTEN_PID`, `$LISTEN_FDS` and `$LISTEN_FDNAMES` from
/// the environment, as `sd_listen_fds(3)` does when asked to, and `$LISTEN_PIDFDID` as well, which
/// systemd 259 and later add, and which it doesn't check. So no call but the first takes the file
/// descriptors, and the processes that this one starts don't inherit what tells of them.
///
/// That changes the environment, which isn't safe while other threads may read it: call this before
/// the process starts any other thread.
pub fn take_listen_fds() -> io::Result<Vec<OwnedFd>> {
    // Read all the variables before any of them can end this early, as all of them go either way.
    let pid = var("LISTEN_PID");
    let fds = var("LISTEN_FDS");
    for name in [
        "LISTEN_PID",
        "LISTEN_PIDFDID",
        "LISTEN_FDS",
        "LISTEN_FDNAMES",
    ] {
        env::remove_var(name);
    }

    let Some(count) = parse_count(pid?.as_deref(), fds?.as_deref(), process::id())? else {
        return Ok(Vec::new());
    };

    take_fds(count)
}

/// The value of the environment variable `name`, if it is set.
fn var(name: &str) -> io::Result<Option<String>> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("${name} isn't valid Unicode"),
        )),
    }
}

/// How many file descriptors systemd passed to the process with PID `own_pid`, going by
/// `$LISTEN_PID` and `$LISTEN_FDS`: `None` if it passed none, because they are unset, or
/// `$LISTEN_PID` is another PID.
///
/// As `sd_listen_fds(3)` does, it takes `$LISTEN_FDS` for garbage if it isn't a number from 1 to
/// [`MAX_LISTEN_FDS`], but only looks at it if the PID is right.
fn parse_count(pid: Option<&str>, fds: Option<&str>, own_pid: u32) -> io::Result<Option<usize>> {
    let Some(pid) = pid else {
        return Ok(None);
    };
    if parse_number::<u32>("$LISTEN_PID", pid)? != own_pid {
        return Ok(None);
    }
    let Some(fds) = fds else {
        return Ok(None);
    };
    match parse_number("$LISTEN_FDS", fds)? {
        count @ 1..=MAX_LISTEN_FDS => Ok(Some(count)),
        count => Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("$LISTEN_FDS is {count}, not a number from 1 to {MAX_LISTEN_FDS}"),
        )),
    }
}

/// Take the `count` file descriptors from [`START`] on, and make them close-on-exec.
///
/// Each is found to be open before it's taken: `$LISTEN_FDS` may tell of more than there are, and
/// an `OwnedFd` of one that isn't open aborts the process when it drops, in a debug build.
fn take_fds(count: usize) -> io::Result<Vec<OwnedFd>> {
    (START..)
        .take(count)
        .map(|fd| -> io::Result<OwnedFd> {
            // SAFETY: It's only borrowed for the call below, which fails with `EBADF` if it isn't
            // open, and does nothing else to it.
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            fcntl_setfd(borrowed, FdFlags::CLOEXEC)?;
            // SAFETY: It's open, as the call above shows. Systemd passed it to this process, as
            // `$LISTEN_PID` says, and nothing else owns it: this is the only place that takes the
            // file descriptors, and the variables that tell of them are gone after the first call.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        })
        .collect()
}

/// The number that `value` is, for the environment variable `name`.
fn parse_number<T: FromStr>(name: &str, value: &str) -> io::Result<T>
where
    T::Err: std::error::Error,
{
    value.parse().map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("{name} is {value:?}, not a number: {e}"),
        )
    })
}

/// The largest count that `$LISTEN_FDS` can have: `sd_listen_fds(3)` takes a larger one for garbage
/// too, as the number of the last file descriptor would overflow.
const MAX_LISTEN_FDS: usize = (RawFd::MAX - START) as usize;

/// The first file descriptor that systemd passes, `SD_LISTEN_FDS_START`.
const START: RawFd = 3;

#[cfg(test)]
mod tests {
    use std::{os::fd::AsRawFd, path::Path, process::Command};

    use rustix::io::{fcntl_getfd, Errno};

    use super::*;

    #[test]
    fn count() {
        assert_eq!(parse_count(Some("42"), Some("3"), 42).unwrap(), Some(3));
        assert_eq!(parse_count(Some("42"), Some("1"), 42).unwrap(), Some(1));
        assert_eq!(
            parse_count(Some("42"), Some(&MAX_LISTEN_FDS.to_string()), 42).unwrap(),
            Some(MAX_LISTEN_FDS)
        );
    }

    #[test]
    fn count_is_none_unless_for_this_process() {
        // Nothing is passed if either variable is unset, or the PID is another one. The count isn't
        // even looked at then, so it can't make anything fail.
        assert_eq!(parse_count(None, Some("3"), 42).unwrap(), None);
        assert_eq!(parse_count(Some("42"), None, 42).unwrap(), None);
        assert_eq!(parse_count(None, None, 42).unwrap(), None);
        assert_eq!(parse_count(Some("41"), Some("3"), 42).unwrap(), None);
        assert_eq!(parse_count(Some("43"), Some("3"), 42).unwrap(), None);
        assert_eq!(parse_count(Some("41"), Some("garbage"), 42).unwrap(), None);
    }

    #[test]
    fn count_garbage() {
        for pid in ["", "x", "-1", " 42", "42 ", "4.2", "4294967296"] {
            let error = parse_count(Some(pid), Some("3"), 42).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData, "$LISTEN_PID={pid:?}");
        }

        let too_many = (MAX_LISTEN_FDS + 1).to_string();
        for fds in [
            "",
            "x",
            "-1",
            "0",
            "3 ",
            "1.5",
            "99999999999999999999",
            &too_many,
        ] {
            let error = parse_count(Some("42"), Some(fds), 42).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidData, "$LISTEN_FDS={fds:?}");
        }
    }

    #[test]
    fn take_listen_fds_from_the_environment() {
        let exports = "LISTEN_PID=$$ LISTEN_FDS=2 LISTEN_FDNAMES=first:second LISTEN_PIDFDID=1";
        if !in_own_process("take_listen_fds_from_the_environment", exports) {
            return;
        }

        let fds = take_listen_fds().unwrap();
        let numbers = fds.iter().map(|fd| fd.as_raw_fd()).collect::<Vec<_>>();
        assert_eq!(numbers, [3, 4]);
        for fd in &fds {
            assert!(fcntl_getfd(fd).unwrap().contains(FdFlags::CLOEXEC));
        }
        assert_listen_variables_removed();
        // There is nothing left to take.
        assert!(take_listen_fds().unwrap().is_empty());
    }

    #[test]
    fn take_listen_fds_for_another_process() {
        let exports = "LISTEN_PID=1 LISTEN_FDS=2 LISTEN_FDNAMES=a:b LISTEN_PIDFDID=1";
        if !in_own_process("take_listen_fds_for_another_process", exports) {
            return;
        }

        assert!(take_listen_fds().unwrap().is_empty());
        assert_listen_variables_removed();
        // The file descriptors are not for this process, which leaves them as they are.
        assert!(is_open(3) && is_open(4));
    }

    #[test]
    fn take_listen_fds_that_are_not_open() {
        // Only the file descriptors 3 and 4 are open.
        let exports = "LISTEN_PID=$$ LISTEN_FDS=3";
        if !in_own_process("take_listen_fds_that_are_not_open", exports) {
            return;
        }

        let error = take_listen_fds().unwrap_err();
        assert_eq!(error.raw_os_error(), Some(Errno::BADF.raw_os_error()));
        assert!(!is_open(3) && !is_open(4));
        assert_listen_variables_removed();
    }

    /// Whether this is the process that the test `test` runs itself in.
    ///
    /// `take_listen_fds` takes file descriptors and changes the environment, which no test can do
    /// among the threads of the others, so each test of it runs in a process of its own. A shell
    /// makes that process the way that systemd makes a service: it opens the file descriptors 3
    /// and 4, and exports `exports`, in which `$$` is the PID of the process, which `exec` keeps.
    /// It closes 5, which a test needs closed, as the process may inherit any descriptor that
    /// another test has open at the time and that isn't close-on-exec.
    ///
    /// In that process, this is true, and the test goes on. Anywhere else, it runs the test there,
    /// and checks that it passed, and the test is done.
    fn in_own_process(test: &str, exports: &str) -> bool {
        if env::var_os(OWN_PROCESS).is_some() {
            return true;
        }

        let name = format!("systemd::tests::{test}");
        let script = format!(
            r#"export {exports}; exec "$0" --exact "$1" --nocapture 3</dev/null 4</dev/null 5<&-"#
        );
        let output = Command::new("sh")
            .args(["-c", &script])
            .arg(env::current_exe().unwrap())
            .arg(&name)
            .env(OWN_PROCESS, "1")
            .output()
            .unwrap();
        // A process that finds no test of that name succeeds all the same.
        let passed =
            String::from_utf8_lossy(&output.stdout).contains(&format!("test {name} ... ok"));
        assert!(output.status.success() && passed, "{output:?}");

        false
    }

    fn is_open(fd: RawFd) -> bool {
        Path::new(&format!("/proc/self/fd/{fd}")).exists()
    }

    fn assert_listen_variables_removed() {
        for var in [
            "LISTEN_PID",
            "LISTEN_PIDFDID",
            "LISTEN_FDS",
            "LISTEN_FDNAMES",
        ] {
            assert!(env::var_os(var).is_none(), "${var} is still set");
        }
    }

    const OWN_PROCESS: &str = "BUSD_TEST_OWN_PROCESS";
}
