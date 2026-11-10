use anyhow::Error;
use serde::Deserialize;

use super::{
    rule::{rules_try_from_rule_elements, Rule},
    xml::{PolicyContext, PolicyElement},
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub enum Policy {
    /// `<policy at_console="true">` or `<policy at_console="false">`, after the `bool`: the rules
    /// for the peers of the users at the console, or for those of the other users.
    Console(Vec<Rule>, bool),
    DefaultContext(Vec<Rule>),
    Group(Vec<Rule>, String),
    MandatoryContext(Vec<Rule>),
    User(Vec<Rule>, String),
}
// TODO: implement Cmp/Ord to help stable-sort Policy values:
// DefaultContext < Group < User < MandatoryContext

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
