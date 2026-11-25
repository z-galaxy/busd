//! The `org.freedesktop.DBus.Debug.Stats` interface.

use zbus::{
    fdo::{Error, Result},
    message::Header,
    names::{BusName, MemberName, OwnedUniqueName},
    Message, SerializeDict, Type,
};

use super::{args, unknown_method, Answer, Driver, BUS_NAME};
use crate::peers::{ConnectionCounts, Counts, Peers};

impl Driver {
    /// Answer the call of `member` of the `org.freedesktop.DBus.Debug.Stats` interface, `call`,
    /// whose header is `header`.
    pub(super) fn answer_stats(
        &mut self,
        peers: &Peers,
        call: &Message,
        header: &Header<'_>,
        member: &MemberName<'_>,
    ) -> Result<Answer> {
        match member.as_str() {
            // Statistics about the message bus itself.
            "GetStats" => {
                let counts = peers.counts();
                self.stats_serial = self.stats_serial.wrapping_add(1);

                Answer::reply(header, &BusStats::new(self.stats_serial, counts))
            }
            "GetConnectionStats" => {
                let body = call.body();
                let stats = self.connection_stats(peers, args(&body)?)?;

                Answer::reply(header, &stats)
            }
            // The match rules of all the peers, by their unique names. A peer without match rules
            // has an empty list. The bus itself is not listed, and neither are the monitors.
            "GetAllMatchRules" => Answer::reply(header, &peers.all_match_rules()),
            _ => Err(unknown_method(member)),
        }
    }

    /// Statistics for the connection identified by `name`, either its unique name or a well-known
    /// name that it owns.
    ///
    /// The bus itself and monitors cannot be queried. The reply omits the counts of queued
    /// messages, bytes and file descriptors, because the bus does not track them.
    fn connection_stats(&mut self, peers: &Peers, name: BusName<'_>) -> Result<ConnectionStats> {
        if name == BUS_NAME {
            return Err(Error::InvalidArgs(
                "`GetConnectionStats` is not meaningful for the bus itself.".to_string(),
            ));
        }
        let Some(counts) = peers.connection_counts(&name) else {
            return Err(Error::NameHasNoOwner(format!(
                "Name `{name}` has no owner."
            )));
        };
        self.connection_stats_serial = self.connection_stats_serial.wrapping_add(1);

        Ok(ConnectionStats::new(self.connection_stats_serial, counts))
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
