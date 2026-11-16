use super::rule::Rule;

/// A `<policy>` element: its rules, and whom they apply to.
///
/// The configuration keeps its policies in the order of the file, rather than sorted by context,
/// as the rank of a rule goes by its place in the file as well as by its context.
#[derive(Clone, Debug, PartialEq)]
pub enum Policy {
    /// `<policy context="default">`: the rules for every peer.
    DefaultContext(Vec<Rule>),
    /// `<policy group="...">`: the rules for the peers in the group so named.
    Group(Vec<Rule>, String),
    /// `<policy context="mandatory">`: the rules for every peer, which outrank all others.
    MandatoryContext(Vec<Rule>),
    /// `<policy user="...">`: the rules for the peers of the user so named.
    User(Vec<Rule>, String),
    /// `<policy at_console="...">`, a context that the bus doesn't support: its rules are left
    /// out.
    Unsupported,
}
