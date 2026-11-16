use std::{future::poll_fn, num::NonZeroU32, sync::Arc, task::Poll};

use anyhow::Result;
use futures_util::{
    future::{select, Either},
    stream::StreamExt,
};
use zbus::{
    names::{BusName, OwnedUniqueName},
    MessageStream,
};
use zruntime::EventListener;

use super::{forward::no_reply, outbox::Outbox, Peers};
use crate::{
    debug,
    name_registry::NameOwnerChanged,
    peer::{Caller, Invalid, Peer, PendingCalls, Stream},
    trace, warn,
};

impl Peers {
    /// Serve `peer`, whose messages come out of `peer_stream`, until it goes, and then the monitor
    /// that it becomes, if it becomes one.
    pub(super) async fn serve_peer(
        self: Arc<Self>,
        mut peer_stream: Stream,
        mut cancellation_listener: EventListener,
        peer: Served,
    ) -> Result<()> {
        let Served {
            unique_name,
            id,
            pending_calls,
            caller,
        } = peer;
        let mut batch = Vec::new();
        let mut outbox = Outbox::default();
        // Whether the peer became a monitor, or the bus pauses to hand over, rather than the peer
        // went.
        let canceled = loop {
            let msg = match select(cancellation_listener, peer_stream.next()).await {
                Either::Left(_) => break true,
                Either::Right((None, _)) => break false,
                Either::Right((Some(msg), listener)) => {
                    cancellation_listener = listener;

                    msg
                }
            };
            // The messages that the peer sent after this one go along with it, as far as they are
            // here already.
            let mut next = Some(msg);
            let mut bytes = 0;
            let mut violation = None;
            let mut ended = false;
            while let Some(msg) = next.take() {
                match msg {
                    Ok(msg) => {
                        bytes += msg.data().len();
                        batch.push(msg);
                    }
                    Err(Invalid::Dropped(e)) => debug!("{e}"),
                    Err(invalid) => {
                        violation = Some(invalid);

                        break;
                    }
                }
                if batch.len() == MAX_BATCH || bytes >= MAX_BATCH_BYTES {
                    break;
                }
                // Polled with the waker of this task, which the stream keeps if it has nothing yet,
                // for the wait for the next batch.
                match poll_fn(|cx| Poll::Ready(peer_stream.poll_next_unpin(cx))).await {
                    Poll::Ready(Some(msg)) => next = Some(msg),
                    Poll::Ready(None) => ended = true,
                    Poll::Pending => (),
                }
            }

            self.forward(
                &unique_name,
                id,
                &caller,
                &pending_calls,
                &batch,
                &mut outbox,
            )
            .await?;
            batch.clear();
            if let Some(violation) = violation {
                warn!("Disconnecting `{unique_name}`, as it sent {violation}");

                break false;
            }
            if ended {
                break false;
            }
        };
        if canceled {
            self.serve_on(peer_stream.into_inner(), unique_name, id)
                .await;

            return Ok(());
        }
        trace!("Peer `{}` disconnected", unique_name);

        // The bus sends what follows on its own, which a hand-over has to wait for.
        let _chore = self.chore();
        // Stream is done means the peer disconnected, unless the peer sent a message that gets it
        // disconnected. Remove it from the list of peers, or from that of the monitors if it became
        // one meanwhile: that, and the end of its stream, close its connection.
        {
            let mut routing = self.routing_mut().await;
            if routing.remove_peer(unique_name.as_str()).is_none() {
                // The bus cleaned up after the peer as it turned it into a monitor, and the
                // `Monitoring` interface emits the signals then.
                routing.monitors.remove(&unique_name);
                // This task may have forwarded calls that the peer sent after the one that made it
                // a monitor, which can't get their replies to it either.
                for peer in routing.peers.values() {
                    peer.pending_calls().forget(id);
                }
                // A pause waits for the monitor no more.
                #[cfg(target_os = "linux")]
                self.paused_changed.notify(usize::MAX);

                return Ok(());
            }
        }
        // A pause waits for the peer no more.
        #[cfg(target_os = "linux")]
        self.paused_changed.notify(usize::MAX);

        self.clean_up(&unique_name, id, pending_calls.take().collect())
            .await
    }

    /// Go on with the peer `unique_name`, numbered `id`, whose messages come out of `stream`, now
    /// that the task that served it was canceled: serve the monitor that it became, unless the bus
    /// pauses to hand over, which takes the stream back.
    async fn serve_on(
        self: Arc<Self>,
        stream: MessageStream,
        unique_name: OwnedUniqueName,
        id: usize,
    ) {
        let Some((stream, listener)) = self.monitor_to_serve(stream, &unique_name, id).await else {
            return;
        };

        self.serve_monitor(stream, listener, unique_name).await;
    }

    /// What the task that served the peer `unique_name`, numbered `id`, whose messages come out of
    /// `stream`, serves the monitor that the peer became with: `stream`, and the listener of the
    /// cancellation of the task, taken under the lock that a pause takes, so that the task hears of
    /// any pause that it missed. Nothing if the peer is gone, or if the bus pauses to hand over,
    /// which takes `stream` back, as it does if the peer is still one.
    async fn monitor_to_serve(
        &self,
        stream: MessageStream,
        unique_name: &OwnedUniqueName,
        id: usize,
    ) -> Option<(MessageStream, EventListener)> {
        let mut guard = self.routing_mut().await;
        let routing = &mut *guard;
        #[cfg(target_os = "linux")]
        if let Some(peer) = routing.peers.get_mut(unique_name) {
            *peer.paused() = Some(stream);
            self.paused_changed.notify(usize::MAX);

            return None;
        }
        let monitor = routing.monitors.get_mut(unique_name)?;
        // This task may have forwarded calls that the peer sent after the one that made it a
        // monitor, as the bus cleaned up after it, which can't get their replies to it either.
        for peer in routing.peers.values() {
            peer.pending_calls().forget(id);
        }
        #[cfg(target_os = "linux")]
        if routing.handing_over {
            *monitor.paused() = Some(stream);
            self.paused_changed.notify(usize::MAX);

            return None;
        }

        Some((stream, monitor.listen_cancellation()))
    }

    /// Serve the monitor `unique_name`, whose messages come out of `stream`, until it goes, or
    /// until the bus pauses to hand over, which `cancellation_listener` tells.
    ///
    /// A monitor is to send nothing, so it gets disconnected as soon as it sends anything.
    pub(super) async fn serve_monitor(
        self: Arc<Self>,
        mut stream: MessageStream,
        cancellation_listener: EventListener,
        unique_name: OwnedUniqueName,
    ) {
        let msg = match select(cancellation_listener, stream.next()).await {
            Either::Left(_) => None,
            Either::Right((msg, _)) => Some(msg),
        };
        let Some(msg) = msg else {
            // The bus pauses to hand over, which takes the stream back, unless it let go of the
            // monitor.
            #[cfg(target_os = "linux")]
            if let Some(monitor) = self.routing_mut().await.monitors.get_mut(&unique_name) {
                *monitor.paused() = Some(stream);
                self.paused_changed.notify(usize::MAX);
            }

            return;
        };
        if let Some(Ok(_)) = msg {
            warn!(
                "Monitor {} sent a message, which is against the rules.",
                unique_name
            );
        }
        // Removing the monitor, and the end of its stream, close its connection.
        self.routing_mut().await.monitors.remove(&unique_name);
        // A pause waits for the monitor no more.
        #[cfg(target_os = "linux")]
        self.paused_changed.notify(usize::MAX);
        debug!("Monitor {} disconnected", unique_name);
    }

    /// Clean up after the peer `unique_name`, numbered `id`, which went, and whose calls that
    /// waited for its replies were those of `waiting`: the caller and the serial of each.
    async fn clean_up(
        &self,
        unique_name: &OwnedUniqueName,
        id: usize,
        waiting: Vec<(OwnedUniqueName, NonZeroU32)>,
    ) -> Result<()> {
        // The calls that the peer made won't get their replies to it now.
        for peer in self.routing().await.peers.values() {
            peer.pending_calls().forget(id);
        }
        let names_changes = self
            .routing_mut()
            .await
            .name_registry
            .release_all(unique_name.inner().clone())
            .await;

        self.finish_clean_up(unique_name, names_changes, waiting)
            .await
    }

    /// Finish cleaning up after the peer `unique_name`, which went, and whose names changed owners
    /// with `names_changes`, as they were released: signal the changes, and answer the calls that
    /// waited for its replies, `waiting`, with errors.
    pub(super) async fn finish_clean_up(
        &self,
        unique_name: &OwnedUniqueName,
        names_changes: Vec<NameOwnerChanged>,
        waiting: Vec<(OwnedUniqueName, NonZeroU32)>,
    ) -> Result<()> {
        for changed in names_changes {
            self.notify_name_changes(changed).await?;
        }
        self.notify_name_changes(NameOwnerChanged {
            name: BusName::from(unique_name.clone()).into(),
            old_owner: Some(unique_name.clone()),
            new_owner: None,
        })
        .await?;
        // Nor will the calls that wait on the peer get its replies, so they get an error from the
        // bus instead, as with dbus-broker, rather than wait for as long as their callers do.
        for (caller, serial) in waiting {
            let error = no_reply(&caller, serial)?;
            self.send_from_bus(&error).await;
        }

        Ok(())
    }
}

/// What the task that serves a peer keeps of the peer: see [`Peers::serve_peer`].
pub(super) struct Served {
    unique_name: OwnedUniqueName,
    /// The number of the peer.
    id: usize,
    /// The calls to the peer that wait for its reply.
    pending_calls: Arc<PendingCalls>,
    /// The peer, as the caller of the calls that wait for replies.
    caller: Arc<Caller>,
}

impl Served {
    /// What the task that serves `peer` keeps of it.
    pub(super) fn new(peer: &Peer) -> Self {
        Self {
            unique_name: peer.unique_name().clone(),
            id: peer.id(),
            pending_calls: peer.pending_calls().clone(),
            caller: peer.caller().clone(),
        }
    }
}

/// How many messages of a peer the bus forwards in one batch, at most.
pub const MAX_BATCH: usize = 16;

/// How many bytes of a peer's messages a batch takes before it ends.
///
/// A batch ends with the message that brings it to this or more, so its messages before the last
/// come to less than this. The time limit of a send to a client,
/// [`SEND_TIMEOUT`](crate::peer::SEND_TIMEOUT), covers all the messages of a batch that go to the
/// client, rather than one message, so a client has to take less than this much more within the
/// time limit than if the bus sent it each message on its own. Messages of up to 1 KiB still go up
/// to [`MAX_BATCH`] in a batch.
pub const MAX_BATCH_BYTES: usize = 16 * 1024;
