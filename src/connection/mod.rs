//! The bus's end of the connections of its clients.
//!
//! It holds:
//!
//! - the handshake that opens a client's connection ([`Connection::accept`]);
//! - the reader of a client's messages ([`Reader`]);
//! - the bus's writes to a client ([`Connection`]);
//! - on Linux, the hand-over of a client's connection to the next instance, and its take-over from
//!   the previous instance, in `Handover`.

#[cfg(target_os = "linux")]
pub mod handover;
#[cfg(target_os = "linux")]
pub use handover::Handover;
mod handshake;
mod read;
pub use read::Reader;
mod socket;
mod write;

use std::{
    io,
    net::Shutdown,
    os::fd::AsFd,
    pin::pin,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use futures_util::future::{select, Either};
use zbus::{names::OwnedUniqueName, Message, OwnedGuid};
use zruntime::{net::unix::UnixStream, Event, Shared, SharedRuntime};

use crate::{debug, warn};
use write::Outgoing;
#[cfg(target_os = "linux")]
use write::State;

/// The bus's end of a client's connection, for writing to the client.
///
/// The bus keeps no messages for a client except those of the sends that the client's socket hasn't
/// taken yet. A send queues its messages, whole, after those of the earlier sends, and waits until
/// the socket has taken their last byte, within the client's time limit. While it waits, it writes
/// whatever the socket takes, its own messages or those ahead of them. So the messages of each send
/// go out together, in the order of the sends. A taken-over connection also holds what the previous
/// instance left to write, and that goes out before the messages of any send.
///
/// The bus disconnects a client if the client doesn't take a send within its time limit, or if a
/// write to the client fails. The bus then writes nothing more to the client, drops what is left to
/// write, and shuts the socket down. The client's reader then yields what the socket still holds,
/// which the client sent before, and then EOF. The bus does the same to a client whose connection
/// it closes (`Connection::close`).
#[derive(Debug)]
pub struct Connection {
    socket: Arc<UnixStream<Shared>>,
    unique_name: OwnedUniqueName,
    fd_passing: bool,
    timeout: Duration,
    /// What is left to write to the client, and how the bus writes to it.
    outgoing: Mutex<Outgoing>,
    /// Notified when a write completes a message, and when the state changes, so that the sends
    /// that wait look again.
    changed: Event,
}

impl Connection {
    /// The two halves of the connection of a client that connected on `socket`, once the client has
    /// authenticated.
    ///
    /// The client authenticates to the bus of `guid` as the user `uid` that its socket reports,
    /// with the `EXTERNAL` mechanism, as with zbus's server. What the client sends after `BEGIN` is
    /// the start of its first message, and the `Reader` yields that first. The `Connection` gives
    /// the client `timeout` to take each send. The client goes by `unique_name`.
    ///
    /// The socket goes on the shared runtime of zruntime that runs the calling code.
    ///
    /// # Errors
    ///
    /// Fails if the runtime can't watch the socket, or if the handshake fails, such as on a line
    /// that holds no command of the protocol, or on EOF before `BEGIN`.
    pub async fn accept(
        socket: std::os::unix::net::UnixStream,
        guid: &OwnedGuid,
        uid: u32,
        unique_name: OwnedUniqueName,
        timeout: Duration,
    ) -> zbus::Result<(Reader, Self)> {
        let socket = Arc::new(UnixStream::from_std(&SharedRuntime::current()?, socket)?);
        let mut reader = Reader::new(socket.clone());
        let fd_passing = handshake::authenticate(&mut reader, &socket, guid, uid).await?;

        Ok((reader, Self::new(socket, unique_name, fd_passing, timeout)))
    }

    /// The two halves of a connection that the previous instance handed over in `handover`.
    ///
    /// The `Reader` yields the unread bytes that the previous instance handed over before what it
    /// reads itself. The `Connection` writes what the previous instance left to write before
    /// anything else, as part of the next send or flush. The client goes by `unique_name`, and the
    /// bus gives it `timeout` to take each send.
    ///
    /// The socket goes on the shared runtime of zruntime that runs the calling code.
    ///
    /// # Errors
    ///
    /// Fails if the runtime can't watch the socket.
    #[cfg(target_os = "linux")]
    pub fn taken_over(
        handover: Handover,
        unique_name: OwnedUniqueName,
        timeout: Duration,
    ) -> io::Result<(Reader, Self)> {
        let Handover {
            socket,
            fd_passing,
            unread,
            unread_fds,
            rest,
            kept,
        } = handover;
        let socket = std::os::unix::net::UnixStream::from(socket);
        let socket = Arc::new(UnixStream::from_std(&SharedRuntime::current()?, socket)?);
        let reader = Reader::taken_over(socket.clone(), unread, unread_fds);
        let outgoing = Outgoing::taken_over(rest, kept);

        Ok((
            reader,
            Self::with_outgoing(socket, unique_name, fd_passing, timeout, outgoing),
        ))
    }

    /// The connection of the client at the other end of `socket`.
    ///
    /// The client goes by `unique_name`, and takes file descriptors if `fd_passing`. The bus gives
    /// the client `timeout` to take each send.
    pub fn new(
        socket: Arc<UnixStream<Shared>>,
        unique_name: OwnedUniqueName,
        fd_passing: bool,
        timeout: Duration,
    ) -> Self {
        Self::with_outgoing(socket, unique_name, fd_passing, timeout, Outgoing::new())
    }

    /// Send `msgs`, in order, in one go, and return the messages that the client can't take.
    ///
    /// A client that didn't negotiate passing file descriptors can't take messages that carry FDs,
    /// so those messages aren't sent.
    ///
    /// This returns once the socket has taken the last byte of the messages, or once the bus has
    /// disconnected the client. The bus disconnects the client if the send fails, or if the client
    /// doesn't take the messages within its time limit. The time limit counts from the start of the
    /// send, including the wait for what was queued before it. A send that starts once the bus is
    /// disconnecting the client sends nothing and returns nothing. If the send is dropped, what it
    /// didn't write stays queued, and the sends after it write that first.
    pub async fn send(&self, msgs: &[Message]) -> Vec<Message> {
        let mut dropped = Vec::new();
        let target = {
            let mut outgoing = self.outgoing();
            if !outgoing.takes_messages() {
                return Vec::new();
            }
            for msg in msgs {
                if self.fd_passing || msg.data().fds().is_empty() {
                    outgoing.push(msg.clone());
                } else {
                    dropped.push(msg.clone());
                }
            }

            outgoing.queued
        };
        if !dropped.is_empty() {
            debug!(
                "Dropping {} of {} messages for `{}`, which have file descriptors that it takes \
                 none of",
                dropped.len(),
                msgs.len(),
                self.unique_name
            );
        }
        self.write_up_to(target, || format!("{} messages", msgs.len()))
            .await;

        dropped
    }

    /// Write what is left to write, as a send of nothing does.
    ///
    /// What is left can be, for example, what a dropped send didn't write. If this fails, or if the
    /// client doesn't take what is left within its time limit, the bus disconnects the client, as
    /// for a send.
    #[cfg(target_os = "linux")]
    pub async fn flush(&self) {
        let (target, unwritten) = {
            let outgoing = self.outgoing();
            if !outgoing.takes_messages() || outgoing.chunks.is_empty() {
                return;
            }

            (outgoing.queued, outgoing.unwritten_len())
        };
        self.write_up_to(target, || {
            format!("the {unwritten} bytes that were left to write to it")
        })
        .await;
    }

    /// From now on, no send waits for the client, including the sends that wait already.
    ///
    /// Each send writes what the socket takes at once, and leaves the rest for the connection's
    /// hand-over.
    #[cfg(target_os = "linux")]
    pub fn pause(&self) {
        {
            let mut outgoing = self.outgoing();
            if outgoing.state == State::Open {
                outgoing.state = State::Paused;
            }
        }
        self.changed.notify(usize::MAX);
    }

    /// The hand-over of the connection to the next instance, with the unread bytes of `reader`, the
    /// client's reader.
    ///
    /// This takes the connection and the reader, so the bus writes nothing more to the client, and
    /// reads nothing more from it. The socket closes with them, so the hand-over holds a duplicate
    /// of it. It also holds the unread bytes of `reader`, and what is left to write to the client,
    /// with the file descriptors of both.
    ///
    /// There is no hand-over if the bus is disconnecting the client, or if the client's socket
    /// fails to be duplicated. The socket closes then, which disconnects the client.
    #[cfg(target_os = "linux")]
    pub fn hand_over(self, reader: Reader) -> Option<Handover> {
        let outgoing = self
            .outgoing
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        if outgoing.state == State::Disconnecting {
            debug!(
                "Leaving `{}` out of the hand-over, as the bus is disconnecting it",
                self.unique_name
            );

            return None;
        }
        let socket = match self.socket.as_fd().try_clone_to_owned() {
            Ok(socket) => socket,
            Err(e) => {
                warn!(
                    "Disconnecting `{}`, as its connection failed to be handed over: {e}",
                    self.unique_name
                );

                return None;
            }
        };
        let (rest, kept) = outgoing.into_unwritten();
        let (unread, unread_fds) = reader.into_unread();

        Some(Handover {
            socket,
            fd_passing: self.fd_passing,
            unread,
            unread_fds,
            rest,
            kept,
        })
    }

    /// Disconnect the client: write nothing more to it, drop what is left to write, and shut its
    /// socket down, as when a write to it fails. The client's reader then yields what the socket
    /// still holds, which the client sent before, and then EOF.
    ///
    /// This does nothing if the bus is disconnecting the client already.
    #[cfg(target_os = "linux")]
    pub fn close(&self) {
        if self.outgoing().cut() {
            self.shut_down();
        }
    }

    /// Whether the bus is disconnecting the client because a write to it failed or timed out, or
    /// because the bus closed the connection.
    #[cfg(target_os = "linux")]
    pub fn is_disconnecting(&self) -> bool {
        self.outgoing().state == State::Disconnecting
    }

    /// How many bytes are left to write to the client.
    #[cfg(target_os = "linux")]
    pub fn unwritten_len(&self) -> usize {
        self.outgoing().unwritten_len()
    }

    /// The unique name of the client.
    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    /// How long the bus gives the client to take each send.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The connection of the client at the other end of `socket`, as [`Connection::new`] makes it,
    /// with `outgoing` to write.
    fn with_outgoing(
        socket: Arc<UnixStream<Shared>>,
        unique_name: OwnedUniqueName,
        fd_passing: bool,
        timeout: Duration,
        outgoing: Outgoing,
    ) -> Self {
        Self {
            socket,
            unique_name,
            fd_passing,
            timeout,
            outgoing: Mutex::new(outgoing),
            changed: Event::new(),
        }
    }

    /// Write what was queued up to `target`, of the bytes ever queued, within the client's time
    /// limit.
    ///
    /// If writing `what` to the client fails or takes longer, disconnect the client.
    async fn write_up_to<W>(&self, target: u64, what: W)
    where
        W: FnOnce() -> String,
    {
        let failure = match self.write_now(target) {
            // Most sends end here, since the socket takes their messages at once, with no timer.
            Ok(true) => return,
            Ok(false) => {
                let runtime = self.socket.runtime();
                match runtime
                    .timeout(self.timeout, self.until_written(target))
                    .await
                {
                    Ok(Ok(())) => return,
                    Ok(Err(e)) => Failure::Error(e),
                    Err(_) => Failure::Timeout,
                }
            }
            Err(e) => Failure::Error(e),
        };
        self.disconnect(failure, &what());
    }

    /// Write what the socket takes now, and return whether the bytes up to `target` leave nothing
    /// more to wait for.
    ///
    /// This writes straight to the socket, rather than once the runtime finds it writable.
    fn write_now(&self, target: u64) -> io::Result<bool> {
        let (completed, done) = {
            let mut outgoing = self.outgoing();
            let completed = match outgoing.write(self.socket.as_fd()) {
                Ok(completed) => completed,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
                Err(e) => return Err(e),
            };

            (completed, outgoing.done(target))
        };
        if completed {
            self.changed.notify(usize::MAX);
        }

        Ok(done)
    }

    /// Wait until the bytes up to `target` are written, or leave nothing more to wait for.
    ///
    /// Meanwhile, this writes what the socket takes, as soon as it takes more. The runtime wakes
    /// the sends that wait for the socket when it finds the socket writable. It doesn't wake them
    /// when another send writes to the socket, and that send may write what this waits for and
    /// leave the socket full again. So each write that completes a message, and each change of the
    /// state, also wakes the waiting sends, through `changed`.
    async fn until_written(&self, target: u64) -> io::Result<()> {
        loop {
            // The listener comes first, so that a change right after the look isn't missed.
            let changed = self.changed.listen();
            if self.outgoing().done(target) {
                return Ok(());
            }
            let writing = self.socket.write_with(|stream| {
                let mut outgoing = self.outgoing();
                // Since the look above, another send may have written what this waits for, or the
                // state may have changed. There is nothing to wait for then.
                if outgoing.done(target) {
                    return Ok(false);
                }

                outgoing.write(stream.as_fd())
            });
            match select(pin!(writing), changed).await {
                Either::Left((Ok(true), _)) => {
                    self.changed.notify(usize::MAX);
                }
                Either::Left((Ok(false), _)) | Either::Right(_) => (),
                Either::Left((Err(e), _)) => return Err(e),
            }
        }
    }

    /// Disconnect the client because writing `what` to it failed or timed out, as `failure` says.
    ///
    /// This writes nothing more to the client, drops what is left to write, and shuts the client's
    /// socket down. It does nothing if the bus is disconnecting the client already, as when another
    /// write got there first.
    fn disconnect(&self, failure: Failure, what: &str) {
        if !self.outgoing().cut() {
            return;
        }
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
        self.shut_down();
    }

    /// Shut the client's socket down, and have the sends that wait look again.
    ///
    /// The client's reader then reaches EOF, once it has taken what the socket still holds.
    fn shut_down(&self) {
        if let Err(e) = self.socket.shutdown(Shutdown::Both) {
            debug!(
                "Failed to shut the socket of `{}` down: {e}",
                self.unique_name
            );
        }
        self.changed.notify(usize::MAX);
    }

    /// What is left to write to the client.
    fn outgoing(&self) -> MutexGuard<'_, Outgoing> {
        // Nothing can panic while the lock is held.
        self.outgoing.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Why a write to a client failed.
enum Failure {
    Timeout,
    Error(io::Error),
}

#[cfg(test)]
mod tests;
