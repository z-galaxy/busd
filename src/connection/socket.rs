//! One `recvmsg` or `sendmsg` on a client's socket, with the file descriptors that go along, as
//! zbus makes them.

use std::{
    io::{self, IoSlice, IoSliceMut},
    mem::MaybeUninit,
    os::fd::{BorrowedFd, OwnedFd},
};

use rustix::net::{
    recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};

/// One `recvmsg` into `buf`: how many bytes came, 0 at EOF.
///
/// It has room for [`MAX_FDS`] file descriptors, and appends those that come to `fds`.
///
/// On an error, nothing is appended to `fds`.
pub(super) fn recv(
    socket: BorrowedFd<'_>,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    let mut iov = [IoSliceMut::new(buf)];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = match recvmsg(socket, &mut iov, &mut control, RecvFlags::empty()) {
        // On Linux, if a peer closes its socket while bytes that it was sent are still unread in
        // it, a read fails with ECONNRESET. It fails only once the reads before it took everything
        // that the peer sent, so that read is EOF, and nothing is lost.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Err(e) if e == rustix::io::Errno::CONNRESET => return Ok(0),
        msg => msg?,
    };
    let mut received = Vec::new();
    for message in control.drain() {
        match message {
            RecvAncillaryMessage::ScmRights(rights) => received.extend(rights),
            // Linux can pass the peer's credentials along, but the bus doesn't ask for them.
            #[cfg(any(target_os = "linux", target_os = "android"))]
            RecvAncillaryMessage::ScmCredentials(_) => (),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected control message",
                ))
            }
        }
    }
    fds.append(&mut received);

    Ok(msg.bytes)
}

/// One `sendmsg` of `bufs`, with `fds` along with their first byte: how many bytes went.
///
/// It fails with [`io::ErrorKind::WriteZero`] if the socket takes none of the bytes, and with
/// [`io::ErrorKind::InvalidInput`] if `fds` are more than [`MAX_FDS`].
pub(super) fn send(
    socket: BorrowedFd<'_>,
    bufs: &[IoSlice<'_>],
    fds: &[BorrowedFd<'_>],
) -> io::Result<usize> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many file descriptors",
        ));
    }
    let sent = sendmsg(socket, bufs, &mut control, SEND_FLAGS)?;
    if sent == 0 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "the socket took nothing",
        ));
    }

    Ok(sent)
}

/// How many file descriptors one [`recv`] has room for and one [`send`] takes: as many as zbus has
/// room for.
///
/// Linux passes at most `SCM_MAX_FD` (253) FDs with a write, and a read returns the FDs of one
/// write at most, so this is never full there. macOS passes more, and zbus takes them whole, up to
/// this many.
const MAX_FDS: usize = 1024;

/// The flags of each [`send`].
///
/// `MSG_NOSIGNAL` makes a write to a peer that has gone away an error rather than a signal. Apple's
/// platforms and Redox have no such flag, so a send there goes without it. On Apple's platforms,
/// each client's socket has `SO_NOSIGPIPE` set instead, since zruntime sets it on every stream
/// there.
#[cfg(not(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox"
)))]
const SEND_FLAGS: SendFlags = SendFlags::NOSIGNAL;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox"
))]
const SEND_FLAGS: SendFlags = SendFlags::empty();
