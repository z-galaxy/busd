//! The standard `org.freedesktop.DBus.Peer` interface, which answers on any path.

use zbus::{fdo::Result, message::Header, names::MemberName};

use super::{unknown_method, Answer, Driver};

impl Driver {
    /// Answer the call of `member` of the `org.freedesktop.DBus.Peer` interface, whose header is
    /// `header`, whatever its path.
    pub(super) fn answer_peer(
        &self,
        header: &Header<'_>,
        member: &MemberName<'_>,
    ) -> Result<Answer> {
        match member.as_str() {
            "Ping" => Answer::reply(header, &()),
            "GetMachineId" => Answer::reply(header, &zbus::fdo::machine_id()?),
            _ => Err(unknown_method(member)),
        }
    }
}
