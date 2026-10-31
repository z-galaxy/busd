//! A client connection that many callers share, as many tasks of one program would.
//!
//! The replies to the calls of the callers that share a connection all come back on it. A task on
//! the runtime of the thread reads them off a stream of the connection, and hands each to the
//! caller whose call it answers, as told by the serial number of the call. The calls and the
//! replies have no sender, for the bus to set, as `calls.rs` says.

use std::{
    collections::HashMap,
    num::{NonZeroU32, NonZeroUsize},
    sync::{Arc, Mutex},
};

use futures_util::TryStreamExt;
use zbus::{names::BusName, Connection, Message, MessageStream};
use zruntime::{mpmc, Shared, Task};

use crate::calls::{call_message, check_reply, replied_to, Result};

/// A client connection that many callers share, and what hands the replies that come back on it
/// to the callers.
pub struct SharedConnection {
    /// The connection that the calls are sent on.
    connection: Connection,
    /// The service that is called.
    destination: BusName<'static>,
    /// The callers that wait for a reply.
    pending: Pending,
    /// The task that hands the replies to the callers, which ends once this is dropped.
    _dispatcher: Task<(), Shared>,
}

impl SharedConnection {
    /// Makes a connection of `callers` callers of the service at `destination` out of
    /// `connection`.
    ///
    /// The task that hands out the replies runs on the runtime of the calling thread.
    pub fn new(connection: Connection, destination: BusName<'static>, callers: usize) -> Self {
        // The stream exists before the first call is sent, as a reply that arrives before it does
        // is not delivered to it.
        let mut replies = MessageStream::from(&connection);
        let queue = NonZeroUsize::new(callers + QUEUE_SPARE).expect("room for the spare replies");
        replies.set_max_queued(queue);
        let pending = Pending::default();
        let dispatcher = zruntime::spawn(dispatch(replies, pending.clone()));

        Self {
            connection,
            destination,
            pending,
            _dispatcher: dispatcher,
        }
    }

    /// Calls the service with `data` as the byte array, and waits for the reply.
    ///
    /// Fails if the service replies with an error, or the connection closes first.
    pub async fn call(&self, data: &[u8]) -> Result<()> {
        let msg = call_message(&self.destination, data)?;
        let (reply_to, reply) = mpmc::bounded(NonZeroUsize::MIN);
        // The caller is entered before the call is sent, and not after: the reply could be handed
        // out as soon as the call is on its way, and be lost for want of a caller to hand it to.
        let serial = msg.primary_header().serial_num();
        self.pending
            .lock()
            .expect(UNPOISONED)
            .insert(serial, reply_to);
        self.connection.send(&msg).await?;

        let reply = reply
            .recv()
            .await
            .map_err(|_| "the connection closed before the reply came")?;

        check_reply(&reply)
    }
}

/// How many replies more than there are callers the queue of the shared connection's stream has
/// room for.
///
/// zbus holds back the reader of a connection's socket whenever the queue of one of its streams is
/// full. A queue for the reply of every caller, and this many to spare, rules that out, so that
/// the replies are read off the socket as fast as the bus writes them, whether or not the task
/// that hands them to the callers has had its turn.
const QUEUE_SPARE: usize = 16;

/// The callers that wait for a reply, by the serial number of their call, each with the sender on
/// which the reply is handed to it.
///
/// The task that hands out the replies has to be `Send`, as `zruntime::spawn` requires. It runs on
/// the thread of the callers all the same, so the lock is never contended.
type Pending = Arc<Mutex<HashMap<NonZeroU32, mpmc::Sender<Message>>>>;

/// Hands each reply that `replies` yields to the caller in `pending` that waits for it, until the
/// stream ends or fails.
///
/// Then the callers that still wait are failed, by dropping what they wait on.
async fn dispatch(mut replies: MessageStream, pending: Pending) {
    while let Ok(Some(msg)) = replies.try_next().await {
        let Some(serial) = replied_to(&msg) else {
            continue;
        };
        let Some(reply_to) = pending.lock().expect(UNPOISONED).remove(&serial) else {
            continue;
        };
        // The channel has room for the one reply that its caller waits for, and a caller that
        // gave up has nothing to hand it to.
        let _ = reply_to.try_send(msg);
    }
    pending.lock().expect(UNPOISONED).clear();
}

/// What is expected of the lock of a `Pending`: it is held only to insert, remove or clear an
/// entry, which cannot panic, so it is never poisoned.
const UNPOISONED: &str = "nothing panics while it holds the lock";
