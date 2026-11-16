use crate::{debug, trace, warn};
use anyhow::{bail, Context, Ok, Result};
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::{
    env,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
    time::Duration,
};
use zbus::{
    address::Transport,
    connection::{self, socket::Channel},
    fdo::ConnectionCredentials,
    Address, Connection, Guid, OwnedGuid,
};
use zruntime::spawn;

#[cfg(target_os = "linux")]
pub(crate) mod handover;
mod listener;
#[cfg(target_os = "linux")]
pub use crate::peers::handover::{ClientHandover, OwnHandover};
#[cfg(target_os = "linux")]
pub use handover::{Handover, STOP_TIMEOUT};
use listener::Listener;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(target_os = "linux")]
pub use systemd::STORE_KEY;

use crate::{
    fdo::{self, DBus, Monitoring, Stats},
    peer::SEND_TIMEOUT,
    peers::{Peers, Us},
    policy::Policy,
};

/// The bus.
///
/// The bus, and every connection it serves, runs on [zruntime]. Create and run it from inside
/// [`zruntime::block_on`], so that the calling thread runs all of them, on its own runtime.
///
/// [zruntime]: https://docs.rs/zruntime
#[derive(Debug)]
pub struct Bus {
    listener: Listener,
    address: Address,
    peers: Peers,
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
        let listener = Listener::bind(unix).await?;

        Self::new(listener, guid).await
    }

    /// Create a bus that listens on `listener`, a socket that listens already, such as the one that
    /// systemd's socket activation passes.
    ///
    /// The bus gives the address of the socket out as its own, with a GUID of its own. The socket
    /// stays with whoever passed it: the bus doesn't remove its file as it goes.
    #[cfg(target_os = "linux")]
    pub async fn for_listener(listener: OwnedFd) -> Result<Self> {
        let listener = Listener::from_fd(listener, false)?;

        Self::new(listener, Guid::generate().into()).await
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Set how long the bus waits for a client to take a message, before it disconnects the client.
    ///
    /// It is [`SEND_TIMEOUT`] unless set otherwise, or given to `Bus::take_over`, and applies to
    /// the clients that connect from then on.
    pub fn set_send_timeout(&mut self, timeout: Duration) {
        self.send_timeout = timeout;
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
        self.peers.users().set_max_objects(max);
    }

    /// Set the policy that the bus goes by: who may connect, own which names, and send and receive
    /// which messages.
    ///
    /// It is the open policy of [`Policy::default`] unless set otherwise, or given to
    /// `Bus::take_over`, and applies to the clients that connect from then on, and to those that
    /// the bus has.
    pub async fn set_policy(&mut self, policy: Arc<Policy>) {
        self.peers.set_policy(policy).await;
    }

    /// Have `ReloadConfig` set the policy that `read` returns, as [`Bus::set_policy`] does.
    ///
    /// `read` reads the configuration of the bus again, on a thread of its own, as that may block:
    /// the bus goes on forwarding the messages of its peers meanwhile, but the calls to the bus
    /// itself wait for the reload, as the bus answers them one at a time. If `read` fails, so does
    /// the call, and the bus goes on by the policy that it has. Until the bus is told how to read
    /// its policy again, it has nothing to reload, and the call fails.
    pub async fn set_reload<F>(&mut self, read: F)
    where
        F: Fn() -> Result<Policy> + Send + Sync + 'static,
    {
        self.peers.set_reload(read).await;
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            self.accept_next().await?;
        }
    }

    // AsyncDrop would have been nice!
    pub async fn cleanup(self) -> Result<()> {
        self.listener.remove().await.map_err(Into::into)
    }

    pub fn peers(&self) -> &Peers {
        &self.peers
    }

    pub fn guid(&self) -> &OwnedGuid {
        &self.guid
    }

    /// Create a bus with `guid` that listens on `listener`.
    async fn new(listener: Listener, guid: OwnedGuid) -> Result<Self> {
        let address = listener.address(&guid)?;
        let peers = Peers::new();
        let us = Self::us(
            &peers,
            &guid,
            #[cfg(target_os = "linux")]
            None,
        )
        .await?;
        let service_conn = peers.add_us(us).await?;
        trace!("Self-dial connection created.");

        Ok(Self::with_peers(
            listener,
            address,
            guid,
            peers,
            service_conn,
        ))
    }

    /// Create a bus with `guid` that listens on `listener` at `address`. `peers` are its peers,
    /// the bus itself among them, and `service_conn` is the connection on which it serves the
    /// D-Bus API.
    fn with_peers(
        listener: Listener,
        address: Address,
        guid: OwnedGuid,
        peers: Peers,
        service_conn: Connection,
    ) -> Self {
        Self {
            listener,
            address,
            peers,
            guid,
            next_id: 0,
            send_timeout: SEND_TIMEOUT,
            _self_conn: service_conn,
        }
    }

    /// The connection on which a bus of `guid` serves the D-Bus API to `peers`, as a peer of its
    /// own, for the bus to be added to them: a new one, or the one that `own` hands over, with the
    /// calls to the bus that the instance before this one didn't answer, and what it sent and
    /// didn't pass on.
    ///
    /// The bus talks to itself over a channel, which is in this process alone.
    pub(crate) async fn us(
        peers: &Peers,
        guid: &OwnedGuid,
        #[cfg(target_os = "linux")] own: Option<OwnHandover>,
    ) -> Result<Us> {
        trace!("Creating self-dial connection.");
        #[cfg(target_os = "linux")]
        let handed_over = own.map(|OwnHandover { service, peer }| {
            let take_over = connection::Builder::take_over;

            (take_over(service), take_over(peer))
        });
        #[cfg(not(target_os = "linux"))]
        let handed_over = None;
        let (service, peer) = handed_over.unwrap_or_else(|| {
            let (service, peer) = Channel::pair();
            let end = |socket| connection::Builder::authenticated_socket(socket, guid.clone());

            (
                end(service).p2p().unique_name(fdo::BUS_NAME),
                end(peer).p2p(),
            )
        });
        let stream = peer.build_message_stream().await?;
        let service = service
            .name(fdo::BUS_NAME)
            .serve_at(fdo::DBus::PATH, DBus::new(peers, guid.clone()))
            .serve_at(fdo::Monitoring::PATH, Monitoring::new(peers))
            .serve_at(fdo::Stats::PATH, Stats::new(peers));

        Ok(Us { service, stream })
    }

    async fn accept_next(&mut self) -> Result<()> {
        let stream = self.listener.accept().await?;
        debug!("Accepted connection on address `{}`", self.address);

        let id = self.next_id();
        let peers = self.peers.clone();
        let guid = self.guid.clone();
        let send_timeout = self.send_timeout;
        spawn(async move {
            let res = async {
                // The client is an object of its user from the start, before its handshake, so
                // its user is the one that its socket tells, as zbus looks it up for a connection
                // over the socket, for a client in another PID namespace too.
                let creds = ConnectionCredentials::of_peer(&stream).await?;
                let uid = creds.unix_user_id().context("The client has no user")?;
                let gids = creds.unix_group_ids().map_or(&[][..], Vec::as_slice);
                // Dropping the stream closes the connection.
                if !peers.may_connect(uid, gids).await {
                    warn!(
                        "Refusing a client of the user {uid}, as the policy does not let it \
                         connect"
                    );

                    return Ok(());
                }
                let builder = connection::Builder::unix_stream(stream);

                peers.add(&guid, id, builder, send_timeout, uid).await
            };
            if let Err(e) = res.await {
                warn!("Failed to establish connection: {}", e);
            }
        })
        .detach();

        Ok(())
    }

    fn next_id(&mut self) -> usize {
        self.next_id += 1;

        self.next_id
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        self.peers.remove_us();
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
