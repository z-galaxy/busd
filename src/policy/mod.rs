//! The policy of a bus: who may connect to it, own which names, and send and receive which
//! messages.
//!
//! The policy is built from the `<policy>` elements of the configuration, each a list of `<allow>`
//! and `<deny>` rules and a *context* that says whom they apply to: everyone (`context="default"`
//! and `context="mandatory"`), the peers of a user (`user="..."`), or the peers in a group
//! (`group="..."`). The deprecated `at_console` context is not supported: the configuration
//! leaves the rules of such a policy out, with a warning.
//!
//! # Rank
//!
//! Every rule has a rank, and of the rules that apply to a peer and match what it asks about, the
//! one of the highest rank gives the answer: allow or deny. A rule of a context outranks every
//! rule of the contexts before it in this list:
//!
//! 1. `context="default"`,
//! 2. `group="..."`,
//! 3. `user="..."`,
//! 4. `context="mandatory"`.
//!
//! Within a context, a rule outranks those before it in the configuration: the rules of all the
//! `<policy>` elements are numbered in the order of the configuration, and a higher number ranks
//! higher, whichever element a rule is in. So the rules of two `<policy context="default">`
//! elements far apart in the configuration rank by where they stand.
//!
//! # Default deny
//!
//! Where no rule matches, the answer is deny, with one exception: a client of the user that the
//! bus runs as may connect if no rule says otherwise, as though the policy began with a rule that
//! allows it, outranked by every rule of the configuration.
//!
//! # Without any policy
//!
//! A configuration without any `<policy>` gets the open policy of [`Policy::open`]: the clients
//! of the user that the bus runs as may connect, and do everything, as under the stock
//! configuration of a session bus. That of a system bus, with `<type>system</type>`, is the
//! exception: without any `<policy>`, it has no rule that allows anything, so the bus denies
//! everything but the connections of its own user.
//!
//! # Which rules apply to a peer
//!
//! A peer is known by its uid and its gids. The rules that apply to it are those of the default
//! and mandatory contexts, those of the `user` policies of its uid, and those of the `group`
//! policies of its gids.
//!
//! The names of the users and groups are resolved to ids as the policy is built, through [`Ids`],
//! and a name that is all digits is the id itself. A policy of a name that does not resolve is
//! left out, with a warning, as is a connect rule that names one.
//!
//! # What the rules match
//!
//! - A connect rule (`user` or `group`) matches a client of the user, or in the group, that it
//!   names; `user="*"` and `group="*"` match every client. Connect rules only count in the default
//!   and mandatory contexts: one in any other is left out, with a warning.
//! - An own rule matches a well-known name: `own="*"` any name, `own="a.b"` the name `a.b`, and
//!   `own_prefix="a.b"` the name `a.b` and the names under it, such as `a.b.c` but not `a.bc`.
//! - A send rule matches a message that the peer sends and the peer that would receive it, and a
//!   receive rule a message that the peer would receive and the peer that sent it: the other party.
//!   Such a rule matches if each of its attributes holds, and one that it does not have, or whose
//!   value is `*`, always holds:
//!   - the type, path, interface and member are those of the message, and a message without an
//!     interface does not match a rule that names one;
//!   - `send_broadcast="true"` holds for a signal without a destination, and
//!     `send_broadcast="false"` for any other message;
//!   - `min_fds` and `max_fds` bound the number of file descriptors of the message;
//!   - `send_destination` and `receive_sender` are one of the names of the other party, and
//!     `send_destination_prefix` is one of them or has one of them under it.
//!
//!   The names of a peer are the well-known names that it owns or queues for, and the bus has the
//!   one name `org.freedesktop.DBus`. Unique names never match.
//!
//! # What is not subject to the policy
//!
//! Replies and errors are not: the bus lets one through exactly when it answers a call that waits
//! for it, which it tracks apart from the policy. So the send and receive rules about them, those
//! of the type `method_return` or `error` or with an `error` attribute, are left out as the policy
//! is built. Nor is the bus itself, whose [`PeerPolicy::unrestricted`] lets it do anything.
//!
//! # How a question is answered
//!
//! The rules are kept in sets that are built once and shared: one for everyone, and one for each
//! uid and each gid. The [`PeerPolicy`] of a peer holds the few sets that apply to it. Within a
//! set, the send rules and the receive rules are indexed by the name of the other party that they
//! name, each list sorted from the highest rank down, so that a message is checked against the
//! rules that may match it, and only until a rule that ranks below the best match so far. Where the
//! highest-ranking send or receive rule of a peer has no condition at all, it gives every message
//! its verdict, which the [`PeerPolicy`] keeps so that it has nothing to check.
//!
//! # Differences from dbus-broker
//!
//! - `own="a.b"` matches only the name `a.b`, as the specification and dbus-daemon have it, where
//!   dbus-broker lets it match the names under it too.
//! - `send_destination_prefix` is supported, as dbus-daemon has it, and dbus-broker does not.
//! - A configuration without any `<policy>` gets the open policy of [`Policy::open`], unless it is
//!   that of a system bus. dbus-broker denies everything without a `<policy>`, whatever the bus.
//! - The deprecated `at_console` context is not supported; dbus-broker still supports it.
//! - The names of users and groups are resolved from `/etc/passwd` and `/etc/group` alone.

use std::{collections::HashMap, fmt, sync::Arc};

use zbus::{message::Type, names::OwnedWellKnownName};

use crate::{
    config::{
        self, Access, BusType, Config, ConnectOperation, MessageMatch, MessageType, Name, Operation,
    },
    debug, warn,
};

mod ids;

pub use ids::Ids;

/// The policy of a bus, as built from its configuration.
#[derive(Debug)]
pub struct Policy {
    /// The connect rules, from the highest rank down.
    connect: Vec<ConnectRule>,
    /// The user that the bus runs as, whose clients may connect where no rule says otherwise.
    bus_uid: u32,
    /// The rules for everyone: those of the default and mandatory contexts.
    everyone: Option<Arc<RuleSet>>,
    /// The rules of the `user` policies, by uid.
    users: HashMap<u32, Arc<RuleSet>>,
    /// The rules of the `group` policies, by gid.
    groups: HashMap<u32, Arc<RuleSet>>,
}

impl Policy {
    /// The policy of `config`, the `<policy>` elements of the configuration in its order, for a
    /// bus that runs as the user `bus_uid`, with the names of users and groups resolved by `ids`.
    ///
    /// Without any `<policy>` at all, the result is the open policy of [`Policy::open`], unless
    /// `config` is that of a system bus. Then, as with policies whose rules are all left out, it
    /// denies everything but the connections of the user of the bus.
    pub fn new(config: &Config, ids: &Ids, bus_uid: u32) -> Self {
        let policies = &config.policies;
        if policies.is_empty() && config.r#type != Some(BusType::System) {
            return Self::open(bus_uid);
        }

        let mut connect = Vec::new();
        let mut everyone = RuleSet::default();
        let mut users = HashMap::<u32, RuleSet>::new();
        let mut groups = HashMap::<u32, RuleSet>::new();
        let mut position = 0;
        for policy in policies {
            let (context, rules, set) = match policy {
                config::Policy::Unsupported => continue,
                config::Policy::DefaultContext(rules) => {
                    (Context::Default, rules, Some(&mut everyone))
                }
                config::Policy::Group(rules, group) => {
                    let set = ids.gid(group).map(|gid| groups.entry(gid).or_default());
                    if set.is_none() {
                        warn!("Ignoring the policy of the group `{group}`, which is unknown");
                    }
                    (Context::Group, rules, set)
                }
                config::Policy::User(rules, user) => {
                    let set = ids.uid(user).map(|uid| users.entry(uid).or_default());
                    if set.is_none() {
                        warn!("Ignoring the policy of the user `{user}`, who is unknown");
                    }
                    (Context::User, rules, set)
                }
                config::Policy::MandatoryContext(rules) => {
                    (Context::Mandatory, rules, Some(&mut everyone))
                }
            };
            // The rules of a policy that is left out are numbered all the same.
            let first = position + 1;
            position += rules.len();
            let Some(set) = set else {
                continue;
            };
            for (rule, position) in rules.iter().zip(first..) {
                add_rule(rule, Rank { context, position }, set, &mut connect, ids);
            }
        }
        connect.sort_by_key(|rule| std::cmp::Reverse(rule.rank));

        Self {
            connect,
            bus_uid,
            everyone: everyone.finish(),
            users: finish_all(users),
            groups: finish_all(groups),
        }
    }

    /// The policy of a bus whose configuration has no `<policy>`, unless it is a system bus: the
    /// clients of `bus_uid` may connect, and may own any name, and send and receive any message.
    pub fn open(bus_uid: u32) -> Self {
        Self {
            connect: Vec::new(),
            bus_uid,
            everyone: Some(Arc::new(RuleSet::unrestricted())),
            users: HashMap::new(),
            groups: HashMap::new(),
        }
    }

    /// Whether a client of `uid`, in the groups `gids`, may connect.
    pub fn may_connect(&self, uid: u32, gids: &[u32]) -> bool {
        let matching = self.connect.iter().find(|rule| match rule.target {
            Target::Anyone => true,
            Target::Uid(rule_uid) => rule_uid == uid,
            Target::Gid(rule_gid) => gids.contains(&rule_gid),
        });
        match matching {
            Some(rule) => rule.allow,
            None => uid == self.bus_uid,
        }
    }

    /// What the policy has for a peer of `uid`, in the groups `gids`.
    pub fn for_peer(&self, uid: u32, gids: &[u32]) -> PeerPolicy {
        let groups = gids.iter().filter_map(|gid| self.groups.get(gid));
        let applying = [self.everyone.as_ref(), self.users.get(&uid)]
            .into_iter()
            .flatten()
            .chain(groups);
        let mut sets: Vec<Arc<RuleSet>> = Vec::new();
        for set in applying {
            // A peer may have a gid more than once among its gids.
            if !sets.iter().any(|known| Arc::ptr_eq(known, set)) {
                sets.push(set.clone());
            }
        }

        PeerPolicy::new(sets)
    }
}

/// [`Policy::open`] for the user of the process.
impl Default for Policy {
    fn default() -> Self {
        Self::open(rustix::process::getuid().as_raw())
    }
}

/// What the policy of a bus has for one peer: the rules that apply to it.
#[derive(Clone, Debug)]
pub struct PeerPolicy {
    /// The sets of rules that apply to the peer.
    sets: Vec<Arc<RuleSet>>,
    /// The verdict of every message that the peer sends, if they all get the same one.
    sends: Option<bool>,
    /// The verdict of every message that the peer receives, if they all get the same one.
    receives: Option<bool>,
}

impl PeerPolicy {
    /// The policy of the bus itself: it may own, send and receive everything.
    pub fn unrestricted() -> Self {
        Self::new(vec![Arc::new(RuleSet::unrestricted())])
    }

    /// Whether the peer may own `name`.
    pub fn may_own(&self, name: &str) -> bool {
        let mut best = None;
        for set in &self.sets {
            for rule in &set.own {
                if outranked(rule.rank, best) {
                    break;
                }
                let matches = match &rule.name {
                    Name::Any => true,
                    Name::Exact(exact) => name == exact,
                    Name::Prefix(prefix) => covers(prefix, name),
                };
                if matches {
                    best = Some(rule.verdict());
                    break;
                }
            }
        }

        allowed(best)
    }

    /// Whether the peer may send the message of `facts` to a peer that has `names`.
    pub fn may_send(&self, facts: &Facts<'_>, names: &[OwnedWellKnownName]) -> bool {
        if let Some(verdict) = self.sends {
            return verdict;
        }

        let mut best = None;
        for set in &self.sets {
            set.send.find(facts, names, &mut best);
        }

        allowed(best)
    }

    /// Whether the peer may receive the message of `facts` from a peer that has `names`.
    pub fn may_receive(&self, facts: &Facts<'_>, names: &[OwnedWellKnownName]) -> bool {
        if let Some(verdict) = self.receives {
            return verdict;
        }

        let mut best = None;
        for set in &self.sets {
            set.receive.find(facts, names, &mut best);
        }

        allowed(best)
    }

    /// Whether the peer may send every message to everyone, so that there is nothing to check.
    pub fn sends_freely(&self) -> bool {
        self.sends == Some(true)
    }

    /// Whether the peer may receive every message from everyone.
    pub fn receives_freely(&self) -> bool {
        self.receives == Some(true)
    }

    /// The policy of a peer that the rules of `sets` apply to.
    fn new(sets: Vec<Arc<RuleSet>>) -> Self {
        let sends = blanket_verdict(sets.iter().filter_map(|set| set.send.head));
        let receives = blanket_verdict(sets.iter().filter_map(|set| set.receive.head));

        Self {
            sets,
            sends,
            receives,
        }
    }
}

/// The policy of a peer that the bus has not given its own yet: it may own, send and receive
/// nothing.
impl Default for PeerPolicy {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// What the policy looks at in a message.
#[derive(Clone, Copy, Debug)]
pub struct Facts<'m> {
    msg_type: Type,
    path: Option<&'m str>,
    interface: Option<&'m str>,
    member: Option<&'m str>,
    /// Whether the message is a signal without a destination.
    broadcast: bool,
    /// How many file descriptors the message carries.
    fds: usize,
}

impl<'m> Facts<'m> {
    /// The facts of `msg`, whose header is `header`.
    pub fn new(msg: &'m zbus::Message, header: &'m zbus::message::Header<'m>) -> Self {
        let msg_type = header.message_type();

        Self {
            msg_type,
            path: header.path().map(|path| path.as_str()),
            interface: header.interface().map(|interface| interface.as_str()),
            member: header.member().map(|member| member.as_str()),
            broadcast: msg_type == Type::Signal && header.destination().is_none(),
            fds: msg.data().fds().len(),
        }
    }

    /// Whether the message is a signal without a destination.
    pub fn is_broadcast(&self) -> bool {
        self.broadcast
    }
}

/// The message as a log line shows it: its type, path, interface and member, with `none` for each
/// one that it lacks.
impl fmt::Display for Facts<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [path, interface, member] =
            [self.path, self.interface, self.member].map(|value| value.unwrap_or("none"));

        write!(
            f,
            "the {:?} (path {path}, interface {interface}, member {member})",
            self.msg_type
        )
    }
}

/// Add `rule`, of `rank`, to `set`, or to `connect` if it is a connect rule, unless it is left
/// out.
fn add_rule(
    rule: &config::Rule,
    rank: Rank,
    set: &mut RuleSet,
    connect: &mut Vec<ConnectRule>,
    ids: &Ids,
) {
    let allow = rule.access == Access::Allow;
    match &rule.operation {
        Operation::Connect(target) => {
            if !matches!(rank.context, Context::Default | Context::Mandatory) {
                warn!(
                    "Ignoring a connect rule outside the default and mandatory contexts: \
                     {target:?}"
                );
                return;
            }
            let Some(target) = Target::new(target, ids) else {
                warn!("Ignoring a connect rule of an unknown user or group: {target:?}");
                return;
            };
            connect.push(ConnectRule {
                rank,
                allow,
                target,
            });
        }
        Operation::Own(ownership) => {
            // The parser gives a name to every own rule that it keeps.
            let Some(name) = &ownership.own else {
                return;
            };
            set.own.push(OwnRule {
                rank,
                allow,
                name: name.clone(),
            });
        }
        Operation::Send(send) => {
            let Some(rule) = MessageRule::new(rank, allow, &send.message, send.broadcast) else {
                debug!("Ignoring a send rule about replies: {send:?}");
                return;
            };
            set.send.add(send.destination.as_ref(), rule);
        }
        Operation::Receive(receive) => {
            let Some(rule) = MessageRule::new(rank, allow, &receive.message, None) else {
                debug!("Ignoring a receive rule about replies: {receive:?}");
                return;
            };
            set.receive.add(receive.sender.as_ref(), rule);
        }
    }
}

/// The rank of a rule: by its context first, and then by its position in the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Rank {
    context: Context,
    position: usize,
}

/// The context of a rule, from the lowest rank up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Context {
    Default,
    Group,
    User,
    Mandatory,
}

/// The answer of a rule that matches: its rank, and whether it allows.
#[derive(Clone, Copy, Debug)]
struct Verdict {
    rank: Rank,
    allow: bool,
}

/// A connect rule.
#[derive(Debug)]
struct ConnectRule {
    rank: Rank,
    allow: bool,
    target: Target,
}

/// The clients that a connect rule matches.
#[derive(Debug)]
enum Target {
    Anyone,
    Uid(u32),
    Gid(u32),
}

impl Target {
    /// The target of `rule`, or nothing if it names a user or group that `ids` does not know.
    fn new(rule: &ConnectOperation, ids: &Ids) -> Option<Self> {
        match (&rule.user, &rule.group) {
            (Some(user), _) if user == "*" => Some(Self::Anyone),
            (Some(user), _) => ids.uid(user).map(Self::Uid),
            (None, Some(group)) if group == "*" => Some(Self::Anyone),
            (None, Some(group)) => ids.gid(group).map(Self::Gid),
            (None, None) => None,
        }
    }
}

/// The rules of one or more policies that apply to the same peers.
#[derive(Debug, Default)]
struct RuleSet {
    /// The own rules, from the highest rank down.
    own: Vec<OwnRule>,
    send: MessageRules,
    receive: MessageRules,
}

impl RuleSet {
    /// The rules that let a peer own, send and receive everything.
    fn unrestricted() -> Self {
        let rank = Rank {
            context: Context::Default,
            position: 0,
        };
        let anything = || MessageRule {
            rank,
            allow: true,
            r#type: None,
            path: None,
            interface: None,
            member: None,
            broadcast: None,
            min_fds: None,
            max_fds: None,
        };
        let mut set = Self::default();
        set.own.push(OwnRule {
            rank,
            allow: true,
            name: Name::Any,
        });
        set.send.add(None, anything());
        set.receive.add(None, anything());
        set.send.sort();
        set.receive.sort();

        set
    }

    /// The set, sorted and shared, or nothing if it has no rules.
    fn finish(mut self) -> Option<Arc<Self>> {
        if self.own.is_empty() && self.send.head.is_none() && self.receive.head.is_none() {
            return None;
        }
        self.own.sort_by_key(|rule| std::cmp::Reverse(rule.rank));
        self.send.sort();
        self.receive.sort();

        Some(Arc::new(self))
    }
}

/// [`RuleSet::finish`] for each of `sets`, leaving out those without rules.
fn finish_all(sets: HashMap<u32, RuleSet>) -> HashMap<u32, Arc<RuleSet>> {
    sets.into_iter()
        .filter_map(|(id, set)| Some((id, set.finish()?)))
        .collect()
}

/// An own rule.
#[derive(Debug)]
struct OwnRule {
    rank: Rank,
    allow: bool,
    name: Name,
}

impl OwnRule {
    fn verdict(&self) -> Verdict {
        Verdict {
            rank: self.rank,
            allow: self.allow,
        }
    }
}

/// The send rules, or the receive rules, of a set, by the name of the other party that they name.
///
/// Each list is sorted from the highest rank down.
#[derive(Debug, Default)]
struct MessageRules {
    /// The rules that name no one, so that they hold whoever the other party is.
    anyone: Vec<MessageRule>,
    /// The rules that name a name, by that name. They are hashed with foldhash, which takes a
    /// fraction of the time of the SipHash of the standard library, as the names of the other
    /// party of each message are looked up here.
    exact: HashMap<String, Vec<MessageRule>, foldhash::fast::RandomState>,
    /// The rules that name a prefix of names, with that prefix.
    prefix: Vec<(String, MessageRule)>,
    /// The highest-ranking rule of all.
    head: Option<Head>,
}

impl MessageRules {
    /// Add `rule`, which names `name` for the other party, if any.
    fn add(&mut self, name: Option<&Name>, rule: MessageRule) {
        if self.head.is_none_or(|head| head.rank < rule.rank) {
            // A rule that names someone has a condition, whatever its other attributes are.
            let names_anyone = matches!(name, None | Some(Name::Any));
            self.head = Some(Head {
                rank: rule.rank,
                every_message: (names_anyone && rule.is_unconditional()).then_some(rule.allow),
            });
        }
        match name {
            None | Some(Name::Any) => self.anyone.push(rule),
            Some(Name::Exact(name)) => self.exact.entry(name.clone()).or_default().push(rule),
            Some(Name::Prefix(prefix)) => self.prefix.push((prefix.clone(), rule)),
        }
    }

    /// Sort each list from the highest rank down.
    fn sort(&mut self) {
        let by_rank = |rule: &MessageRule| std::cmp::Reverse(rule.rank);
        self.anyone.sort_by_key(by_rank);
        for rules in self.exact.values_mut() {
            rules.sort_by_key(by_rank);
        }
        self.prefix.sort_by_key(|(_, rule)| by_rank(rule));
    }

    /// Make `best` the verdict of the highest-ranking rule that matches `facts` and one of
    /// `names`, the names of the other party, if it outranks `best`.
    fn find(&self, facts: &Facts<'_>, names: &[OwnedWellKnownName], best: &mut Option<Verdict>) {
        find_first(&self.anyone, facts, best);
        for name in names {
            if let Some(rules) = self.exact.get(name.as_str()) {
                find_first(rules, facts, best);
            }
        }
        for (prefix, rule) in &self.prefix {
            if outranked(rule.rank, *best) {
                break;
            }
            if names.iter().any(|name| covers(prefix, name.as_str())) && rule.matches(facts) {
                *best = Some(rule.verdict());
                break;
            }
        }
    }
}

/// Make `best` the verdict of the first of `rules` that matches `facts`, if it outranks `best`.
fn find_first(rules: &[MessageRule], facts: &Facts<'_>, best: &mut Option<Verdict>) {
    for rule in rules {
        if outranked(rule.rank, *best) {
            return;
        }
        if rule.matches(facts) {
            *best = Some(rule.verdict());
            return;
        }
    }
}

/// The highest-ranking rule of a [`MessageRules`].
#[derive(Clone, Copy, Debug)]
struct Head {
    rank: Rank,
    /// Its verdict, if it has no condition, so that every message gets it.
    every_message: Option<bool>,
}

/// The verdict that every message gets from rules whose heads are `heads`, if they all get the
/// same one: that of the highest-ranking head, if it has no condition, or deny if there are no
/// rules.
fn blanket_verdict(heads: impl Iterator<Item = Head>) -> Option<bool> {
    match heads.max_by_key(|head| head.rank) {
        Some(head) => head.every_message,
        None => Some(false),
    }
}

/// A send or receive rule, but for the name of the other party, which [`MessageRules`] keeps.
#[derive(Debug)]
struct MessageRule {
    rank: Rank,
    allow: bool,
    r#type: Option<Type>,
    path: Option<String>,
    interface: Option<String>,
    member: Option<String>,
    broadcast: Option<bool>,
    min_fds: Option<usize>,
    max_fds: Option<usize>,
}

impl MessageRule {
    /// The rule built from `message` and `broadcast`, or `None` if the rule is about replies,
    /// which are not subject to the policy.
    fn new(
        rank: Rank,
        allow: bool,
        message: &MessageMatch,
        broadcast: Option<bool>,
    ) -> Option<Self> {
        let r#type = match message.r#type {
            None | Some(MessageType::Any) => None,
            Some(MessageType::MethodCall) => Some(Type::MethodCall),
            Some(MessageType::Signal) => Some(Type::Signal),
            Some(MessageType::MethodReturn | MessageType::Error) => return None,
        };
        if message.error.is_some() {
            return None;
        }
        let fds = |fds: Option<u32>| fds.map(|fds| usize::try_from(fds).unwrap_or(usize::MAX));

        Some(Self {
            rank,
            allow,
            r#type,
            path: attribute(&message.path),
            interface: attribute(&message.interface),
            member: attribute(&message.member),
            broadcast,
            min_fds: fds(message.min_fds),
            max_fds: fds(message.max_fds),
        })
    }

    /// Whether the rule matches every message.
    fn is_unconditional(&self) -> bool {
        self.r#type.is_none()
            && self.path.is_none()
            && self.interface.is_none()
            && self.member.is_none()
            && self.broadcast.is_none()
            && self.min_fds.is_none()
            && self.max_fds.is_none()
    }

    /// Whether the message of `facts` matches the rule, but for the name of the other party.
    fn matches(&self, facts: &Facts<'_>) -> bool {
        self.r#type.is_none_or(|r#type| r#type == facts.msg_type)
            && holds(&self.path, facts.path)
            && holds(&self.interface, facts.interface)
            && holds(&self.member, facts.member)
            && self
                .broadcast
                .is_none_or(|broadcast| broadcast == facts.broadcast)
            && self.min_fds.is_none_or(|min| facts.fds >= min)
            && self.max_fds.is_none_or(|max| facts.fds <= max)
    }

    fn verdict(&self) -> Verdict {
        Verdict {
            rank: self.rank,
            allow: self.allow,
        }
    }
}

/// The value of an attribute that names a path, interface or member, or nothing if it is `*`,
/// which always holds.
fn attribute(value: &Option<String>) -> Option<String> {
    value.as_ref().filter(|value| *value != "*").cloned()
}

/// Whether the attribute `expected` of a rule holds for `actual`, the value of a message.
fn holds(expected: &Option<String>, actual: Option<&str>) -> bool {
    expected
        .as_deref()
        .is_none_or(|expected| actual == Some(expected))
}

/// Whether `prefix` covers `name`: whether `name` is `prefix` or a name under it.
fn covers(prefix: &str, name: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// Whether a rule of `rank` ranks below `best`, so that it can't change the answer.
fn outranked(rank: Rank, best: Option<Verdict>) -> bool {
    best.is_some_and(|best| best.rank > rank)
}

/// The answer of `best`, the verdict of the highest-ranking rule that matches, if any.
fn allowed(best: Option<Verdict>) -> bool {
    best.is_some_and(|best| best.allow)
}

#[cfg(test)]
mod tests;
