use std::{
    pin::Pin,
    task::{ready, Context, Poll},
};

use crate::trace;
use anyhow::{bail, Result};
use futures_util::{Stream as FutureStream, StreamExt};
use zbus::{message, names::OwnedUniqueName, Message, MessageStream};

use crate::peer::Peer;

/// Message stream for a peer.
///
/// This stream ensures the following for each message produced:
///
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
    type Item = Result<Message>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Message>>> {
        let this = self.get_mut();
        let msg = ready!(this.msg_stream.poll_next_unpin(cx));

        Poll::Ready(msg.map(|msg| checked(msg?, &this.unique_name)))
    }
}

/// `msg`, which the peer `unique_name` sent, with its sender field set if it has none.
fn checked(msg: Message, unique_name: &OwnedUniqueName) -> Result<Message> {
    let header = msg.header();

    // Ensure destination field is present and readable for non-signals.
    if msg.message_type() != message::Type::Signal && header.destination().is_none() {
        bail!("missing destination field");
    }

    // Ensure sender field is present. If it is not we add it using the unique name of the peer, as
    // most clients leave it out.
    match header.sender() {
        Some(sender) if *sender == *unique_name => Ok(msg),
        Some(_) => bail!("failed to parse message: Invalid sender field"),
        None => {
            let new_msg = msg.with_sender(unique_name)?;
            trace!("Added sender field to message: {:?}", new_msg);

            Ok(new_msg)
        }
    }
}
