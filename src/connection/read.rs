//! The reader of a client's messages.

use std::{
    collections::VecDeque,
    fmt,
    future::poll_fn,
    io, mem,
    os::fd::{AsFd, OwnedFd},
    pin::Pin,
    rc::Rc,
    task::{ready, Context, Poll},
};

use futures_util::Stream;
#[cfg(target_os = "linux")]
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use zbus::{message::PrimaryHeader, Message, OwnedGuid};
use zruntime::{net::unix::UnixStream, Local};

use super::{
    handshake::{write_answer, Server},
    socket,
};
use crate::trace;

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
    socket: Rc<UnixStream<Local>>,
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
    pub fn new(socket: Rc<UnixStream<Local>>) -> Self {
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

    /// The reader of the client at the other end of `socket`, taken over from the previous
    /// instance.
    ///
    /// It yields `unread` first, and then what it reads from the socket. `unread` holds the bytes
    /// that the previous instance read but no message took, from the start of a message. `fds`
    /// holds the file descriptors that came with those bytes and that no message took.
    ///
    /// The FDs wait as one set, which stands for the sets that waited in the previous instance.
    /// There was at least one such set, so no more sets wait here than waited there. If `unread`
    /// starts a message longer than the reader reads at a time and holds nothing more, it gets its
    /// own allocation, as any such message does. Any other bytes go into the buffer that the reader
    /// reads into.
    #[cfg(target_os = "linux")]
    pub fn taken_over(socket: Rc<UnixStream<Local>>, unread: Vec<u8>, fds: Vec<OwnedFd>) -> Self {
        let fd_sets = match fds.len() {
            0 => VecDeque::new(),
            count => VecDeque::from([count]),
        };
        // The buffer is only as long as what it holds, which leaves it no room to read into. The
        // reader grows it back to its usual size before each read.
        let (buffer, end, long, long_filled) = match long_start(&unread) {
            Some(len) => {
                let filled = unread.len();
                let mut long = unread;
                long.resize(len, 0);

                (Vec::new(), 0, long, filled)
            }
            None => {
                let end = unread.len();

                (unread, end, Vec::new(), 0)
            }
        };

        Self {
            socket,
            buffer,
            start: 0,
            end,
            long,
            long_filled,
            fds,
            fd_sets,
            ended: false,
        }
    }

    /// The unread bytes, from the start of a message, and their file descriptors that no message
    /// took, in the order that they came in, for the next instance to take over.
    ///
    /// If the reader is in the middle of a message longer than it reads at a time, the unread bytes
    /// are the bytes of that message that came in. Otherwise, they are what the reader read after
    /// the last message that it yielded.
    #[cfg(target_os = "linux")]
    pub fn into_unread(self) -> (Vec<u8>, Vec<OwnedFd>) {
        let unread = if self.long.is_empty() {
            let mut buffer = self.buffer;
            buffer.copy_within(self.start..self.end, 0);
            buffer.truncate(self.end - self.start);

            buffer
        } else {
            let mut long = self.long;
            long.truncate(self.long_filled);

            long
        };

        (unread, self.fds)
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

    /// Authenticate the client whose messages the reader reads, and return whether it asked to pass
    /// file descriptors.
    ///
    /// The bus is the server of `guid`. It authenticates the client with the `EXTERNAL` mechanism,
    /// as the user `uid` that its socket reports. What the client sent after `BEGIN` stays in the
    /// reader, as the start of its first message.
    ///
    /// The bus answers each command of the client on the reader's socket, as zbus 6.4's server
    /// answers it. A client that claims to be another user is rejected, and may try again. The
    /// handshake fails, without an answer, on:
    ///
    /// - a line that holds no command that zbus knows, such as `AUTH` with a mechanism that zbus
    ///   doesn't support;
    /// - a line that isn't UTF-8;
    /// - a first line that doesn't start with the 0 byte that a client sends first;
    /// - a user that isn't a number;
    /// - a line that doesn't end with `\r\n`, or that doesn't fit in 4 KiB;
    /// - the fourth read of the handshake that brings file descriptors;
    /// - EOF.
    pub(super) async fn authenticate(&mut self, guid: &OwnedGuid, uid: u32) -> zbus::Result<bool> {
        let mut server = Server::new(guid, uid);
        loop {
            let line = poll_fn(|cx| self.poll_line(cx)).await?;
            let Some(answer) = server.answer_line(&line)? else {
                return Ok(server.fd_passing());
            };
            trace!("Answering `{answer}`");
            write_answer(&self.socket, &answer).await?;
        }
    }

    /// The next line of the handshake, without the `\r\n` that ends it, as soon as it is whole.
    ///
    /// The line is read into the same buffer as the messages. So the first message is read from
    /// what the client sends after the last line of the handshake, and from the file descriptors
    /// that come with it.
    ///
    /// A line that has a `\n` without a `\r` before it is an error, and so is a line that doesn't
    /// fit in the 4 KiB buffer. EOF is an error too.
    fn poll_line(&mut self, cx: &mut Context<'_>) -> Poll<zbus::Result<Vec<u8>>> {
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
    ///
    /// A read is made only once no whole message is left in the buffer, and the start of a message
    /// longer than [`BUFFER_SIZE`] has gone to its own allocation by then. So the buffer holds less
    /// than that size, and it has room after its bytes once it is of that size again.
    fn poll_fill(&mut self, cx: &mut Context<'_>) -> Poll<zbus::Result<usize>> {
        if self.start > 0 {
            self.buffer.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        // The buffer of a taken-over reader is only as long as the unread bytes that it took over,
        // which leaves no room after them. A read into no room would come back with nothing,
        // as at EOF.
        if self.buffer.len() < BUFFER_SIZE {
            self.buffer.resize(BUFFER_SIZE, 0);
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
        // Fewer FDs than the header counts are an error of the message, which takes none of them.
        let waiting = self.fds.len();
        let msg = Message::from_parts(bytes, &mut self.fds)?;
        let mut taken = waiting - self.fds.len();
        while let Some(set) = self.fd_sets.front_mut() {
            if *set > taken {
                *set -= taken;
                break;
            }
            taken -= *set;
            self.fd_sets.pop_front();
        }

        Ok(msg)
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
    stream: &UnixStream<Local>,
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

/// The length of the message that `bytes` start, if it is longer than [`BUFFER_SIZE`], and `bytes`
/// hold no more than that message.
#[cfg(target_os = "linux")]
fn long_start(bytes: &[u8]) -> Option<usize> {
    let (_, len) = PrimaryHeader::parse_with_len(bytes).ok()?;

    (len > BUFFER_SIZE && bytes.len() <= len).then_some(len)
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
