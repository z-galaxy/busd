use super::{MessageType, Name};

/// An `<allow>` or `<deny>` rule of a `<policy>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub access: Access,
    pub operation: Operation,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Access {
    Allow,
    Deny,
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
pub struct ConnectOperation {
    pub group: Option<String>,
    pub user: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NameOwnership {
    pub own: Option<Name>,
}

#[derive(Clone, Debug, PartialEq)]
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

#[derive(Clone, Debug, PartialEq)]
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
