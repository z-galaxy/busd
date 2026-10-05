use std::future::Future;

use crate::warn;
use futures_util::StreamExt;
use zbus::{
    names::{BusName, OwnedUniqueName},
    Connection, MessageStream,
};

use crate::{match_rules::MatchRules, name_registry::NameRegistry};

use super::Peer;

/// A peer connection.
#[derive(Debug)]
pub struct Monitor {
    conn: Connection,
    unique_name: OwnedUniqueName,
    match_rules: MatchRules,
}

impl Monitor {
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    /// # Panics
    ///
    /// Same as [`MatchRules::matches`].
    pub fn interested(&self, msg: &zbus::Message, name_registry: &NameRegistry) -> bool {
        if self.match_rules.is_empty()
            || msg.header().destination().unwrap() == &BusName::from(&self.unique_name)
        {
            return true;
        }
        self.match_rules.matches(msg, name_registry)
    }

    /// Monitor the monitor.
    ///
    /// This method returns once the monitor connection is closed.
    pub fn monitor(&self) -> impl Future<Output = ()> + 'static {
        let mut stream = MessageStream::from(&self.conn);
        let unique_name = self.unique_name.clone();
        async move {
            if let Some(Ok(_)) = stream.next().await {
                warn!(
                    "Monitor {} sent a message, which is against the rules.",
                    unique_name
                );
            }
        }
    }

    pub(super) fn new(peer: Peer, match_rules: MatchRules) -> Self {
        Self {
            conn: peer.conn().clone(),
            unique_name: peer.unique_name().clone(),
            match_rules,
        }
    }
}
