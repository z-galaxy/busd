//! The hand-over of a client's connection to the next instance, and its take-over from the previous
//! instance: [`Handover`].
//!
//! The bus hands a connection over in a record in busd's own format. The record goes with the
//! connection's socket and with the file descriptors of the bytes that it holds. The unread bytes
//! of a connection are those that the bus read from its socket and didn't take as messages.
//!
//! # The record
//!
//! Integers are little-endian, and offsets count from the start of the record. The record is read
//! in place, at any address, so it needs no alignment:
//!
//! ```text
//! offset  size            field
//!      0  8               b"busd-cnx"
//!      8  2               format version, 1
//!     10  2               flags: bit 0 if the client passes file descriptors; no other bits
//!     12  4               W, the number of whole messages left to write
//!     16  8               U, the length of the unread bytes
//!     24  8               C, the length of the rest of a partly written message
//!     32  4               F, the number of file descriptors of the unread bytes
//!     36  4               0
//!     40  8 × W           for each whole message: its length, then its number of file
//!                         descriptors, 4 bytes each
//!         U               the unread bytes, then zeros up to a multiple of 8
//!         C + the W       the rest, then the whole messages, then zeros up to a multiple of 8,
//!                         to the end of the record
//! ```
//!
//! The file descriptors that go with the record are the socket, then the `F` of the unread bytes,
//! then those of the whole messages, in order.

use std::{
    fmt, io, iter, mem,
    os::fd::{AsFd, BorrowedFd, OwnedFd},
};

use zbus::Message;
use zoter::{Restore, Save};

/// The hand-over of a client's connection: its socket, its unread bytes, and what is left to write
/// to it, with the file descriptors of both.
///
/// The unread bytes are those that the bus read from the socket and didn't take as messages. The
/// bytes left to write are those that the bus took to send to the client and didn't write.
///
/// [`Connection::hand_over`](super::Connection::hand_over) makes it, and
/// [`Connection::taken_over`](super::Connection::taken_over) gives the next instance the two halves
/// of the connection back from it. It goes to the next instance as an item of the hand-over, in
/// busd's own record format ([`Save`]), and the next instance restores it from that record
/// ([`Restore`]).
///
/// `Debug` shows how many bytes and file descriptors it holds, not the bytes themselves.
pub struct Handover {
    pub(super) socket: OwnedFd,
    pub(super) fd_passing: bool,
    /// The unread bytes, from the start of a message, and their file descriptors that no message
    /// took, in the order that they came in.
    pub(super) unread: Vec<u8>,
    pub(super) unread_fds: Vec<OwnedFd>,
    /// The rest of a partly written message, without its file descriptors, which went with its
    /// first byte.
    pub(super) rest: Vec<u8>,
    /// The whole messages left to write, with their file descriptors.
    pub(super) kept: Vec<Message>,
}

impl Handover {
    /// The socket of the client.
    pub fn socket(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }

    /// How many bytes were read off the socket and are handed over.
    pub fn unread_len(&self) -> usize {
        self.unread.len()
    }

    /// How many bytes are left to write to the client.
    pub fn unwritten_len(&self) -> usize {
        self.rest.len() + self.kept.iter().map(|msg| msg.data().len()).sum::<usize>()
    }
}

impl fmt::Debug for Handover {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let unwritten_fds: usize = self.kept.iter().map(|msg| msg.data().fds().len()).sum();

        f.debug_struct("Handover")
            .field("socket", &self.socket)
            .field("fd_passing", &self.fd_passing)
            .field("unread_len", &self.unread_len())
            .field("unread_fds", &self.unread_fds.len())
            .field("unwritten_len", &self.unwritten_len())
            .field("unwritten_fds", &unwritten_fds)
            .finish()
    }
}

/// The record of the hand-over, and its file descriptors, for the next instance.
///
/// [`Restore`] gives the hand-over back from them.
impl Save for Handover {
    /// The exact length of the record, a multiple of 8. It is never `None`.
    fn saved_len(&self) -> Option<usize> {
        Some(
            HEADER
                + ENTRY * self.kept.len()
                + self.unread.len().next_multiple_of(8)
                + self.unwritten_len().next_multiple_of(8),
        )
    }

    /// Copy the record into `dest`, which can be any memory, at any address.
    ///
    /// # Errors
    ///
    /// Fails with [`zoter::Error::Io`] of kind [`io::ErrorKind::InvalidInput`] if `dest` is not
    /// [`Save::saved_len`] bytes long, or if the hand-over has more messages or file descriptors
    /// than a record can count (4 billion), or a message longer than a record can count (4 GiB).
    /// Nothing is written on an error.
    fn save(&self, dest: &mut [u8]) -> zoter::Result<()> {
        if Some(dest.len()) != self.saved_len() {
            return Err(invalid_input("the memory is not as long as the record").into());
        }
        let count = |count: usize| {
            u32::try_from(count)
                .map_err(|_| invalid_input("too many messages or file descriptors for a record"))
        };
        let kept = count(self.kept.len())?;
        let unread_fds = count(self.unread_fds.len())?;
        let table = self
            .kept
            .iter()
            .map(|msg| Ok([count(msg.data().len())?, count(msg.data().fds().len())?]))
            .collect::<io::Result<Vec<_>>>()?;
        let flags = if self.fd_passing { FD_PASSING } else { 0 };

        // Nothing fails from here on.
        let mut record = Record(dest);
        record.put(MAGIC);
        record.put(&VERSION.to_le_bytes());
        record.put(&flags.to_le_bytes());
        record.put(&kept.to_le_bytes());
        record.put(&(self.unread.len() as u64).to_le_bytes());
        record.put(&(self.rest.len() as u64).to_le_bytes());
        record.put(&unread_fds.to_le_bytes());
        record.zeros(4);
        for [len, fds] in table {
            record.put(&len.to_le_bytes());
            record.put(&fds.to_le_bytes());
        }
        record.put(&self.unread);
        record.zeros(padding(self.unread.len()));
        record.put(&self.rest);
        for msg in &self.kept {
            record.put(msg.data().bytes());
        }
        record.zeros(padding(self.unwritten_len()));
        debug_assert!(
            record.0.is_empty(),
            "the record is longer than what was copied into it"
        );

        Ok(())
    }

    /// The file descriptors to pass on with the record, in the order that [`Restore::restore`]
    /// takes them.
    ///
    /// They are the socket, then the FDs of the unread bytes, then the FDs of the whole messages
    /// left to write, in their order.
    fn fds(&self) -> Vec<BorrowedFd<'_>> {
        let kept = self.kept.iter().flat_map(|msg| msg.data().fds());

        iter::once(self.socket.as_fd())
            .chain(self.unread_fds.iter().map(AsFd::as_fd))
            .chain(kept.map(AsFd::as_fd))
            .collect()
    }
}

/// The hand-over that a record holds, with its file descriptors.
impl Restore for Handover {
    type Error = io::Error;

    /// The hand-over that [`Save::save`] copied into `record`.
    ///
    /// `record` holds exactly the bytes that `save` wrote, at any address, and `fds` holds the file
    /// descriptors that [`Save::fds`] gave, in their order. This copies what it needs out of
    /// `record` before it returns.
    ///
    /// # Errors
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] for a record of a later format version, and with
    /// [`io::ErrorKind::InvalidData`] for anything else that isn't a record with the file
    /// descriptors that it counts. On an error, the file descriptors close.
    fn restore(record: &[u8], fds: Vec<OwnedFd>) -> io::Result<Self> {
        if record.get(..MAGIC.len()) != Some(MAGIC) {
            return Err(malformed());
        }
        match u16::from_le_bytes(field(record, 8)?) {
            VERSION => (),
            version if version > VERSION => return Err(unsupported()),
            _ => return Err(malformed()),
        }
        let flags = u16::from_le_bytes(field(record, 10)?);
        if flags & !FD_PASSING != 0 || field::<4>(record, 36)? != [0; 4] {
            return Err(malformed());
        }
        let kept = count_at(record, 12)?;
        let (unread, rest) = (len_at(record, 16)?, len_at(record, 24)?);
        let unread_fds = count_at(record, 32)?;
        let table_len = kept.checked_mul(ENTRY).ok_or_else(malformed)?;
        let table: Vec<_> = bytes(record, HEADER, table_len)?
            .as_chunks()
            .0
            .iter()
            .map(entry)
            .collect();
        let (unread, unread_end) = section(record, HEADER + table_len, unread)?;
        let unwritten = table
            .iter()
            .try_fold(rest, |len, (msg_len, _)| len.checked_add(*msg_len))
            .ok_or_else(malformed)?;
        let (unwritten, end) = section(record, unread_end, unwritten)?;
        // The socket comes first, then the FDs of the unread bytes, then those of each whole
        // message. The check subtracts each count from the number of FDs instead of adding the
        // counts, since their sum can overflow on a 32-bit target.
        let mut left = fds
            .len()
            .checked_sub(1)
            .and_then(|n| n.checked_sub(unread_fds));
        for (_, count) in &table {
            left = left.and_then(|left| left.checked_sub(*count));
        }
        if end != record.len() || left != Some(0) {
            return Err(malformed());
        }

        let mut fds = fds.into_iter();
        let socket = fds.next().expect("the descriptors were counted");
        let unread_fds = fds.by_ref().take(unread_fds).collect();
        let (rest, mut whole) = unwritten.split_at(rest);
        let kept = table
            .iter()
            .map(|(len, count)| {
                let (bytes, after) = whole.split_at(*len);
                whole = after;
                let mut fds: Vec<_> = fds.by_ref().take(*count).collect();
                let msg = Message::from_parts(bytes.to_vec(), &mut fds).map_err(|_| malformed())?;
                // The record counts as many FDs for the message as its header does.
                if !fds.is_empty() {
                    return Err(malformed());
                }

                Ok(msg)
            })
            .collect::<io::Result<_>>()?;

        Ok(Self {
            socket,
            fd_passing: flags & FD_PASSING != 0,
            unread: unread.to_vec(),
            unread_fds,
            rest: rest.to_vec(),
            kept,
        })
    }
}

/// Memory that a record is copied into, from its start on.
struct Record<'d>(&'d mut [u8]);

impl Record<'_> {
    /// Copy `bytes` into the memory, after what was copied into it before.
    fn put(&mut self, bytes: &[u8]) {
        let (head, rest) = mem::take(&mut self.0).split_at_mut(bytes.len());
        head.copy_from_slice(bytes);
        self.0 = rest;
    }

    /// Zero the next `len` bytes of the memory.
    fn zeros(&mut self, len: usize) {
        let (head, rest) = mem::take(&mut self.0).split_at_mut(len);
        head.fill(0);
        self.0 = rest;
    }
}

/// The length and the number of file descriptors of a message, from `entry`, its entry in a
/// record's table.
fn entry(entry: &[u8; ENTRY]) -> (usize, usize) {
    let (len, count) = entry.split_at(4);
    let decode = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().expect("4 bytes")) as usize;

    (decode(len), decode(count))
}

/// The count of 4 bytes of `record` at `at`.
fn count_at(record: &[u8], at: usize) -> io::Result<usize> {
    Ok(u32::from_le_bytes(field(record, at)?) as usize)
}

/// The length of 8 bytes of `record` at `at`.
fn len_at(record: &[u8], at: usize) -> io::Result<usize> {
    usize::try_from(u64::from_le_bytes(field(record, at)?)).map_err(|_| malformed())
}

/// The `N` bytes of `record` at `at`.
fn field<const N: usize>(record: &[u8], at: usize) -> io::Result<[u8; N]> {
    Ok(bytes(record, at, N)?
        .try_into()
        .expect("the bytes of a field are as many as it has"))
}

/// The `len` bytes of `record` at `at`, and the end of the zeros that follow them up to a multiple
/// of 8.
fn section(record: &[u8], at: usize, len: usize) -> io::Result<(&[u8], usize)> {
    let padded = len.checked_next_multiple_of(8).ok_or_else(malformed)?;
    let (section, padding) = bytes(record, at, padded)?.split_at(len);
    if padding.iter().any(|byte| *byte != 0) {
        return Err(malformed());
    }

    Ok((section, at + padded))
}

/// The `len` bytes of `record` at `at`.
fn bytes(record: &[u8], at: usize, len: usize) -> io::Result<&[u8]> {
    at.checked_add(len)
        .and_then(|end| record.get(at..end))
        .ok_or_else(malformed)
}

/// How many zeros follow `len` bytes up to a multiple of 8.
fn padding(len: usize) -> usize {
    len.next_multiple_of(8) - len
}

/// The error of a record, or of file descriptors, from which no hand-over can be restored.
fn malformed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "not the record of the hand-over of a connection, with its file descriptors",
    )
}

/// The error of a record of a later format version, which this version can't read.
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "the record of the hand-over of a connection of a later version of its format",
    )
}

/// The error, for `reason`, of a hand-over that [`Save::save`] can't copy into the memory that it
/// is given.
fn invalid_input(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason)
}

/// The bytes that start a record.
const MAGIC: &[u8] = b"busd-cnx";
/// Record format version. Readers reject records with a newer version.
const VERSION: u16 = 1;
/// The flag of a record of a client that passes file descriptors.
const FD_PASSING: u16 = 1;
/// The length of a record's header, after which comes the table of the whole messages.
const HEADER: usize = 40;
/// The length of an entry of the table: a message's length, then its number of file descriptors,
/// 4 bytes each.
const ENTRY: usize = 8;

#[cfg(test)]
mod tests;
