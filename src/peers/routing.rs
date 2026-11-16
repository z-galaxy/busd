use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use zbus::{
    names::{OwnedUniqueName, UniqueName},
    OwnedMatchRule,
};

use crate::{
    name_registry::NameRegistry,
    peer::{Monitor, Peer},
    policy::Policy,
};

/// What the bus routes messages by: its peers, its monitors and the owners of its names.
///
/// They are all behind one lock, so that the bus takes one lock to pick the recipients of a batch.
#[derive(Debug, Default)]
pub struct Routing {
    /// The peers, by their unique names.
    ///
    /// They are hashed, with foldhash, so that finding the peer that a message goes to takes one
    /// hash of the name and one comparison with the name of the peer, rather than kept in a tree,
    /// which would compare the name with the keys of each node on the way, every comparison
    /// through the `:busd.` that the unique names of all clients start with.
    pub peers: HashMap<OwnedUniqueName, Peer, foldhash::fast::RandomState>,
    /// The owners of the well-known names, and those that queue for them.
    pub name_registry: NameRegistry,
    /// The monitors, by their unique names.
    pub(super) monitors: BTreeMap<OwnedUniqueName, Monitor>,
    /// Whether the bus is handing over to its next instance, so that it pauses the clients that
    /// connect, and takes none of their messages.
    #[cfg(target_os = "linux")]
    pub(super) handing_over: bool,
    /// Whether the bus takes no more clients, as it is done pausing those that it hands over: see
    /// [`Peers::add`](super::Peers::add).
    #[cfg(target_os = "linux")]
    pub(super) sealed: bool,
    /// How many match rules the peers have, kept up to date by the methods that change them, so
    /// that counting them takes no pass over the peers.
    pub(super) match_rule_counts: MatchRuleCounts,
    /// The policy of the bus, which each peer but the bus itself gets its part of as it is added.
    pub(super) policy: Arc<Policy>,
}

impl Routing {
    /// Add `rule` to the match rules of the peer `name`.
    pub fn add_match_rule(
        &mut self,
        name: &UniqueName<'_>,
        rule: OwnedMatchRule,
    ) -> zbus::fdo::Result<()> {
        let peer = self
            .peers
            .get_mut(name.as_str())
            .ok_or_else(|| zbus::fdo::Error::NameHasNoOwner(format!("No such peer: {name}")))?;
        if peer.add_match_rule(rule) {
            let rules = peer.match_rules().len();
            self.match_rule_counts.add(1, rules);
        }

        Ok(())
    }

    /// Remove `rule` from the match rules of the peer `name`.
    pub fn remove_match_rule(
        &mut self,
        name: &UniqueName<'_>,
        rule: OwnedMatchRule,
    ) -> zbus::fdo::Result<()> {
        let peer = self
            .peers
            .get_mut(name.as_str())
            .ok_or_else(|| zbus::fdo::Error::NameHasNoOwner(format!("No such peer: {name}")))?;
        peer.remove_match_rule(rule)?;
        self.match_rule_counts.total = self.match_rule_counts.total.saturating_sub(1);

        Ok(())
    }

    /// Add `peer` under `name`, along with the match rules that it has already, and give it what
    /// the policy has for it, unless it is the bus itself.
    pub(super) fn insert_peer(&mut self, name: OwnedUniqueName, mut peer: Peer) {
        if !peer.is_bus() {
            peer.set_policy(self.policy.for_peer(peer.user().uid(), peer.groups()));
        }
        let rules = peer.match_rules().len();
        self.match_rule_counts.add(rules, rules);
        self.peers.insert(name, peer);
    }

    /// Remove the peer `name`, along with its match rules.
    pub(super) fn remove_peer(&mut self, name: &str) -> Option<Peer> {
        let peer = self.peers.remove(name)?;
        let rules = peer.match_rules().len();
        self.match_rule_counts.total = self.match_rule_counts.total.saturating_sub(rules);

        Some(peer)
    }
}

/// How many match rules the peers have between them, and the most they had.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MatchRuleCounts {
    /// The match rules that the peers have between them.
    pub(super) total: usize,
    /// The most that `total` has been.
    pub(super) peak_total: usize,
    /// The most match rules that one peer had at a time.
    pub(super) peak_per_peer: usize,
}

impl MatchRuleCounts {
    /// Note that a peer gained `gained` match rules, and has `rules` now.
    fn add(&mut self, gained: usize, rules: usize) {
        self.total += gained;
        self.peak_total = self.peak_total.max(self.total);
        self.peak_per_peer = self.peak_per_peer.max(rules);
    }
}
