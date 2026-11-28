//! The standard `org.freedesktop.DBus.Properties` interface, for the properties of the interfaces
//! of the bus, of which only `org.freedesktop.DBus` has any.

use std::collections::HashMap;

use zbus::{
    fdo::{Error, Result},
    message::{Body, Header},
    names::{InterfaceName, MemberName},
    Message, Variant,
};

use super::{args, dbus::INTERFACES, unknown_method, Answer, Driver, Object, BUS_NAME};

impl Driver {
    /// Answer the call of `member` of the `org.freedesktop.DBus.Properties` interface, `call`,
    /// whose header is `header`, to `object`.
    pub(super) fn answer_properties(
        &self,
        call: &Message,
        header: &Header<'_>,
        member: &MemberName<'_>,
        object: Object,
    ) -> Result<Answer> {
        let body = call.body();
        match member.as_str() {
            "Get" => {
                let (interface, property): (InterfaceName<'_>, &str) = args(&body)?;
                object.ensure_has(&interface)?;
                let value = PROPERTIES
                    .into_iter()
                    .find(|name| interface == BUS_NAME && *name == property)
                    .map(value_of)
                    .ok_or_else(|| unknown_property(property))?;

                Answer::reply(header, &value)
            }
            "GetAll" => {
                let interface: InterfaceName<'_> = args(&body)?;
                object.ensure_has(&interface)?;
                let properties: HashMap<_, _> = PROPERTIES
                    .into_iter()
                    .filter(|_| interface == BUS_NAME)
                    .map(|name| (name, value_of(name)))
                    .collect();

                Answer::reply(header, &properties)
            }
            // None of the properties can be set.
            "Set" => {
                let (interface, property) = set_args(&body)?;
                object.ensure_has(&interface)?;

                Err(unknown_property(property))
            }
            _ => Err(unknown_method(member)),
        }
    }
}

/// The properties of the `org.freedesktop.DBus` interface.
///
/// `Features` lists the abstract features of the bus, by which clients can tell its capabilities.
/// busd has none of them. `Interfaces` lists the interfaces of the object of the bus, by which
/// clients can tell its capabilities without parsing its introspection XML. The reference
/// implementation of the bus has had `Interfaces` since version 1.11.x.
///
/// `Interfaces` leaves out the interfaces that every bus has, as the specification says. It leaves
/// out `org.freedesktop.DBus` and `org.freedesktop.DBus.Properties`, since a call to
/// `org.freedesktop.DBus.Properties` that asks for the properties of `org.freedesktop.DBus` tells
/// of both. It also leaves out `org.freedesktop.DBus.Peer` and
/// `org.freedesktop.DBus.Introspectable`, which tell of no feature of the bus.
const PROPERTIES: [&str; 2] = ["Features", "Interfaces"];

/// The value of `property`, one of [`PROPERTIES`].
fn value_of(property: &str) -> Variant<'static> {
    let values: Vec<&str> = match property {
        "Interfaces" => INTERFACES.to_vec(),
        _ => Vec::new(),
    };

    Variant::from(values)
}

/// The interface and the property of the call of `Set` whose body is `body`, or the bus's error for
/// a call with other arguments, as [`args`] gives it.
///
/// The value is left unread, as none of the properties can be set: deserialized as a
/// [`Variant`](enum@Variant), an array in it would take 64 bytes for each of its elements on 64-bit
/// targets, and a client could so have the bus take 64 times the memory of the arrays of bytes that
/// it sends (z-galaxy/zbus#2175).
fn set_args(body: &Body) -> Result<(InterfaceName<'_>, &str)> {
    if body.signature() != "ssv" {
        let error = zbus::Error::SignatureMismatch(body.signature().clone(), "`(ssv)`".into());

        return Err(Error::InvalidArgs(error.to_string()));
    }

    body.deserialize_ignore_signature()
        .map_err(|e| Error::InvalidArgs(e.to_string()))
}

impl Object {
    /// Fails unless the object has `interface`.
    fn ensure_has(self, interface: &InterfaceName<'_>) -> Result<()> {
        if !self.has(interface) {
            return Err(Error::UnknownInterface(format!(
                "Unknown interface '{interface}'"
            )));
        }

        Ok(())
    }
}

/// The error of a call about `property`, which the interface that the call names doesn't have.
fn unknown_property(property: &str) -> Error {
    Error::UnknownProperty(format!("Unknown property '{property}'"))
}
