//! The `org.freedesktop.DBus.Monitoring` interface.

use zbus::{
    fdo::{Error, Result},
    message::Header,
    names::MemberName,
    Message,
};

use super::{args, failed, name_change_signals, unknown_method, Answer, Driver};
use crate::{debug, match_rules::MatchRules, name_registry::NameOwnerChanged, peers::Peers};

impl Driver {
    /// Answer the call of `member` of the `org.freedesktop.DBus.Monitoring` interface, `call`,
    /// whose header is `header`.
    pub(super) async fn answer_monitoring(
        &self,
        peers: &Peers,
        call: &Message,
        header: &Header<'_>,
        member: &MemberName<'_>,
    ) -> Result<Answer> {
        match member.as_str() {
            "BecomeMonitor" => {
                let body = call.body();
                let (match_rules, _flags): (MatchRules, u32) = args(&body)?;

                peers.become_monitor(header, match_rules).await
            }
            _ => Err(unknown_method(member)),
        }
    }
}

impl Peers {
    /// Answer `BecomeMonitor` of the peer that sent the call of `header`.
    ///
    /// The call makes the peer a monitor of the messages that `match_rules` match, or of every
    /// message if there are none. Since it is no longer a peer, it lets go of its names, and of its
    /// unique name last. The reply comes before the signals of those names, because some clients,
    /// such as `busctl monitor`, get confused otherwise.
    async fn become_monitor(&self, header: &Header<'_>, match_rules: MatchRules) -> Result<Answer> {
        let Some(owner) = header.sender().map(|sender| sender.to_owned()) else {
            return Err(Error::Failed("The call has no sender".to_string()));
        };
        self.ensure_privileged(&owner).await?;
        if !self.make_monitor(&owner, match_rules).await {
            return Err(Error::NameHasNoOwner(format!("No such peer: {owner}")));
        }
        debug!("{} became a monitor", owner);
        let reply = super::reply(header, &())?;
        // The names pass on to the peers that queue for them, as when their owner goes.
        let mut changes = self
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
}
