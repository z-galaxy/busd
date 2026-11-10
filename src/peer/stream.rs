use std::{
    fmt,
    pin::Pin,
    task::{ready, Context, Poll},
};

use crate::trace;
use anyhow::anyhow;
use futures_util::{Stream as FutureStream, StreamExt};
use zbus::{message, names::OwnedUniqueName, Message, MessageStream};

use crate::peer::{Peer, MAX_MESSAGE_FDS};

/// Message stream for a peer.
///
/// This stream ensures the following for each message produced:
///
/// * It carries no more file descriptors than the bus can pass on, [`MAX_MESSAGE_FDS`].
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
            unique_name: peer.unique_name().clone(),
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
    /// It carries more file descriptors than the bus can pass on, [`MAX_MESSAGE_FDS`]: the bus
    /// disconnects the peer.
    TooManyFds,
    /// Anything else that is wrong with it: the bus drops the message, and serves the peer on.
    Dropped(anyhow::Error),
}

impl fmt::Display for Invalid {
    /// What the peer sent, to go on from "Disconnecting the peer, as it sent".
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyFds => write!(
                f,
                "a message with more than {MAX_MESSAGE_FDS} file descriptors"
            ),
            Self::Dropped(e) => write!(f, "{e}"),
        }
    }
}

/// `msg`, which the peer `unique_name` sent, with its sender field set if it has none.
fn checked(msg: Message, unique_name: &OwnedUniqueName) -> Result<Message, Invalid> {
    // Passing such a message on would fail, and get its recipients disconnected.
    if msg.data().fds().len() > MAX_MESSAGE_FDS {
        return Err(Invalid::TooManyFds);
    }

    let header = msg.header();

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
            let new_msg = msg
                .with_sender(unique_name)
                .map_err(|e| Invalid::Dropped(e.into()))?;
            trace!("Added sender field to message: {:?}", new_msg);

            Ok(new_msg)
        }
    }
}
