use std::sync::{Arc, MutexGuard, PoisonError};

use super::Peers;

impl Peers {
    /// Note a chore of the bus, under way until the returned guard goes.
    ///
    /// A chore is work that the bus does on its own, besides forwarding the messages of its peers
    /// and answering the calls to it, and that sends messages, such as its clean-up after a peer
    /// that goes. A hand-over waits for them.
    pub(crate) fn chore(self: &Arc<Self>) -> Chore {
        let mut chores = self.chores();
        chores.running += 1;
        chores.started += 1;

        Chore(self.clone())
    }

    pub(super) fn chores(&self) -> MutexGuard<'_, Chores> {
        // Nothing can panic while the lock is held.
        self.chores.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A chore of the bus, under way until this goes: see [`Peers::chore`].
#[derive(Debug)]
pub(crate) struct Chore(Arc<Peers>);

impl Drop for Chore {
    fn drop(&mut self) {
        self.0.chores().running -= 1;
        self.0.chores_changed.notify(usize::MAX);
    }
}

/// A client whose connection the bus is setting up, until this goes.
pub(super) struct Connecting<'p>(&'p Peers);

impl<'p> Connecting<'p> {
    pub(super) fn new(peers: &'p Peers) -> Self {
        peers.chores().connecting += 1;

        Self(peers)
    }
}

impl Drop for Connecting<'_> {
    fn drop(&mut self) {
        self.0.chores().connecting -= 1;
        self.0.chores_changed.notify(usize::MAX);
    }
}

/// What the bus does on its own, besides forwarding the messages of its peers.
#[derive(Debug, Default)]
pub(super) struct Chores {
    /// How many chores are under way.
    pub(super) running: usize,
    /// How many chores have started, ever, so that a wait for the bus to settle can tell whether
    /// one started meanwhile.
    pub(super) started: u64,
    /// How many clients the bus is setting the connections of up: their handshakes are under way.
    pub(super) connecting: usize,
}
