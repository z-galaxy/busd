mod stream;
pub use stream::*;
mod client;
pub use client::*;
mod monitor;
pub use monitor::*;
mod recipient;
pub use recipient::*;
mod pending_calls;
pub use pending_calls::*;

use crate::trace;
#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::Result;
use std::{sync::Arc, time::Duration};
use zbus::{
    connection::{self, AuthMechanism},
    fdo::ConnectionCredentials,
    names::BusName,
    Connection, MessageStream, OwnedGuid, OwnedMatchRule,
};

#[cfg(target_os = "linux")]
use crate::user::Users;
use crate::{
    match_rules::MatchRules,
    name_registry::NameRegistry,
    policy::PeerPolicy,
    user::{Charge, User},
};

/// A peer connection.
#[derive(Debug)]
pub struct Peer {
    client: Client,
    /// The most match rules that the peer had at a time, since this instance of the bus has it.
    peak_match_rules: usize,
    /// The calls to the peer that wait for its reply.
    pending_calls: Arc<PendingCalls>,
    /// The peer, as the caller of the calls that wait for replies.
    caller: Arc<Caller>,
    /// The gids of the client, as its credentials tell; none where they can't be told.
    groups: Box<[u32]>,
    /// What the policy of the bus has for the peer.
    policy: PeerPolicy,
    greeted: bool,
}

impl Peer {
    pub async fn new(
        guid: OwnedGuid,
        id: usize,
        builder: connection::Builder<'_>,
        send_timeout: Duration,
        user: Arc<User>,
        charge: Charge,
    ) -> Result<(Self, Stream)> {
        debug_assert_ne!(id, BUS_ID, "a client numbered as the bus");
        let unique_name = unique_name(id);
        // Use `build_message_stream` to activate a message receiver before the socket reader
        // task is spawned, so that messages the client pipelines right after authentication
        // (notably `Hello`) are never dropped in the race window between `build()` and stream
        // creation. See https://github.com/z-galaxy/zbus/pull/1760.
        //
        // A client authenticates with the EXTERNAL mechanism, the only one that dbus-broker takes
        // too: as the uid that its socket tells. zbus keeps the credentials that it looks up for
        // that, so it has them without waiting for the sends to the client, when asked for them.
        let msg_stream = builder
            .server(guid)
            .p2p()
            .auth_mechanism(AuthMechanism::External)
            .build_message_stream()
            .await?;
        let conn = Connection::from(&msg_stream);
        trace!("created: {:?}", conn);
        // Those credentials tell the groups of the client too.
        let groups = groups(conn.peer_creds().await?);
        let recipient = Recipient::client(conn.clone(), unique_name.clone(), send_timeout)?;
        let client = Client::new(
            conn,
            recipient,
            id,
            unique_name,
            MatchRules::default(),
            charge,
        );

        // The bus gives the peer its policy as it adds the peer.
        Ok(Self::assemble(
            client,
            msg_stream,
            user,
            groups,
            PeerPolicy::default(),
            false,
        ))
    }

    /// The peer, as a client of the bus.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The calls to the peer that wait for its reply.
    pub fn pending_calls(&self) -> &Arc<PendingCalls> {
        &self.pending_calls
    }

    /// The peer, as the caller of the calls that wait for replies.
    pub fn caller(&self) -> &Arc<Caller> {
        &self.caller
    }

    /// The user of the peer.
    pub fn user(&self) -> &User {
        self.caller.user()
    }

    /// The gids of the client of the peer.
    pub fn groups(&self) -> &[u32] {
        &self.groups
    }

    /// What the policy of the bus has for the peer.
    pub fn policy(&self) -> &PeerPolicy {
        &self.policy
    }

    /// Have the peer go by `policy`, what the policy of the bus has for it.
    pub fn set_policy(&mut self, policy: PeerPolicy) {
        self.policy = policy;
    }

    /// Whether the peer called `Hello`.
    pub fn greeted(&self) -> bool {
        self.greeted
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
        let unique_name = self.client.unique_name();
        match msg.header().destination() {
            Some(BusName::Unique(name)) => *name == **unique_name,
            Some(BusName::WellKnown(name)) => name_registry
                .lookup(name)
                .is_some_and(|owner| *owner == *unique_name),
            None => self.client.match_rules().matches(msg, name_registry),
        }
    }

    /// Add a match rule.
    ///
    /// Whether the peer did not have the rule already: see [`MatchRules::add`].
    pub fn add_match_rule(&mut self, rule: OwnedMatchRule) -> bool {
        let match_rules = self.client.match_rules_mut();
        let added = match_rules.add(rule);
        self.peak_match_rules = self.peak_match_rules.max(match_rules.len());

        added
    }

    /// The most match rules that the peer had at a time, since this instance of the bus has it.
    pub fn peak_match_rules(&self) -> usize {
        self.peak_match_rules
    }

    /// Remove the first rule that matches.
    pub fn remove_match_rule(&mut self, rule: OwnedMatchRule) -> zbus::fdo::Result<()> {
        self.client.match_rules_mut().remove(rule)
    }

    /// This can only be called once.
    pub fn hello(&mut self) -> zbus::fdo::Result<()> {
        if self.greeted {
            return Err(zbus::fdo::Error::Failed(
                "Can only call `Hello` method once".to_string(),
            ));
        }
        self.greeted = true;

        Result::Ok(())
    }

    /// Turn the peer into a monitor with `match_rules`. The monitor is the same client, and keeps
    /// what the client had as a peer, such as a write under way, which continues.
    ///
    /// What only the peer had, such as its calls that wait for its reply, is dropped.
    pub fn become_monitor(self, match_rules: MatchRules) -> Monitor {
        // This tells the task that served the peer that the client is not a peer anymore.
        self.client.notify_canceled();
        let mut client = self.client;
        *client.match_rules_mut() = match_rules;

        Monitor::new(client)
    }

    /// The peer that another instance of the bus handed over, `client`.
    ///
    /// The peer is an object of its user among `users`, as it was in the instance that handed it
    /// over, whatever the quota of the user. It fails if its credentials can't be looked up, which
    /// tell its user.
    #[cfg(target_os = "linux")]
    pub(crate) async fn handed_over(
        client: HandedOver,
        send_timeout: Duration,
        users: &Users,
    ) -> Result<(Self, Stream)> {
        let HandedOver {
            id,
            handover,
            greeted,
            match_rules,
        } = client;
        debug_assert_ne!(id, BUS_ID, "a client numbered as the bus");
        let unique_name = unique_name(id);
        // The client authenticated with the instance of the bus that handed it over, and the
        // connection goes on from where that instance left it: with the messages that zbus read of
        // the client and that the bus didn't take, and the part of the one that the client was in
        // the middle of sending, if any, before what the socket holds.
        let msg_stream = connection::Builder::take_over(handover)
            .build_message_stream()
            .await?;
        let conn = Connection::from(&msg_stream);
        trace!("handed over: {:?}", conn);
        // No handshake looked the credentials of the client up, as for a new client, so they are
        // looked up now, before anything is sent to the client, which a lookup would wait for,
        // what the instance that handed the client over left unwritten included. zbus keeps them.
        // They tell the user of the client too.
        let creds = conn.peer_creds().await?;
        let uid = creds.unix_user_id().context("The client has no user")?;
        let groups = groups(creds);
        let user = users.user(uid);
        let recipient = Recipient::client(conn.clone(), unique_name.clone(), send_timeout)?;
        let charge = Charge::taken_over(&user, &user);
        let client = Client::new(conn, recipient, id, unique_name, match_rules, charge);

        // The bus gives the peer its policy as it adds the peer.
        Ok(Self::assemble(
            client,
            msg_stream,
            user,
            groups,
            PeerPolicy::default(),
            greeted,
        ))
    }

    /// The peer, as a client of the bus, as a mutable reference.
    #[cfg(target_os = "linux")]
    pub(crate) fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }

    /// Build a peer from `client`, and the stream of the peer's messages, which come from
    /// `msg_stream`. The peer belongs to `user`, is in the groups `groups`, has `policy`, and
    /// called `Hello` if `greeted`.
    fn assemble(
        client: Client,
        msg_stream: MessageStream,
        user: Arc<User>,
        groups: Box<[u32]>,
        policy: PeerPolicy,
        greeted: bool,
    ) -> (Self, Stream) {
        let unique_name = client.unique_name().clone();
        let peer = Self {
            peak_match_rules: client.match_rules().len(),
            client,
            pending_calls: Arc::new(PendingCalls::new(user.clone())),
            caller: Caller::new(unique_name, user),
            groups,
            policy,
            greeted,
        };
        let stream = Stream::for_peer(&peer, msg_stream);

        (peer, stream)
    }
}

/// A client that another instance of the bus handed over, as [`Peer::handed_over`] takes it.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct HandedOver {
    /// The number of the client.
    pub(crate) id: usize,
    /// The connection of the client, as the instance of the bus that handed it over left it.
    pub(crate) handover: connection::Handover,
    /// Whether the client called `Hello`.
    pub(crate) greeted: bool,
    /// The match rules that the client added.
    pub(crate) match_rules: MatchRules,
}

/// The gids that `creds` tell, or none if they can't be told.
fn groups(creds: &ConnectionCredentials) -> Box<[u32]> {
    creds
        .unix_group_ids()
        .map_or_else(Box::default, |gids| gids.as_slice().into())
}

/// The number of the bus itself, which no client has.
///
/// It is the number of the recipient of the calls to the bus, and of the sender of the bus's own
/// messages.
pub(crate) const BUS_ID: usize = 0;

/// How many file descriptors a message may carry: as many as Linux passes in one write
/// (`SCM_MAX_FD`), which is how zbus passes those of a message.
///
/// The bus couldn't pass a message with more on, so it disconnects a client that sends one.
pub const MAX_MESSAGE_FDS: usize = 253;
