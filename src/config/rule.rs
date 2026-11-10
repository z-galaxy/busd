use anyhow::{Error, Result};
use serde::Deserialize;

use super::{
    xml::{RuleAttributes, RuleElement},
    MessageType, Name,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ConnectOperation {
    pub group: Option<String>,
    pub user: Option<String>,
}

impl From<RuleAttributes> for ConnectOperation {
    fn from(value: RuleAttributes) -> Self {
        Self {
            group: value.group,
            user: value.user,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub enum Operation {
    /// rules checked when a new connection to the message bus is established
    Connect(ConnectOperation),
    /// rules checked when a connection attempts to own a well-known bus names
    Own(NameOwnership),
    /// rules that are checked for each recipient of a message
    Receive(ReceiveOperation),
    /// rules that are checked when a connection attempts to send a message
    Send(SendOperation),
}

type OptionalOperation = Option<Operation>;

impl TryFrom<RuleAttributes> for OptionalOperation {
    type Error = Error;

    fn try_from(value: RuleAttributes) -> std::result::Result<Self, Self::Error> {
        let has_connect = value.group.is_some() || value.user.is_some();
        let has_own = value.own.is_some() || value.own_prefix.is_some();
        // The `*_requested_reply` attributes make a send or receive rule, but are ignored
        // otherwise: they only concern replies, which the policy of the bus does not apply to.
        let has_send = value.send_broadcast.is_some()
            || value.send_destination.is_some()
            || value.send_destination_prefix.is_some()
            || value.send_error.is_some()
            || value.send_interface.is_some()
            || value.send_member.is_some()
            || value.send_path.is_some()
            || value.send_requested_reply.is_some()
            || value.send_type.is_some();
        let has_receive = value.receive_error.is_some()
            || value.receive_interface.is_some()
            || value.receive_member.is_some()
            || value.receive_path.is_some()
            || value.receive_sender.is_some()
            || value.receive_requested_reply.is_some()
            || value.receive_type.is_some();

        let operations_count: i8 = vec![has_connect, has_own, has_receive, has_send]
            .into_iter()
            .map(i8::from)
            .sum();

        if operations_count > 1 {
            return Err(Error::msg(format!("do not mix rule attributes for connect, own, receive, and/or send attributes in the same rule: {value:?}")));
        }

        if has_connect {
            Ok(Some(Operation::Connect(ConnectOperation::from(value))))
        } else if has_own {
            Ok(Some(Operation::Own(NameOwnership::from(value))))
        } else if has_receive {
            Ok(Some(Operation::Receive(ReceiveOperation::from(value))))
        } else if has_send {
            Ok(Some(Operation::Send(SendOperation::from(value))))
        } else if value.eavesdrop.is_some() {
            // `eavesdrop` is ignored, so a rule that has nothing else is a receive rule for every
            // message: `<allow eavesdrop="true"/>` is how the stock session.conf lets everyone
            // receive everything.
            Ok(Some(Operation::Receive(ReceiveOperation::from(value))))
        } else {
            Err(Error::msg(format!("rule must specify supported attributes for connect, own, receive, or send operations: {value:?}")))
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct NameOwnership {
    pub own: Option<Name>,
}

impl From<RuleAttributes> for NameOwnership {
    fn from(value: RuleAttributes) -> Self {
        let own = match value {
            RuleAttributes {
                own: Some(some),
                own_prefix: None,
                ..
            } if some == "*" => Some(Name::Any),
            RuleAttributes {
                own: Some(some),
                own_prefix: None,
                ..
            } => Some(Name::Exact(some)),
            RuleAttributes {
                own: None,
                own_prefix: Some(some),
                ..
            } => Some(Name::Prefix(some)),
            _ => None,
        };
        Self { own }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ReceiveOperation {
    pub error: Option<String>,
    pub interface: Option<String>,
    pub max_fds: Option<u32>,
    pub member: Option<String>,
    pub min_fds: Option<u32>,
    pub path: Option<String>,
    pub sender: Option<String>,
    pub r#type: Option<MessageType>,
}

impl From<RuleAttributes> for ReceiveOperation {
    fn from(value: RuleAttributes) -> Self {
        Self {
            error: value.receive_error,
            interface: value.receive_interface,
            max_fds: value.max_fds,
            member: value.receive_member,
            min_fds: value.min_fds,
            path: value.receive_path,
            sender: value.receive_sender,
            r#type: value.receive_type,
        }
    }
}

type OptionalRule = Option<Rule>;

impl TryFrom<RuleElement> for OptionalRule {
    type Error = Error;

    fn try_from(value: RuleElement) -> std::result::Result<Self, Self::Error> {
        match value {
            RuleElement::Allow(RuleAttributes {
                group: Some(_),
                user: Some(_),
                ..
            })
            | RuleElement::Deny(RuleAttributes {
                group: Some(_),
                user: Some(_),
                ..
            }) => Err(Error::msg(format!(
                "`group` cannot be combined with `user` in the same rule: {value:?}"
            ))),
            RuleElement::Allow(RuleAttributes {
                own: Some(_),
                own_prefix: Some(_),
                ..
            })
            | RuleElement::Deny(RuleAttributes {
                own: Some(_),
                own_prefix: Some(_),
                ..
            }) => Err(Error::msg(format!(
                "`own_prefix` cannot be combined with `own` in the same rule: {value:?}"
            ))),
            RuleElement::Allow(RuleAttributes {
                send_destination: Some(_),
                send_destination_prefix: Some(_),
                ..
            })
            | RuleElement::Deny(RuleAttributes {
                send_destination: Some(_),
                send_destination_prefix: Some(_),
                ..
            }) => Err(Error::msg(format!(
                "`send_destination_prefix` cannot be combined with `send_destination` in the same rule: {value:?}"
            ))),
            RuleElement::Allow(attrs) => match OptionalOperation::try_from(attrs)? {
                Some(some) => Ok(Some((Access::Allow, some))),
                None => Ok(None),
            },
            RuleElement::Deny(RuleAttributes {
                eavesdrop: Some(true),
                ..
            }) => {
                // Such a rule only concerns eavesdropping, which the bus does not do.
                Ok(None)
            }
            RuleElement::Deny(attrs) => match OptionalOperation::try_from(attrs)? {
                Some(some) => Ok(Some((Access::Deny, some))),
                None => Ok(None),
            },
        }
    }
}

pub type Rule = (Access, Operation);

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub enum Access {
    Allow,
    Deny,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct SendOperation {
    pub broadcast: Option<bool>,
    pub destination: Option<Name>,
    pub error: Option<String>,
    pub interface: Option<String>,
    pub max_fds: Option<u32>,
    pub member: Option<String>,
    pub min_fds: Option<u32>,
    pub path: Option<String>,
    pub r#type: Option<MessageType>,
}

impl From<RuleAttributes> for SendOperation {
    fn from(value: RuleAttributes) -> Self {
        let destination = match value {
            RuleAttributes {
                send_destination: Some(some),
                send_destination_prefix: None,
                ..
            } if some == "*" => Some(Name::Any),
            RuleAttributes {
                send_destination: Some(some),
                send_destination_prefix: None,
                ..
            } => Some(Name::Exact(some)),
            RuleAttributes {
                send_destination: None,
                send_destination_prefix: Some(some),
                ..
            } => Some(Name::Prefix(some)),
            _ => None,
        };
        Self {
            broadcast: value.send_broadcast,
            destination,
            error: value.send_error,
            interface: value.send_interface,
            max_fds: value.max_fds,
            member: value.send_member,
            min_fds: value.min_fds,
            path: value.send_path,
            r#type: value.send_type,
        }
    }
}

pub fn rules_try_from_rule_elements(value: Vec<RuleElement>) -> Result<Vec<Rule>> {
    let mut rules = vec![];
    for rule in value {
        let rule = OptionalRule::try_from(rule)?;
        if let Some(some) = rule {
            rules.push(some);
        }
    }
    Ok(rules)
}
