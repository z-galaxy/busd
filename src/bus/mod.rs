use anyhow::{bail, Ok, Result};
use std::{env, path::Path, str::FromStr, sync::Arc};
use tokio::{fs::remove_file, spawn};
use tracing::{debug, info, trace, warn};
use zbus::{
    address::{
        transport::{Tcp, Unix, UnixSocket},
        Transport,
    },
    connection::{self, socket::BoxedSplit},
    Address, AuthMechanism, Connection, Guid, OwnedGuid,
};

use crate::{
    fdo::{self, DBus, Monitoring},
    peers::Peers,
};

/// The bus.
#[derive(Debug)]
pub struct Bus {
    inner: Inner,
    listener: Listener,
}

// All (cheaply) cloneable fields of `Bus` go here.
#[derive(Clone, Debug)]
pub struct Inner {
    address: Address,
    peers: Arc<Peers>,
    guid: OwnedGuid,
    next_id: usize,
    auth_mechanism: AuthMechanism,
    _self_conn: Connection,
}

#[derive(Debug)]
enum Listener {
    Unix(tokio::net::UnixListener),
    Tcp(tokio::net::TcpListener),
}

impl Bus {
    pub async fn for_address(address: Option<&str>) -> Result<Self> {
        let mut address = match address {
            Some(address) => Address::from_str(address)?,
            None => Address::from_str(&default_address())?,
        };
        let guid: OwnedGuid = match address.guid() {
            Some(guid) => guid.to_owned().into(),
            None => {
                let guid = Guid::generate();
                address = address.set_guid(guid.clone())?;

                guid.into()
            }
        };
        let (listener, auth_mechanism) = match address.transport() {
            Transport::Unix(unix) => {
                // Resolve address specification into address that clients can use.
                let addr = Self::unix_addr(unix)?;
                address = Address::new(Transport::Unix(Unix::new(UnixSocket::File(
                    addr.as_pathname()
                        .expect("Address created for UNIX socket should always have a path.")
                        .to_path_buf(),
                ))))
                .set_guid(guid.clone())?;

                (
                    Self::unix_stream(addr.clone()).await?,
                    AuthMechanism::External,
                )
            }
            Transport::Tcp(tcp) => (Self::tcp_stream(tcp).await?, AuthMechanism::Anonymous),
            _ => bail!("Unsupported address `{}`.", address),
        };

        let peers = Peers::new();

        let dbus = DBus::new(peers.clone(), guid.clone());
        let monitoring = Monitoring::new(peers.clone());

        // Create a peer for ourselves.
        trace!("Creating self-dial connection.");
        let (client_socket, peer_socket) = zbus::connection::socket::Channel::pair();
        let service_conn = connection::Builder::authenticated_socket(client_socket, guid.clone())?
            .p2p()
            .unique_name(fdo::BUS_NAME)?
            .name(fdo::BUS_NAME)?
            .serve_at(fdo::DBus::PATH, dbus)?
            .serve_at(fdo::Monitoring::PATH, monitoring)?
            .build()
            .await?;
        let peer_conn = connection::Builder::authenticated_socket(peer_socket, guid.clone())?
            .p2p()
            .build()
            .await?;

        peers.add_us(peer_conn).await;
        trace!("Self-dial connection created.");

        Ok(Self {
            listener,
            inner: Inner {
                address,
                peers,
                guid,
                next_id: 0,
                auth_mechanism,
                _self_conn: service_conn,
            },
        })
    }

    pub fn address(&self) -> &Address {
        &self.inner.address
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

    fn unix_addr(unix: &Unix) -> Result<std::os::unix::net::SocketAddr> {
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

                addr
            }
            UnixSocket::File(path) => {
                let addr = SocketAddr::from_pathname(path)?;
                info!(
                    "Listening on UNIX socket file `{}`.",
                    path.to_string_lossy()
                );

                addr
            }
            UnixSocket::Dir(dir) | UnixSocket::TmpDir(dir) => {
                let path = dir.join(format!("dbus-{}", fastrand::u32(1_000_000..u32::MAX)));
                let addr = SocketAddr::from_pathname(&path)?;
                info!(
                    "Listening on UNIX socket file `{}`.",
                    path.to_string_lossy()
                );

                addr
            }
            _ => bail!("Unsupported address."),
        })
    }

    async fn unix_stream(addr: std::os::unix::net::SocketAddr) -> Result<Listener> {
        // TODO: Use tokio::net::UnixListener directly once it supports abstract sockets:
        //
        // https://github.com/tokio-rs/tokio/issues/4610

        let std_listener =
            tokio::task::spawn_blocking(move || std::os::unix::net::UnixListener::bind_addr(&addr))
                .await??;
        std_listener.set_nonblocking(true)?;
        tokio::net::UnixListener::from_std(std_listener)
            .map(Listener::Unix)
            .map_err(Into::into)
    }

    async fn tcp_stream(tcp: &Tcp) -> Result<Listener> {
        if tcp.nonce_file().is_some() {
            bail!("`nonce-tcp` transport is not supported (yet).");
        }
        info!("Listening on `{}:{}`.", tcp.host(), tcp.port());
        let address = (tcp.host(), tcp.port());

        tokio::net::TcpListener::bind(address)
            .await
            .map(Listener::Tcp)
            .map_err(Into::into)
    }

    async fn accept_next(&mut self) -> Result<()> {
        let socket = self.accept().await?;

        let id = self.next_id();
        let inner = self.inner.clone();
        spawn(async move {
            if let Err(e) = inner
                .peers
                .clone()
                .add(&inner.guid, id, socket, inner.auth_mechanism)
                .await
            {
                warn!("Failed to establish connection: {}", e);
            }
        });

        Ok(())
    }

    async fn accept(&mut self) -> Result<BoxedSplit> {
        let stream = match &mut self.listener {
            Listener::Unix(listener) => listener.accept().await.map(|(stream, _)| stream.into())?,
            Listener::Tcp(listener) => listener.accept().await.map(|(stream, _)| stream.into())?,
        };
        debug!("Accepted connection on address `{}`", self.inner.address);

        Ok(stream)
    }

    pub fn peers(&self) -> &Arc<Peers> {
        &self.inner.peers
    }

    pub fn guid(&self) -> &OwnedGuid {
        &self.inner.guid
    }

    pub fn auth_mechanism(&self) -> AuthMechanism {
        self.inner.auth_mechanism
    }

    fn next_id(&mut self) -> usize {
        self.inner.next_id += 1;

        self.inner.next_id
    }
}

fn default_address() -> String {
    let runtime_dir = env::var("XDG_RUNTIME_DIR")
        .as_ref()
        .map(|s| Path::new(s).to_path_buf())
        .ok()
        .unwrap_or_else(|| {
            Path::new("/run")
                .join("user")
                .join(format!("{}", rustix::process::getuid()))
        });

    format!("unix:dir={}", runtime_dir.display())
}

#[cfg(not(unix))]
fn default_address() -> String {
    "tcp:host=127.0.0.1,port=4242".to_string()
}
