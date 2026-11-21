//! The standard `org.freedesktop.DBus.Peer` interface, which answers on any path.

use std::fs;

use zbus::{
    fdo::{Error, Result},
    message::Header,
    names::MemberName,
};

use super::{unknown_method, Answer};

/// Answer the call of `member` of the `org.freedesktop.DBus.Peer` interface, whose header is
/// `header`, whatever its path.
pub(super) fn answer(header: &Header<'_>, member: &MemberName<'_>) -> Result<Answer> {
    match member.as_str() {
        "Ping" => Answer::reply(header, &()),
        "GetMachineId" => Answer::reply(header, &machine_id()?),
        _ => Err(unknown_method(member)),
    }
}

/// The ID of the machine that the bus runs on, as the reference implementation of D-Bus keeps it,
/// in `/var/lib/dbus/machine-id`, or as systemd keeps it, in `/etc/machine-id`, without the end of
/// its line.
fn machine_id() -> Result<String> {
    let mut id = match fs::read_to_string(MACHINE_ID_OF_DBUS) {
        Ok(id) => id,
        Err(e) => match fs::read_to_string(MACHINE_ID) {
            Ok(id) => id,
            Err(_) => {
                return Err(Error::IOError(format!(
                    "Failed to read from {MACHINE_ID_OF_DBUS} or {MACHINE_ID}: {e}"
                )))
            }
        },
    };
    let len = id.trim_end().len();
    id.truncate(len);

    Ok(id)
}

/// Where the reference implementation of D-Bus keeps the ID of the machine.
const MACHINE_ID_OF_DBUS: &str = "/var/lib/dbus/machine-id";

/// Where systemd keeps the ID of the machine.
const MACHINE_ID: &str = "/etc/machine-id";
