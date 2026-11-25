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
use anyhow::{Context, Result};
use std::{rc::Rc, time::Duration};
use zbus::{fdo::ConnectionCredentials, names::BusName, OwnedGuid, OwnedMatchRule};

#[cfg(target_os = "linux")]
use crate::{connection, user::Users};
use crate::{
    connection::{Connection, Reader},
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
    pending_calls: Rc<PendingCalls>,
    /// The peer, as the caller of the calls that wait for replies.
    caller: Rc<Caller>,
    /// The credentials of the client, as its socket reported them when the client connected, or
    /// when the bus took the client over.
    credentials: Rc<ConnectionCredentials>,
    /// What the policy of the bus has for the peer.
    policy: PeerPolicy,
    greeted: bool,
}

impl Peer {
    /// The peer numbered `id`, and the stream of its messages, once the client that connected on
    /// `socket` has authenticated to the bus of `guid`.
    ///
    /// The client has `credentials`, as its socket reports them, and the bus gives it
    /// `send_timeout` to take each message. The peer is of `user`, and `charge` counts it as an
    /// object of that user.
    ///
    /// It fails if `credentials` tell no user, if the socket can't go on the `LocalRuntime` that
    /// the calling thread runs, or if the handshake does.
    pub async fn new(
        guid: &OwnedGuid,
        id: usize,
        socket: std::os::unix::net::UnixStream,
        credentials: ConnectionCredentials,
        send_timeout: Duration,
        user: Rc<User>,
        charge: Charge,
    ) -> Result<(Self, Stream)> {
        debug_assert_ne!(id, BUS_ID, "a client numbered as the bus");
        // A client authenticates with the EXTERNAL mechanism, the only one that dbus-broker takes
        // too: as the uid that its socket tells, which `credentials` tell too.
        let uid = credentials
            .unix_user_id()
            .context("The client has no user")?;
        let (reader, connection) =
            Connection::accept(socket, guid, uid, unique_name(id), send_timeout).await?;
        trace!("created: {:?}", connection);
        let client = Client::new(Rc::new(connection), id, MatchRules::default(), charge);

        // The bus gives the peer its policy as it adds the peer.
        Ok(Self::assemble(
            client,
            reader,
            user,
            Rc::new(credentials),
            PeerPolicy::default(),
            false,
        ))
    }

    /// The peer, as a client of the bus.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The calls to the peer that wait for its reply.
    pub fn pending_calls(&self) -> &Rc<PendingCalls> {
        &self.pending_calls
    }

    /// The peer, as the caller of the calls that wait for replies.
    pub fn caller(&self) -> &Rc<Caller> {
        &self.caller
    }

    /// The user of the peer.
    pub fn user(&self) -> &User {
        self.caller.user()
    }

    /// The gids of the client of the peer, as its credentials tell; none where they can't be told.
    pub fn groups(&self) -> &[u32] {
        self.credentials.unix_group_ids().map_or(&[], Vec::as_slice)
    }

    /// The credentials of the peer's client, as its socket reports them.
    pub fn credentials(&self) -> &Rc<ConnectionCredentials> {
        &self.credentials
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
    /// tell its user, or if its socket can't go on the `LocalRuntime` that the calling thread runs.
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
        // The credentials of the client are looked up from its socket, as for a new client. The
        // socket reports them at once, whatever the previous instance left unwritten. They tell
        // the user of the client too.
        let credentials = ConnectionCredentials::from_socket(handover.socket())?;
        let uid = credentials
            .unix_user_id()
            .context("The client has no user")?;
        let user = users.user(uid);
        // The client authenticated with the instance of the bus that handed it over, and the
        // connection goes on from where that instance left it. The reader yields the bytes that
        // the previous instance read from the client and didn't take, from the start of a message,
        // before what the socket holds. The connection writes what the previous instance left to
        // write to the client before anything else.
        let (reader, connection) = Connection::taken_over(handover, unique_name(id), send_timeout)?;
        trace!("handed over: {:?}", connection);
        let charge = Charge::taken_over(&user, &user);
        let client = Client::new(Rc::new(connection), id, match_rules, charge);

        // The bus gives the peer its policy as it adds the peer.
        Ok(Self::assemble(
            client,
            reader,
            user,
            Rc::new(credentials),
            PeerPolicy::default(),
            greeted,
        ))
    }

    /// The peer, as a client of the bus, as a mutable reference.
    #[cfg(target_os = "linux")]
    pub(crate) fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }

    /// The peer's client, which the peer gives up.
    #[cfg(target_os = "linux")]
    pub(crate) fn into_client(self) -> Client {
        self.client
    }

    /// Build a peer from `client`, and the stream of the peer's messages, which `reader` reads. The
    /// peer belongs to `user`, its client has `credentials`, it has `policy`, and it called `Hello`
    /// if `greeted`.
    fn assemble(
        client: Client,
        reader: Reader,
        user: Rc<User>,
        credentials: Rc<ConnectionCredentials>,
        policy: PeerPolicy,
        greeted: bool,
    ) -> (Self, Stream) {
        let unique_name = client.unique_name().clone();
        let peer = Self {
            peak_match_rules: client.match_rules().len(),
            client,
            pending_calls: Rc::new(PendingCalls::new(user.clone())),
            caller: Caller::new(unique_name, user),
            credentials,
            policy,
            greeted,
        };
        let stream = Stream::for_peer(&peer, reader);

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

/// The number of the bus itself, which no client has.
///
/// It is the number of the recipient of the calls to the bus, and of the sender of the bus's own
/// messages.
pub(crate) const BUS_ID: usize = 0;

/// How many file descriptors a message may carry: as many as Linux passes in one write
/// (`SCM_MAX_FD`), because the bus passes the FDs of a message along with its first byte.
///
/// The bus couldn't pass a message with more on, so it disconnects a client that sends one.
pub const MAX_MESSAGE_FDS: usize = 253;
