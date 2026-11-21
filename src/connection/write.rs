//! What is left to write to a client, and how the bus groups it into writes.

use std::{
    collections::VecDeque,
    io::{self, IoSlice},
    os::fd::{AsFd, BorrowedFd},
};

use zbus::Message;

use super::socket;

/// What is left to write to a client, in the order that it goes out, and how much of what the bus
/// queued was written.
#[derive(Debug)]
pub(super) struct Outgoing {
    /// What is left to write, oldest first.
    pub(super) chunks: VecDeque<Chunk>,
    /// How much of the first chunk is written.
    pub(super) front_written: usize,
    /// How many bytes were ever queued, and how many were written.
    ///
    /// A send waits until `written` reaches what `queued` was once its messages were queued.
    pub(super) queued: u64,
    pub(super) written: u64,
    pub(super) state: State,
}

impl Outgoing {
    /// Nothing to write, to a client that the bus writes to.
    pub(super) fn new() -> Self {
        Self {
            chunks: VecDeque::new(),
            front_written: 0,
            queued: 0,
            written: 0,
            state: State::Open,
        }
    }

    /// What the previous instance left to write to a client, to write before anything else.
    ///
    /// That is `rest`, the rest of a partly written message, if there is one, and then `kept`, the
    /// whole messages, with their file descriptors.
    #[cfg(target_os = "linux")]
    pub(super) fn taken_over(rest: Vec<u8>, kept: Vec<Message>) -> Self {
        let mut outgoing = Self::new();
        if !rest.is_empty() {
            outgoing.queued += rest.len() as u64;
            outgoing.chunks.push_back(Chunk::Rest(rest));
        }
        for msg in kept {
            outgoing.push(msg);
        }

        outgoing
    }

    /// Queue `msg`, whole, after what is queued already.
    pub(super) fn push(&mut self, msg: Message) {
        self.queued += msg.data().len() as u64;
        self.chunks.push_back(Chunk::Message(msg));
    }

    /// What is left to write, for the next instance to write.
    ///
    /// That is the rest of the message at the front, if a write ended in the middle of it, or if
    /// the front is the rest of a message already. The rest goes without the message's file
    /// descriptors, which went with its first byte. Then come the whole messages, with their FDs.
    #[cfg(target_os = "linux")]
    pub(super) fn into_unwritten(self) -> (Vec<u8>, Vec<Message>) {
        let Self {
            mut chunks,
            front_written: written,
            ..
        } = self;
        let rest = match chunks.pop_front() {
            Some(Chunk::Rest(mut rest)) => {
                rest.drain(..written);

                rest
            }
            Some(Chunk::Message(msg)) if written > 0 => msg.data().bytes()[written..].to_vec(),
            Some(front) => {
                chunks.push_front(front);

                Vec::new()
            }
            None => Vec::new(),
        };
        let kept = chunks
            .into_iter()
            .filter_map(|chunk| match chunk {
                Chunk::Message(msg) => Some(msg),
                // Only the first chunk is ever the rest of a message.
                Chunk::Rest(_) => None,
            })
            .collect();

        (rest, kept)
    }

    /// Whether a send of the bytes up to `target`, of those ever queued, has nothing more to wait
    /// for.
    ///
    /// That is so when they are written, when the bus writes nothing more to the client, or when it
    /// waits for nothing.
    pub(super) fn done(&self, target: u64) -> bool {
        self.written >= target || self.state != State::Open
    }

    /// Write to `socket` what it takes now, from the front of the queue, and return whether a chunk
    /// got done.
    ///
    /// The chunks go in writes as [`Outgoing::next_write`] groups them, until the socket takes no
    /// more. This writes nothing unless messages are still written ([`Outgoing::takes_messages`]).
    /// It fails with [`io::ErrorKind::WouldBlock`] if the socket took nothing at all, so that the
    /// caller waits for room, and with the error of a write that fails.
    pub(super) fn write(&mut self, socket: BorrowedFd<'_>) -> io::Result<bool> {
        if !self.takes_messages() {
            return Ok(false);
        }
        let mut done = false;
        let mut wrote = false;
        while !self.chunks.is_empty() {
            let sent = {
                let (len, with_fds) = self.next_write();
                let mut bufs = [IoSlice::new(&[]); MESSAGES_PER_WRITE];
                for (i, (buf, chunk)) in bufs.iter_mut().zip(&self.chunks).take(len).enumerate() {
                    let from = if i == 0 { self.front_written } else { 0 };
                    *buf = IoSlice::new(&chunk.bytes()[from..]);
                }
                let fds = match self.chunks.front() {
                    Some(front) if with_fds => front.fds(),
                    _ => Vec::new(),
                };

                socket::send(socket, &bufs[..len], &fds)
            };
            let sent = match sent {
                Ok(sent) => sent,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && wrote => break,
                Err(e) => return Err(e),
            };
            wrote = true;
            done |= self.advance(sent);
        }

        Ok(done)
    }

    /// Stop writing, drop what is left to write, file descriptors and all, and return whether this
    /// did.
    ///
    /// This does nothing if the bus stopped writing already, as when another write got there first.
    /// The bus stops writing this way when a write fails or times out, and when it closes the
    /// connection.
    pub(super) fn cut(&mut self) -> bool {
        if !self.takes_messages() {
            return false;
        }
        self.state = State::Disconnecting;
        self.chunks.clear();
        self.front_written = 0;

        true
    }

    /// Whether messages are still queued and written.
    ///
    /// They aren't once a write failed or timed out, or once the bus closed the connection.
    pub(super) fn takes_messages(&self) -> bool {
        match self.state {
            State::Open => true,
            #[cfg(target_os = "linux")]
            State::Paused => true,
            State::Disconnecting => false,
        }
    }

    /// How many bytes are left to write.
    #[cfg(target_os = "linux")]
    pub(super) fn unwritten_len(&self) -> usize {
        let queued: usize = self.chunks.iter().map(|chunk| chunk.bytes().len()).sum();

        queued - self.front_written
    }

    /// The chunks that the next write takes off the front of the queue: how many, from
    /// `front_written` on in the first, and whether the first one's file descriptors go along.
    ///
    /// The write groups the chunks much as zbus groups messages into writes. It starts at the front
    /// and adds the messages after it that carry no file descriptors, up to `MESSAGES_PER_WRITE`
    /// chunks in all, until it has `BYTES_PER_WRITE` bytes or more. A message with FDs goes in its
    /// own writes, and the first of those carries the FDs with the message's first byte. Some
    /// readers, dbus-broker among them, give the FDs that a read returns to the message that the
    /// read ends in. Those FDs belong to that message only if the write holds nothing else. The
    /// rest of a partly written message goes without FDs.
    ///
    /// zbus writes the rest of its partly written messages before it groups the messages after
    /// them. This regroups the rest of the front message with the messages after it, and counts
    /// only the bytes that are left of it. Either way, the bytes go out in the same order.
    pub(super) fn next_write(&self) -> (usize, bool) {
        let Some(front) = self.chunks.front() else {
            return (0, false);
        };
        if front.carries_fds() {
            return (1, self.front_written == 0);
        }
        let mut bytes = front.bytes().len() - self.front_written;
        let joining = self
            .chunks
            .iter()
            .skip(1)
            .take(MESSAGES_PER_WRITE - 1)
            .take_while(|chunk| {
                let joins = bytes < BYTES_PER_WRITE && !chunk.carries_fds();
                bytes += chunk.bytes().len();

                joins
            })
            .count();

        (1 + joining, false)
    }

    /// Note that a write took `sent` more bytes off the front of the queue, and return whether a
    /// chunk got done.
    fn advance(&mut self, mut sent: usize) -> bool {
        self.written += sent as u64;
        let mut done = false;
        while let Some(front) = self.chunks.front() {
            let left = front.bytes().len() - self.front_written;
            if sent < left {
                self.front_written += sent;
                break;
            }
            sent -= left;
            self.front_written = 0;
            self.chunks.pop_front();
            done = true;
        }

        done
    }
}

/// The part of one message that is queued to write.
#[derive(Debug)]
pub(super) enum Chunk {
    /// A whole message, with its file descriptors, which go along with its first byte.
    Message(Message),
    /// The rest of a message that the previous instance partly wrote, without its file
    /// descriptors, which went along with its first byte.
    #[cfg(target_os = "linux")]
    Rest(Vec<u8>),
}

impl Chunk {
    /// The bytes of the chunk.
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Message(msg) => msg.data().bytes(),
            #[cfg(target_os = "linux")]
            Self::Rest(rest) => rest,
        }
    }

    /// Whether the chunk has file descriptors, which go along with its first byte.
    fn carries_fds(&self) -> bool {
        match self {
            Self::Message(msg) => !msg.data().fds().is_empty(),
            #[cfg(target_os = "linux")]
            Self::Rest(_) => false,
        }
    }

    /// The file descriptors of the chunk.
    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        match self {
            Self::Message(msg) => msg.data().fds().iter().map(AsFd::as_fd).collect(),
            #[cfg(target_os = "linux")]
            Self::Rest(_) => Vec::new(),
        }
    }
}

/// How the bus writes to a client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    /// The bus writes to the client, and a send waits for the client to take its messages.
    Open,
    /// The bus pauses to hand over: sends write what the socket takes at once, and wait for
    /// nothing. What is left goes with the hand-over.
    #[cfg(target_os = "linux")]
    Paused,
    /// A write failed or timed out, or the bus closed the connection: nothing more is written, and
    /// the socket is shut down.
    Disconnecting,
}

/// The most chunks that one write takes, as zbus takes in one write.
///
/// Each chunk is its own buffer in the write. POSIX lets a platform limit a write to as few as
/// 16 buffers.
const MESSAGES_PER_WRITE: usize = 16;

/// How many bytes a write takes before it ends.
///
/// A write ends with the message that brings it to this or more, as zbus's writes end. Small
/// messages, which cost the most when each gets its own write, still go a dozen or more at a time.
/// A message of this size or more is the last of its write.
const BYTES_PER_WRITE: usize = 16 * 1024;
