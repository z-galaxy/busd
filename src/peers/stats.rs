use std::collections::HashMap;

use zbus::{
    names::{BusName, OwnedUniqueName},
    OwnedMatchRule,
};

use super::Peers;
use crate::fdo;

impl Peers {
    /// What the bus counts for the `GetStats` method of its `org.freedesktop.DBus.Debug.Stats`
    /// interface.
    ///
    /// This goes over all the peers, once, under the read lock of the routing.
    pub async fn counts(&self) -> Counts {
        let routing = self.routing().await;
        let mut greeted = 0;
        let mut not_greeted = 0;
        for (name, peer) in routing.peers.iter() {
            if *name == fdo::BUS_NAME {
                continue;
            }
            if peer.greeted() {
                greeted += 1;
            } else {
                not_greeted += 1;
            }
        }
        let connecting = self.chores().connecting;
        let match_rules = routing.match_rule_counts;

        Counts {
            active_connections: greeted + routing.monitors.len(),
            incomplete_connections: connecting + not_greeted,
            match_rules: match_rules.total,
            peak_match_rules: match_rules.peak_total,
            peak_match_rules_per_connection: match_rules.peak_per_peer,
            bus_names: routing.name_registry.all_names().len(),
            peak_bus_names: routing.name_registry.peak_names(),
            peak_bus_names_per_connection: routing.name_registry.peak_names_per_owner(),
        }
    }

    /// What the bus counts for the peer that `name` is the unique name of, or that owns `name`,
    /// if there is one, for the `GetConnectionStats` method of its
    /// `org.freedesktop.DBus.Debug.Stats` interface.
    ///
    /// A monitor is no peer, so there is nothing for its name. A client that has yet to say
    /// `Hello` is a peer like any other for this, as it is for `GetNameOwner`.
    pub async fn connection_counts(&self, name: &BusName<'_>) -> Option<ConnectionCounts> {
        let routing = self.routing().await;
        let unique_name = match name {
            BusName::Unique(name) => name.as_str(),
            BusName::WellKnown(name) => routing.name_registry.lookup(name)?.as_str(),
        };
        let (unique_name, peer) = routing.peers.get_key_value(unique_name)?;
        let owned = routing.name_registry.owned_by(unique_name.as_str());

        Some(ConnectionCounts {
            unique_name: unique_name.clone(),
            match_rules: peer.match_rules().len(),
            peak_match_rules: peer.peak_match_rules(),
            bus_names: owned.now,
            peak_bus_names: owned.peak,
        })
    }

    /// The match rules of each peer but the bus itself, by the unique name of the peer, for the
    /// `GetAllMatchRules` method of the `org.freedesktop.DBus.Debug.Stats` interface.
    ///
    /// A peer without match rules has an empty list, and a monitor has none, as it is no peer. A
    /// client that has yet to say `Hello` is listed like any other peer, as `ListNames` lists it.
    /// This goes over all the peers and their rules, once, under the read lock of the routing.
    pub async fn all_match_rules(&self) -> HashMap<OwnedUniqueName, Vec<OwnedMatchRule>> {
        let routing = self.routing().await;

        routing
            .peers
            .iter()
            .filter(|(name, _)| **name != fdo::BUS_NAME)
            .map(|(name, peer)| (name.clone(), peer.match_rules().iter().cloned().collect()))
            .collect()
    }
}

/// What the bus counts for the `GetStats` method of its `org.freedesktop.DBus.Debug.Stats`
/// interface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// The clients that called `Hello`, monitors included.
    pub active_connections: usize,
    /// The clients that are connected but did not call `Hello` yet.
    pub incomplete_connections: usize,
    /// The match rules that the peers have between them.
    pub match_rules: usize,
    /// The most match rules that the peers had between them at a time.
    pub peak_match_rules: usize,
    /// The most match rules that one peer had at a time.
    pub peak_match_rules_per_connection: usize,
    /// The well-known names that have an owner.
    pub bus_names: usize,
    /// The most well-known names that had an owner at a time.
    pub peak_bus_names: usize,
    /// The most well-known names that one peer owned at a time.
    pub peak_bus_names_per_connection: usize,
}

/// What the bus counts for one peer: see [`Peers::connection_counts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionCounts {
    /// The unique name of the peer.
    pub unique_name: OwnedUniqueName,
    /// The match rules that the peer has.
    pub match_rules: usize,
    /// The most match rules that the peer had at a time.
    pub peak_match_rules: usize,
    /// The well-known names that the peer owns.
    pub bus_names: usize,
    /// The most well-known names that the peer owned at a time.
    pub peak_bus_names: usize,
}
