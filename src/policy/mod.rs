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

use std::{collections::HashMap, sync::Arc};

use zbus::{message::Type, names::OwnedWellKnownName};

use crate::{
    config::{self, Access, BusType, Config, ConnectOperation, MessageType, Name, Operation},
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
    /// The policy of the bus itself, as a peer: it may own, send and receive everything.
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
    pub msg_type: Type,
    pub path: Option<&'m str>,
    pub interface: Option<&'m str>,
    pub member: Option<&'m str>,
    /// Whether the message is a signal without a destination.
    pub broadcast: bool,
    /// How many file descriptors the message carries.
    pub fds: usize,
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
}

/// Add `rule`, of `rank`, to `set`, or to `connect` if it is a connect rule, unless it is left
/// out.
fn add_rule(
    (access, operation): &config::rule::Rule,
    rank: Rank,
    set: &mut RuleSet,
    connect: &mut Vec<ConnectRule>,
    ids: &Ids,
) {
    let allow = *access == Access::Allow;
    match operation {
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
            let condition = Condition {
                r#type: &send.r#type,
                error: &send.error,
                path: &send.path,
                interface: &send.interface,
                member: &send.member,
                broadcast: send.broadcast,
                min_fds: send.min_fds,
                max_fds: send.max_fds,
            };
            let Some(rule) = MessageRule::new(rank, allow, condition) else {
                debug!("Ignoring a send rule about replies: {send:?}");
                return;
            };
            set.send.add(send.destination.as_ref(), rule);
        }
        Operation::Receive(receive) => {
            let condition = Condition {
                r#type: &receive.r#type,
                error: &receive.error,
                path: &receive.path,
                interface: &receive.interface,
                member: &receive.member,
                broadcast: None,
                min_fds: receive.min_fds,
                max_fds: receive.max_fds,
            };
            let Some(rule) = MessageRule::new(rank, allow, condition) else {
                debug!("Ignoring a receive rule about replies: {receive:?}");
                return;
            };
            let sender = receive.sender.as_ref().map(|sender| match sender.as_str() {
                "*" => Name::Any,
                _ => Name::Exact(sender.clone()),
            });
            set.receive.add(sender.as_ref(), rule);
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
    /// The rule of `condition`, or nothing if it is about replies, which are not subject to the
    /// policy.
    fn new(rank: Rank, allow: bool, condition: Condition<'_>) -> Option<Self> {
        let r#type = match condition.r#type {
            None | Some(MessageType::Any) => None,
            Some(MessageType::MethodCall) => Some(Type::MethodCall),
            Some(MessageType::Signal) => Some(Type::Signal),
            Some(MessageType::MethodReturn | MessageType::Error) => return None,
        };
        if condition.error.is_some() {
            return None;
        }
        let fds = |fds: Option<u32>| fds.map(|fds| usize::try_from(fds).unwrap_or(usize::MAX));

        Some(Self {
            rank,
            allow,
            r#type,
            path: attribute(condition.path),
            interface: attribute(condition.interface),
            member: attribute(condition.member),
            broadcast: condition.broadcast,
            min_fds: fds(condition.min_fds),
            max_fds: fds(condition.max_fds),
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

/// The attributes of a send or receive rule that [`MessageRule::new`] takes.
struct Condition<'r> {
    r#type: &'r Option<MessageType>,
    error: &'r Option<String>,
    path: &'r Option<String>,
    interface: &'r Option<String>,
    member: &'r Option<String>,
    broadcast: Option<bool>,
    min_fds: Option<u32>,
    max_fds: Option<u32>,
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
mod tests {
    use zbus::names::OwnedWellKnownName;

    use super::*;

    /// Without any policy, everything is allowed, but only the clients of the bus's user may
    /// connect: for a configuration that says nothing of its bus, and for that of a session bus.
    #[test]
    fn no_policy_is_open() {
        for config in ["", "<type>session</type>"] {
            let policy = Policy::new(&parse(config), &ids(), BUS_UID);

            assert!(policy.may_connect(BUS_UID, &[]));
            assert!(!policy.may_connect(ALICE, &[ALICE]));
            let peer = policy.for_peer(BUS_UID, &[]);
            assert!(peer.sends_freely());
            assert!(peer.receives_freely());
            assert!(peer.may_own("org.example.Service"));
            assert!(peer.may_send(&call(), &[]));
            assert!(peer.may_receive(&signal(), &names(&["org.example.Service"])));
        }
    }

    /// A system bus without any policy has no rule that allows anything: its own user may still
    /// connect, and nothing else is allowed, to that user either.
    #[test]
    fn no_policy_of_a_system_bus_denies_everything() {
        let policy = Policy::new(&parse("<type>system</type>"), &ids(), BUS_UID);

        assert!(policy.may_connect(BUS_UID, &[]));
        assert!(!policy.may_connect(ALICE, &[ALICE]));
        for uid in [BUS_UID, ROOT, ALICE] {
            let peer = policy.for_peer(uid, &[]);
            assert!(!peer.sends_freely());
            assert!(!peer.receives_freely());
            assert!(!peer.may_own("org.example.Service"));
            assert!(!peer.may_send(&call(), &names(&["org.freedesktop.DBus"])));
            assert!(!peer.may_receive(&signal(), &[]));
        }
    }

    /// The policy of the bus itself lets it own, send and receive everything.
    #[test]
    fn the_bus_is_unrestricted() {
        let peer = PeerPolicy::unrestricted();

        assert!(peer.sends_freely());
        assert!(peer.receives_freely());
        assert!(peer.may_own("org.freedesktop.DBus"));
        assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
        assert!(peer.may_receive(&signal(), &[]));
    }

    /// The default policy of a peer denies everything.
    #[test]
    fn the_default_peer_policy_denies_everything() {
        let peer = PeerPolicy::default();

        assert!(!peer.sends_freely());
        assert!(!peer.receives_freely());
        assert!(!peer.may_own("org.example.Service"));
        assert!(!peer.may_send(&call(), &names(&["org.example.Service"])));
        assert!(!peer.may_receive(&signal(), &[]));
    }

    /// Where no rule matches, the answer is deny.
    #[test]
    fn what_no_rule_matches_is_denied() {
        let policy = policy(
            r#"<policy context="default">
                <allow own="org.example.Other"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[ALICE]);
        assert!(!peer.sends_freely());
        assert!(!peer.receives_freely());
        assert!(!peer.may_own("org.example.Service"));
        assert!(!peer.may_send(&call(), &names(&["org.example.Service"])));
        assert!(!peer.may_receive(&call(), &names(&["org.example.Service"])));
        assert!(!policy.may_connect(ALICE, &[ALICE]));
    }

    /// Within a context, a rule outranks the rules before it in the configuration.
    #[test]
    fn later_rules_outrank_earlier_ones() {
        let allow_then_deny = policy(
            r#"<policy context="default">
                <allow own="org.example.Service"/>
                <deny own="org.example.Service"/>
            </policy>"#,
        );
        assert!(!allow_then_deny
            .for_peer(ALICE, &[])
            .may_own("org.example.Service"));

        let deny_then_allow = policy(
            r#"<policy context="default">
                <deny own="org.example.Service"/>
                <allow own="org.example.Service"/>
            </policy>"#,
        );
        assert!(deny_then_allow
            .for_peer(ALICE, &[])
            .may_own("org.example.Service"));
    }

    /// The rules of separate policies of one context rank by where they stand in the
    /// configuration, whatever stands between them.
    #[test]
    fn rules_of_separate_policies_rank_by_position() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_destination="org.example.Service"/>
                <deny own="*"/>
            </policy>
            <policy user="alice">
                <allow own="org.example.Service"/>
            </policy>
            <policy context="default">
                <deny send_destination="org.example.Service" send_member="Forbidden"/>
                <allow own="*"/>
            </policy>"#,
        );

        let peer = policy.for_peer(BOB, &[]);
        let service = names(&["org.example.Service"]);
        assert!(peer.may_send(&call(), &service));
        let forbidden = Facts {
            member: Some("Forbidden"),
            ..call()
        };
        assert!(!peer.may_send(&forbidden, &service));
        assert!(peer.may_own("org.example.Other"));
    }

    /// A rule of a context outranks every rule of the contexts below it, wherever it stands in
    /// the configuration: default < group < user < mandatory.
    #[test]
    fn contexts_rank_by_kind() {
        let policy = policy(
            r#"<policy context="mandatory">
                <deny own="org.example.Mandatory"/>
            </policy>
            <policy user="alice">
                <allow own="org.example.Mandatory"/>
                <deny own="org.example.User"/>
            </policy>
            <policy group="wheel">
                <allow own="org.example.User"/>
                <deny own="org.example.Group"/>
            </policy>
            <policy context="default">
                <allow own="*"/>
            </policy>"#,
        );

        let alice = policy.for_peer(ALICE, &[ALICE, WHEEL]);
        assert!(!alice.may_own("org.example.Mandatory"));
        assert!(!alice.may_own("org.example.User"));
        assert!(!alice.may_own("org.example.Group"));
        assert!(alice.may_own("org.example.Default"));

        // Bob is in wheel too, but has no user policy, so the group's rules give his answers.
        let bob = policy.for_peer(BOB, &[BOB, WHEEL]);
        assert!(!bob.may_own("org.example.Mandatory"));
        assert!(bob.may_own("org.example.User"));
        assert!(!bob.may_own("org.example.Group"));
    }

    /// The rules of a user policy apply to the peers of that user alone, whether the policy names
    /// the user or its uid, and a policy of a user that does not resolve is left out.
    #[test]
    fn user_policies_apply_to_their_users() {
        let policy = policy(
            r#"<policy user="alice">
                <allow own="org.example.Alice"/>
            </policy>
            <policy user="1001">
                <allow own="org.example.Bob"/>
            </policy>
            <policy user="mallory">
                <allow own="*"/>
            </policy>"#,
        );

        let alice = policy.for_peer(ALICE, &[]);
        assert!(alice.may_own("org.example.Alice"));
        assert!(!alice.may_own("org.example.Bob"));
        let bob = policy.for_peer(BOB, &[]);
        assert!(!bob.may_own("org.example.Alice"));
        assert!(bob.may_own("org.example.Bob"));
        let other = policy.for_peer(4000, &[]);
        assert!(!other.may_own("org.example.Alice"));
        assert!(!other.may_own("org.example.Other"));
    }

    /// The rules of a group policy apply to the peers that have the group among their gids,
    /// whether the policy names the group or its gid, and a policy of a group that does not
    /// resolve is left out.
    #[test]
    fn group_policies_apply_to_their_members() {
        let policy = policy(
            r#"<policy group="wheel">
                <allow own="org.example.Wheel"/>
            </policy>
            <policy group="20">
                <allow own="org.example.Twenty"/>
            </policy>
            <policy group="nogroup">
                <allow own="*"/>
            </policy>"#,
        );

        let member = policy.for_peer(BOB, &[BOB, WHEEL, WHEEL]);
        assert!(member.may_own("org.example.Wheel"));
        assert!(!member.may_own("org.example.Twenty"));
        assert!(!member.may_own("org.example.Other"));
        let other = policy.for_peer(BOB, &[BOB, 20]);
        assert!(!other.may_own("org.example.Wheel"));
        assert!(other.may_own("org.example.Twenty"));
        // Being the user of the group's name is not being in the group.
        let named = policy.for_peer(ALICE, &[ALICE]);
        assert!(!named.may_own("org.example.Wheel"));
    }

    /// The rules of a policy of the `at_console` context are left out, whichever value it has, but
    /// the configuration has a policy all the same, so it is not the open one.
    #[test]
    fn console_policies_are_left_out() {
        let policy = policy(
            r#"<policy at_console="true">
                <allow own="org.example.Console"/>
            </policy>
            <policy at_console="false">
                <allow own="org.example.NotConsole"/>
            </policy>"#,
        );

        for uid in [ALICE, 999, ROOT] {
            let peer = policy.for_peer(uid, &[]);
            assert!(!peer.may_own("org.example.Console"), "uid {uid}");
            assert!(!peer.may_own("org.example.NotConsole"), "uid {uid}");
            assert!(!peer.sends_freely(), "uid {uid}");
        }
    }

    /// A client of the bus's user may connect where no rule matches it, and no other may.
    #[test]
    fn the_bus_user_may_connect_where_no_rule_matches() {
        let policy = policy(
            r#"<policy context="default">
                <allow own="*"/>
            </policy>"#,
        );

        assert!(policy.may_connect(BUS_UID, &[]));
        assert!(!policy.may_connect(ALICE, &[ALICE]));
    }

    /// Every connect rule that matches a client of the bus's user outranks the implicit allow.
    #[test]
    fn connect_rules_outrank_the_bus_user() {
        let policy = policy(
            r#"<policy context="default">
                <deny user="*"/>
            </policy>"#,
        );

        assert!(!policy.may_connect(BUS_UID, &[]));
    }

    /// A connect rule matches the clients of the user or in the group that it names, by name or
    /// by number, and `*` matches all.
    #[test]
    fn connect_rules_match_users_and_groups() {
        let policy = policy(
            r#"<policy context="default">
                <allow user="alice"/>
                <allow user="2000"/>
                <allow group="wheel"/>
                <allow group="mallory"/>
            </policy>"#,
        );

        assert!(policy.may_connect(ALICE, &[ALICE]));
        assert!(policy.may_connect(2000, &[]));
        assert!(policy.may_connect(BOB, &[BOB, WHEEL]));
        assert!(!policy.may_connect(BOB, &[BOB]));

        let everyone = self::policy(
            r#"<policy context="default">
                <allow group="*"/>
            </policy>"#,
        );
        assert!(everyone.may_connect(BOB, &[]));
    }

    /// A mandatory connect rule outranks a default one, wherever they stand.
    #[test]
    fn mandatory_connect_rules_outrank_default_ones() {
        let policy = policy(
            r#"<policy context="mandatory">
                <deny user="bob"/>
            </policy>
            <policy context="default">
                <allow user="*"/>
            </policy>"#,
        );

        assert!(policy.may_connect(ALICE, &[]));
        assert!(!policy.may_connect(BOB, &[]));
    }

    /// Connect rules count in the default and mandatory contexts alone.
    #[test]
    fn connect_rules_of_other_contexts_are_left_out() {
        let policy = policy(
            r#"<policy user="alice">
                <allow user="*"/>
            </policy>
            <policy group="wheel">
                <allow user="*"/>
            </policy>
            <policy at_console="true">
                <allow user="*"/>
            </policy>"#,
        );

        assert!(!policy.may_connect(ALICE, &[ALICE, WHEEL]));
    }

    /// `own` matches its name alone, `own="*"` every name, and `own_prefix` its name and the
    /// names under it, on a boundary between elements.
    #[test]
    fn own_rules_match_names() {
        let exact = policy(r#"<policy context="default"><allow own="a.b"/></policy>"#);
        let peer = exact.for_peer(ALICE, &[]);
        assert!(peer.may_own("a.b"));
        assert!(!peer.may_own("a.b.c"));
        assert!(!peer.may_own("a.bc"));
        assert!(!peer.may_own("a"));

        let any = policy(r#"<policy context="default"><allow own="*"/></policy>"#);
        assert!(any.for_peer(ALICE, &[]).may_own("org.example.Service"));

        let prefix = policy(r#"<policy context="default"><allow own_prefix="a.b"/></policy>"#);
        let peer = prefix.for_peer(ALICE, &[]);
        assert!(peer.may_own("a.b"));
        assert!(peer.may_own("a.b.c"));
        assert!(peer.may_own("a.b.c.d"));
        assert!(!peer.may_own("a.bc"));
        assert!(!peer.may_own("a"));
    }

    /// `send_destination` matches a name of the receiver exactly, `send_destination_prefix` that
    /// name and those under it, and `send_destination="*"` any receiver.
    #[test]
    fn send_rules_match_destinations() {
        let exact = policy(
            r#"<policy context="default">
                <allow send_destination="a.b"/>
            </policy>"#,
        );
        let peer = exact.for_peer(ALICE, &[]);
        assert!(!peer.sends_freely());
        assert!(peer.may_send(&call(), &names(&["a.b"])));
        assert!(!peer.may_send(&call(), &names(&["a.b.c"])));
        assert!(!peer.may_send(&call(), &[]));

        let prefix = policy(
            r#"<policy context="default">
                <allow send_destination_prefix="a.b"/>
            </policy>"#,
        );
        let peer = prefix.for_peer(ALICE, &[]);
        assert!(peer.may_send(&call(), &names(&["a.b"])));
        assert!(peer.may_send(&call(), &names(&["a.b.c"])));
        assert!(!peer.may_send(&call(), &names(&["a.bc"])));
        assert!(!peer.may_send(&call(), &[]));

        let any = policy(
            r#"<policy context="default">
                <allow send_destination="*"/>
            </policy>"#,
        );
        let peer = any.for_peer(ALICE, &[]);
        assert!(peer.sends_freely());
        assert!(peer.may_send(&call(), &[]));
    }

    /// A rule matches where one of the names of the other party does, a name it queues for
    /// counting as one it owns.
    #[test]
    fn any_name_of_the_other_party_matches() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_destination="org.example.Queued"/>
                <allow receive_sender="org.example.Queued"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        let names = names(&["org.example.Owned", "org.example.Queued"]);
        assert!(peer.may_send(&call(), &names));
        assert!(peer.may_receive(&call(), &names));
    }

    /// A unique name is no well-known name, so a rule that names one never matches.
    #[test]
    fn unique_names_never_match() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_destination=":1.42"/>
                <allow receive_sender=":1.42"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        assert!(!peer.may_send(&call(), &[]));
        assert!(!peer.may_receive(&call(), &[]));
    }

    /// `send_path`, `send_interface` and `send_member` match the message's own, and a message
    /// without an interface does not match a rule that names one.
    #[test]
    fn send_rules_match_paths_interfaces_and_members() {
        let path =
            policy(r#"<policy context="default"><allow send_path="/org/example"/></policy>"#);
        let peer = path.for_peer(ALICE, &[]);
        assert!(peer.may_send(&call(), &[]));
        assert!(!peer.may_send(
            &Facts {
                path: Some("/org"),
                ..call()
            },
            &[]
        ));

        let interface = policy(
            r#"<policy context="default"><allow send_interface="org.example.Iface"/></policy>"#,
        );
        let peer = interface.for_peer(ALICE, &[]);
        assert!(peer.may_send(&call(), &[]));
        let other = Facts {
            interface: Some("org.example.Other"),
            ..call()
        };
        assert!(!peer.may_send(&other, &[]));
        let without = Facts {
            interface: None,
            ..call()
        };
        assert!(!peer.may_send(&without, &[]));

        let member = policy(r#"<policy context="default"><allow send_member="Do"/></policy>"#);
        let peer = member.for_peer(ALICE, &[]);
        assert!(peer.may_send(&call(), &[]));
        assert!(!peer.may_send(
            &Facts {
                member: Some("Undo"),
                ..call()
            },
            &[]
        ));
    }

    /// An attribute whose value is `*` always holds.
    #[test]
    fn wildcard_attributes_always_hold() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_path="*" send_interface="*" send_member="*" send_type="*"/>
                <allow receive_sender="*" receive_interface="*"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        assert!(peer.sends_freely());
        assert!(peer.receives_freely());
        let without = Facts {
            interface: None,
            ..signal()
        };
        assert!(peer.may_send(&without, &[]));
        assert!(peer.may_receive(&without, &[]));
    }

    /// `send_type` matches the type of the message.
    #[test]
    fn send_rules_match_types() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_type="signal"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        assert!(peer.may_send(&signal(), &[]));
        assert!(!peer.may_send(&call(), &[]));
    }

    /// `send_broadcast="true"` matches a signal without a destination, and
    /// `send_broadcast="false"` any other message.
    #[test]
    fn send_rules_match_broadcasts() {
        let broadcast = Facts {
            broadcast: true,
            ..signal()
        };

        let only = policy(r#"<policy context="default"><allow send_broadcast="true"/></policy>"#);
        let peer = only.for_peer(ALICE, &[]);
        assert!(peer.may_send(&broadcast, &[]));
        assert!(!peer.may_send(&signal(), &names(&["org.example.Service"])));
        assert!(!peer.may_send(&call(), &names(&["org.example.Service"])));

        let not = policy(r#"<policy context="default"><allow send_broadcast="false"/></policy>"#);
        let peer = not.for_peer(ALICE, &[]);
        assert!(!peer.may_send(&broadcast, &[]));
        assert!(peer.may_send(&signal(), &names(&["org.example.Service"])));
        assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
    }

    /// `min_fds` and `max_fds` bound the number of file descriptors of the message, inclusive.
    #[test]
    fn send_rules_match_numbers_of_fds() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_type="method_call" min_fds="1" max_fds="2"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        for (fds, allowed) in [(0, false), (1, true), (2, true), (3, false)] {
            assert_eq!(
                peer.may_send(&Facts { fds, ..call() }, &[]),
                allowed,
                "{fds} fds"
            );
        }
    }

    /// Receive rules match the names of the sender and the attributes of the message as send
    /// rules do those of the receiver.
    #[test]
    fn receive_rules_match_senders_and_messages() {
        let policy = policy(
            r#"<policy context="default">
                <allow receive_sender="org.example.Service" receive_type="method_call"
                       receive_path="/org/example" receive_interface="org.example.Iface"
                       receive_member="Do" max_fds="1"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        assert!(!peer.receives_freely());
        let service = names(&["org.example.Service"]);
        assert!(peer.may_receive(&call(), &service));
        assert!(!peer.may_receive(&call(), &names(&["org.example.Other"])));
        assert!(!peer.may_receive(&call(), &[]));
        assert!(!peer.may_receive(&signal(), &service));
        assert!(!peer.may_receive(
            &Facts {
                path: Some("/"),
                ..call()
            },
            &service
        ));
        let without = Facts {
            interface: None,
            ..call()
        };
        assert!(!peer.may_receive(&without, &service));
        assert!(!peer.may_receive(
            &Facts {
                member: Some("Undo"),
                ..call()
            },
            &service
        ));
        assert!(!peer.may_receive(&Facts { fds: 2, ..call() }, &service));
        // The send rules are not the receive rules.
        assert!(!peer.may_send(&call(), &service));
    }

    /// Within each list of send rules, those that name no one, those that name a name and those
    /// that name a prefix, a rule outranks the rules before it.
    #[test]
    fn later_send_rules_outrank_earlier_ones() {
        let service = names(&["org.example.Service"]);
        for (first, second) in [
            (r#"send_type="method_call""#, r#"send_member="Do""#),
            (
                r#"send_destination="org.example.Service""#,
                r#"send_destination="org.example.Service" send_member="Do""#,
            ),
            (
                r#"send_destination_prefix="org.example""#,
                r#"send_destination_prefix="org.example.Service""#,
            ),
        ] {
            for (allowed, denied) in [(first, second), (second, first)] {
                let peer = policy(&format!(
                    r#"<policy context="default">
                        <deny {denied}/>
                        <allow {allowed}/>
                    </policy>"#
                ))
                .for_peer(ALICE, &[]);
                assert!(peer.may_send(&call(), &service), "{denied} then {allowed}");

                let peer = policy(&format!(
                    r#"<policy context="default">
                        <allow {allowed}/>
                        <deny {denied}/>
                    </policy>"#
                ))
                .for_peer(ALICE, &[]);
                assert!(!peer.may_send(&call(), &service), "{allowed} then {denied}");
            }
        }
    }

    /// Within each list of receive rules, those that name no one and those that name a name, a
    /// rule outranks the rules before it.
    #[test]
    fn later_receive_rules_outrank_earlier_ones() {
        let service = names(&["org.example.Service"]);
        for (first, second) in [
            (r#"receive_type="method_call""#, r#"receive_member="Do""#),
            (
                r#"receive_sender="org.example.Service""#,
                r#"receive_sender="org.example.Service" receive_member="Do""#,
            ),
        ] {
            for (allowed, denied) in [(first, second), (second, first)] {
                let peer = policy(&format!(
                    r#"<policy context="default">
                        <deny {denied}/>
                        <allow {allowed}/>
                    </policy>"#
                ))
                .for_peer(ALICE, &[]);
                assert!(
                    peer.may_receive(&call(), &service),
                    "{denied} then {allowed}"
                );

                let peer = policy(&format!(
                    r#"<policy context="default">
                        <allow {allowed}/>
                        <deny {denied}/>
                    </policy>"#
                ))
                .for_peer(ALICE, &[]);
                assert!(
                    !peer.may_receive(&call(), &service),
                    "{allowed} then {denied}"
                );
            }
        }
    }

    /// A mandatory send or receive rule outranks a default one that the configuration has after
    /// it.
    #[test]
    fn mandatory_message_rules_outrank_later_default_ones() {
        let policy = policy(
            r#"<policy context="mandatory">
                <deny send_destination="org.example.Service"/>
                <deny receive_sender="org.example.Service"/>
            </policy>
            <policy context="default">
                <allow send_destination="org.example.Service"/>
                <allow receive_sender="org.example.Service"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        let service = names(&["org.example.Service"]);
        assert!(!peer.may_send(&call(), &service));
        assert!(!peer.may_receive(&call(), &service));
    }

    /// The highest-ranking match decides, whether it is a rule that names no one, a name or a
    /// prefix, and in whichever set of the peer it is.
    #[test]
    fn the_highest_match_decides_across_lists_and_sets() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_destination="org.example.Service"/>
                <deny send_destination_prefix="org.example"/>
                <allow send_member="Do"/>
            </policy>
            <policy user="alice">
                <deny send_destination="org.example.Service" send_member="Do"/>
            </policy>
            <policy group="wheel">
                <allow send_destination_prefix="org.example" send_member="Do"/>
            </policy>"#,
        );
        let service = names(&["org.example.Service"]);
        let undo = Facts {
            member: Some("Undo"),
            ..call()
        };

        // The rule that names no one outranks the prefix, which outranks the name.
        let bob = policy.for_peer(BOB, &[]);
        assert!(bob.may_send(&call(), &service));
        assert!(!bob.may_send(&undo, &service));
        assert!(bob.may_send(&call(), &[]));
        // The group's prefix outranks the default rules.
        let member = policy.for_peer(BOB, &[WHEEL]);
        assert!(member.may_send(&call(), &service));
        assert!(!member.may_send(&undo, &service));
        // The user's name outranks the group's prefix and the default rules.
        let alice = policy.for_peer(ALICE, &[WHEEL]);
        assert!(!alice.may_send(&call(), &service));
        assert!(alice.may_send(&call(), &names(&["org.example.Other"])));
    }

    /// Where the highest-ranking rule of a list does not match, a lower one of the list still
    /// decides.
    #[test]
    fn a_lower_rule_decides_where_the_higher_ones_miss() {
        let policy = policy(
            r#"<policy context="default">
                <deny send_destination="org.example.Service"/>
                <allow send_destination="org.example.Service" send_member="Do"/>
                <deny send_destination="org.example.Service" send_member="Never"/>
                <allow send_destination_prefix="org.example" send_member="Do"/>
                <deny send_destination_prefix="org.example" send_member="Never"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
        assert!(peer.may_send(&call(), &names(&["org.example.Other"])));
        let undo = Facts {
            member: Some("Undo"),
            ..call()
        };
        assert!(!peer.may_send(&undo, &names(&["org.example.Service"])));
        assert!(!peer.may_send(&undo, &names(&["org.example.Other"])));
    }

    /// A peer sends or receives freely where its highest-ranking rule has no condition and
    /// allows, and the verdict of such a rule that denies holds for every message.
    #[test]
    fn an_unconditional_top_rule_decides_every_message() {
        let policy = policy(
            r#"<policy context="default">
                <deny send_destination="org.example.Service"/>
                <allow send_destination="*"/>
                <allow receive_sender="org.example.Service"/>
                <deny eavesdrop="false"/>
            </policy>
            <policy user="alice">
                <deny send_type="method_call"/>
            </policy>"#,
        );

        let bob = policy.for_peer(BOB, &[]);
        assert!(bob.sends_freely());
        assert!(bob.may_send(&call(), &names(&["org.example.Service"])));
        assert!(!bob.receives_freely());
        assert!(!bob.may_receive(&call(), &names(&["org.example.Service"])));
        // Alice's own rule outranks the unconditional one, and has a condition.
        let alice = policy.for_peer(ALICE, &[]);
        assert!(!alice.sends_freely());
        assert!(!alice.may_send(&call(), &[]));
        assert!(alice.may_send(&signal(), &[]));
    }

    /// The rules about replies and errors are left out, so they neither decide nor keep a peer
    /// from sending freely.
    #[test]
    fn rules_about_replies_are_left_out() {
        let policy = policy(
            r#"<policy context="default">
                <allow send_destination="*"/>
                <deny send_type="method_return"/>
                <deny send_type="error"/>
                <deny send_error="org.example.Error"/>
                <allow receive_sender="org.example.Service"/>
                <deny receive_type="method_return"/>
            </policy>"#,
        );

        let peer = policy.for_peer(ALICE, &[]);
        assert!(peer.sends_freely());
        assert!(peer.may_send(&call(), &names(&["org.example.Service"])));
        assert!(peer.may_receive(&call(), &names(&["org.example.Service"])));
    }

    /// Everything is free under the stock session.conf.
    #[test]
    fn the_stock_session_conf_lets_everything_through() {
        let config = Config::read_file("tests/data/session.conf").expect("should parse");
        let policy = Policy::new(&config, &ids(), ALICE);

        assert!(policy.may_connect(ALICE, &[ALICE]));
        let peer = policy.for_peer(ALICE, &[ALICE]);
        assert!(peer.sends_freely());
        assert!(peer.receives_freely());
        assert!(peer.may_own("org.example.Service"));
    }

    /// Under the stock system.conf, an ordinary user may connect, send signals and talk to the
    /// bus, but not call services, use the statistics of the bus, which root may, or own names.
    #[test]
    fn the_stock_system_conf_holds_back_ordinary_users() {
        let config = Config::read_file("tests/data/system.conf").expect("should parse");
        let ids = Ids::parse("root:x:0:0:::", "");
        let policy = Policy::new(&config, &ids, BUS_UID);
        let bus = names(&["org.freedesktop.DBus"]);
        let service = names(&["org.example.Service"]);
        let hello = Facts {
            path: Some("/org/freedesktop/DBus"),
            interface: Some("org.freedesktop.DBus"),
            member: Some("Hello"),
            ..call()
        };
        let stats = Facts {
            interface: Some("org.freedesktop.DBus.Debug.Stats"),
            member: Some("GetStats"),
            ..hello
        };

        assert!(policy.may_connect(ALICE, &[ALICE]));
        let alice = policy.for_peer(ALICE, &[ALICE]);
        assert!(!alice.sends_freely());
        assert!(!alice.may_send(&call(), &service));
        assert!(alice.may_send(&signal(), &service));
        assert!(alice.may_send(
            &Facts {
                broadcast: true,
                ..signal()
            },
            &[]
        ));
        assert!(alice.may_send(&hello, &bus));
        assert!(!alice.may_send(&stats, &bus));
        assert!(alice.may_receive(&call(), &service));
        assert!(!alice.may_own("org.example.Service"));

        let root = policy.for_peer(ROOT, &[ROOT]);
        assert!(root.may_send(&stats, &bus));
        assert!(!root.may_send(&call(), &service));
        assert!(!root.may_own("org.example.Service"));
    }

    /// The facts of a message are those of its header, and a signal without a destination is a
    /// broadcast.
    #[test]
    fn facts_of_a_message() {
        let msg = zbus::Message::signal("/org/example", "org.example.Iface", "Changed")
            .build(&())
            .expect("should build");
        let header = msg.header();

        let facts = Facts::new(&msg, &header);
        assert_eq!(facts.msg_type, Type::Signal);
        assert_eq!(facts.path, Some("/org/example"));
        assert_eq!(facts.interface, Some("org.example.Iface"));
        assert_eq!(facts.member, Some("Changed"));
        assert!(facts.broadcast);
        assert_eq!(facts.fds, 0);

        let msg = zbus::Message::method_call("/org/example", "Do")
            .destination("org.example.Service")
            .build(&())
            .expect("should build");
        let header = msg.header();

        let facts = Facts::new(&msg, &header);
        assert_eq!(facts.msg_type, Type::MethodCall);
        assert_eq!(facts.interface, None);
        assert!(!facts.broadcast);
    }

    /// The policy of the `<policy>` elements of `policies`, for a bus of `BUS_UID`.
    fn policy(policies: &str) -> Policy {
        Policy::new(&parse(policies), &ids(), BUS_UID)
    }

    /// The configuration that has `elements` in it.
    fn parse(elements: &str) -> Config {
        Config::parse(&format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
            "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
            <busconfig>{elements}</busconfig>"#
        ))
        .expect("should parse XML input")
    }

    /// The ids of the users root, messagebus, alice and bob, and of the groups root and wheel.
    fn ids() -> Ids {
        Ids::parse(
            "root:x:0:0:::\nmessagebus:x:101:101:::\nalice:x:1000:1000:::\nbob:x:1001:1001:::\n",
            "root:x:0:\nwheel:x:10:alice,bob\n",
        )
    }

    /// A method call of `org.example.Iface.Do` on `/org/example`, with a destination.
    fn call() -> Facts<'static> {
        Facts {
            msg_type: Type::MethodCall,
            path: Some("/org/example"),
            interface: Some("org.example.Iface"),
            member: Some("Do"),
            broadcast: false,
            fds: 0,
        }
    }

    /// A signal like [`call`], with a destination.
    fn signal() -> Facts<'static> {
        Facts {
            msg_type: Type::Signal,
            ..call()
        }
    }

    /// The well-known names of `names`.
    fn names(names: &[&str]) -> Vec<OwnedWellKnownName> {
        names
            .iter()
            .map(|name| OwnedWellKnownName::try_from(*name).expect("should be a well-known name"))
            .collect()
    }

    const BUS_UID: u32 = 101;
    const ROOT: u32 = 0;
    const ALICE: u32 = 1000;
    const BOB: u32 = 1001;
    const WHEEL: u32 = 10;
}
