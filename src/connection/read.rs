//! The reader of a client's messages.

use std::{
    collections::VecDeque,
    fmt, io, mem,
    os::fd::{AsFd, OwnedFd},
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};

use futures_util::Stream;
#[cfg(target_os = "linux")]
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use zbus::{
    message::{Header, PrimaryHeader},
    Message,
};
use zruntime::{net::unix::UnixStream, Shared};

use super::socket;

/// The reader of a client's messages.
///
/// It yields the messages that the client sends, in order, as the bus takes them. It reads only
/// when the bus takes messages, and only once no whole message is left in its buffer. A read then
/// takes as much as the socket holds, up to 4 KiB. The reader takes each whole message out of its
/// buffer without reading again. A message longer than 4 KiB gets its own allocation, and the rest
/// of the message is read straight into it. What the client sent and the bus didn't read waits in
/// the socket.
///
/// # File descriptors
///
/// Each message takes as many of the file descriptors that came as its header counts. It takes them
/// from the first, in the order that they came in, whichever reads brought them, as the reference
/// implementation of D-Bus gives them out. The FDs that no message took wait for the messages after
/// them, in up to 3 sets, as zbus lets them wait. Each set holds the FDs that came with one read. A
/// read that would leave a fourth set waiting is an error.
///
/// A client that sends the FDs of each message along with the bytes of that message never has a
/// fourth set waiting. A read is made only once no whole message is left, and on Linux a read
/// returns the FDs of one write at most. So the sets that wait can only be those of the message
/// that is partly in, and of the message after it. A platform that hands out the FDs of several
/// writes with one read only makes fewer sets.
///
/// # The end
///
/// The stream ends once the client has closed its end of the socket or shut its writes down, at the
/// end of a message. It also ends once the bus has shut the socket down, at the end of what the
/// socket still holds. An error ends the stream too, after the messages before it. The errors are:
///
/// - a message that isn't one, such as a message whose header is invalid, or that lacks the FDs
///   that its header counts;
/// - EOF in the middle of a message;
/// - too many FDs that wait;
/// - a read that fails.
///
/// The error comes once, and the end of the stream after it.
pub struct Reader {
    socket: Arc<UnixStream<Shared>>,
    /// The unread bytes, which were read and not taken yet: `buffer[start..end]`.
    buffer: Vec<u8>,
    start: usize,
    end: usize,
    /// A message longer than the buffer, of which `long_filled` bytes came in, or nothing. While
    /// there is one, the buffer holds nothing, because the bytes that it held of the message moved
    /// into this allocation.
    long: Vec<u8>,
    long_filled: usize,
    /// The file descriptors that came and that no message took, in the order that they came in.
    fds: Vec<OwnedFd>,
    /// How many of `fds` came with each read whose file descriptors are still among them, oldest
    /// first.
    fd_sets: VecDeque<usize>,
    /// Whether the stream ended, at EOF or on an error.
    ///
    /// Nothing comes out after that.
    ended: bool,
}

impl Stream for Reader {
    type Item = zbus::Result<Message>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        let next = ready!(this.poll_message(cx));
        if !matches!(next, Ok(Some(_))) {
            this.ended = true;
        }

        Poll::Ready(next.transpose())
    }
}

impl Reader {
    /// The reader of the client at the other end of `socket`.
    pub fn new(socket: Arc<UnixStream<Shared>>) -> Self {
        Self {
            socket,
            buffer: vec![0; BUFFER_SIZE],
            start: 0,
            end: 0,
            long: Vec::new(),
            long_filled: 0,
            fds: Vec::new(),
            fd_sets: VecDeque::new(),
            ended: false,
        }
    }

    /// Whether the client closed its end of the socket or shut its writes down, the bus shut the
    /// socket down, or the socket failed, even if the socket still holds bytes to read.
    ///
    /// It doesn't wait, and it is false where the socket can't be asked.
    #[cfg(target_os = "linux")]
    pub fn is_closed(&self) -> bool {
        let mut fds = [PollFd::new(&*self.socket, PollFlags::RDHUP)];
        let closed = PollFlags::RDHUP | PollFlags::HUP | PollFlags::ERR;

        poll(&mut fds, Some(&Timespec::default())).is_ok() && fds[0].revents().intersects(closed)
    }

    /// The next line of the handshake, without the `\r\n` that ends it, as soon as it is whole.
    ///
    /// The line is read into the same buffer as the messages. So the first message is read from
    /// what the client sends after the last line of the handshake, and from the file descriptors
    /// that come with it.
    ///
    /// A line that has a `\n` without a `\r` before it is an error, and so is a line that doesn't
    /// fit in the 4 KiB buffer. EOF is an error too.
    pub(super) fn poll_line(&mut self, cx: &mut Context<'_>) -> Poll<zbus::Result<Vec<u8>>> {
        loop {
            let buffered = &self.buffer[self.start..self.end];
            if let Some(lf) = buffered.iter().position(|&byte| byte == b'\n') {
                let Some(cr) = lf.checked_sub(1).filter(|&cr| buffered[cr] == b'\r') else {
                    return Poll::Ready(Err(handshake(
                        "A line of the handshake doesn't end with `\\r\\n`",
                    )));
                };
                let line = buffered[..cr].to_vec();
                self.start += lf + 1;

                return Poll::Ready(Ok(line));
            }
            if buffered.len() >= BUFFER_SIZE {
                return Poll::Ready(Err(handshake(
                    "A line of the handshake is longer than 4 KiB",
                )));
            }
            if ready!(self.poll_fill(cx))? == 0 {
                return Poll::Ready(Err(handshake("The input ended in the handshake")));
            }
        }
    }

    /// The next message, as soon as it is whole, or none at EOF.
    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<zbus::Result<Option<Message>>> {
        loop {
            if !self.long.is_empty() {
                while self.long_filled < self.long.len() {
                    let room = &mut self.long[self.long_filled..];
                    let read = ready!(poll_recv(
                        &self.socket,
                        cx,
                        room,
                        &mut self.fds,
                        &mut self.fd_sets
                    ))?;
                    if read == 0 {
                        return Poll::Ready(Err(ended_in_a_message()));
                    }
                    self.long_filled += read;
                }
                let bytes = mem::take(&mut self.long);
                self.long_filled = 0;

                return Poll::Ready(self.frame(bytes).map(Some));
            }

            let buffered = &self.buffer[self.start..self.end];
            if buffered.len() >= MIN_MESSAGE_SIZE {
                let (_, len) = PrimaryHeader::parse_with_len(buffered)?;
                if buffered.len() >= len {
                    // A copy of the message alone, which the message keeps as its own allocation.
                    let bytes = buffered[..len].to_vec();
                    self.start += len;
                    if self.start == self.end {
                        self.start = 0;
                        self.end = 0;
                    }

                    return Poll::Ready(self.frame(bytes).map(Some));
                }
                if len > BUFFER_SIZE {
                    // The message gets its own allocation. What the buffer holds of the message
                    // moves into it, and the rest of the message is read straight into it.
                    let mut long = Vec::with_capacity(len);
                    long.extend_from_slice(buffered);
                    self.long_filled = long.len();
                    long.resize(len, 0);
                    self.long = long;
                    self.start = 0;
                    self.end = 0;

                    continue;
                }
            }

            if ready!(self.poll_fill(cx))? == 0 {
                return Poll::Ready(match self.end {
                    0 => Ok(None),
                    _ => Err(ended_in_a_message()),
                });
            }
        }
    }

    /// Read what the socket holds into the room after the bytes in the buffer, and return how many
    /// bytes came, 0 at EOF.
    ///
    /// The buffer first moves its bytes to its start.
    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<zbus::Result<usize>> {
        if self.start > 0 {
            self.buffer.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        let room = &mut self.buffer[self.end..];
        let read = ready!(poll_recv(
            &self.socket,
            cx,
            room,
            &mut self.fds,
            &mut self.fd_sets
        ))?;
        self.end += read;

        Poll::Ready(Ok(read))
    }

    /// The message of `bytes`, which hold exactly one message, with the file descriptors that its
    /// header counts, taken from the front of the waiting FDs.
    fn frame(&mut self, bytes: Vec<u8>) -> zbus::Result<Message> {
        let count = if self.fds.is_empty() {
            0
        } else {
            // Fewer FDs than the header counts are an error of the message.
            let count = Header::parse(&bytes)?.unix_fds().unwrap_or(0) as usize;

            count.min(self.fds.len())
        };
        let fds = self.fds.drain(..count).collect();
        let mut taken = count;
        while let Some(set) = self.fd_sets.front_mut() {
            if *set > taken {
                *set -= taken;
                break;
            }
            taken -= *set;
            self.fd_sets.pop_front();
        }

        Message::from_parts(bytes, fds)
    }
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader")
            .field("socket", &self.socket)
            .field("buffered", &(self.end - self.start))
            .field("long", &self.long.len())
            .field("long_filled", &self.long_filled)
            .field("fds", &self.fds.len())
            .field("fd_sets", &self.fd_sets)
            .field("ended", &self.ended)
            .finish()
    }
}

/// One read of `stream` into `room`, as soon as the socket has anything: how many bytes came,
/// 0 at EOF.
///
/// The file descriptors that came join those that wait in `fds`, as their own set in `fd_sets`. If
/// [`MAX_FD_SETS`] sets wait already, that is an error.
fn poll_recv(
    stream: &UnixStream<Shared>,
    cx: &mut Context<'_>,
    room: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    fd_sets: &mut VecDeque<usize>,
) -> Poll<zbus::Result<usize>> {
    let mut came = Vec::new();
    let read =
        ready!(stream.poll_read_with(cx, |stream| socket::recv(stream.as_fd(), room, &mut came)))?;
    if !came.is_empty() {
        if fd_sets.len() == MAX_FD_SETS {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many file descriptors that no message counts",
            )
            .into()));
        }
        fd_sets.push_back(came.len());
        fds.append(&mut came);
    }

    Poll::Ready(Ok(read))
}

/// The error of an input that ended in the middle of a message.
fn ended_in_a_message() -> zbus::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "the input ended in the middle of a message",
    )
    .into()
}

/// The error of a handshake that went wrong, as `what` says.
fn handshake(what: &str) -> zbus::Error {
    zbus::Error::Handshake(what.to_string())
}

/// The size of the start of a message that tells its length: the primary header, and the length of
/// the header fields, which [`PrimaryHeader::parse_with_len`] reads. No message is shorter.
const MIN_MESSAGE_SIZE: usize = 16;

/// How many bytes a [`Reader`] reads at a time, at most, as zbus reads.
///
/// Most messages are much smaller than this, so one read takes in several messages that come in
/// quick succession. A longer message costs a copy of no more than this out of the buffer, before
/// the rest of it is read straight into its own allocation.
const BUFFER_SIZE: usize = 4 * 1024;

/// How many sets of file descriptors may wait for messages to take them, as zbus lets wait.
///
/// Each set holds the FDs that came with one read. See [`Reader`] for why a client that sends the
/// FDs of each message with the message never has more sets waiting than this.
const MAX_FD_SETS: usize = 3;
