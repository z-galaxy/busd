use zbus::names::BusName;

use crate::name_registry::NameRegistry;

use super::Client;

/// A peer connection.
#[derive(Debug)]
pub struct Monitor {
    client: Client,
}

impl Monitor {
    /// The monitor, as a client of the bus.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// # Panics
    ///
    /// Same as [`MatchRules::matches`].
    ///
    /// [`MatchRules::matches`]: crate::match_rules::MatchRules::matches
    pub fn interested(&self, msg: &zbus::Message, name_registry: &NameRegistry) -> bool {
        let match_rules = self.client.match_rules();
        if match_rules.is_empty()
            || msg.header().destination() == Some(&BusName::from(self.client.unique_name()))
        {
            return true;
        }
        match_rules.matches(msg, name_registry)
    }

    /// The monitor, as a client of the bus, as a mutable reference.
    #[cfg(target_os = "linux")]
    pub(crate) fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }

    /// Turn `client`, a peer until then, into a monitor.
    pub(super) fn new(client: Client) -> Self {
        Self { client }
    }
}
