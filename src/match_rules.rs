use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use zbus::{names::BusName, OwnedMatchRule, Type};

use crate::name_registry::NameRegistry;

/// A collection of match rules.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Type)]
pub struct MatchRules(HashSet<OwnedMatchRule>);

impl MatchRules {
    /// Match the given message against the rules.
    ///
    /// # Panics
    ///
    /// if header or SENDER is not set.
    pub fn matches(&self, msg: &zbus::Message, name_registry: &NameRegistry) -> bool {
        let hdr = msg.header();

        let ret = self.0.iter().any(|rule| {
            // First make use of zbus API
            match rule.matches(msg) {
                Ok(false) => return false,
                Ok(true) => (),
                Err(e) => {
                    crate::warn!("error matching rule: {}", e);

                    return false;
                }
            }

            // Then match sender and destination involving well-known names, manually.
            if let Some(sender) = rule.sender().and_then(|name| match name {
                BusName::WellKnown(name) => name_registry.lookup(name),
                // Unique name is already taken care of by the zbus API.
                BusName::Unique(_) => None,
            }) {
                if sender != hdr.sender().expect("SENDER field unset") {
                    return false;
                }
            }

            // The destination.
            if let Some(destination) = rule.destination() {
                match hdr.destination() {
                    Some(BusName::WellKnown(name)) => match name_registry.lookup(name) {
                        Some(name) if name == destination => (),
                        Some(_) => return false,
                        None => return false,
                    },
                    // A unique name is already taken care of by the zbus API. A message without a
                    // destination, such as a broadcast signal, matches, as with the zbus API and
                    // dbus-broker.
                    Some(BusName::Unique(_)) | None => {}
                }
            }

            true
        });

        ret
    }

    pub fn add(&mut self, rule: OwnedMatchRule) {
        self.0.insert(rule);
    }

    /// Remove the first rule that matches.
    pub fn remove(&mut self, rule: OwnedMatchRule) -> zbus::fdo::Result<()> {
        if !self.0.remove(&rule) {
            return Err(zbus::fdo::Error::MatchRuleNotFound(
                "No such match rule".to_string(),
            ));
        }

        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
