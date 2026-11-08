use std::{
    io,
    pin::pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use zbus::{names::OwnedUniqueName, Connection, Message};
use zruntime::{lock::Mutex, SharedRuntime};
#[cfg(target_os = "linux")]
use zruntime::{spawn, Shared, Task};

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
///
/// While the bus hands over to its next instance, it doesn't wait for room: zbus keeps what the
/// socket doesn't take, which goes on with the hand-over of the connection, and the next instance
/// writes that to the client first, with [`Recipient::flush`].
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

    /// Write what the connection of the client took to send and didn't write yet, such as what the
    /// instance of the bus that handed the client over sent it and it didn't take, in turn with the
    /// sends to the client, in a task of its own: none if nothing is left to write, nor for the bus
    /// itself.
    ///
    /// A client that this fails to, or that doesn't take it within its time limit, gets
    /// disconnected, as when a send to it fails, or times out. Dropping the task stops the write:
    /// what it didn't write is left for the next send to the client to write first, or for the
    /// hand-over of the connection to hand over.
    #[cfg(target_os = "linux")]
    pub(crate) fn flush(&self) -> Option<Task<(), Shared>> {
        let Kind::Client(client) = &self.0 else {
            return None;
        };
        if client.conn.unwritten_len() == 0 {
            return None;
        }
        let client = client.clone();

        Some(spawn(async move { client.flush().await }))
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
}

impl Client {
    /// Send `msgs`, and return those that the client can't take, for their file descriptors.
    async fn send(&self, msgs: &[Message]) -> Vec<Message> {
        if self.disconnecting.load(Ordering::SeqCst) {
            return Vec::new();
        }
        let (failure, first) = {
            let send = pin!(self.runtime.timeout(self.timeout, self.send_in_turn(msgs)));
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
        self.disconnect(failure, &format!("{} messages", msgs.len()))
            .await;

        Vec::new()
    }

    /// Write what the connection has left to write, in turn with the sends, within the time limit
    /// of the client, and disconnect the client if that fails, or takes longer, as a send does.
    #[cfg(target_os = "linux")]
    async fn flush(&self) {
        if self.disconnecting.load(Ordering::SeqCst) {
            return;
        }
        let unwritten = self.conn.unwritten_len();
        let (failure, first) = {
            let flush = pin!(self.runtime.timeout(self.timeout, self.flush_in_turn()));
            let failure = match flush.await {
                Ok(Ok(())) => return,
                Ok(Err(e)) => Failure::Error(e),
                Err(_) => Failure::Timeout,
            };
            // As for a send: see `Client::send`.
            let first = !self.disconnecting.swap(true, Ordering::SeqCst);

            (failure, first)
        };
        // Another send got here first, and disconnects the client.
        if !first {
            return;
        }
        let what = format!("the {unwritten} bytes that were left to write to it");
        self.disconnect(failure, &what).await;
    }

    /// Disconnect the client, as sending it `what` failed, or timed out, as `failure` says.
    async fn disconnect(&self, failure: Failure, what: &str) {
        match failure {
            Failure::Timeout => warn!(
                "Disconnecting `{}`, as it didn't take {what} within {:?}",
                self.unique_name, self.timeout
            ),
            Failure::Error(e) => debug!(
                "Disconnecting `{}`, as sending {what} to it failed: {e}",
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

    /// Write what the connection has left to write once no send to the client is under way, unless
    /// the client is being disconnected by then.
    #[cfg(target_os = "linux")]
    async fn flush_in_turn(&self) -> zbus::Result<()> {
        let _turn = self.sending.lock().await;
        if self.disconnecting.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.conn.flush().await
    }
}

/// Why a send to a client failed.
enum Failure {
    Timeout,
    Error(zbus::Error),
}
