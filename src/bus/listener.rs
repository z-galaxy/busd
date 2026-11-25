//! The socket on which the bus accepts its clients.

#[cfg(target_os = "linux")]
use rustix::net::{sockopt, AddressFamily, SocketType};
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::path::Path;
use std::{
    io,
    os::unix::net::SocketAddr,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use rustix::io::Errno;
use zbus::{
    address::{
        transport::{Unix, UnixSocket},
        Transport,
    },
    Address, OwnedGuid,
};
use zruntime::{fs::remove_file, Local};

use crate::{debug, info, runtime, warn};

/// The socket on which the bus accepts its clients.
#[derive(Debug)]
pub(super) struct Listener {
    socket: zruntime::net::unix::UnixListener<Local>,
    /// When the listener last logged a warning that accepting a connection failed.
    warned: Option<Instant>,
    /// The socket to which clients connect to reach the bus.
    unix: Unix,
    /// Whether the bus bound the socket itself. If not, the bus took the socket from whoever
    /// passed it, such as systemd's socket activation, and that party owns the socket's file.
    bound: bool,
}

impl Listener {
    /// Bind a socket for `unix`, the transport of the bus's address. If a socket file that no one
    /// listens on anymore is in the way, the socket replaces it.
    ///
    /// For a directory, the socket is a file of a random name in it.
    pub(super) async fn bind(unix: &Unix) -> Result<Self> {
        // Resolve address specification into address that clients can use.
        let (addr, socket) = unix_addr(unix)?;
        let unix = Unix::new(socket);
        let socket = unix_stream(addr)
            .await
            .with_context(|| format!("failed to listen on {unix}"))?;

        Ok(Self {
            socket,
            warned: None,
            unix,
            bound: true,
        })
    }

    /// A listener on `socket`, which already listens. `bound` is true if a bus bound the socket
    /// itself, for example the instance that handed it over. It is false if someone else bound
    /// it, for example systemd for socket activation.
    #[cfg(target_os = "linux")]
    pub(super) fn from_fd(socket: OwnedFd, bound: bool) -> Result<Self> {
        if !sockopt::socket_acceptconn(&socket)? {
            bail!("The socket to listen on doesn't listen.");
        }
        if sockopt::socket_type(&socket)? != SocketType::STREAM {
            bail!("The socket to listen on isn't a stream socket.");
        }
        let family = sockopt::socket_domain(&socket)?;
        if family != AddressFamily::UNIX {
            bail!(
                "The socket to listen on is of the {family:?} family: the bus only listens on unix \
                 sockets."
            );
        }
        let socket = std::os::unix::net::UnixListener::from(socket);
        let unix = unix_socket(&socket.local_addr()?)?;
        info!("Listening on the UNIX socket `{unix}` that was passed.");
        let socket = zruntime::net::unix::UnixListener::from_std(&runtime::local()?, socket)?;

        Ok(Self {
            socket,
            warned: None,
            unix: Unix::new(unix),
            bound,
        })
    }

    /// The address at which clients reach the socket, for a bus with `guid`.
    pub(super) fn address(&self, guid: &OwnedGuid) -> Result<Address> {
        Address::new(Transport::Unix(self.unix.clone()))
            .set_guid(guid.clone())
            .map_err(Into::into)
    }

    /// The next client that connects.
    ///
    /// Accepting fails when the bus or the system runs out of resources: file descriptors
    /// (`EMFILE`, `ENFILE`) or memory (`ENOBUFS`, `ENOMEM`). On Linux, `accept4` reserves a file
    /// descriptor before it checks for a pending connection, so it fails as soon as the bus has
    /// none left, even when no client is connecting.
    ///
    /// Such a failure pauses accepting new connections, while the bus keeps serving its existing
    /// clients and new clients wait in the socket's backlog. The listener retries after a delay
    /// that starts at [`FIRST_WAIT`] and doubles with each failed attempt, up to [`LONGEST_WAIT`].
    /// It logs a warning at most once every [`WARN_INTERVAL`], and logs again when it accepts a
    /// connection after a warning. If a client disconnects before its connection is accepted
    /// (`ECONNABORTED`), the listener retries at once.
    ///
    /// This returns an error only when retrying can't help (`EBADF`, `EFAULT`, `EINVAL`,
    /// `ENOTSOCK` and `EOPNOTSUPP`): the listening socket is broken, or the call is invalid.
    pub(super) async fn accept(&mut self) -> io::Result<std::os::unix::net::UnixStream> {
        let mut wait = Duration::ZERO;
        let mut warned = false;
        loop {
            let e = match self.socket.accept().await {
                Ok((stream, _)) => {
                    if warned {
                        info!("Accepting new connections again.");
                    }

                    // The client's connection is given the standard library stream that the
                    // zruntime one wraps, and drives that stream on the runtime that serves the
                    // client.
                    return Ok(stream.into_std());
                }
                Err(e) => e,
            };
            match Errno::from_io_error(&e) {
                Some(
                    Errno::BADF | Errno::FAULT | Errno::INVAL | Errno::NOTSOCK | Errno::OPNOTSUPP,
                ) => return Err(e),
                Some(Errno::CONNABORTED) => {
                    debug!("A client disconnected before its connection was accepted.");

                    continue;
                }
                _ => (),
            }
            if !warned && self.warned.is_none_or(|at| at.elapsed() >= WARN_INTERVAL) {
                warn!(
                    "Failed to accept a new connection: {e}. Retrying, while existing clients are \
                     still served."
                );
                self.warned = Some(Instant::now());
                warned = true;
            }
            wait = (wait * 2).clamp(FIRST_WAIT, LONGEST_WAIT);
            self.socket.runtime().sleep(wait).await;
        }
    }

    /// Whether the bus bound the socket itself.
    pub(super) fn is_bound(&self) -> bool {
        self.bound
    }

    /// A duplicate of the socket, on which the next instance of the bus listens.
    #[cfg(target_os = "linux")]
    pub(super) fn try_clone_to_owned(&self) -> io::Result<OwnedFd> {
        self.socket.as_fd().try_clone_to_owned()
    }

    /// Remove the socket's file, if the bus bound the socket and the socket has a file.
    pub(super) async fn remove(&self) -> io::Result<()> {
        if !self.is_bound() {
            return Ok(());
        }
        match self.unix.path() {
            UnixSocket::File(path) => remove_file(path).await,
            _ => Ok(()),
        }
    }
}

/// The delay before the first retry, after accepting a connection fails.
const FIRST_WAIT: Duration = Duration::from_millis(5);

/// The longest delay between retries, however long accepting keeps failing.
const LONGEST_WAIT: Duration = Duration::from_secs(1);

/// The shortest time between two warnings that accepting a connection failed. A bus that keeps
/// running out of file descriptors, and accepts a connection each time some are freed, so doesn't
/// log a warning for every connection.
const WARN_INTERVAL: Duration = Duration::from_secs(60);

/// The address to listen on for `unix`, and the socket that clients connect to.
///
/// For a directory, the socket is a file of a random name in it.
fn unix_addr(unix: &Unix) -> Result<(SocketAddr, UnixSocket)> {
    Ok(match unix.path() {
        #[cfg(target_os = "linux")]
        UnixSocket::Abstract(name) => {
            use std::os::linux::net::SocketAddrExt;

            let addr = SocketAddr::from_abstract_name(name.as_encoded_bytes())?;
            info!(
                "Listening on abstract UNIX socket `{}`.",
                name.to_string_lossy()
            );

            (addr, UnixSocket::Abstract(name.clone()))
        }
        UnixSocket::File(path) => {
            let addr = SocketAddr::from_pathname(path)?;
            info!(
                "Listening on UNIX socket file `{}`.",
                path.to_string_lossy()
            );

            (addr, UnixSocket::File(path.clone()))
        }
        UnixSocket::Dir(dir) | UnixSocket::TmpDir(dir) => {
            let path = dir.join(format!("dbus-{}", fastrand::u32(1_000_000..u32::MAX)));
            let addr = SocketAddr::from_pathname(&path)?;
            info!(
                "Listening on UNIX socket file `{}`.",
                path.to_string_lossy()
            );

            (addr, UnixSocket::File(path))
        }
        _ => bail!("Unsupported address."),
    })
}

async fn unix_stream(addr: SocketAddr) -> Result<zruntime::net::unix::UnixListener<Local>> {
    let runtime = runtime::local()?;
    let std_listener = zruntime::unblock(move || bind(&addr)).await?;

    zruntime::net::unix::UnixListener::from_std(&runtime, std_listener).map_err(Into::into)
}

/// A socket bound to `addr`, in place of a socket file there that no one listens on anymore, as one
/// that a bus left behind when it was killed.
///
/// On Linux, the file goes only if it's a socket that refuses connections. Elsewhere, as on macOS,
/// a socket also refuses connections while more of them wait for its bus than it takes, so the file
/// stays. The reference implementation of D-Bus removes any socket file in its way, even one that
/// another bus still listens on, which this leaves alone.
fn bind(addr: &SocketAddr) -> io::Result<std::os::unix::net::UnixListener> {
    let error = match std::os::unix::net::UnixListener::bind_addr(addr) {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => e,
        result => return result,
    };
    #[cfg(target_os = "linux")]
    if let Some(path) = addr.as_pathname() {
        return replace_stale(addr, path, error);
    }

    Err(error)
}

/// A socket bound to `addr` in place of the socket file at `path`, the path of `addr`, if no one
/// listens on that anymore, or else `error`, which binding `addr` failed with.
#[cfg(target_os = "linux")]
fn replace_stale(
    addr: &SocketAddr,
    path: &Path,
    error: io::Error,
) -> io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::{fs::FileTypeExt, net::UnixListener};

    // The file goes as the bus that it's of does, which may be in the meantime.
    let is_socket = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return UnixListener::bind_addr(addr),
        Err(e) => return Err(e),
        io::Result::Ok(metadata) => metadata.file_type().is_socket(),
    };
    if !is_socket {
        return Err(error);
    }
    match probe(path)? {
        Probe::InUse => return Err(error),
        Probe::Gone => return UnixListener::bind_addr(addr),
        Probe::Stale => (),
    }
    info!(
        "Replacing the socket file `{}`, which no one listens on.",
        path.display()
    );
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => (),
    }

    UnixListener::bind_addr(addr)
}

/// What connecting to the socket at `path` tells of it.
///
/// The connection doesn't wait: a bus that listens on the socket but doesn't accept the connections
/// that wait for it, as one that hangs, would have it wait for ever.
#[cfg(target_os = "linux")]
fn probe(path: &Path) -> io::Result<Probe> {
    use rustix::{
        io::{ioctl_fionbio, Errno},
        net::{connect, socket, AddressFamily, SocketAddrUnix, SocketType},
    };

    let socket = socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
    ioctl_fionbio(&socket, true)?;
    let addr = SocketAddrUnix::new(path)?;

    io::Result::Ok(match connect(&socket, &addr) {
        Err(Errno::CONNREFUSED) => Probe::Stale,
        Err(Errno::NOENT) => Probe::Gone,
        _ => Probe::InUse,
    })
}

/// What connecting to a socket file tells of it.
#[cfg(target_os = "linux")]
enum Probe {
    /// A bus listens on it, or it can't be told whether one does.
    InUse,
    /// No one listens on it anymore: it refuses connections.
    Stale,
    /// The file went in the meantime.
    Gone,
}

/// The socket that clients connect to, to reach a listener bound to `addr`.
#[cfg(target_os = "linux")]
fn unix_socket(addr: &SocketAddr) -> Result<UnixSocket> {
    use std::{
        ffi::OsStr,
        os::{linux::net::SocketAddrExt, unix::ffi::OsStrExt},
    };

    if let Some(path) = addr.as_pathname() {
        return Ok(UnixSocket::File(path.to_owned()));
    }
    if let Some(name) = addr.as_abstract_name() {
        return Ok(UnixSocket::Abstract(OsStr::from_bytes(name).to_owned()));
    }

    bail!("The UNIX socket to listen on has no name.")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        fs,
        os::unix::net::{SocketAddr, UnixListener, UnixStream},
    };

    use ntest::timeout;

    use super::*;
    use crate::testing::socket_path;

    /// A socket file that no one listens on gets replaced, but not one that a bus listens on, nor a
    /// file of another kind.
    #[test]
    fn bind_replaces_a_stale_socket_file() {
        let path = socket_path();
        let addr = SocketAddr::from_pathname(&path).unwrap();
        // A listener that goes leaves its file behind.
        UnixListener::bind_addr(&addr).unwrap();
        assert!(path.exists());

        {
            let _listener = bind(&addr).unwrap();
            UnixStream::connect(&path).unwrap();
            let error = bind(&addr).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        }

        fs::remove_file(&path).unwrap();
        fs::write(&path, b"").unwrap();
        let error = bind(&addr).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(path.is_file());
        fs::remove_file(&path).unwrap();
    }

    /// A socket file that a bus listens on stays, and the bus finds so at once, even if the bus
    /// doesn't accept the connections that wait for it.
    #[test]
    #[timeout(10000)]
    fn bind_leaves_a_busy_socket_file() {
        use rustix::{
            io::{ioctl_fionbio, Errno},
            net::{self, connect, listen, socket, AddressFamily, SocketAddrUnix, SocketType},
        };

        let path = socket_path();
        let unix = SocketAddrUnix::new(&path).unwrap();
        let listener = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
        net::bind(&listener, &unix).unwrap();
        listen(&listener, 0).unwrap();
        // The connections wait until the queue of the listener is full.
        let mut waiting = Vec::new();
        loop {
            let client = socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
            ioctl_fionbio(&client, true).unwrap();
            match connect(&client, &unix) {
                Err(Errno::AGAIN) => break,
                Err(e) => panic!("Failed to connect: {e}"),
                _ => waiting.push(client),
            }
        }

        let error = bind(&SocketAddr::from_pathname(&path).unwrap()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(path.exists());
        fs::remove_file(&path).unwrap();
    }
}
