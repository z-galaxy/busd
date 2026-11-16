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
    pub sender: Option<Name>,
    pub message: MessageMatch,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SendOperation {
    pub destination: Option<Name>,
    pub broadcast: Option<bool>,
    pub message: MessageMatch,
}

/// The conditions that a send or receive rule sets on the message itself, whoever the other party
/// is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MessageMatch {
    pub r#type: Option<MessageType>,
    pub error: Option<String>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub min_fds: Option<u32>,
    pub max_fds: Option<u32>,
}
