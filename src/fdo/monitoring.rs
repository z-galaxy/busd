//! The `org.freedesktop.DBus.Monitoring` interface.

use zbus::{
    fdo::{Error, Result},
    message::Header,
    names::MemberName,
    Message,
};

use super::{args, ensure_privileged, failed, name_change_signals, unknown_method, Answer};
use crate::{debug, match_rules::MatchRules, name_registry::NameOwnerChanged, peers::Peers};

/// Answer the call of `member` of the `org.freedesktop.DBus.Monitoring` interface, `call`, whose
/// header is `header`.
pub(super) async fn answer(
    peers: &Peers,
    call: &Message,
    header: &Header<'_>,
    member: &MemberName<'_>,
) -> Result<Answer> {
    match member.as_str() {
        "BecomeMonitor" => {
            let body = call.body();
            let (match_rules, _flags): (MatchRules, u32) = args(&body)?;

            become_monitor(peers, header, match_rules).await
        }
        _ => Err(unknown_method(member)),
    }
}

/// Answer `BecomeMonitor` of the peer that sent the call of `header`, which has the peer become a
/// monitor of the messages that `match_rules` match, every message if there are none.
///
/// The reply comes before the signals of the names that the peer lets go of, as it is a peer no
/// more, its unique name last: some clients, such as `busctl monitor`, get confused otherwise.
async fn become_monitor(
    peers: &Peers,
    header: &Header<'_>,
    match_rules: MatchRules,
) -> Result<Answer> {
    let Some(owner) = header.sender().map(|sender| sender.to_owned()) else {
        return Err(Error::Failed("The call has no sender".to_string()));
    };
    ensure_privileged(peers, &owner).await?;
    if !peers.make_monitor(&owner, match_rules).await {
        return Err(Error::NameHasNoOwner(format!("No such peer: {owner}")));
    }
    debug!("{} became a monitor", owner);
    let reply = super::reply(header, &())?;
    // The names pass on to those that queue for them, as when their owner goes.
    let mut changes = peers
        .routing_mut()
        .await
        .names_mut()
        .release_all(owner.clone());
    changes.push(NameOwnerChanged::vanished(owner.into()));

    Ok(Answer {
        before: Vec::new(),
        reply,
        after: name_change_signals(&changes).map_err(failed)?,
    })
}
