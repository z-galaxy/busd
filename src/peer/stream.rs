use std::{
    fmt, io,
    os::fd::AsFd,
    pin::Pin,
    task::{ready, Context, Poll},
};

use crate::trace;
use anyhow::anyhow;
use futures_util::{Stream as FutureStream, StreamExt};
use zbus::{
    message,
    names::{InterfaceName, OwnedUniqueName, UniqueName},
    wire::Endian,
    Message, MessageStream, ObjectPath,
};

use crate::peer::{Peer, MAX_MESSAGE_FDS};

/// Message stream for a peer.
///
/// This stream ensures the following for each message produced:
///
/// * Its body holds what its signature says.
/// * It carries no more file descriptors than the bus can pass on, [`MAX_MESSAGE_FDS`].
/// * It has neither the interface nor the path that the specification reserves for local use.
/// * The destination field is present and readable for non-signals.
/// * The sender field is present and set to the unique name of the peer.
pub struct Stream {
    msg_stream: MessageStream,
    unique_name: OwnedUniqueName,
}

impl Stream {
    pub fn for_peer(peer: &Peer, msg_stream: MessageStream) -> Self {
        Self {
            msg_stream,
            unique_name: peer.client().unique_name().clone(),
        }
    }

    /// The stream of the messages of the connection of the peer, which this takes them from: the
    /// one that has had all of them, from the first, as the monitor that the peer becomes, and a
    /// hand-over of the connection, take it.
    pub fn into_inner(self) -> MessageStream {
        self.msg_stream
    }
}

impl FutureStream for Stream {
    type Item = Result<Message, Invalid>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let msg = ready!(this.msg_stream.poll_next_unpin(cx));

        Poll::Ready(msg.map(|msg| match msg {
            Ok(msg) => checked(msg, &this.unique_name),
            Err(e) => Err(Invalid::Dropped(e.into())),
        }))
    }
}

/// A message of a peer that the bus doesn't pass on.
#[derive(Debug)]
pub enum Invalid {
    /// Its body doesn't hold what its signature says: the bus disconnects the peer, as dbus-broker
    /// does, since the bus passes bodies on without reading them.
    Body(zbus::Error),
    /// It carries more file descriptors than the bus can pass on, [`MAX_MESSAGE_FDS`]: the bus
    /// disconnects the peer.
    TooManyFds,
    /// It has the interface or the path that the specification reserves for the `Disconnected`
    /// signal that a D-Bus library raises locally as its connection goes, which clients are not
    /// to send: the bus disconnects the peer, as the reference implementation and dbus-broker do.
    Local,
    /// Anything else that is wrong with it: the bus drops the message, and serves the peer on.
    Dropped(anyhow::Error),
}

impl fmt::Display for Invalid {
    /// What the peer sent, to go on from "Disconnecting the peer, as it sent".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Body(e) => write!(
                f,
                "a message whose body doesn't hold what its signature says: {e}"
            ),
            Self::TooManyFds => write!(
                f,
                "a message with more than {MAX_MESSAGE_FDS} file descriptors"
            ),
            Self::Local => write!(
                f,
                "a message with the interface or the path that the specification reserves for \
                 local use, `{LOCAL_INTERFACE}`"
            ),
            Self::Dropped(e) => write!(f, "{e}"),
        }
    }
}

/// `msg`, which the peer `unique_name` sent, with its sender field set if it has none.
fn checked(msg: Message, unique_name: &OwnedUniqueName) -> Result<Message, Invalid> {
    // The bus passes the bodies of messages on without deserializing them, so it checks each body
    // here. It disconnects a peer that sends a body that doesn't hold what its signature says, as
    // dbus-broker does.
    msg.body().validate().map_err(Invalid::Body)?;

    // Passing such a message on would fail, and get its recipients disconnected.
    if msg.data().fds().len() > MAX_MESSAGE_FDS {
        return Err(Invalid::TooManyFds);
    }

    let header = msg.header();

    // A recipient that goes by such a message, as libdbus has its applications do, would take it
    // for the loss of its connection.
    if header
        .interface()
        .is_some_and(|interface| *interface == LOCAL_INTERFACE)
        || header.path().is_some_and(|path| *path == LOCAL_PATH)
    {
        return Err(Invalid::Local);
    }

    // Ensure destination field is present and readable for non-signals.
    if msg.message_type() != message::Type::Signal && header.destination().is_none() {
        return Err(Invalid::Dropped(anyhow!("missing destination field")));
    }

    // Ensure sender field is present. If it is not we add it using the unique name of the peer, as
    // most clients leave it out.
    match header.sender() {
        Some(sender) if *sender == *unique_name => Ok(msg),
        Some(_) => Err(Invalid::Dropped(anyhow!(
            "failed to parse message: Invalid sender field"
        ))),
        None => {
            let new_msg = add_sender(&msg, unique_name).map_err(|e| Invalid::Dropped(e.into()))?;
            trace!("Added sender field to message: {:?}", new_msg);

            Ok(new_msg)
        }
    }
}

/// `msg`, which has no sender field, with `sender` for one, appended to its header fields, as
/// zbus's `Message::with_sender` appends it.
fn add_sender(msg: &Message, sender: &UniqueName<'_>) -> zbus::Result<Message> {
    debug_assert!(msg.header().sender().is_none());
    let data = msg.data();
    let bytes: &[u8] = data;
    let endian = Endian::from(msg.primary_header().endian_sig());
    // The primary header (12 bytes), then the length of the array of the fields (4 bytes).
    let fields_end = 16 + endian.read_u32(&bytes[12..16]) as usize;
    // Each field starts on an 8-byte boundary, and so does the body.
    let field_start = fields_end.next_multiple_of(8);
    let body = &bytes[field_start..];
    // The field: its code and the signature of its value, `s` (4 bytes), the length of the name
    // (4 bytes), then the name and its 0.
    let new_fields_end = field_start + 8 + sender.len() + 1;
    let body_offset = new_fields_end.next_multiple_of(8);
    let mut new = Vec::with_capacity(body_offset + body.len());
    let mut word = [0; 4];
    new.extend_from_slice(&bytes[..12]);
    endian.write_u32(&mut word, (new_fields_end - 16) as u32);
    new.extend_from_slice(&word);
    new.extend_from_slice(&bytes[16..fields_end]);
    new.resize(field_start, 0);
    new.extend_from_slice(&[SENDER, 1, b's', 0]);
    endian.write_u32(&mut word, sender.len() as u32);
    new.extend_from_slice(&word);
    new.extend_from_slice(sender.as_bytes());
    new.push(0);
    new.resize(body_offset, 0);
    new.extend_from_slice(body);
    // The copy carries duplicates of the FDs of `msg`, as with `with_sender`: `msg` goes with its
    // own.
    let fds = data
        .fds()
        .iter()
        .map(|fd| fd.as_fd().try_clone_to_owned())
        .collect::<io::Result<_>>()?;

    Message::from_parts(new, fds)
}

/// The interface that the specification reserves for the signal that a library raises locally.
const LOCAL_INTERFACE: InterfaceName<'static> =
    InterfaceName::from_static_str_unchecked("org.freedesktop.DBus.Local");

/// The path that the specification reserves for the signal that a library raises locally.
const LOCAL_PATH: ObjectPath<'static> =
    ObjectPath::from_static_str_unchecked("/org/freedesktop/DBus/Local");

/// The code of the sender field of a header.
const SENDER: u8 = 7;

#[cfg(test)]
mod tests {
    use std::{fs::File, num::NonZeroU32};

    use zbus::wire::Fd;

    use super::*;

    /// A message whose body doesn't hold what its signature says (a boolean of 2) is a violation,
    /// even a message that the bus would otherwise only drop. A message whose body does (a boolean
    /// of 1) passes, with the sender field of its peer.
    #[test]
    fn bodies_are_checked() {
        let peer = OwnedUniqueName::try_from(":busd.1").unwrap();
        let with_body = |builder: message::Builder<'_>, body: &[u8]| {
            let builder = builder.endian(Endian::Little);
            // SAFETY: The body is 4 bytes, the size of a boolean, and carries no file descriptors.
            // At worst it holds a number other than 0 or 1, and `checked` is to refuse that before
            // anything reads the body.
            unsafe { builder.build_raw_body(body, "b", vec![]) }.unwrap()
        };
        let signal = || Message::signal("/org/busd/Test", "org.busd.Test", "Bool");

        // A message without a sender field, one with the peer's sender field, and two that the bus
        // would otherwise only drop, serving the peer on. Those two are a call without a
        // destination, and a signal with the sender field of another peer.
        for builder in [
            signal(),
            signal().sender(":busd.1"),
            Message::method_call("/org/busd/Test", "Bool"),
            signal().sender(":busd.2"),
        ] {
            let refused = checked(with_body(builder, &[2, 0, 0, 0]), &peer);
            assert!(matches!(refused, Err(Invalid::Body(_))), "{refused:?}");
        }
        let passed = checked(with_body(signal(), &[1, 0, 0, 0]), &peer).unwrap();
        assert_eq!(passed.header().sender(), Some(&peer.as_ref()));
        assert!(passed.body().deserialize::<bool>().unwrap());
    }

    /// `add_sender` adds the sender field byte for byte as zbus's `Message::with_sender` adds it,
    /// to messages of either byte order, file descriptors and all.
    ///
    /// `add_sender` builds its message with `Message::from_parts`, which takes the new bytes and
    /// the duplicates of the file descriptors as one whole message. The body of the message still
    /// holds what its signature says.
    #[test]
    fn add_sender_matches_with_sender() {
        let sender = UniqueName::try_from(":busd.42").unwrap();
        let files = [(); 2].map(|()| File::open("/dev/null").unwrap());
        for endian in [Endian::Little, Endian::Big] {
            for msg in messages(endian, &files) {
                let ours = add_sender(&msg, &sender).unwrap();
                let theirs = msg.with_sender(&sender).unwrap();
                assert_eq!(ours.data().bytes(), theirs.data().bytes(), "{msg:?}");
                assert_eq!(ours.header().sender(), Some(&sender));
                assert_eq!(ours.data().fds().len(), msg.data().fds().len());
                ours.body().validate().unwrap();
            }
        }
    }

    /// Four messages of `endian` without a sender field, each with its own serial:
    ///
    /// - a call with a body of several types;
    /// - a signal without a body;
    /// - a call to a unique name whose body ends off an 8-byte boundary;
    /// - a signal that brings `files`.
    fn messages(endian: Endian, files: &[File; 2]) -> [Message; 4] {
        let serial = |n| NonZeroU32::new(n).unwrap();

        [
            Message::method_call("/org/zbus/Test", "Ping")
                .destination("org.zbus.Test")
                .interface("org.zbus.Test")
                .serial(serial(1))
                .endian(endian)
                .build(&(1u8, "abc", vec![1u32, 2, 3])),
            Message::signal("/a", "a.b", "C")
                .serial(serial(2))
                .endian(endian)
                .build(&()),
            Message::method_call("/", "X")
                .destination(":1.7")
                .serial(serial(3))
                .endian(endian)
                .build(&("x".repeat(13),)),
            Message::signal("/org/zbus/Test", "org.zbus.Test", "Fds")
                .serial(serial(4))
                .endian(endian)
                .build(&(Fd::from(&files[0]), Fd::from(&files[1]))),
        ]
        .map(Result::unwrap)
    }
}
