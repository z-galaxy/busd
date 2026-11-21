use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use zruntime::Event;
#[cfg(target_os = "linux")]
use zruntime::EventListener;

/// What the bus does on its own, besides forwarding its peers' messages, and an event that is
/// notified when a chore ends or a handshake ends. Starting a chore or a handshake doesn't notify
/// the event.
///
/// A handle: cheap to clone, and the clones share the counts.
#[derive(Clone, Debug, Default)]
pub(crate) struct Chores {
    inner: Arc<Inner>,
}

impl Chores {
    /// Note a chore of the bus, under way until the returned guard goes.
    ///
    /// A chore is work that the bus does on its own, besides forwarding the messages of its peers,
    /// and that sends messages, such as its clean-up after a peer that goes. A call to the bus is a
    /// chore too, from the moment its caller's task queues it for the bus until the bus has
    /// answered it. A hand-over waits for them.
    pub(crate) fn start(&self) -> Chore {
        {
            let mut counts = self.inner.counts();
            counts.running += 1;
            counts.started += 1;
        }

        Chore(self.clone())
    }

    /// Note a client whose connection the bus is setting up, until the returned guard is dropped.
    pub(super) fn connecting(&self) -> Connecting {
        self.inner.counts().connecting += 1;

        Connecting(self.clone())
    }

    /// The number of clients whose connections the bus is setting up.
    pub(super) fn handshakes(&self) -> usize {
        self.inner.counts().connecting
    }

    /// The number of chores under way.
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn running(&self) -> usize {
        self.inner.counts().running
    }

    /// A listener that is notified when the next chore or handshake ends. A chore or a handshake
    /// that starts doesn't notify it.
    #[cfg(target_os = "linux")]
    pub(super) fn listen(&self) -> EventListener {
        self.inner.changed.listen()
    }

    /// Wait until no chore is under way, and return how many chores have started by then.
    #[cfg(target_os = "linux")]
    pub(super) async fn done(&self) -> u64 {
        loop {
            // The listener comes first, so that a chore that ends right after the check isn't
            // missed.
            let listener = self.listen();
            let started = {
                let counts = self.inner.counts();

                (counts.running == 0).then_some(counts.started)
            };
            if let Some(started) = started {
                return started;
            }
            listener.await;
        }
    }

    /// Whether no chore is under way and none started since the count of started chores was
    /// `started`.
    #[cfg(target_os = "linux")]
    pub(super) fn settled_since(&self, started: u64) -> bool {
        let counts = self.inner.counts();

        counts.running == 0 && counts.started == started
    }
}

/// A chore of the bus, under way until this guard is dropped. See [`Chores::start`].
#[derive(Debug)]
pub(crate) struct Chore(Chores);

impl Drop for Chore {
    fn drop(&mut self) {
        self.0.inner.counts().running -= 1;
        self.0.inner.changed.notify(usize::MAX);
    }
}

/// A client whose connection the bus is setting up, until this goes.
#[derive(Debug)]
pub(super) struct Connecting(Chores);

impl Drop for Connecting {
    fn drop(&mut self) {
        self.0.inner.counts().connecting -= 1;
        self.0.inner.changed.notify(usize::MAX);
    }
}

#[derive(Debug, Default)]
struct Inner {
    counts: Mutex<Counts>,
    /// Notified when a chore or a handshake ends, that is when a [`Chore`] or a [`Connecting`]
    /// guard is dropped. Starting a chore or a handshake changes `counts` without notifying.
    changed: Event,
}

impl Inner {
    fn counts(&self) -> MutexGuard<'_, Counts> {
        // Nothing can panic while the lock is held.
        self.counts.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Default)]
struct Counts {
    /// How many chores are under way.
    running: usize,
    /// How many chores have started, ever, so that a wait for the bus to settle can tell whether
    /// one started meanwhile.
    started: u64,
    /// How many clients the bus is setting the connections of up: their handshakes are under way.
    connecting: usize,
}
