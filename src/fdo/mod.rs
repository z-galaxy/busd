mod dbus;
pub use dbus::*;
mod monitoring;
pub use monitoring::*;
mod stats;
pub use stats::*;
use zbus::{message, names::UniqueName};

use crate::peers::{Peers, WeakPeers};

pub const BUS_NAME: &str = "org.freedesktop.DBus";

/// Fails unless the peer that sent the call of `hdr` is privileged, as [`Peers::privileged`] has
/// it: dbus-broker only lets those peers call the methods that this guards, whatever the policy of
/// the configuration says.
async fn ensure_privileged(peers: &Peers, hdr: &message::Header<'_>) -> zbus::fdo::Result<()> {
    let name = msg_sender(hdr);
    match peers.privileged(name).await {
        Some(true) => Ok(()),
        Some(false) => Err(zbus::fdo::Error::AccessDenied(NOT_PRIVILEGED.to_string())),
        None => Err(zbus::fdo::Error::NameHasNoOwner(format!(
            "No such peer: {name}"
        ))),
    }
}

/// Upgrade `peers` to the peers it refers to. Fails with the error that a call gets while the bus
/// shuts down.
fn upgrade(peers: &WeakPeers) -> zbus::fdo::Result<Peers> {
    peers
        .upgrade()
        // Can it happen in any other situation than the bus shutting down?
        .ok_or_else(|| zbus::fdo::Error::Failed("Bus shutting down.".to_string()))
}

/// The description of the error for a peer that is not privileged, worded (grammar included) as
/// dbus-broker words it.
const NOT_PRIVILEGED: &str =
    "The caller does not have the necessary privileged to call this method";

/// Helper for getting the peer name from a message header.
fn msg_sender<'h>(hdr: &'h message::Header<'h>) -> &'h UniqueName<'h> {
    // SAFETY: The bus (that's us!) is supposed to ensure a valid sender on the message.
    hdr.sender().expect("Missing `sender` header")
}
