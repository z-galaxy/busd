mod stream;
use event_listener::{Event, EventListener};
pub use stream::*;
mod monitor;
pub use monitor::*;

use anyhow::Result;
use tracing::trace;
use zbus::{
    connection::{self, socket::BoxedSplit},
    names::{BusName, OwnedUniqueName},
    AuthMechanism, Connection, MessageStream, OwnedGuid, OwnedMatchRule,
};

use crate::{fdo, match_rules::MatchRules, name_registry::NameRegistry};

/// A peer connection.
#[derive(Debug)]
pub struct Peer {
    conn: Connection,
    unique_name: OwnedUniqueName,
    match_rules: MatchRules,
    greeted: bool,
    canceled_event: Event,
}

impl Peer {
    pub async fn new(
        guid: OwnedGuid,
        id: usize,
        socket: BoxedSplit,
        auth_mechanism: AuthMechanism,
    ) -> Result<(Self, Stream)> {
        let unique_name = OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap();
        // Use `build_message_stream` to activate a message receiver before the socket reader
        // task is spawned, so that messages the client pipelines right after authentication
        // (notably `Hello`) are never dropped in the race window between `build()` and stream
        // creation. See https://github.com/z-galaxy/zbus/pull/1760.
        let msg_stream = connection::Builder::socket(socket)
            .server(guid)?
            .p2p()
            .auth_mechanism(auth_mechanism)
            .build_message_stream()
            .await?;
        let conn = Connection::from(&msg_stream);
        trace!("created: {:?}", conn);

        let peer = Self {
            conn,
            unique_name,
            match_rules: MatchRules::default(),
            greeted: false,
            canceled_event: Event::new(),
        };
        let stream = Stream::for_peer(&peer, msg_stream);
        Ok((peer, stream))
    }

    // This the the bus itself, serving the FDO D-Bus API.
    pub async fn new_us(msg_stream: MessageStream) -> (Self, Stream) {
        let unique_name = OwnedUniqueName::try_from(fdo::BUS_NAME).unwrap();
        let conn = Connection::from(&msg_stream);

        let peer = Self {
            conn,
            unique_name,
            match_rules: MatchRules::default(),
            greeted: true,
            canceled_event: Event::new(),
        };
        let stream = Stream::for_peer(&peer, msg_stream);
        (peer, stream)
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn listen_cancellation(&self) -> EventListener {
        self.canceled_event.listen()
    }

    /// # Panics
    ///
    /// Same as [`MatchRules::matches`].
    pub fn interested(&self, msg: &zbus::Message, name_registry: &NameRegistry) -> bool {
        msg.header().destination() == Some(&BusName::Unique(self.unique_name.as_ref()))
            || self.match_rules.matches(msg, name_registry)
    }

    pub fn add_match_rule(&mut self, rule: OwnedMatchRule) {
        self.match_rules.add(rule);
    }

    /// Remove the first rule that matches.
    pub fn remove_match_rule(&mut self, rule: OwnedMatchRule) -> zbus::fdo::Result<()> {
        self.match_rules.remove(rule)
    }

    /// This can only be called once.
    pub async fn hello(&mut self) -> zbus::fdo::Result<()> {
        if self.greeted {
            return Err(zbus::fdo::Error::Failed(
                "Can only call `Hello` method once".to_string(),
            ));
        }
        self.greeted = true;

        Result::Ok(())
    }

    pub fn become_monitor(self, match_rules: MatchRules) -> Monitor {
        Monitor::new(self, match_rules)
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.canceled_event.notify(usize::MAX);
    }
}
