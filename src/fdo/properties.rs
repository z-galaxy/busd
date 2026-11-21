//! The standard `org.freedesktop.DBus.Properties` interface, for the properties of the interfaces
//! of the bus, of which only `org.freedesktop.DBus` has any.

use std::collections::HashMap;

use zbus::{
    fdo::{Error, Result},
    message::Header,
    names::{InterfaceName, MemberName},
    Message, Variant,
};

use super::{args, dbus::INTERFACES, unknown_method, Answer, Object, BUS_NAME};

/// Answer the call of `member` of the `org.freedesktop.DBus.Properties` interface, `call`, whose
/// header is `header`, to `object`.
pub(super) fn answer(
    call: &Message,
    header: &Header<'_>,
    member: &MemberName<'_>,
    object: Object,
) -> Result<Answer> {
    let body = call.body();
    match member.as_str() {
        "Get" => {
            let (interface, property): (InterfaceName<'_>, &str) = args(&body)?;
            ensure_has(object, &interface)?;
            let value = PROPERTIES
                .into_iter()
                .find(|name| interface == BUS_NAME && *name == property)
                .map(value_of)
                .ok_or_else(|| unknown_property(property))?;

            Answer::reply(header, &value)
        }
        "GetAll" => {
            let interface: InterfaceName<'_> = args(&body)?;
            ensure_has(object, &interface)?;
            let properties: HashMap<_, _> = PROPERTIES
                .into_iter()
                .filter(|_| interface == BUS_NAME)
                .map(|name| (name, value_of(name)))
                .collect();

            Answer::reply(header, &properties)
        }
        // None of the properties can be set.
        "Set" => {
            let (interface, property, _): (InterfaceName<'_>, &str, Variant<'_>) = args(&body)?;
            ensure_has(object, &interface)?;

            Err(unknown_property(property))
        }
        _ => Err(unknown_method(member)),
    }
}

/// The properties of the `org.freedesktop.DBus` interface.
///
/// `Features` lists the abstract features of the bus, which clients can tell its capabilities by,
/// none of which busd has. `Interfaces` lists the interfaces of the object of the bus, which
/// clients can tell its capabilities by without parsing its introspection XML, since version 1.11.x
/// of the reference implementation of the bus. It leaves the interfaces that every bus has out, as
/// the specification says: `org.freedesktop.DBus` and `org.freedesktop.DBus.Properties`, as a call
/// to the latter that asks for the properties of the former tells of them, and
/// `org.freedesktop.DBus.Peer` and `org.freedesktop.DBus.Introspectable`, which tell of no feature
/// of the bus.
const PROPERTIES: [&str; 2] = ["Features", "Interfaces"];

/// The value of `property`, one of [`PROPERTIES`].
fn value_of(property: &str) -> Variant<'static> {
    let values: Vec<&str> = match property {
        "Interfaces" => INTERFACES.to_vec(),
        _ => Vec::new(),
    };

    Variant::from(values)
}

/// Fails unless `object` has `interface`.
fn ensure_has(object: Object, interface: &InterfaceName<'_>) -> Result<()> {
    if !object.has(interface) {
        return Err(Error::UnknownInterface(format!(
            "Unknown interface '{interface}'"
        )));
    }

    Ok(())
}

/// The error of a call about `property`, which the interface that the call names doesn't have.
fn unknown_property(property: &str) -> Error {
    Error::UnknownProperty(format!("Unknown property '{property}'"))
}
