mod stream;
pub use stream::*;
use zruntime::{Event, EventListener};
mod monitor;
pub use monitor::*;
mod recipient;
pub use recipient::*;
mod pending_calls;
pub use pending_calls::*;

use crate::trace;
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use zbus::{
    connection::{self, AuthMechanism},
    names::{BusName, OwnedUniqueName},
    Connection, MessageStream, OwnedGuid, OwnedMatchRule,
};

use crate::{fdo, match_rules::MatchRules, name_registry::NameRegistry};

/// A peer connection.
#[derive(Debug)]
pub struct Peer {
    conn: Connection,
    recipient: Recipient,
    id: usize,
    unique_name: OwnedUniqueName,
    match_rules: MatchRules,
    /// The calls to the peer that wait for its reply.
    pending_calls: Arc<PendingCalls>,
    greeted: bool,
    canceled_event: Event,
}

impl Peer {
    pub async fn new(
        guid: OwnedGuid,
        id: usize,
        builder: connection::Builder<'_>,
        send_timeout: Duration,
    ) -> Result<(Self, Stream)> {
        debug_assert_ne!(id, BUS_ID, "a client numbered as the bus");
        let unique_name = OwnedUniqueName::try_from(format!(":busd.{id}")).unwrap();
        // Use `build_message_stream` to activate a message receiver before the socket reader
        // task is spawned, so that messages the client pipelines right after authentication
        // (notably `Hello`) are never dropped in the race window between `build()` and stream
        // creation. See https://github.com/z-galaxy/zbus/pull/1760.
        //
        // zbus checks the body of each message of the client as it receives it, as the bus passes
        // the bodies of messages on without deserializing them, and the connection ends on one
        // that doesn't hold what its signature says, as with dbus-broker.
        //
        // A client authenticates with the EXTERNAL mechanism, the only one that dbus-broker takes
        // too: as the uid that its socket tells.
        let msg_stream = builder
            .server(guid)
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .validate_bodies()
            .build_message_stream()
            .await?;
        let conn = Connection::from(&msg_stream);
        trace!("created: {:?}", conn);

        let peer = Self {
            recipient: Recipient::client(conn.clone(), unique_name.clone(), send_timeout)?,
            conn,
            id,
            unique_name,
            match_rules: MatchRules::default(),
            pending_calls: Arc::default(),
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
            recipient: Recipient::bus(conn.clone()),
            conn,
            id: BUS_ID,
            unique_name,
            match_rules: MatchRules::default(),
            pending_calls: Arc::default(),
            greeted: true,
            canceled_event: Event::new(),
        };
        let stream = Stream::for_peer(&peer, msg_stream);
        (peer, stream)
    }

    pub fn unique_name(&self) -> &OwnedUniqueName {
        &self.unique_name
    }

    /// The number of the peer, which no other peer that the bus ever has shares: 0 for the bus
    /// itself, and from 1 on for clients, whose unique names carry it.
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Where the bus sends the messages for the peer.
    pub fn recipient(&self) -> &Recipient {
        &self.recipient
    }

    /// The calls to the peer that wait for its reply.
    pub fn pending_calls(&self) -> &Arc<PendingCalls> {
        &self.pending_calls
    }

    pub fn listen_cancellation(&self) -> EventListener {
        self.canceled_event.listen()
    }

    /// Whether the message goes to this peer.
    ///
    /// A message with a destination goes to its destination alone, whatever the match rules of
    /// the peers. One without goes to the peers with a match rule that matches it.
    ///
    /// # Panics
    ///
    /// Same as [`MatchRules::matches`].
    pub fn interested(&self, msg: &zbus::Message, name_registry: &NameRegistry) -> bool {
        match msg.header().destination() {
            Some(BusName::Unique(name)) => *name == *self.unique_name,
            Some(BusName::WellKnown(name)) => name_registry
                .lookup(name)
                .is_some_and(|owner| *owner == self.unique_name),
            None => self.match_rules.matches(msg, name_registry),
        }
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

/// The number of the bus, as a peer of its own.
const BUS_ID: usize = 0;

/// How many file descriptors a message may carry: as many as Linux passes in one write
/// (`SCM_MAX_FD`), which is how zbus passes those of a message.
///
/// The bus couldn't pass a message with more on, so it disconnects a client that sends one.
pub const MAX_MESSAGE_FDS: usize = 253;
