use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use zbus::{
    fdo::{Error, Result},
    interface,
    names::{BusName, OwnedUniqueName},
    OwnedMatchRule, SerializeDict, Type,
};

use crate::{
    fdo::BUS_NAME,
    peers::{ConnectionCounts, Counts, Peers},
};

#[derive(Debug)]
pub struct Stats {
    peers: Weak<Peers>,
    /// The number of the last call of `GetStats`.
    serial: u32,
    /// The number of the last call of `GetConnectionStats` that the bus answered, which counts
    /// independently of that of `GetStats`, as with the reference implementation of D-Bus.
    connection_serial: u32,
}

impl Stats {
    pub const PATH: &'static str = "/org/freedesktop/DBus";
    pub const INTERFACE: &'static str = "org.freedesktop.DBus.Debug.Stats";

    pub fn new(peers: Arc<Peers>) -> Self {
        Self {
            peers: Arc::downgrade(&peers),
            serial: 0,
            connection_serial: 0,
        }
    }

    /// The peers of the bus, unless the bus is shutting down.
    fn peers(&self) -> Result<Arc<Peers>> {
        self.peers
            .upgrade()
            // Can it happen in any other situation than the bus shutting down?
            .ok_or_else(|| Error::Failed("Bus shutting down.".to_string()))
    }
}

// One call at a time, in order, as with the `org.freedesktop.DBus` interface, and with the same
// rule: a method never waits for anything that waits for the bus to take the calls after it.
// What the methods ask the peers for only takes the lock of the routing, as the methods of that
// interface do.
#[interface(
    interface = "org.freedesktop.DBus.Debug.Stats",
    introspection_docs = false,
    spawn = false
)]
impl Stats {
    /// Statistics about the message bus itself.
    async fn get_stats(&mut self) -> Result<BusStats> {
        let counts = self.peers()?.counts().await;
        self.serial = self.serial.wrapping_add(1);

        Ok(BusStats::new(self.serial, counts))
    }

    /// Statistics for the connection identified by `name`, either its unique name or a well-known
    /// name that it owns.
    ///
    /// The bus itself and monitors cannot be queried. The reply omits the counts of queued
    /// messages, bytes and file descriptors, because the bus does not track them.
    async fn get_connection_stats(&mut self, name: BusName<'_>) -> Result<ConnectionStats> {
        if name == BUS_NAME {
            return Err(Error::InvalidArgs(
                "`GetConnectionStats` is not meaningful for the bus itself.".to_string(),
            ));
        }
        let Some(counts) = self.peers()?.connection_counts(&name).await else {
            return Err(Error::NameHasNoOwner(format!(
                "Name `{name}` has no owner."
            )));
        };
        self.connection_serial = self.connection_serial.wrapping_add(1);

        Ok(ConnectionStats::new(self.connection_serial, counts))
    }

    /// The match rules of all the connections, by their unique names.
    ///
    /// A connection without match rules has an empty list. The bus itself is not listed, and
    /// neither are the monitors.
    async fn get_all_match_rules(&self) -> Result<HashMap<OwnedUniqueName, Vec<OwnedMatchRule>>> {
        Ok(self.peers()?.all_match_rules().await)
    }
}

/// The reply of `GetStats`.
#[derive(Debug, SerializeDict, Type)]
#[zbus(signature = "a{sv}", rename_all = "PascalCase")]
struct BusStats {
    serial: u32,
    active_connections: u32,
    incomplete_connections: u32,
    match_rules: u32,
    peak_match_rules: u32,
    peak_match_rules_per_connection: u32,
    bus_names: u32,
    peak_bus_names: u32,
    peak_bus_names_per_connection: u32,
}

impl BusStats {
    fn new(serial: u32, counts: Counts) -> Self {
        Self {
            serial,
            active_connections: fit(counts.active_connections),
            incomplete_connections: fit(counts.incomplete_connections),
            match_rules: fit(counts.match_rules),
            peak_match_rules: fit(counts.peak_match_rules),
            peak_match_rules_per_connection: fit(counts.peak_match_rules_per_connection),
            bus_names: fit(counts.bus_names),
            peak_bus_names: fit(counts.peak_bus_names),
            peak_bus_names_per_connection: fit(counts.peak_bus_names_per_connection),
        }
    }
}

/// The reply of `GetConnectionStats`.
#[derive(Debug, SerializeDict, Type)]
#[zbus(signature = "a{sv}", rename_all = "PascalCase")]
struct ConnectionStats {
    serial: u32,
    unique_name: OwnedUniqueName,
    match_rules: u32,
    peak_match_rules: u32,
    bus_names: u32,
    peak_bus_names: u32,
}

impl ConnectionStats {
    fn new(serial: u32, counts: ConnectionCounts) -> Self {
        Self {
            serial,
            unique_name: counts.unique_name,
            match_rules: fit(counts.match_rules),
            peak_match_rules: fit(counts.peak_match_rules),
            bus_names: fit(counts.bus_names),
            peak_bus_names: fit(counts.peak_bus_names),
        }
    }
}

/// `count` as the `u32` that the replies have: one that does not fit saturates.
fn fit(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}
