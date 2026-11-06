//! The protocols of systemd that let a service take its sockets from it, and tell it how it's
//! doing.
//!
//! A service that systemd starts gets the sockets of the socket units that activate it:
//! [`take_listen_fds`] takes those. A service tells the manager that it's ready, among other
//! things, with [`Notifier::notify`].
//!
//! These are the protocols of [`sd_listen_fds(3)`] and [`sd_notify(3)`], with nothing of the bus in
//! them. The manager only takes notifications from the processes that the `NotifyAccess=` of the
//! unit allows: the main process, unless the unit says otherwise.
//!
//! [`sd_listen_fds(3)`]: https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html
//! [`sd_notify(3)`]: https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html

use std::{
    env::{self, VarError},
    ffi::OsStr,
    io::{self, ErrorKind},
    os::{
        fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::OsStrExt,
    },
    process,
    str::FromStr,
};

use rustix::{
    io::{fcntl_setfd, FdFlags},
    net::{sendto, socket_with, AddressFamily, SendFlags, SocketAddrUnix, SocketFlags, SocketType},
};

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

/// The notification socket of the service manager, `$NOTIFY_SOCKET`.
///
/// A service sends it its state, such as `READY=1`. The manager takes the messages one at a time,
/// in the order they came, and ties each to a service by the process that sent it, so they have to
/// come from a process that the `NotifyAccess=` of the service lets.
///
/// A send blocks while the queue of the socket of the manager is full, which only happens if the
/// manager is far behind, but it doesn't wait for the manager to read the message. The manager
/// says nothing back.
#[derive(Debug)]
pub struct Notifier {
    socket: OwnedFd,
    address: SocketAddrUnix,
}

impl Notifier {
    /// The one that `$NOTIFY_SOCKET` names, if it's set and not empty.
    ///
    /// Systemd sets it for the services that take notifications, so `None` means that this isn't
    /// one, or isn't a service of systemd at all. It leaves the variable in the environment.
    pub fn from_env() -> io::Result<Option<Self>> {
        match env::var_os("NOTIFY_SOCKET") {
            Some(address) if !address.is_empty() => Self::new(address).map(Some),
            _ => Ok(None),
        }
    }

    /// The one at `address`, in the form of `$NOTIFY_SOCKET`.
    ///
    /// That is the path of a socket if `address` starts with a `/`, and the name of an abstract
    /// socket after an `@` if it starts with that. A `/` or an `@` alone names nothing, which
    /// fails with [`ErrorKind::InvalidInput`]. Systemd also has `vsock:` addresses, by which a
    /// virtual machine tells its host that it has booted: they fail with
    /// [`ErrorKind::Unsupported`], as does anything else that is neither of the two.
    pub fn new(address: impl AsRef<OsStr>) -> io::Result<Self> {
        let address = match address.as_ref().as_bytes() {
            [b'/' | b'@'] => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "the notification socket has no name",
                ))
            }
            path @ [b'/', ..] => SocketAddrUnix::new(path)?,
            [b'@', name @ ..] => SocketAddrUnix::new_abstract_name(name)?,
            other => {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "the notification socket `{}` is neither a path nor an abstract name",
                        String::from_utf8_lossy(other),
                    ),
                ))
            }
        };
        let socket = socket_with(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            None,
        )?;

        Ok(Self { socket, address })
    }

    /// Send `state`.
    ///
    /// `state` is a list of assignments such as `READY=1`, each on a line of its own. The manager
    /// takes a message of 4096 bytes at most, and ignores a longer one, as well as one that it
    /// can't make sense of, so this only fails if the message can't be sent.
    pub fn notify(&self, state: &str) -> io::Result<()> {
        sendto(
            &self.socket,
            state.as_bytes(),
            SendFlags::NOSIGNAL,
            &self.address,
        )?;

        Ok(())
    }
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
    use std::{
        env::temp_dir,
        fs::remove_file,
        io::IoSliceMut,
        os::fd::AsRawFd,
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };

    use rand::{
        distr::{Alphanumeric, SampleString},
        rng,
    };
    use rustix::{
        io::{fcntl_getfd, Errno},
        net::{
            bind, recvmsg,
            sockopt::{set_socket_timeout, Timeout},
            RecvAncillaryBuffer, RecvFlags, ReturnFlags,
        },
    };

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
    fn notify() {
        for (manager, notifier) in managers() {
            notifier.notify("READY=1\nSTATUS=Up").unwrap();

            let state = manager.receive();
            assert_eq!(state, "READY=1\nSTATUS=Up");
        }
    }

    #[test]
    fn notify_without_a_manager() {
        let notifier = Notifier::new(unique_path()).unwrap();

        let error = notifier.notify("READY=1").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn notifier_addresses() {
        // The socket of the notifier is not connected, so these don't need a manager.
        Notifier::new("/run/systemd/notify").unwrap();
        Notifier::new("@/org/freedesktop/systemd1/notify").unwrap();
        // The longest names that fit a socket address: 108 bytes for a path, 107 for an abstract
        // name.
        Notifier::new(format!("/{}", "a".repeat(107))).unwrap();
        Notifier::new(format!("@{}", "a".repeat(107))).unwrap();

        for address in ["/", "@"] {
            let error = Notifier::new(address).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput, "{address:?}");
        }
        for address in [
            "",
            "x",
            "relative/path",
            "unix:/run/notify",
            "vsock:2:1234",
            "vsock-stream:2:1234",
        ] {
            let error = Notifier::new(address).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported, "{address:?}");
        }
        for address in [
            format!("/{}", "a".repeat(108)),
            format!("@{}", "a".repeat(108)),
        ] {
            let error = Notifier::new(&address).unwrap_err();
            assert_eq!(
                error.raw_os_error(),
                Some(Errno::NAMETOOLONG.raw_os_error()),
                "{address:?}"
            );
        }
    }

    #[test]
    fn notifier_from_the_environment() {
        let exports = "NOTIFY_SOCKET=/run/systemd/notify";
        if !in_own_process("notifier_from_the_environment", exports) {
            return;
        }

        assert!(Notifier::from_env().unwrap().is_some());
        // The variable stays, for the processes that this one starts to inherit.
        assert!(env::var_os("NOTIFY_SOCKET").is_some());

        env::set_var("NOTIFY_SOCKET", "");
        assert!(Notifier::from_env().unwrap().is_none());
        env::remove_var("NOTIFY_SOCKET");
        assert!(Notifier::from_env().unwrap().is_none());

        env::set_var("NOTIFY_SOCKET", "vsock:2:1234");
        let error = Notifier::from_env().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unsupported);
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

    /// A manager at a socket file, and another at an abstract socket, with a notifier for each.
    fn managers() -> [(Manager, Notifier); 2] {
        [Manager::at_path(), Manager::at_abstract_name()]
    }

    /// A socket that a notifier sends to: the notification socket of the manager.
    struct Manager {
        socket: OwnedFd,
        /// The file of the socket, if it's not an abstract one.
        path: Option<PathBuf>,
    }

    impl Manager {
        fn at_path() -> (Self, Notifier) {
            let path = unique_path();
            let address = SocketAddrUnix::new(&path).unwrap();
            let manager = Self::bind(&address, Some(path.clone()));

            (manager, Notifier::new(&path).unwrap())
        }

        fn at_abstract_name() -> (Self, Notifier) {
            let name = Alphanumeric.sample_string(&mut rng(), 10);
            let address = SocketAddrUnix::new_abstract_name(name.as_bytes()).unwrap();
            let manager = Self::bind(&address, None);

            (manager, Notifier::new(format!("@{name}")).unwrap())
        }

        fn bind(address: &SocketAddrUnix, path: Option<PathBuf>) -> Self {
            let socket = socket_with(
                AddressFamily::UNIX,
                SocketType::DGRAM,
                SocketFlags::CLOEXEC,
                None,
            )
            .unwrap();
            bind(&socket, address).unwrap();
            // Fail, rather than wait for ever, for a message that never comes.
            set_socket_timeout(&socket, Timeout::Recv, Some(Duration::from_secs(10))).unwrap();

            Self { socket, path }
        }

        /// The text of the next message.
        fn receive(&self) -> String {
            let mut text = [0; 8192];
            let message = recvmsg(
                &self.socket,
                &mut [IoSliceMut::new(&mut text)],
                &mut RecvAncillaryBuffer::new(&mut []),
                RecvFlags::empty(),
            )
            .unwrap();
            assert!(!message.flags.contains(ReturnFlags::TRUNC));

            String::from_utf8(text[..message.bytes].to_vec()).unwrap()
        }
    }

    impl Drop for Manager {
        fn drop(&mut self) {
            if let Some(path) = &self.path {
                let _ = remove_file(path);
            }
        }
    }

    fn unique_path() -> PathBuf {
        temp_dir().join(Alphanumeric.sample_string(&mut rng(), 10))
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
