use crate::{debug, info, trace, warn};
use anyhow::{bail, Context, Ok, Result};
#[cfg(target_os = "linux")]
use rustix::net::{sockopt, AddressFamily, SocketType};
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::{
    env, io,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use zbus::{
    address::{
        transport::{Unix, UnixSocket},
        Transport,
    },
    connection,
    fdo::ConnectionCredentials,
    Address, Connection, Guid, OwnedGuid,
};
use zruntime::{fs::remove_file, spawn, Shared, SharedRuntime};

#[cfg(target_os = "linux")]
mod systemd;

use crate::{
    fdo::{self, DBus, Monitoring},
    peer::SEND_TIMEOUT,
    peers::Peers,
};

/// The bus.
///
/// The bus, and every connection it serves, runs on [zruntime]. Create and run it from inside
/// [`zruntime::block_on`], so that the calling thread runs all of them, on its own runtime.
///
/// [zruntime]: https://docs.rs/zruntime
#[derive(Debug)]
pub struct Bus {
    inner: Inner,
    listener: zruntime::net::unix::UnixListener<Shared>,
    /// Whether the bus bound the socket that it listens on itself, rather than take it from
    /// whoever passed it, such as systemd's socket activation, which then owns the socket's file.
    bound: bool,
}

// All (cheaply) cloneable fields of `Bus` go here.
#[derive(Clone, Debug)]
pub struct Inner {
    address: Address,
    peers: Arc<Peers>,
    guid: OwnedGuid,
    next_id: usize,
    send_timeout: Duration,
    _self_conn: Connection,
}

impl Bus {
    pub async fn for_address(address: Option<&str>) -> Result<Self> {
        let address = match address {
            Some(address) => Address::from_str(address)?,
            None => Address::from_str(&default_address())?,
        };
        let Transport::Unix(unix) = address.transport() else {
            bail!("Unsupported address `{address}`: the bus only listens on unix sockets.");
        };
        let guid: OwnedGuid = match address.guid() {
            Some(guid) => guid.to_owned().into(),
            None => Guid::generate().into(),
        };
        // Resolve address specification into address that clients can use.
        let (addr, socket) = Self::unix_addr(unix)?;
        let unix = Unix::new(socket);
        let listener = Self::unix_stream(addr)
            .await
            .with_context(|| format!("failed to listen on {unix}"))?;
        let address = Address::new(Transport::Unix(unix)).set_guid(guid.clone())?;

        Self::new(listener, address, guid, true).await
    }

    /// Create a bus that listens on `listener`, a socket that listens already, such as the one that
    /// systemd's socket activation passes.
    ///
    /// The bus gives the address of the socket out as its own, with a GUID of its own. The socket
    /// stays with whoever passed it: the bus doesn't remove its file as it goes.
    #[cfg(target_os = "linux")]
    pub async fn for_listener(listener: OwnedFd) -> Result<Self> {
        let (listener, unix) = Self::listener(listener)?;
        let guid = Guid::generate();
        let address = Address::new(Transport::Unix(unix)).set_guid(guid.clone())?;

        Self::new(listener, address, guid.into(), false).await
    }

    pub fn address(&self) -> &Address {
        &self.inner.address
    }

    /// Set how long the bus waits for a client to take a message, before it disconnects the client.
    ///
    /// It is [`SEND_TIMEOUT`] unless set otherwise, and applies to the clients that connect from
    /// then on.
    pub fn set_send_timeout(&mut self, timeout: Duration) {
        self.inner.send_timeout = timeout;
    }

    /// Set how many objects the bus keeps for each user at most: its connections, and the calls to
    /// them that wait for replies.
    ///
    /// The bus closes a connection of a user beyond them, and answers a call to a connection of a
    /// user beyond them with a `LimitsExceeded` error. The calls of other users only get a share of
    /// them, as [`Charge::new`] has it. It is [`MAX_OBJECTS`] unless set otherwise, and applies to
    /// every user at once.
    ///
    /// [`Charge::new`]: crate::user::Charge::new
    /// [`MAX_OBJECTS`]: crate::user::MAX_OBJECTS
    pub fn set_max_objects_per_user(&mut self, max: usize) {
        self.inner.peers.users().set_max_objects(max);
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            self.accept_next().await?;
        }
    }

    // AsyncDrop would have been nice!
    pub async fn cleanup(self) -> Result<()> {
        if !self.bound {
            return Ok(());
        }
        match self.inner.address.transport() {
            Transport::Unix(unix) => match unix.path() {
                UnixSocket::File(path) => remove_file(path).await.map_err(Into::into),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    pub fn peers(&self) -> &Arc<Peers> {
        &self.inner.peers
    }

    pub fn guid(&self) -> &OwnedGuid {
        &self.inner.guid
    }

    /// A bus that listens on `listener`, at `address`: `bound` says whether it bound the listener
    /// itself.
    async fn new(
        listener: zruntime::net::unix::UnixListener<Shared>,
        address: Address,
        guid: OwnedGuid,
        bound: bool,
    ) -> Result<Self> {
        let peers = Peers::new();

        let dbus = DBus::new(peers.clone(), guid.clone());
        let monitoring = Monitoring::new(peers.clone());

        // Create a peer for ourselves.
        trace!("Creating self-dial connection.");
        let (client_socket, peer_socket) = zbus::connection::socket::Channel::pair();
        let service_conn = connection::Builder::authenticated_socket(client_socket, guid.clone())
            .p2p()
            .unique_name(fdo::BUS_NAME)
            .name(fdo::BUS_NAME)
            .serve_at(fdo::DBus::PATH, dbus)
            .serve_at(fdo::Monitoring::PATH, monitoring)
            .build()
            .await?;
        let peer_stream = connection::Builder::authenticated_socket(peer_socket, guid.clone())
            .p2p()
            .build_message_stream()
            .await?;

        peers.add_us(peer_stream, service_conn.clone()).await?;
        trace!("Self-dial connection created.");

        Ok(Self {
            listener,
            inner: Inner {
                address,
                peers,
                guid,
                next_id: 0,
                send_timeout: SEND_TIMEOUT,
                _self_conn: service_conn,
            },
            bound,
        })
    }

    /// The listener on `listener`, a socket that listens already, and the socket that clients
    /// connect to, to reach it.
    #[cfg(target_os = "linux")]
    fn listener(listener: OwnedFd) -> Result<(zruntime::net::unix::UnixListener<Shared>, Unix)> {
        if !sockopt::socket_acceptconn(&listener)? {
            bail!("The socket to listen on doesn't listen.");
        }
        if sockopt::socket_type(&listener)? != SocketType::STREAM {
            bail!("The socket to listen on isn't a stream socket.");
        }
        let family = sockopt::socket_domain(&listener)?;
        if family != AddressFamily::UNIX {
            bail!(
                "The socket to listen on is of the {family:?} family: the bus only listens on unix \
                 sockets."
            );
        }
        let listener = std::os::unix::net::UnixListener::from(listener);
        let socket = Self::unix_socket(&listener.local_addr()?)?;
        info!("Listening on the UNIX socket `{socket}` that was passed.");
        let runtime = SharedRuntime::current()?;
        let listener = zruntime::net::unix::UnixListener::from_std(&runtime, listener)?;

        Ok((listener, Unix::new(socket)))
    }

    /// The address to listen on for `unix`, and the socket that clients connect to.
    ///
    /// For a directory, the socket is a file of a random name in it.
    fn unix_addr(unix: &Unix) -> Result<(std::os::unix::net::SocketAddr, UnixSocket)> {
        use std::os::unix::net::SocketAddr;

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

    async fn unix_stream(
        addr: std::os::unix::net::SocketAddr,
    ) -> Result<zruntime::net::unix::UnixListener<Shared>> {
        let runtime = SharedRuntime::current()?;
        let std_listener = zruntime::unblock(move || bind(&addr)).await?;

        zruntime::net::unix::UnixListener::from_std(&runtime, std_listener).map_err(Into::into)
    }

    /// The socket that clients connect to, to reach a listener bound to `addr`.
    #[cfg(target_os = "linux")]
    fn unix_socket(addr: &std::os::unix::net::SocketAddr) -> Result<UnixSocket> {
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

    async fn accept_next(&mut self) -> Result<()> {
        let (stream, _) = self.listener.accept().await?;
        debug!("Accepted connection on address `{}`", self.inner.address);

        let id = self.next_id();
        let inner = self.inner.clone();
        spawn(async move {
            let res = async {
                // zbus is given the standard library stream that the zruntime one wraps, and the
                // connection then drives it on the runtime it is built on.
                let stream = stream.into_std();
                // The client is an object of its user from the start, before its handshake, so
                // its user is the one that its socket tells, as zbus looks it up for a connection
                // over the socket, for a client in another PID namespace too.
                let uid = ConnectionCredentials::of_peer(&stream)
                    .await?
                    .unix_user_id()
                    .context("The client has no user")?;
                let builder = connection::Builder::unix_stream(stream);

                inner
                    .peers
                    .clone()
                    .add(&inner.guid, id, builder, inner.send_timeout, uid)
                    .await
            };
            if let Err(e) = res.await {
                warn!("Failed to establish connection: {}", e);
            }
        })
        .detach();

        Ok(())
    }

    fn next_id(&mut self) -> usize {
        self.inner.next_id += 1;

        self.inner.next_id
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        self.inner.peers.remove_us();
    }
}

/// A socket in the user's runtime directory, `$XDG_RUNTIME_DIR` or else `/run/user/<uid>`, or in
/// the directory for temporary files if the user has no runtime directory.
fn default_address() -> String {
    let dir = env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let dir = Path::new("/run/user").join(rustix::process::getuid().to_string());

            dir.is_dir().then_some(dir)
        })
        .unwrap_or_else(env::temp_dir);

    format!("unix:dir={}", dir.display())
}

/// A socket bound to `addr`, in place of a socket file there that no one listens on anymore, as one
/// that a bus left behind when it was killed.
///
/// On Linux, the file goes only if it's a socket that refuses connections. Elsewhere, as on macOS,
/// a socket also refuses connections while more of them wait for its bus than it takes, so the file
/// stays. The reference implementation of D-Bus removes any socket file in its way, even one that
/// another bus still listens on, which this leaves alone.
fn bind(addr: &std::os::unix::net::SocketAddr) -> io::Result<std::os::unix::net::UnixListener> {
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
    addr: &std::os::unix::net::SocketAddr,
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{
        fs,
        os::unix::net::{SocketAddr, UnixListener, UnixStream},
    };

    use ntest::timeout;
    use rand::{
        distr::{Alphanumeric, SampleString},
        rng,
    };

    use super::*;

    /// A socket file that no one listens on gets replaced, but not one that a bus listens on, nor a
    /// file of another kind.
    #[test]
    fn bind_replaces_a_stale_socket_file() {
        let path = env::temp_dir().join(Alphanumeric.sample_string(&mut rng(), 10));
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

        let path = env::temp_dir().join(Alphanumeric.sample_string(&mut rng(), 10));
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
