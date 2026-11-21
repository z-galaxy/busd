//! The standard `org.freedesktop.DBus.Introspectable` interface, which describes the objects of
//! the bus, each with the objects below it, as XML.

use std::fmt::Write;

use zbus::{fdo::Result, message::Header, names::MemberName};

use super::{
    unknown_method, Answer, Object, BUS_NAME, INTROSPECTABLE, MONITORING, PEER, PROPERTIES, STATS,
};

/// Answer the call of `member` of the `org.freedesktop.DBus.Introspectable` interface, whose
/// header is `header`, to `object`.
pub(super) fn answer(
    header: &Header<'_>,
    member: &MemberName<'_>,
    object: Object,
) -> Result<Answer> {
    match member.as_str() {
        "Introspect" => Answer::reply(header, &introspect(object)),
        _ => Err(unknown_method(member)),
    }
}

/// The introspection XML of `object`: its interfaces, and the objects below it, with theirs.
fn introspect(object: Object) -> String {
    let mut xml = String::from(DOCTYPE);
    xml.push_str("<node>\n");
    write_node(&mut xml, object, INDENT);
    xml.push_str("</node>\n");

    xml
}

/// Write what the node of `object` holds to `xml`, indented by `level` spaces: the interfaces of
/// `object`, in the order of their names, then the node of the object below it, if any.
fn write_node(xml: &mut String, object: Object, level: usize) {
    let interfaces = INTERFACES
        .iter()
        .filter(|(name, _)| object.has(name))
        .map(|(_, interface)| interface);
    for interface in interfaces {
        for line in interface.lines() {
            // Writing to a `String` doesn't fail.
            let _ = writeln!(xml, "{:level$}{line}", "");
        }
    }
    if let Some((name, child)) = object.child() {
        let _ = writeln!(xml, "{:level$}<node name=\"{name}\">", "");
        write_node(xml, child, level + INDENT);
        let _ = writeln!(xml, "{:level$}</node>", "");
    }
}

/// The interfaces that the objects of the bus have, each with its XML, in the order of their
/// names.
const INTERFACES: [(&str, &str); 6] = [
    (BUS_NAME, DBUS_XML),
    (STATS, STATS_XML),
    (INTROSPECTABLE, INTROSPECTABLE_XML),
    (MONITORING, MONITORING_XML),
    (PEER, PEER_XML),
    (PROPERTIES, PROPERTIES_XML),
];

/// How many spaces a node indents what it holds by.
const INDENT: usize = 2;

/// What the introspection XML starts with: the type of the document.
const DOCTYPE: &str = r#"
<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
"#;

/// The `org.freedesktop.DBus` interface.
const DBUS_XML: &str = r#"<interface name="org.freedesktop.DBus">
  <method name="Hello">
    <arg type="s" direction="out"/>
  </method>
  <method name="RequestName">
    <arg name="name" type="s" direction="in"/>
    <arg name="flags" type="u" direction="in"/>
    <arg type="u" direction="out"/>
  </method>
  <method name="ReleaseName">
    <arg name="name" type="s" direction="in"/>
    <arg type="u" direction="out"/>
  </method>
  <method name="GetNameOwner">
    <arg name="name" type="s" direction="in"/>
    <arg type="s" direction="out"/>
  </method>
  <method name="AddMatch">
    <arg name="rule" type="s" direction="in"/>
  </method>
  <method name="RemoveMatch">
    <arg name="rule" type="s" direction="in"/>
  </method>
  <method name="GetAdtAuditSessionData">
    <arg name="_bus_name" type="s" direction="in"/>
    <arg type="ay" direction="out"/>
  </method>
  <method name="GetConnectionCredentials">
    <arg name="bus_name" type="s" direction="in"/>
    <arg type="a{sv}" direction="out"/>
  </method>
  <method name="GetConnectionSELinuxSecurityContext">
    <arg name="bus_name" type="s" direction="in"/>
    <arg type="ay" direction="out"/>
  </method>
  <method name="GetConnectionUnixProcessID">
    <arg name="bus_name" type="s" direction="in"/>
    <arg type="u" direction="out"/>
  </method>
  <method name="GetConnectionUnixUser">
    <arg name="bus_name" type="s" direction="in"/>
    <arg type="u" direction="out"/>
  </method>
  <method name="GetId">
    <arg type="s" direction="out"/>
  </method>
  <method name="ListActivatableNames">
    <arg type="as" direction="out"/>
  </method>
  <method name="ListNames">
    <arg type="as" direction="out"/>
  </method>
  <method name="ListQueuedOwners">
    <arg name="name" type="s" direction="in"/>
    <arg type="as" direction="out"/>
  </method>
  <method name="NameHasOwner">
    <arg name="name" type="s" direction="in"/>
    <arg type="b" direction="out"/>
  </method>
  <method name="StartServiceByName">
    <arg name="name" type="s" direction="in"/>
    <arg name="_flags" type="u" direction="in"/>
    <arg type="u" direction="out"/>
  </method>
  <method name="UpdateActivationEnvironment">
    <arg name="_environment" type="a{ss}" direction="in"/>
  </method>
  <method name="ReloadConfig">
  </method>
  <method name="Dune">
    <arg type="s" direction="out"/>
  </method>
  <signal name="NameOwnerChanged">
    <arg name="name" type="s"/>
    <arg name="old_owner" type="s"/>
    <arg name="new_owner" type="s"/>
  </signal>
  <signal name="NameLost">
    <arg name="name" type="s"/>
  </signal>
  <signal name="NameAcquired">
    <arg name="name" type="s"/>
  </signal>
  <property name="Features" type="as" access="read"/>
  <property name="Interfaces" type="as" access="read"/>
</interface>"#;

/// The `org.freedesktop.DBus.Debug.Stats` interface.
const STATS_XML: &str = r#"<interface name="org.freedesktop.DBus.Debug.Stats">
  <method name="GetStats">
    <arg type="a{sv}" direction="out"/>
  </method>
  <method name="GetConnectionStats">
    <arg name="name" type="s" direction="in"/>
    <arg type="a{sv}" direction="out"/>
  </method>
  <method name="GetAllMatchRules">
    <arg type="a{sas}" direction="out"/>
  </method>
</interface>"#;

/// The `org.freedesktop.DBus.Introspectable` interface.
const INTROSPECTABLE_XML: &str = r#"<interface name="org.freedesktop.DBus.Introspectable">
  <method name="Introspect">
    <arg type="s" direction="out"/>
  </method>
</interface>"#;

/// The `org.freedesktop.DBus.Monitoring` interface.
const MONITORING_XML: &str = r#"<interface name="org.freedesktop.DBus.Monitoring">
  <method name="BecomeMonitor">
    <arg name="match_rules" type="as" direction="in"/>
    <arg name="_flags" type="u" direction="in"/>
  </method>
</interface>"#;

/// The `org.freedesktop.DBus.Peer` interface.
const PEER_XML: &str = r#"<interface name="org.freedesktop.DBus.Peer">
  <method name="Ping">
  </method>
  <method name="GetMachineId">
    <arg type="s" direction="out"/>
  </method>
</interface>"#;

/// The `org.freedesktop.DBus.Properties` interface.
const PROPERTIES_XML: &str = r#"<interface name="org.freedesktop.DBus.Properties">
  <method name="Get">
    <arg name="interface_name" type="s" direction="in"/>
    <arg name="property_name" type="s" direction="in"/>
    <arg type="v" direction="out"/>
  </method>
  <method name="Set">
    <arg name="interface_name" type="s" direction="in"/>
    <arg name="property_name" type="s" direction="in"/>
    <arg name="value" type="v" direction="in"/>
  </method>
  <method name="GetAll">
    <arg name="interface_name" type="s" direction="in"/>
    <arg type="a{sv}" direction="out"/>
  </method>
  <signal name="PropertiesChanged">
    <arg name="interface_name" type="s"/>
    <arg name="changed_properties" type="a{sv}"/>
    <arg name="invalidated_properties" type="as"/>
  </signal>
</interface>"#;
