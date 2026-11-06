#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
use zbus::MessageStream;
use zbus::{
    names::{BusName, OwnedUniqueName},
    Connection,
};
use zruntime::{Event, EventListener};

use crate::{match_rules::MatchRules, name_registry::NameRegistry, user::Charge};

use super::{Peer, Recipient};

/// A peer connection.
#[derive(Debug)]
pub struct Monitor {
    conn: Connection,
    recipient: Recipient,
    id: usize,
    unique_name: OwnedUniqueName,
    match_rules: MatchRules,
    /// The monitor, as an object that the bus keeps for its user, as it was as a peer.
    _charge: Option<Charge>,
    /// Notified as the monitor goes, and as the bus pauses to hand over, for the task that serves
    /// the monitor to see why.
    canceled_event: Event,
    /// The stream of the messages of the monitor, once the task that served the monitor, or the
    /// peer that it was, gave it back, as the bus pauses to hand over.
    #[cfg(target_os = "linux")]
    paused: Option<MessageStream>,
}

impl Monitor {
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Where the bus sends the messages for the monitor: where it sent those for its peer.
    pub fn recipient(&self) -> &Recipient {
        &self.recipient
    }

    /// The number of the peer that became the monitor, which no other peer, or monitor, that the
    /// bus ever has shares.
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    pub fn match_rules(&self) -> &MatchRules {
        &self.match_rules
    }

    /// # Panics
    ///
    /// Same as [`MatchRules::matches`].
    pub fn interested(&self, msg: &zbus::Message, name_registry: &NameRegistry) -> bool {
        if self.match_rules.is_empty()
            || msg.header().destination() == Some(&BusName::from(&self.unique_name))
        {
            return true;
        }
        self.match_rules.matches(msg, name_registry)
    }

    pub fn listen_cancellation(&self) -> EventListener {
        self.canceled_event.listen()
    }

    /// Pause the monitor, as the bus hands over from `since`, as [`Peer::pause`] pauses a peer.
    #[cfg(target_os = "linux")]
    pub(crate) fn pause(&self, since: Instant) {
        self.recipient.hand_over_from(since);
        self.canceled_event.notify(usize::MAX);
    }

    /// The stream of the messages of the monitor, if the task that served the monitor, or the peer
    /// that it was, gave it back, as the bus pauses to hand over.
    #[cfg(target_os = "linux")]
    pub(crate) fn paused(&mut self) -> &mut Option<MessageStream> {
        &mut self.paused
    }

    pub(super) fn new(mut peer: Peer, match_rules: MatchRules) -> Self {
        Self {
            conn: peer.conn().clone(),
            recipient: peer.recipient().clone(),
            id: peer.id(),
            unique_name: peer.unique_name().clone(),
            match_rules,
            _charge: peer.charge.take(),
            canceled_event: Event::new(),
            #[cfg(target_os = "linux")]
            paused: peer.paused.take(),
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.canceled_event.notify(usize::MAX);
    }
}
