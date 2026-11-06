//! The protocols of systemd that let a service take its sockets from it, tell it how it's doing,
//! and keep its file descriptors across restarts.
//!
//! A service that systemd starts gets the sockets of the socket units that activate it, and the
//! file descriptors that it stored in the manager before it was restarted: [`take_listen_fds`]
//! takes those. A service tells the manager that it's ready, among other things, with
//! [`Notifier::notify`]. A service stores a file descriptor with [`Notifier::store`], and the
//! manager keeps it open until the service is stopped, however often the service restarts
//! meanwhile. That is how a service can restart without dropping its clients: the manager holds on
//! to their connections.
//!
//! These are the protocols of [`sd_listen_fds(3)`], [`sd_notify(3)`] and the [file descriptor
//! store], with nothing of the bus in them: what to store, and when, is for the caller to decide.
//! The unit of the service needs `FileDescriptorStoreMax=` for the manager to store anything, and
//! the manager only takes notifications from the processes that the `NotifyAccess=` of the unit
//! allows: the main process, unless the unit says otherwise.
//!
//! [`sd_listen_fds(3)`]: https://www.freedesktop.org/software/systemd/man/latest/sd_listen_fds.html
//! [`sd_notify(3)`]: https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html
//! [file descriptor store]: https://systemd.io/FILE_DESCRIPTOR_STORE/

use std::{
    env::{self, VarError},
    ffi::OsStr,
    fs::File,
    io::{self, ErrorKind, IoSlice},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, fs::FileExt},
    },
    process,
    str::FromStr,
    time::{Duration, Instant},
};

use rustix::{
    buffer::spare_capacity,
    cmsg_space,
    event::{poll, PollFd, PollFlags, Timespec},
    fs::{fcntl_add_seals, memfd_create, MemfdFlags, SealFlags},
    io::{fcntl_setfd, pread, retry_on_intr, Errno, FdFlags},
    net::{
        sendmsg_addr, socket_with, AddressFamily, SendAncillaryBuffer, SendAncillaryMessage,
        SendFlags, SocketAddrUnix, SocketFlags, SocketType,
    },
    pipe::{pipe_with, PipeFlags},
    process::{getpid, Pid},
};

/// The file descriptors that systemd passed to this process, with their names.
///
/// Systemd passes a service the sockets of the socket units that activate it, and the file
/// descriptors that the service stored in the manager (see [`Notifier::store`]), which come last.
/// The name of a socket is the `FileDescriptorName=` of its unit, by default the name of the unit
/// (`connection` if the unit has `Accept=yes`). That of a stored file descriptor is the name it was
/// stored under, or `stored` if it was stored with none, and `unknown` stands for a name that
/// systemd didn't send. The names aren't unique, and the file descriptors come in no set order but
/// for the stored ones coming last, so tell them apart by their names rather than their places.
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
pub fn take_listen_fds() -> io::Result<Vec<(String, OwnedFd)>> {
    // Read all the variables before any of them can end this early, as all of them go either way.
    let pid = var("LISTEN_PID");
    let fds = var("LISTEN_FDS");
    let names = var("LISTEN_FDNAMES");
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
    // Once taken, the file descriptors close again if the names turn out to make no sense.
    let fds = take_fds(count)?;
    let names = parse_names(names?.as_deref(), count)?;

    Ok(names.into_iter().zip(fds).collect())
}

/// The notification socket of the service manager, `$NOTIFY_SOCKET`.
///
/// A service sends it its state, such as `READY=1`, and the file descriptors that it wants the
/// manager to keep for it. The manager takes the messages one at a time, in the order they came,
/// and ties each to a service by the process that sent it, so they have to come from a process that
/// the `NotifyAccess=` of the service lets.
///
/// A send blocks while the queue of the socket of the manager is full, which only happens if the
/// manager is far behind, but it doesn't wait for the manager to read the message. The manager
/// says nothing back, so [`barrier`](Self::barrier) is how to tell that it got to the messages.
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

    /// Store `fds` in the fdstore under `name`.
    ///
    /// The manager keeps a duplicate of each of them for the service, and hands it to the main
    /// process of the next instance of the service, along with `name` ([`take_listen_fds`]). It
    /// keeps them while the service restarts, and closes them when the service is stopped for good
    /// (`FileDescriptorStorePreserve=` changes that), or when they are removed
    /// ([`remove`](Self::remove)). It doesn't watch them for hang-ups (`FDPOLL=0`, from systemd 246
    /// on): by default, the manager closes a stored file descriptor once it sees a hang-up or an
    /// error on it, which, for a socket whose peer hangs up, would drop what the peer sent before
    /// that, for the next instance to read. It takes no more of them than the
    /// `FileDescriptorStoreMax=` of the unit
    /// ([`fd_store_max`]) at a time, and closes those that don't fit, as well as those that it has
    /// already: it stores an open file description once, under the name that the first of its file
    /// descriptors came with ([`same_file`] tells those apart as the manager does, where the kernel
    /// tells). This only fails if the message can't be sent, as the manager says nothing of what it
    /// did with it.
    /// [`barrier`] tells that it has dealt with it.
    ///
    /// A name is no longer than 255 characters, all of them printable ASCII ones but `:`, as
    /// systemd ignores a name that isn't, and stores the file descriptors with none instead, as
    /// `stored`. This refuses an empty name as well, which systemd takes, but which is likelier a
    /// mistake than a name. A message has one name for all of its file descriptors, so those that
    /// need names of their own take a call each.
    ///
    /// [`barrier`]: Self::barrier
    pub fn store(&self, name: &str, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        check_name(name)?;

        self.notify_with_fds(&format!("FDSTORE=1\nFDPOLL=0\nFDNAME={name}"), fds)
    }

    /// Remove the fds stored under `name`.
    ///
    /// That is all of them, as names aren't unique. The manager closes them, which is also how the
    /// process lets go of a socket that it stored: while the manager has a copy of it, the peer of
    /// a connection doesn't see the connection end when the process closes its own, so remove the
    /// file descriptor of a connection when you close it. The name has to be a valid one, as for
    /// [`store`](Self::store).
    pub fn remove(&self, name: &str) -> io::Result<()> {
        check_name(name)?;

        self.notify(&format!("FDSTOREREMOVE=1\nFDNAME={name}"))
    }

    /// Send `state`.
    ///
    /// `state` is a list of assignments such as `READY=1`, each on a line of its own. The manager
    /// takes a message of 4096 bytes at most, and ignores a longer one, as well as one that it
    /// can't make sense of, so this only fails if the message can't be sent.
    pub fn notify(&self, state: &str) -> io::Result<()> {
        self.notify_with_fds(state, &[])
    }

    /// Wait, `timeout` at most, for the manager to process the notifications sent so far.
    ///
    /// This sends a message that asks the manager to close a file descriptor once it gets to the
    /// message, and waits for that. The manager takes its messages in order, so it has dealt with
    /// the earlier ones by then. That doesn't say that it took them: it ignores those that the
    /// `NotifyAccess=` of the service doesn't allow.
    ///
    /// A process that the manager didn't start needs this before it exits, as the manager may
    /// otherwise fail to tie its messages to the service. For the main process of the service, it
    /// is only a way to know that the manager has dealt with its messages.
    ///
    /// It fails with [`ErrorKind::TimedOut`] if the manager takes longer than `timeout`. It blocks
    /// the thread meanwhile.
    pub fn barrier(&self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now().checked_add(timeout);
        // The manager has the only other copy of the write end once this sends it, and closes it
        // once it gets to the message, which hangs up the read end.
        let read = {
            let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
            self.notify_with_fds("BARRIER=1", &[write.as_fd()])?;

            read
        };

        // A hang-up is reported whether `poll` is asked for it or not.
        let mut fds = [PollFd::new(&read, PollFlags::empty())];
        match retry_on_intr(|| poll(&mut fds, remaining(deadline).as_ref()))? {
            0 => Err(io::Error::new(
                ErrorKind::TimedOut,
                "systemd didn't process the notifications in time",
            )),
            _ => Ok(()),
        }
    }

    /// Send `state`, as [`notify`](Self::notify) does, and `fds` along with it.
    ///
    /// The manager closes the file descriptors that come with a message unless the message stores
    /// them, as [`store`](Self::store) does. No more than 253 file descriptors fit in a message,
    /// which is as many as Linux passes at once: more fail with `E2BIG`, without a message being
    /// sent.
    pub fn notify_with_fds(&self, state: &str, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        if fds.len() > MAX_FDS {
            return Err(Errno::TOOBIG.into());
        }

        let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        // A message with no file descriptors has no control data at all.
        if !fds.is_empty() {
            let fits = control.push(SendAncillaryMessage::ScmRights(fds));
            debug_assert!(
                fits,
                "the space is for as many file descriptors as a message takes"
            );
        }
        let state = [IoSlice::new(state.as_bytes())];
        sendmsg_addr(
            &self.socket,
            &self.address,
            &state,
            &mut control,
            SendFlags::NOSIGNAL,
        )?;

        Ok(())
    }
}

/// How many file descriptors the fdstore of the service takes, `$FDSTORE`, if it has one.
///
/// Systemd sets it to the `FileDescriptorStoreMax=` of the unit when that isn't 0, from version 254
/// on, and the manager closes the file descriptors that a service stores beyond that without
/// telling the service. It is `None` if the variable is unset, 0, or not a number.
pub fn fd_store_max() -> Option<usize> {
    parse_store_max(env::var("FDSTORE").ok().as_deref())
}

/// A sealed memfd that holds `bytes`.
///
/// A memfd is a file in memory that has no name in any directory, which suits the state that a
/// service passes on to its next instance through the fdstore: store it with [`Notifier::store`],
/// and read it with [`read_memfd`]. The seals keep anyone from resizing it or writing to it,
/// through this file descriptor or any other of the same file, so the reader gets exactly what was
/// written here. `name` is for debugging only: it shows as `memfd:<name>` among the files of the
/// process, and can be 249 bytes at most.
pub fn memfd(name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    let flags = MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING;
    // With `vm.memfd_noexec=2`, the kernel refuses a memfd that can be made executable, and a
    // kernel before 6.3 doesn't know the flag that prevents that. This is what systemd does about
    // both.
    let fd = match memfd_create(name, flags | MemfdFlags::NOEXEC_SEAL) {
        Err(Errno::INVAL) => memfd_create(name, flags)?,
        fd => fd?,
    };
    // Write at an offset, so that the offset of the file stays at its start, where a reader that
    // doesn't seek expects it.
    let file = File::from(fd);
    file.write_all_at(bytes, 0)?;
    let seals = SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL;
    fcntl_add_seals(&file, seals)?;

    Ok(file.into())
}

/// All of what the memfd `fd` holds, from its start.
///
/// It reads at offsets of its own, and leaves the file offset of `fd` where it is, as that is
/// shared with every duplicate of `fd`. Any file that can be read at an offset will do, not just a
/// memfd.
pub fn read_memfd(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    loop {
        bytes.reserve(READ_CHUNK);
        let offset = bytes.len() as u64;
        if retry_on_intr(|| pread(fd, spare_capacity(&mut bytes), offset))? == 0 {
            return Ok(bytes);
        }
    }
}

/// Whether `a` and `b` are of the same open file description: whether one is a duplicate of the
/// other, as [`OwnedFd::try_clone`] makes, and as passing a file descriptor to another process
/// does.
///
/// The fdstore keeps one file descriptor of each open file description, and drops the others
/// without a word ([`Notifier::store`]). This asks the kernel, as systemd does: from Linux 6.10 on
/// (`F_DUPFD_QUERY`), or else through `kcmp(2)`. Without either, as when a filter of system calls
/// denies `kcmp(2)`, it fails with an error of kind [`ErrorKind::Unsupported`], rather than guess
/// as systemd does then: systemd takes two file descriptors of one file with the same flags for
/// the same, unless the file is a device, while they may be of open file descriptions of their
/// own, with offsets of their own, which a duplicate of one in place of the other would share.
pub fn same_file(a: BorrowedFd<'_>, b: BorrowedFd<'_>) -> io::Result<bool> {
    let (raw_a, raw_b) = (a.as_raw_fd(), b.as_raw_fd());
    if raw_a == raw_b {
        return Ok(true);
    }
    // SAFETY: The command reads and writes no memory, and both file descriptors are open, as
    // they are borrowed.
    match unsafe { libc::fcntl(raw_a, F_DUPFD_QUERY, raw_b) } {
        -1 => {
            let e = io::Error::last_os_error();
            // A kernel before 6.10 doesn't know the command.
            if e.raw_os_error() != Some(libc::EINVAL) {
                return Err(e);
            }
        }
        same => return Ok(same > 0),
    }
    let pid = libc::c_long::from(Pid::as_raw(Some(getpid())));
    // SAFETY: The call reads and writes no memory, and takes each of its arguments as a long.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid,
            pid,
            KCMP_FILE,
            libc::c_long::from(raw_a),
            libc::c_long::from(raw_b),
        )
    };
    match order {
        -1 => {
            let e = io::Error::last_os_error();
            if !e
                .raw_os_error()
                .is_some_and(|errno| KCMP_UNAVAILABLE.contains(&errno))
            {
                return Err(e);
            }
        }
        order => return Ok(order == 0),
    }

    Err(io::Error::new(
        ErrorKind::Unsupported,
        "the kernel tells neither through `F_DUPFD_QUERY` nor through `kcmp(2)` whether two file \
         descriptors are of the same open file description",
    ))
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

/// The names of `count` file descriptors, going by `$LISTEN_FDNAMES`.
///
/// There is one for each, or none at all, in which case they are all `unknown`.
fn parse_names(names: Option<&str>, count: usize) -> io::Result<Vec<String>> {
    let Some(names) = names else {
        return Ok(vec![UNKNOWN.to_owned(); count]);
    };
    let names: Vec<_> = names.split(':').map(str::to_owned).collect();
    if names.len() != count {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "$LISTEN_FDNAMES has {} names for {count} file descriptors",
                names.len()
            ),
        ));
    }

    Ok(names)
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

/// Check that `name` is one that systemd takes for a stored file descriptor, and not empty.
///
/// A newline isn't printable, so a name can't add assignments of its own to the message.
fn check_name(name: &str) -> io::Result<()> {
    let printable = |b: u8| (b' '..=b'~').contains(&b) && b != b':';
    if name.is_empty() || name.len() > MAX_NAME_LEN || !name.bytes().all(printable) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "a file descriptor name is 1 to {MAX_NAME_LEN} printable ASCII characters, \
                 other than `:`"
            ),
        ));
    }

    Ok(())
}

/// How long `poll` is to wait until `deadline`: not at all once it has passed, and for ever if
/// there is none, or if what is left is too long for a `Timespec`.
fn remaining(deadline: Option<Instant>) -> Option<Timespec> {
    let left = deadline?.saturating_duration_since(Instant::now());

    Timespec::try_from(left).ok()
}

/// The value of `$FDSTORE`, if it's a number that isn't 0.
fn parse_store_max(value: Option<&str>) -> Option<usize> {
    value?.parse().ok().filter(|max| *max > 0)
}

/// The largest count that `$LISTEN_FDS` can have: `sd_listen_fds(3)` takes a larger one for garbage
/// too, as the number of the last file descriptor would overflow.
const MAX_LISTEN_FDS: usize = (RawFd::MAX - START) as usize;

/// The first file descriptor that systemd passes, `SD_LISTEN_FDS_START`.
const START: RawFd = 3;

/// The name of a file descriptor that systemd sent no names for.
const UNKNOWN: &str = "unknown";

/// How many file descriptors fit in a message: `SCM_MAX_FD` of Linux, as unix(7) tells.
const MAX_FDS: usize = 253;

/// The longest name that systemd takes for a stored file descriptor.
const MAX_NAME_LEN: usize = 255;

/// How much room [`read_memfd`] makes for the next read, at least.
const READ_CHUNK: usize = 8192;

/// The command of `fcntl(2)` that tells whether two file descriptors are of the same open file
/// description, from Linux 6.10 on, `F_LINUX_SPECIFIC_BASE + 3`, which libc has no name for yet.
const F_DUPFD_QUERY: libc::c_int = 1024 + 3;

/// What `kcmp(2)` compares of two file descriptors: their open file descriptions.
const KCMP_FILE: libc::c_long = 0;

/// The errors of `kcmp(2)` that tell that it can't be called, as systemd takes them: the kernel
/// lacks it, or a filter of system calls denies it.
const KCMP_UNAVAILABLE: [libc::c_int; 4] =
    [libc::ENOSYS, libc::EOPNOTSUPP, libc::EPERM, libc::EACCES];

#[cfg(test)]
mod tests {
    use std::{
        env::temp_dir,
        fs::remove_file,
        io::IoSliceMut,
        os::fd::AsRawFd,
        path::{Path, PathBuf},
        process::Command,
        sync::mpsc::channel,
        thread,
    };

    use rand::{
        distr::{Alphanumeric, SampleString},
        rng,
    };
    use rustix::{
        fs::ftruncate,
        io::{fcntl_getfd, pwrite, read, write},
        net::{
            bind, recvmsg,
            sockopt::{set_socket_timeout, Timeout},
            RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
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
    fn names() {
        let names = parse_names(Some("web:ssh:stored"), 3).unwrap();
        assert_eq!(names, ["web", "ssh", "stored"]);

        // Names aren't unique, and can be anything but the separator, empty ones included.
        let names = parse_names(Some("a:a:unknown:"), 4).unwrap();
        assert_eq!(names, ["a", "a", "unknown", ""]);
        assert_eq!(parse_names(Some(""), 1).unwrap(), [""]);
        assert_eq!(parse_names(Some("one"), 1).unwrap(), ["one"]);
    }

    #[test]
    fn names_unknown() {
        assert_eq!(
            parse_names(None, 3).unwrap(),
            ["unknown", "unknown", "unknown"]
        );
        assert_eq!(parse_names(None, 1).unwrap(), ["unknown"]);
    }

    #[test]
    fn names_that_do_not_match_the_count() {
        for (names, count) in [("a:b", 3), ("a:b:c", 2), ("a", 2), ("", 2), ("a:b", 1)] {
            let error = parse_names(Some(names), count).unwrap_err();
            assert_eq!(
                error.kind(),
                ErrorKind::InvalidData,
                "{names:?} for {count}"
            );
        }
    }

    #[test]
    fn store_max() {
        assert_eq!(parse_store_max(Some("16")), Some(16));
        assert_eq!(parse_store_max(Some("1")), Some(1));
        assert_eq!(parse_store_max(None), None);
        for value in ["", "0", "-1", "x", "1.5", " 1", "99999999999999999999"] {
            assert_eq!(parse_store_max(Some(value)), None, "$FDSTORE={value:?}");
        }
    }

    #[test]
    fn fd_store_max_from_the_environment() {
        if !in_own_process("fd_store_max_from_the_environment", "FDSTORE=16") {
            return;
        }

        assert_eq!(fd_store_max(), Some(16));
        env::set_var("FDSTORE", "0");
        assert_eq!(fd_store_max(), None);
        env::remove_var("FDSTORE");
        assert_eq!(fd_store_max(), None);
    }

    #[test]
    fn valid_names() {
        let longest = "a".repeat(MAX_NAME_LEN);
        let printable = ('!'..='~').filter(|c| *c != ':').collect::<String>();
        for name in ["x", "busd-client-17-0", "with spaces", &printable, &longest] {
            check_name(name).unwrap_or_else(|e| panic!("{name:?}: {e}"));
        }
    }

    #[test]
    fn invalid_names() {
        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        let invalid = [
            "",
            "a:b",
            ":",
            "a\nb",
            "a\rb",
            "a\tb",
            "a\0b",
            "a\x1fb",
            "a\x7fb",
            "é",
            "日本",
            // Valid names, with the assignment of another message after them.
            "x\nFDSTOREREMOVE=1",
            &too_long,
        ];
        for name in invalid {
            assert_eq!(
                check_name(name).unwrap_err().kind(),
                ErrorKind::InvalidInput,
                "{name:?}"
            );
        }
    }

    #[test]
    fn notify() {
        for (manager, notifier) in managers() {
            notifier.notify("READY=1\nSTATUS=Up").unwrap();

            let (state, fds) = manager.receive();
            assert_eq!(state, "READY=1\nSTATUS=Up");
            assert!(fds.is_empty());
        }
    }

    #[test]
    fn store() {
        for (manager, notifier) in managers() {
            let (read_a, write_a) = pipe();
            let (read_b, write_b) = pipe();

            notifier
                .store("x", &[write_a.as_fd(), write_b.as_fd()])
                .unwrap();

            let (state, fds) = manager.receive();
            assert_eq!(state, "FDSTORE=1\nFDPOLL=0\nFDNAME=x");
            // These are the pipes that were sent, in the order they were sent.
            assert_eq!(fds.len(), 2);
            write(&fds[0], b"a").unwrap();
            write(&fds[1], b"b").unwrap();
            assert_eq!(read_byte(&read_a), b'a');
            assert_eq!(read_byte(&read_b), b'b');
        }
    }

    #[test]
    fn remove() {
        for (manager, notifier) in managers() {
            notifier.remove("x y").unwrap();

            let (state, fds) = manager.receive();
            assert_eq!(state, "FDSTOREREMOVE=1\nFDNAME=x y");
            assert!(fds.is_empty());
        }
    }

    #[test]
    fn invalid_names_are_not_sent() {
        for (manager, notifier) in managers() {
            let (_read, write) = pipe();

            let error = notifier.store("a:b", &[write.as_fd()]).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);
            let error = notifier.remove("").unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidInput);

            // If either had been sent, the manager would get it before this.
            notifier.notify("READY=1").unwrap();
            let (state, fds) = manager.receive();
            assert_eq!(state, "READY=1");
            assert!(fds.is_empty());
        }
    }

    #[test]
    fn notify_the_most_file_descriptors() {
        for (manager, notifier) in managers() {
            let (_read, write) = pipe();
            let fd = write.as_fd();

            notifier.notify_with_fds("READY=1", &[fd; MAX_FDS]).unwrap();
            assert_eq!(manager.receive().1.len(), MAX_FDS);

            let error = notifier
                .notify_with_fds("READY=1", &[fd; MAX_FDS + 1])
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(Errno::TOOBIG.raw_os_error()));
            // The manager gets nothing of it: the next message is this one.
            notifier.notify("STATUS=Next").unwrap();
            assert_eq!(manager.receive().0, "STATUS=Next");
        }
    }

    #[test]
    fn notify_without_a_manager() {
        let notifier = Notifier::new(unique_path()).unwrap();

        let error = notifier.notify("READY=1").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        let error = notifier.barrier(Duration::from_secs(1)).unwrap_err();
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
    fn barrier_waits_for_the_manager() {
        for (manager, notifier) in managers() {
            let delay = Duration::from_millis(200);
            let manager = thread::spawn(move || {
                let (state, fds) = manager.receive();
                // Take the time to deal with the message, then close its file descriptor.
                thread::sleep(delay);
                (state, fds.len())
            });

            let start = Instant::now();
            notifier.barrier(Duration::from_secs(30)).unwrap();
            assert!(start.elapsed() >= delay, "{:?}", start.elapsed());

            assert_eq!(manager.join().unwrap(), ("BARRIER=1".to_owned(), 1));
        }
    }

    #[test]
    fn barrier_times_out() {
        for (manager, notifier) in managers() {
            let (release, released) = channel::<()>();
            let manager = thread::spawn(move || {
                let (state, fds) = manager.receive();
                // Keep the file descriptor until the barrier gave up on it.
                let _ = released.recv();
                (state, fds.len())
            });

            let timeout = Duration::from_millis(200);
            let start = Instant::now();
            let error = notifier.barrier(timeout).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::TimedOut);
            assert!(start.elapsed() >= timeout, "{:?}", start.elapsed());

            release.send(()).unwrap();
            assert_eq!(manager.join().unwrap(), ("BARRIER=1".to_owned(), 1));
        }
    }

    #[test]
    fn memfd_round_trip() {
        let large = (0..=u8::MAX)
            .cycle()
            .take(3 * READ_CHUNK + 1)
            .collect::<Vec<_>>();
        for bytes in [&b""[..], b"state", &large] {
            let fd = memfd("busd-test", bytes).unwrap();

            assert_eq!(read_memfd(fd.as_fd()).unwrap(), bytes);
            assert!(fcntl_getfd(&fd).unwrap().contains(FdFlags::CLOEXEC));
        }
    }

    #[test]
    fn memfd_is_sealed() {
        let fd = memfd("busd-test", b"state").unwrap();

        assert_eq!(write(&fd, b"x").unwrap_err(), Errno::PERM);
        assert_eq!(pwrite(&fd, b"x", 0).unwrap_err(), Errno::PERM);
        assert_eq!(ftruncate(&fd, 0).unwrap_err(), Errno::PERM);
        assert_eq!(ftruncate(&fd, 1 << 20).unwrap_err(), Errno::PERM);
        // The seals themselves are sealed.
        assert_eq!(
            fcntl_add_seals(&fd, SealFlags::FUTURE_WRITE).unwrap_err(),
            Errno::PERM
        );
        assert_eq!(read_memfd(fd.as_fd()).unwrap(), b"state");
    }

    #[test]
    fn memfd_name_too_long() {
        assert!(memfd(&"a".repeat(249), b"").is_ok());
        assert_eq!(
            memfd(&"a".repeat(250), b"").unwrap_err().raw_os_error(),
            Some(Errno::INVAL.raw_os_error())
        );
    }

    #[test]
    fn read_memfd_from_the_start() {
        let fd = memfd("busd-test", b"state").unwrap();

        // The offset of the file descriptor is somewhere else than the start, and stays there.
        read_byte(&fd);
        assert_eq!(read_memfd(fd.as_fd()).unwrap(), b"state");
        assert_eq!(read_byte(&fd), b't');
    }

    #[test]
    fn read_any_file() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/systemd.rs");
        let file = File::open(path).unwrap();

        let bytes = read_memfd(file.as_fd()).unwrap();
        assert!(bytes.len() > READ_CHUNK);
        assert_eq!(bytes, std::fs::read(path).unwrap());
    }

    #[test]
    fn same_file_tells_duplicates() {
        let null = File::open("/dev/null").unwrap();
        let duplicate = null.try_clone().unwrap();
        assert!(same_file(null.as_fd(), duplicate.as_fd()).unwrap());
        assert!(same_file(null.as_fd(), null.as_fd()).unwrap());

        // Another open file description of the same file, and the two ends of a pipe, which are
        // of the same file too.
        let other = File::open("/dev/null").unwrap();
        assert!(!same_file(null.as_fd(), other.as_fd()).unwrap());
        let (read, write) = pipe();
        assert!(!same_file(read.as_fd(), write.as_fd()).unwrap());
        assert!(!same_file(null.as_fd(), read.as_fd()).unwrap());
    }

    #[test]
    fn take_listen_fds_from_the_environment() {
        let exports = "LISTEN_PID=$$ LISTEN_FDS=2 LISTEN_FDNAMES=first:second LISTEN_PIDFDID=1";
        if !in_own_process("take_listen_fds_from_the_environment", exports) {
            return;
        }

        let fds = take_listen_fds().unwrap();
        let names = fds
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["first", "second"]);
        let numbers = fds.iter().map(|(_, fd)| fd.as_raw_fd()).collect::<Vec<_>>();
        assert_eq!(numbers, [3, 4]);
        for (_, fd) in &fds {
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
    fn take_listen_fds_closes_all_on_error() {
        // There are two file descriptors, and one name.
        let exports = "LISTEN_PID=$$ LISTEN_FDS=2 LISTEN_FDNAMES=a";
        if !in_own_process("take_listen_fds_closes_all_on_error", exports) {
            return;
        }

        let error = take_listen_fds().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(!is_open(3) && !is_open(4));
        assert_listen_variables_removed();
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

        /// The next message: its text, and the file descriptors that come with it.
        fn receive(&self) -> (String, Vec<OwnedFd>) {
            let mut text = [0; 8192];
            let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let message = recvmsg(
                &self.socket,
                &mut [IoSliceMut::new(&mut text)],
                &mut control,
                RecvFlags::CMSG_CLOEXEC,
            )
            .unwrap();
            assert!(!message
                .flags
                .intersects(ReturnFlags::TRUNC | ReturnFlags::CTRUNC));

            let mut fds = Vec::new();
            for message in control.drain() {
                if let RecvAncillaryMessage::ScmRights(received) = message {
                    fds.extend(received);
                }
            }

            (
                String::from_utf8(text[..message.bytes].to_vec()).unwrap(),
                fds,
            )
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

    fn pipe() -> (OwnedFd, OwnedFd) {
        pipe_with(PipeFlags::CLOEXEC).unwrap()
    }

    fn read_byte(fd: &OwnedFd) -> u8 {
        let mut byte = [0];
        assert_eq!(read(fd, &mut byte).unwrap(), 1);

        byte[0]
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
