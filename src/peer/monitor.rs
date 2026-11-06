use zbus::{
    names::{BusName, OwnedUniqueName},
    Connection,
};

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

    pub(super) fn new(mut peer: Peer, match_rules: MatchRules) -> Self {
        Self {
            conn: peer.conn().clone(),
            recipient: peer.recipient().clone(),
            id: peer.id(),
            unique_name: peer.unique_name().clone(),
            match_rules,
            _charge: peer.charge.take(),
        }
    }
}
