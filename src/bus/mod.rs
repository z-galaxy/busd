use crate::{debug, info, trace, warn};
use anyhow::{bail, Context, Ok, Result};
use std::{
    env,
    os::fd::AsFd,
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
    connection, Address, Connection, Guid, OwnedGuid,
};
use zruntime::{fs::remove_file, spawn, Shared, SharedRuntime};

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

        peers.add_us(peer_stream, service_conn.clone()).await;
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
        })
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
        match self.inner.address.transport() {
            Transport::Unix(unix) => match unix.path() {
                UnixSocket::File(path) => remove_file(path).await.map_err(Into::into),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
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
        let std_listener =
            zruntime::unblock(move || std::os::unix::net::UnixListener::bind_addr(&addr)).await?;

        zruntime::net::unix::UnixListener::from_std(&runtime, std_listener).map_err(Into::into)
    }

    async fn accept_next(&mut self) -> Result<()> {
        let (stream, _) = self.listener.accept().await?;
        debug!("Accepted connection on address `{}`", self.inner.address);

        let id = self.next_id();
        let inner = self.inner.clone();
        spawn(async move {
            // The socket is asked for the uid of the client itself, rather than the connection
            // that zbus makes of it, as zbus looks up the groups of the client along with its uid,
            // which takes a thread for blocking work, and longer than the rest of the connection.
            let res = match peer_uid(&stream) {
                Result::Ok(uid) => {
                    // zbus is given the standard library stream that the zruntime one wraps, and
                    // the connection then drives it on the runtime it is built on.
                    let builder = connection::Builder::unix_stream(stream.into_std());
                    inner
                        .peers
                        .clone()
                        .add(&inner.guid, id, builder, inner.send_timeout, uid)
                        .await
                }
                Err(e) => Err(e.into()),
            };
            if let Err(e) = res {
                warn!("Failed to establish connection: {}", e);
            }
        })
        .detach();

        Ok(())
    }

    pub fn peers(&self) -> &Arc<Peers> {
        &self.inner.peers
    }

    pub fn guid(&self) -> &OwnedGuid {
        &self.inner.guid
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

/// The uid of the peer of the unix socket `socket`.
#[cfg(any(target_os = "android", target_os = "linux"))]
fn peer_uid(socket: impl AsFd) -> std::io::Result<u32> {
    let ucred = rustix::net::sockopt::socket_peercred(socket)?;

    Result::Ok(ucred.uid.as_raw())
}

/// The uid of the peer of the unix socket `socket`.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
fn peer_uid(socket: impl AsFd) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;

    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: The descriptor is that of `socket`, which stays open for as long as the call lasts,
    // and `uid` and `gid` are valid for the writes of the call.
    let ret = unsafe { libc::getpeereid(socket.as_fd().as_raw_fd(), &mut uid, &mut gid) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Result::Ok(uid)
}

/// The uid of the peer of the unix socket `socket`, which no call on this platform tells, so that
/// no client can authenticate.
#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
fn peer_uid(_socket: impl AsFd) -> std::io::Result<u32> {
    Err(std::io::ErrorKind::Unsupported.into())
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
