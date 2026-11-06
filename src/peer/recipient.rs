use std::{
    io,
    pin::pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
#[cfg(target_os = "linux")]
use std::{
    sync::{self, PoisonError},
    time::Instant,
};

use zbus::{names::OwnedUniqueName, Connection, Message};
use zruntime::{lock::Mutex, SharedRuntime};

use crate::{debug, warn};

/// Where the bus sends the messages for a peer, or a monitor.
///
/// It is cheap to clone, so that the bus can pick the recipients of messages while it holds its
/// locks, and send the messages to them once it has let go of those. The clones share the state of
/// the peer's connection, which a peer hands on to the monitor that it becomes.
///
/// The bus keeps no queue of messages for a client: it writes the messages to the client's socket
/// as it forwards them, those that it forwards to the client together in one go, and waits for
/// room there if need be. A client that keeps messages waiting for longer than its time limit,
/// [`SEND_TIMEOUT`] by default, gets disconnected. So what is limited is how long a client that
/// doesn't read holds up the messages for it, rather than how many bytes of them may pile up for
/// it, as with the quotas of dbus-broker. dbus-broker disconnects a client that can't take a reply
/// or a signal that it expects, as the bus does, but it rejects a method call that the callee can't
/// take, with `LimitsExceeded`, and keeps the callee. The bus disconnects the callee then too, as a
/// send that times out may leave part of a message on the socket.
#[derive(Debug, Clone)]
pub struct Recipient(Kind);

impl Recipient {
    /// A client on `conn`, which the bus gives `timeout` to take each message.
    pub fn client(
        conn: Connection,
        unique_name: OwnedUniqueName,
        timeout: Duration,
    ) -> io::Result<Self> {
        let client = Client {
            conn,
            unique_name,
            runtime: SharedRuntime::current()?,
            timeout,
            sending: Mutex::new(()),
            disconnecting: AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            deadline: sync::Mutex::new(None),
        };

        Ok(Self(Kind::Client(Arc::new(client))))
    }

    /// The bus itself, on `conn`.
    ///
    /// A message for the bus waits for room on its connection for as long as it takes, which
    /// holds back a client that sends to the bus faster than the bus can keep up. That wait has
    /// no time limit of its own, as the bus never gets disconnected, but it is bounded all the
    /// same: the bus only ever waits on a client within the time limit of that client.
    pub fn bus(conn: Connection) -> Self {
        Self(Kind::Bus(conn))
    }

    /// Stop sending to the client, without disconnecting it, as the bus handed the client over to
    /// its next instance, which sends it whatever comes next.
    #[cfg(target_os = "linux")]
    pub(crate) fn hand_over(&self) {
        if let Kind::Client(client) = &self.0 {
            client.disconnecting.store(true, Ordering::SeqCst);
        }
    }

    /// Give the client as long as it has to take a message, from `since` on, to take all that the
    /// bus sends it from then on, as the bus hands over from `since`: a client that doesn't read
    /// then holds the hand-over up for no longer than that, rather than for that long each time
    /// that the bus sends it something.
    #[cfg(target_os = "linux")]
    pub(crate) fn hand_over_from(&self, since: Instant) {
        if let Kind::Client(client) = &self.0 {
            *client
                .deadline
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = since.checked_add(client.timeout);
        }
    }

    /// How long the bus gives the client to take each message; none for the bus itself.
    #[cfg(target_os = "linux")]
    pub(crate) fn send_timeout(&self) -> Option<Duration> {
        match &self.0 {
            Kind::Client(client) => Some(client.timeout),
            Kind::Bus(_) => None,
        }
    }

    /// Whether the bus is disconnecting the client, as a send to it failed or timed out.
    #[cfg(target_os = "linux")]
    pub(crate) fn disconnecting(&self) -> bool {
        match &self.0 {
            Kind::Client(client) => client.disconnecting.load(Ordering::SeqCst),
            Kind::Bus(_) => false,
        }
    }

    /// Send `msgs`, in order, in one go: with as few writes as the socket takes.
    ///
    /// A client that a send to fails, or doesn't take the messages within its time limit, gets
    /// disconnected: the end of its connection has the bus remove it, along with its names. The
    /// messages with file descriptors, for a client that didn't negotiate passing them, are dropped
    /// instead, as the client can still take the others, and returned.
    pub async fn send(&self, msgs: &[Message]) -> Vec<Message> {
        match &self.0 {
            Kind::Client(client) => client.send(msgs).await,
            Kind::Bus(conn) => {
                if let Err(e) = conn.send_all(msgs).await {
                    warn!("Error sending messages to the bus: {}", e);
                }

                Vec::new()
            }
        }
    }

    /// The unique name of the client, none for the bus itself.
    pub fn unique_name(&self) -> Option<&OwnedUniqueName> {
        match &self.0 {
            Kind::Client(client) => Some(&client.unique_name),
            Kind::Bus(_) => None,
        }
    }
}

/// How long the bus waits for a client to take the messages that it sends it in one go, by
/// default, before it disconnects the client.
///
/// The wait counts from when the bus starts to send the messages, so it covers the wait for the
/// other messages on their way to the client, as well as the write of these. The bus forwards the
/// messages of each sender in batches, of up to [`MAX_BATCH`] messages and [`MAX_BATCH_BYTES`]
/// bytes but for the last message, and sends a client in one go those of a batch that go to it.
/// The wait is for room on the client's socket, for the most part, so a client that stops reading
/// only runs into this limit if the messages for it fill its socket, and it doesn't take them
/// within this long of when the bus started to send them.
///
/// This is long, so that a client that gets stuck for a few seconds, as under heavy I/O, catches
/// up and keeps its connection, rather than losing it, along with its names and its match rules.
/// Even the largest messages, of 128 MiB, only take reading at 12.8 MiB/s in this time. The cost
/// is how long a client that stops reading holds up others before it gets disconnected: the bus
/// forwards the batches of each sender one after the other, so one that is subscribed to the
/// signals of the bus holds up every message of the bus, its replies included, for this long, and
/// a monitor holds up every sender that it watches. Several such clients that stop taking messages
/// at the same batch hold it up for this long all together, as the bus sends a batch to all of its
/// recipients at once, while those that stop at different batches hold it up for this long each.
/// One such holdup still ends well within the 25 s that a method call waits for its reply by
/// default, with libdbus, GDBus and sd-bus.
///
/// [`MAX_BATCH`]: crate::peers::MAX_BATCH
/// [`MAX_BATCH_BYTES`]: crate::peers::MAX_BATCH_BYTES
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
enum Kind {
    Client(Arc<Client>),
    Bus(Connection),
}

/// The connection of a client, as the bus sends messages on it.
#[derive(Debug)]
struct Client {
    conn: Connection,
    unique_name: OwnedUniqueName,
    runtime: SharedRuntime,
    timeout: Duration,
    /// Held by each send, from its wait for the connection to the end of its write, so that the
    /// sends to the client go one at a time.
    sending: Mutex<()>,
    /// Whether the client is being disconnected, as a send to it failed or timed out, or the bus
    /// handed it over.
    ///
    /// Such a send may have written part of its message, so nothing more is written to the
    /// client. A send checks this once its turn comes, which is only once the send that failed
    /// has let go of the connection.
    disconnecting: AtomicBool,
    /// When the client has to have taken what the bus sends it by, at the latest, if the bus hands
    /// over.
    #[cfg(target_os = "linux")]
    deadline: sync::Mutex<Option<Instant>>,
}

impl Client {
    /// Send `msgs`, and return those that the client can't take, for their file descriptors.
    async fn send(&self, msgs: &[Message]) -> Vec<Message> {
        if self.disconnecting.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let timeout = self.timeout();
        let (failure, first) = {
            let send = pin!(self.runtime.timeout(timeout, self.send_in_turn(msgs)));
            let failure = match send.await {
                Ok(Ok(dropped)) => return dropped,
                Ok(Err(e)) => Failure::Error(e),
                Err(_) => Failure::Timeout,
            };
            // A send that timed out is still there, with whatever part of its message it wrote,
            // and it may hold the connection until the end of this block. Flag the client before
            // it lets go, so that no other send writes after that part.
            let first = !self.disconnecting.swap(true, Ordering::SeqCst);

            (failure, first)
        };
        // Another send got here first, and disconnects the client.
        if !first {
            return Vec::new();
        }
        match failure {
            Failure::Timeout => warn!(
                "Disconnecting `{}`, as it didn't take {} messages within {:?}",
                self.unique_name,
                msgs.len(),
                timeout
            ),
            Failure::Error(e) => debug!(
                "Disconnecting `{}`, as sending messages to it failed: {e}",
                self.unique_name
            ),
        }
        // Closing the connection ends the stream of the messages from the client, and the bus
        // removes the client then. It waits for the send that holds the connection, if any, which
        // its own time limit bounds.
        if let Err(e) = self.conn.clone().close().await {
            debug!(
                "Failed to close the connection of `{}`: {e}",
                self.unique_name
            );
        }

        Vec::new()
    }

    /// How long a send that starts now may take: the time limit of the client, or what is left of
    /// it for the hand-over, if the bus hands over.
    fn timeout(&self) -> Duration {
        #[cfg(target_os = "linux")]
        if let Some(deadline) = *self.deadline.lock().unwrap_or_else(PoisonError::into_inner) {
            return self
                .timeout
                .min(deadline.saturating_duration_since(Instant::now()));
        }

        self.timeout
    }

    /// Send `msgs` once no other send to the client is under way, unless the client is being
    /// disconnected by then, and return those that the client can't take, for their file
    /// descriptors.
    async fn send_in_turn(&self, msgs: &[Message]) -> zbus::Result<Vec<Message>> {
        let _turn = self.sending.lock().await;
        if self.disconnecting.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }

        match self.conn.send_all(msgs).await {
            // Some of the messages have file descriptors, and the client didn't negotiate passing
            // them. zbus finds that before it writes anything, so the others can still go.
            Err(zbus::Error::Unsupported) => {
                let (dropped, without_fds): (Vec<_>, Vec<_>) = msgs
                    .iter()
                    .cloned()
                    .partition(|msg| !msg.data().fds().is_empty());
                debug!(
                    "Dropping {} of {} messages for `{}`, which have file descriptors that it \
                     takes none of",
                    dropped.len(),
                    msgs.len(),
                    self.unique_name
                );

                self.conn.send_all(&without_fds).await.map(|()| dropped)
            }
            result => result.map(|()| Vec::new()),
        }
    }
}

/// Why a send to a client failed.
enum Failure {
    Timeout,
    Error(zbus::Error),
}
