use zbus::names::BusName;

use crate::name_registry::NameRegistry;

use super::Client;

/// A monitor: a client that, as a peer, asked the bus to see the messages that go through it
/// (`org.freedesktop.DBus.Monitoring.BecomeMonitor`), and is not a peer anymore.
///
/// The bus sends it the messages that it is [`interested`](Self::interested) in, and takes no
/// messages from it: the bus disconnects a monitor that sends a message.
#[derive(Debug)]
pub struct Monitor {
    client: Client,
}

impl Monitor {
    /// The monitor, as a client of the bus.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Whether the monitor gets `msg`. A monitor without match rules gets every message. Otherwise
    /// it gets the messages that one of its rules matches, and the messages for the monitor itself.
    ///
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

    /// The monitor's client, which the monitor gives up.
    #[cfg(target_os = "linux")]
    pub(crate) fn into_client(self) -> Client {
        self.client
    }

    /// Turn `client`, a peer until then, into a monitor.
    pub(super) fn new(client: Client) -> Self {
        Self { client }
    }
}
