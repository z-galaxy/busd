use std::pin::Pin;

use anyhow::{bail, Error, Result};
use futures_util::{Stream as FutureStream, TryStream, TryStreamExt};
use tracing::trace;
use zbus::{message, Message, MessageStream};

use crate::peer::Peer;

/// Message stream for a peer.
///
/// This stream ensures the following for each message produced:
///
/// * The destination field is present and readable for non-signals.
/// * The sender field is present and set to the unique name of the peer.
pub struct Stream {
    stream: Pin<Box<StreamInner>>,
}

type StreamInner = dyn TryStream<Ok = Message, Error = Error, Item = Result<Message>> + Send;

impl Stream {
    pub fn for_peer(peer: &Peer, msg_stream: MessageStream) -> Self {
        let unique_name = peer.unique_name().clone();
        let stream = msg_stream.map_err(Into::into).and_then(move |msg| {
            let unique_name = unique_name.clone();
            async move {
                let header = msg.header();

                // Ensure destination field is present and readable for non-signals.
                if msg.message_type() != message::Type::Signal && header.destination().is_none() {
                    bail!("missing destination field");
                }

                // Ensure sender field is present. If it is not we add it using the unique name
                // of the peer.
                match header.sender() {
                    Some(sender) if *sender == unique_name => Ok(msg),
                    Some(_) => bail!("failed to parse message: Invalid sender field"),
                    None => {
                        let signature = header.signature();
                        let body = msg.body();
                        let body_bytes = body.data();
                        let fds = body_bytes
                            .fds()
                            .iter()
                            .map(|fd| fd.try_clone().map(Into::into))
                            .collect::<zbus::zvariant::Result<Vec<_>>>()?;
                        let builder =
                            message::Builder::from(header.clone()).sender(&unique_name)?;
                        let new_msg =
                            unsafe { builder.build_raw_body(body_bytes, signature, fds)? };
                        trace!("Added sender field to message: {:?}", new_msg);

                        Ok(new_msg)
                    }
                }
            }
        });

        Self {
            stream: Box::pin(stream),
        }
    }
}

impl FutureStream for Stream {
    type Item = Result<Message>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<Option<Result<Message>>> {
        FutureStream::poll_next(Pin::new(&mut self.get_mut().stream), cx)
    }
}
