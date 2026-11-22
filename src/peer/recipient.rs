use std::{rc::Rc, time::Duration};

use zbus::{names::OwnedUniqueName, Message};
#[cfg(target_os = "linux")]
use zruntime::{spawn_local, Local, Task};

use crate::{connection::Connection, fdo::Calls};

/// Where the bus sends the messages for a peer or a monitor, and the calls to the bus itself.
///
/// The calls to the bus go to the queue of the task that answers them.
///
/// It is cheap to clone, so that the bus can pick the recipients of messages while it holds its
/// locks, and send the messages to them once it has let go of those. The clones share the
/// connection of the client, which a peer hands on to the monitor that it becomes.
///
/// The bus keeps no messages for a client except those of the sends that its socket hasn't taken
/// yet: it writes the messages to the client's socket as it forwards them, those that it forwards
/// to the client together in one go, in the order of the sends, and waits for room there if need
/// be. A client that keeps messages waiting for longer than its time limit, [`SEND_TIMEOUT`] by
/// default, gets disconnected. So what is limited is how long a client that doesn't read holds up
/// the messages for it, rather than how many bytes of them may pile up for it, as with the quotas
/// of dbus-broker. dbus-broker disconnects a client that can't take a reply or a signal that it
/// expects, as the bus does, but it rejects a method call that the callee can't take, with
/// `LimitsExceeded`, and keeps the callee. The bus disconnects the callee then too, as a send that
/// times out may leave part of a message on the socket.
///
/// While the bus hands over to its next instance, it doesn't wait for room: the connection of the
/// client keeps what the socket doesn't take, which goes on with the hand-over of the connection,
/// and the next instance writes that to the client first, with `Recipient::flush`.
#[derive(Debug, Clone)]
pub struct Recipient(Kind);

impl Recipient {
    /// A client on `connection`, which the bus gives the time limit of the connection to take each
    /// message.
    pub fn client(connection: Rc<Connection>) -> Self {
        Self(Kind::Client(connection))
    }

    /// The bus itself, which answers the calls that are queued in `calls`.
    ///
    /// A call to the bus waits for room in the queue for as long as it takes, which holds back a
    /// client that calls the bus faster than the bus can answer. That wait has no time limit of its
    /// own, as the bus never gets disconnected, but it is bounded all the same: the bus only ever
    /// waits on a client within the time limit of that client. The bus takes no other messages.
    pub(crate) fn bus(calls: Calls) -> Self {
        Self(Kind::Bus(calls))
    }

    /// Take no more calls, if this is the bus itself, as the bus hands over.
    ///
    /// The bus answers the calls in its queue, and stops. For a client, this does nothing, since
    /// the client's connection goes with the client.
    #[cfg(target_os = "linux")]
    pub(crate) fn hand_over(&self) {
        if let Kind::Bus(calls) = &self.0 {
            calls.close();
        }
    }

    /// Write what the connection of the client took to send and didn't write yet, such as what the
    /// instance of the bus that handed the client over sent it and it didn't take, ahead of what
    /// the bus sends the client after it, in a task of its own: none if nothing is left to write,
    /// nor for the bus itself.
    ///
    /// A client that this fails to, or that doesn't take it within its time limit, gets
    /// disconnected, as when a send to it fails, or times out. Dropping the task stops the write:
    /// what it didn't write is left for the next send to the client to write first, or for the
    /// hand-over of the connection to hand over.
    #[cfg(target_os = "linux")]
    pub(crate) fn flush(&self) -> Option<Task<(), Local>> {
        let Kind::Client(connection) = &self.0 else {
            return None;
        };
        if connection.unwritten_len() == 0 {
            return None;
        }
        let connection = connection.clone();

        Some(spawn_local(async move { connection.flush().await }))
    }

    /// Send `msgs`, in order, in one go: with as few writes as the socket takes.
    ///
    /// A client that a send to fails, or doesn't take the messages within its time limit, gets
    /// disconnected: the end of its connection has the bus remove it, along with its names. The
    /// messages with file descriptors, for a client that didn't negotiate passing them, are dropped
    /// instead, as the client can still take the others, and returned.
    pub async fn send(&self, msgs: &[Message]) -> Vec<Message> {
        match &self.0 {
            Kind::Client(connection) => connection.send(msgs).await,
            Kind::Bus(calls) => {
                calls.queue(msgs).await;

                Vec::new()
            }
        }
    }

    /// The unique name of the client, none for the bus itself.
    pub fn unique_name(&self) -> Option<&OwnedUniqueName> {
        match &self.0 {
            Kind::Client(connection) => Some(connection.unique_name()),
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
    Client(Rc<Connection>),
    Bus(Calls),
}
