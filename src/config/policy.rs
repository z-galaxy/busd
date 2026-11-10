use anyhow::Error;
use serde::Deserialize;

use super::{
    rule::{rules_try_from_rule_elements, Rule},
    xml::{PolicyContext, PolicyElement},
};

/// A `<policy>` element: its rules, and whom they apply to.
///
/// The configuration keeps its policies in the order of the file, rather than sorted by context,
/// as the rank of a rule goes by its place in the file as well as by its context.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub enum Policy {
    /// `<policy at_console="true">` or `<policy at_console="false">`, after the `bool`: the rules
    /// for the peers of the users at the console, or for those of the other users.
    Console(Vec<Rule>, bool),
    /// `<policy context="default">`: the rules for every peer.
    DefaultContext(Vec<Rule>),
    /// `<policy group="...">`: the rules for the peers in the group so named.
    Group(Vec<Rule>, String),
    /// `<policy context="mandatory">`: the rules for every peer, which outrank all others.
    MandatoryContext(Vec<Rule>),
    /// `<policy user="...">`: the rules for the peers of the user so named.
    User(Vec<Rule>, String),
}

pub type OptionalPolicy = Option<Policy>;

impl TryFrom<PolicyElement> for OptionalPolicy {
    type Error = Error;

    fn try_from(value: PolicyElement) -> std::result::Result<Self, Self::Error> {
        match value {
            PolicyElement {
                at_console: Some(at_console),
                context: None,
                group: None,
                rules,
                user: None,
            } if at_console == "true" || at_console == "false" => Ok(Some(Policy::Console(
                rules_try_from_rule_elements(rules)?,
                at_console == "true",
            ))),
            PolicyElement {
                at_console: None,
                context: Some(c),
                group: None,
                rules,
                user: None,
            } => Ok(Some(match c {
                PolicyContext::Default => {
                    Policy::DefaultContext(rules_try_from_rule_elements(rules)?)
                }
                PolicyContext::Mandatory => {
                    Policy::MandatoryContext(rules_try_from_rule_elements(rules)?)
                }
            })),
            PolicyElement {
                at_console: None,
                context: None,
                group: Some(group),
                rules,
                user: None,
            } => Ok(Some(Policy::Group(
                rules_try_from_rule_elements(rules)?,
                group,
            ))),
            PolicyElement {
                at_console: None,
                context: None,
                group: None,
                rules,
                user: Some(user),
            } => Ok(Some(Policy::User(
                rules_try_from_rule_elements(rules)?,
                user,
            ))),
            _ => Err(Error::msg(format!(
                "policy contains conflicting attributes: {value:?}"
            ))),
        }
    }
}
