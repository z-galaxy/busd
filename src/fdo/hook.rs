//! A hook of the tests in the bus: two methods on paths of their own, which hold the answer of the
//! bus to a call at gates that a test opens.
//!
//! - `Pass` of [`GATED_INTERFACE`], on [`GATED_PATH`], waits at [`Hook::gate`], then has the bus
//!   send as many `Passed` signals as it is told, each with its number, and its reply.
//! - `Follow` of [`FOLLOWING_INTERFACE`], on [`FOLLOWING_PATH`], has the bus send its reply, then
//!   waits at [`Hook::follow`], then has the bus send a `Followed` signal, which follows the reply.

use std::sync::atomic::{AtomicBool, Ordering};

use zbus::Message;
use zruntime::Event;

use super::{reply, BUS_NAME};
use crate::peers::Peers;

/// The gates of the hook of a bus.
#[derive(Debug, Default)]
pub(crate) struct Hook {
    /// What `Pass` waits at, before it answers.
    pub(crate) gate: Gate,
    /// What the signal of `Follow` waits at, once the bus sent the reply.
    pub(crate) follow: Gate,
}

/// A gate that the answer of the bus to a call waits at, until the test opens it.
#[derive(Debug, Default)]
pub(crate) struct Gate {
    reached: AtomicBool,
    open: AtomicBool,
    changed: Event,
}

impl Gate {
    /// Wait for an answer to have reached the gate.
    pub(crate) async fn reached(&self) {
        self.wait_for(&self.reached).await;
    }

    /// Open the gate, for the answers that wait at it, and those that reach it, to pass.
    pub(crate) fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.changed.notify(usize::MAX);
    }

    /// Wait at the gate until it is open, and tell that it was reached.
    pub(crate) async fn pass(&self) {
        self.reached.store(true, Ordering::SeqCst);
        self.changed.notify(usize::MAX);
        self.wait_for(&self.open).await;
    }

    async fn wait_for(&self, flag: &AtomicBool) {
        loop {
            // The listener comes first, so that a change right after the check isn't missed.
            let listener = self.changed.listen();
            if flag.load(Ordering::SeqCst) {
                return;
            }
            listener.await;
        }
    }
}

/// The path of `Pass`.
pub(crate) const GATED_PATH: &str = "/org/busd/Test/Gated";

/// The interface of `Pass`.
pub(crate) const GATED_INTERFACE: &str = "org.busd.Test.Gated";

/// The path of `Follow`.
pub(crate) const FOLLOWING_PATH: &str = "/org/busd/Test/Following";

/// The interface of `Follow`.
pub(crate) const FOLLOWING_INTERFACE: &str = "org.busd.Test.Following";

/// What the bus of `peers` sends in answer to `call`, if it is a call of the hook, past what it
/// sent already.
pub(super) async fn answer(peers: &Peers, call: &Message) -> Option<Vec<Message>> {
    let header = call.header();
    let hook = peers.hook();
    match header.path()?.as_str() {
        GATED_PATH => {
            let signals: u32 = call.body().deserialize().unwrap();
            hook.gate.pass().await;
            let passed = (0..signals).map(|n| {
                Message::signal(GATED_PATH, GATED_INTERFACE, "Passed")
                    .sender(BUS_NAME)
                    .build(&n)
                    .unwrap()
            });

            Some(passed.chain([reply(&header, &()).unwrap()]).collect())
        }
        FOLLOWING_PATH => {
            peers.send_from_bus(&[reply(&header, &()).unwrap()]).await;
            hook.follow.pass().await;
            let followed = Message::signal(FOLLOWING_PATH, FOLLOWING_INTERFACE, "Followed")
                .sender(BUS_NAME)
                .build(&())
                .unwrap();

            Some(vec![followed])
        }
        _ => None,
    }
}
