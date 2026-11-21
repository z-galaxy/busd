use std::{collections::HashMap, sync::mpsc, time::Duration};

use anyhow::{ensure, Context};
use futures_util::StreamExt;
use ntest::timeout;
use serde::Deserialize;
use zbus::{connection, Connection, MessageStream, OwnedVariant};
use zruntime::{unblock, SharedRuntime};

use super::*;
use crate::{
    bus::Bus,
    policy::Policy,
    testing::{connect, unix_address, with, with_bus},
};

/// The introspection of each object of the bus lists the interfaces that the bus serves on it, and
/// the object below it, down to the object of the bus, which has the interfaces of the bus too.
///
/// The bus answers each method that the introspection lists, and has each property that it lists,
/// of the type that it says.
#[test]
#[timeout(30000)]
fn introspection_lists_what_the_bus_answers() {
    zruntime::block_on(with_bus(|address| async move {
        let conn = connect(&address).await?;
        let standard = [INTROSPECTABLE, PEER, PROPERTIES];
        let mut node = None;
        for (path, child) in [
            ("/", Some("org")),
            ("/org", Some("freedesktop")),
            ("/org/freedesktop", Some("DBus")),
            (PATH, None),
        ] {
            let reply = call(&conn, path, INTROSPECTABLE, "Introspect", &()).await;
            let xml: String = reply.body().deserialize()?;
            let introspected: Node = quick_xml::de::from_str(&xml)?;
            let interfaces: Vec<_> = introspected
                .interfaces
                .iter()
                .map(|interface| interface.name.as_str())
                .collect();
            let children: Vec<_> = introspected
                .nodes
                .iter()
                .map(|node| node.name.as_deref())
                .collect();
            if child.is_some() {
                ensure!(interfaces == standard, "`{path}` has {interfaces:?}");
            }
            let expected: Vec<_> = child.into_iter().map(Some).collect();
            ensure!(children == expected, "`{path}` has {children:?} below it");
            node = Some(introspected);
        }
        let node = node.context("no object introspected")?;
        let interfaces: Vec<_> = node
            .interfaces
            .iter()
            .map(|interface| interface.name.as_str())
            .collect();
        ensure!(
            interfaces
                == [
                    BUS_NAME,
                    STATS,
                    INTROSPECTABLE,
                    MONITORING,
                    PEER,
                    PROPERTIES
                ],
            "the bus has {interfaces:?}"
        );

        // A call with no arguments gets an error at most for its missing arguments, if the method
        // takes any, or because the method fails at what it does.
        for interface in &node.interfaces {
            for method in &interface.methods {
                let reply = call(&conn, PATH, &interface.name, &method.name, &()).await;
                let error = error_name(&reply);
                ensure!(
                    !error.as_deref().is_some_and(|name| [
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "org.freedesktop.DBus.Error.UnknownInterface",
                        "org.freedesktop.DBus.Error.UnknownObject",
                    ]
                    .contains(&name)),
                    "`{}.{}` is unknown: {reply:?}",
                    interface.name,
                    method.name
                );
            }
            let reply = call(&conn, PATH, &interface.name, "NoSuchMethod", &()).await;
            ensure!(
                error_name(&reply).as_deref() == Some("org.freedesktop.DBus.Error.UnknownMethod"),
                "a method that `{}` doesn't list got {reply:?}",
                interface.name
            );

            let reply = call(&conn, PATH, PROPERTIES, "GetAll", &interface.name).await;
            let all: HashMap<String, OwnedVariant> = reply.body().deserialize()?;
            let mut names: Vec<_> = all.keys().map(String::as_str).collect();
            names.sort_unstable();
            let mut listed: Vec<_> = interface
                .properties
                .iter()
                .map(|property| property.name.as_str())
                .collect();
            listed.sort_unstable();
            ensure!(
                names == listed,
                "`{}` has the properties {names:?}, and lists {listed:?}",
                interface.name
            );
            for property in &interface.properties {
                let value = &all[&property.name];
                ensure!(
                    value.value_signature().to_string() == property.ty,
                    "`{}` is of the type `{}`, not `{}`",
                    property.name,
                    value.value_signature(),
                    property.ty
                );
            }
        }

        Ok(())
    }));
}

/// The bus has the two properties of the `org.freedesktop.DBus` interface, which can't be set, and
/// no others. The interfaces that it doesn't serve on an object have none there.
#[test]
#[timeout(30000)]
fn properties_of_the_bus() {
    zruntime::block_on(with_bus(|address| async move {
        let conn = connect(&address).await?;

        let reply = call(&conn, PATH, PROPERTIES, "Get", &(BUS_NAME, "Features")).await;
        let features: Vec<String> = reply.body().deserialize::<OwnedVariant>()?.try_into()?;
        ensure!(features.is_empty(), "the bus has the features {features:?}");
        let reply = call(&conn, PATH, PROPERTIES, "Get", &(BUS_NAME, "Interfaces")).await;
        let interfaces: Vec<String> = reply.body().deserialize::<OwnedVariant>()?.try_into()?;
        ensure!(
            interfaces == [MONITORING, STATS],
            "the bus lists {interfaces:?}"
        );

        let reply = call(&conn, PATH, PROPERTIES, "GetAll", &BUS_NAME).await;
        let all: HashMap<String, OwnedVariant> = reply.body().deserialize()?;
        let mut names: Vec<_> = all.keys().map(String::as_str).collect();
        names.sort_unstable();
        ensure!(names == ["Features", "Interfaces"], "the bus has {names:?}");
        let reply = call(&conn, PATH, PROPERTIES, "GetAll", &MONITORING).await;
        let all: HashMap<String, OwnedVariant> = reply.body().deserialize()?;
        ensure!(all.is_empty(), "`{MONITORING}` has {all:?}");

        let empty = zbus::Variant::from(Vec::<&str>::new());
        let get = |path, body: &(&str, &str)| call_to(path, PROPERTIES, "Get").build(body);
        let set =
            |body: &(&str, &str, &zbus::Variant<'_>)| call_to(PATH, PROPERTIES, "Set").build(body);
        for (call, error) in [
            (get(PATH, &(BUS_NAME, "NoSuch"))?, "UnknownProperty"),
            (get(PATH, &(MONITORING, "Features"))?, "UnknownProperty"),
            (
                get(PATH, &("org.busd.NoSuch", "Features"))?,
                "UnknownInterface",
            ),
            (get("/", &(BUS_NAME, "Features"))?, "UnknownInterface"),
            (
                call_to("/", PROPERTIES, "GetAll").build(&BUS_NAME)?,
                "UnknownInterface",
            ),
            (
                call_to(PATH, PROPERTIES, "Get").build(&7u32)?,
                "InvalidArgs",
            ),
            (set(&(BUS_NAME, "Features", &empty))?, "UnknownProperty"),
            (
                set(&("org.busd.NoSuch", "Features", &empty))?,
                "UnknownInterface",
            ),
        ] {
            let reply = answer(&conn, &call).await;
            let expected = format!("org.freedesktop.DBus.Error.{error}");
            ensure!(
                error_name(&reply).as_deref() == Some(expected.as_str()),
                "{call:?} got {reply:?}"
            );
        }

        Ok(())
    }));
}

/// `org.freedesktop.DBus.Peer` answers on any path: a ping, and the machine's ID as zbus finds it.
///
/// On a machine that has no ID, the bus answers with the error that zbus gives. macOS always has
/// one, so there the bus must answer with 32 hexadecimal digits.
#[test]
#[timeout(30000)]
fn peer_answers_on_any_path() {
    zruntime::block_on(with_bus(|address| async move {
        let conn = connect(&address).await?;
        let expected = zbus::fdo::machine_id().map_err(|e| e.name().to_string());

        for path in [PATH, "/", "/org/busd/NoSuch"] {
            let reply = call(&conn, path, PEER, "Ping", &()).await;
            ensure!(
                error_name(&reply).is_none(),
                "a ping on `{path}` got {reply:?}"
            );
            let reply = call(&conn, path, PEER, "GetMachineId", &()).await;
            let id = match error_name(&reply) {
                Some(error) => Err(error),
                None => Ok(reply.body().deserialize::<String>()?),
            };
            if cfg!(target_os = "macos") {
                let is_id = id
                    .as_ref()
                    .is_ok_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
                ensure!(is_id, "the ID of the machine on `{path}` is {id:?}");
            } else {
                ensure!(
                    id == expected,
                    "the ID of the machine on `{path}` is {id:?}, where zbus finds {expected:?}"
                );
            }
        }

        Ok(())
    }));
}

/// The bus answers `Hello` before it sends `NameAcquired` for the peer's unique name, so that the
/// peer has its name by then.
///
/// It sends `NameAcquired` for a name that a peer asks for before the reply to `RequestName`. Both
/// orders are those of dbus-broker.
#[test]
#[timeout(30000)]
fn order_of_the_replies_and_the_signals_of_names() {
    zruntime::block_on(with_bus(|address| async move {
        // A connection that says `Hello` itself.
        let conn = connection::Builder::address(address.as_str())
            .p2p()
            .build()
            .await?;
        let mut messages = MessageStream::from(&conn);
        let hello = Message::method_call(PATH, "Hello")
            .destination(BUS_NAME)
            .interface(BUS_NAME)
            .build(&())?;
        conn.send(&hello).await?;
        let reply = messages.next().await.context("the stream ended")??;
        ensure!(
            reply.header().reply_serial() == Some(hello.primary_header().serial_num()),
            "the bus sent {reply:?} first"
        );
        let unique_name: String = reply.body().deserialize()?;
        let acquired = messages.next().await.context("the stream ended")??;
        ensure!(
            acquired
                .header()
                .member()
                .is_some_and(|member| member == "NameAcquired")
                && acquired.body().deserialize::<String>()? == unique_name,
            "the bus sent {acquired:?} after the reply"
        );

        let request = Message::method_call(PATH, "RequestName")
            .destination(BUS_NAME)
            .interface(BUS_NAME)
            .build(&("org.busd.Ordered", 0u32))?;
        conn.send(&request).await?;
        let acquired = messages.next().await.context("the stream ended")??;
        ensure!(
            acquired
                .header()
                .member()
                .is_some_and(|member| member == "NameAcquired")
                && acquired.body().deserialize::<&str>()? == "org.busd.Ordered",
            "the bus sent {acquired:?} first"
        );
        let reply = messages.next().await.context("the stream ended")??;
        ensure!(
            reply.header().reply_serial() == Some(request.primary_header().serial_num()),
            "the bus sent {reply:?} after `NameAcquired`"
        );

        Ok(())
    }));
}

/// A reload of the configuration holds back the calls to the bus that come after it, since the bus
/// answers its calls one at a time. While the bus reads the configuration on a separate thread, the
/// later calls wait, and the bus answers them in order once the reload is done.
#[test]
#[timeout(30000)]
fn reload_holds_later_calls_back() {
    zruntime::block_on(async {
        let (reload, blocked) = BlockedReload::new();
        let mut bus = Bus::for_address(Some(&unix_address())).await.unwrap();
        bus.set_reload(reload).await;

        with(bus, |bus| async move {
            let conn = connect(bus.address()).await?;
            let mut messages = MessageStream::from(&conn);
            let calls = [
                call_of(BUS_NAME, "ReloadConfig"),
                call_of(BUS_NAME, "GetId"),
            ];
            for call in &calls {
                conn.send(call).await?;
            }
            let runtime = SharedRuntime::current()?;
            let early = runtime.timeout(QUIET, next_reply(&mut messages)).await;
            ensure!(
                early.is_err(),
                "the bus answered during the reload: {early:?}"
            );

            blocked.started().await.let_go().await;
            for call in &calls {
                let reply = next_reply(&mut messages).await?;
                ensure!(
                    reply.header().reply_serial() == Some(call.primary_header().serial_num()),
                    "the bus sent {reply:?} instead of the reply to {call:?}"
                );
                ensure!(error_name(&reply).is_none(), "{call:?} got {reply:?}");
            }

            Ok(())
        })
        .await;
    });
}

/// A call that expects no reply gets none, even when the bus has an error for it. The bus still
/// does what the call asks for, and sends the signals that the call makes it send.
#[test]
#[timeout(30000)]
fn calls_that_expect_no_reply_get_none() {
    zruntime::block_on(with_bus(|address| async move {
        let conn = connect(&address).await?;
        let mut messages = MessageStream::from(&conn);
        let quiet =
            |path, member| call_to(path, BUS_NAME, member).with_flags(Flags::NO_REPLY_EXPECTED);
        let calls = [
            quiet("/org/busd/NoSuch", "GetId").build(&())?,
            quiet(PATH, "RequestName").build(&("org.busd.Quiet", 0u32))?,
            quiet(PATH, "NoSuchMethod").build(&())?,
        ];
        for call in &calls {
            conn.send(call).await?;
        }
        let last = call_of(BUS_NAME, "GetId");
        conn.send(&last).await?;
        let mut acquired = false;
        loop {
            let msg = messages.next().await.context("the stream ended")??;
            let header = msg.header();
            if msg.message_type() == message::Type::Signal {
                acquired |= header
                    .member()
                    .is_some_and(|member| member == "NameAcquired")
                    && msg.body().deserialize::<&str>()? == "org.busd.Quiet";

                continue;
            }
            ensure!(
                header.reply_serial() == Some(last.primary_header().serial_num()),
                "a call that expects no reply got {msg:?}"
            );
            break;
        }
        ensure!(
            acquired,
            "the bus didn't give the name of a call without reply"
        );

        Ok(())
    }));
}

/// A call to an object, an interface or a method that the bus doesn't have gets the error that says
/// which. A call without an interface gets `Failed`, and a call with the wrong arguments gets
/// `InvalidArgs`. Each error comes from the bus.
#[test]
#[timeout(30000)]
fn calls_of_what_the_bus_has_not() {
    zruntime::block_on(with_bus(|address| async move {
        let conn = connect(&address).await?;
        for (path, interface, member, error) in [
            ("/org/busd/NoSuch", Some(BUS_NAME), "GetId", "UnknownObject"),
            (
                "/org/freedesktop/DBus/NoSuch",
                Some(INTROSPECTABLE),
                "Introspect",
                "UnknownObject",
            ),
            ("/", Some(BUS_NAME), "GetId", "UnknownInterface"),
            (
                "/org",
                Some(MONITORING),
                "BecomeMonitor",
                "UnknownInterface",
            ),
            (PATH, Some("org.busd.NoSuch"), "GetId", "UnknownInterface"),
            (PATH, Some(BUS_NAME), "NoSuchMethod", "UnknownMethod"),
            (PATH, None, "GetId", "Failed"),
            (PATH, Some(BUS_NAME), "RequestName", "InvalidArgs"),
        ] {
            let builder = Message::method_call(path, member).destination(BUS_NAME);
            let builder = match interface {
                Some(interface) => builder.interface(interface),
                None => builder,
            };
            let reply = answer(&conn, &builder.build(&())?).await;
            let expected = format!("org.freedesktop.DBus.Error.{error}");
            ensure!(
                error_name(&reply).as_deref() == Some(expected.as_str()),
                "`{member}` of {interface:?} on `{path}` got {reply:?}"
            );
            ensure!(
                reply
                    .header()
                    .sender()
                    .is_some_and(|sender| sender == BUS_NAME),
                "the error comes from {:?}",
                reply.header().sender()
            );
        }

        Ok(())
    }));
}

/// A node of the introspection XML: an object, its interfaces, and the nodes below it.
#[derive(Debug, Deserialize)]
struct Node {
    #[serde(rename = "@name", default)]
    name: Option<String>,
    #[serde(rename = "interface", default)]
    interfaces: Vec<Interface>,
    #[serde(rename = "node", default)]
    nodes: Vec<Node>,
}

/// An interface of the introspection XML, with its methods and properties.
#[derive(Debug, Deserialize)]
struct Interface {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "method", default)]
    methods: Vec<Method>,
    #[serde(rename = "property", default)]
    properties: Vec<Property>,
}

#[derive(Debug, Deserialize)]
struct Method {
    #[serde(rename = "@name")]
    name: String,
}

#[derive(Debug, Deserialize)]
struct Property {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@type")]
    ty: String,
}

/// The reload of [`BlockedReload::new`], yet to start.
pub(crate) struct BlockedReload {
    start: mpsc::Receiver<()>,
    let_go: mpsc::SyncSender<()>,
}

impl BlockedReload {
    /// A reload of the configuration that waits on its thread until it is let go of, through the
    /// returned [`BlockedReload`], and then reads the open policy.
    pub(crate) fn new() -> (impl FnMut() -> anyhow::Result<Policy> + Send, Self) {
        let (started, start) = mpsc::sync_channel(1);
        let (let_go, waiting) = mpsc::sync_channel(0);
        let reload = move || {
            started.send(()).unwrap();
            waiting.recv().unwrap();

            Ok(Policy::default())
        };

        (reload, Self { start, let_go })
    }

    /// Wait for the reload to start.
    pub(crate) async fn started(self) -> StartedReload {
        let Self { start, let_go } = self;
        unblock(move || start.recv().unwrap()).await;

        StartedReload(let_go)
    }
}

/// The reload of [`BlockedReload::new`], which waits to be let go of.
pub(crate) struct StartedReload(mpsc::SyncSender<()>);

impl StartedReload {
    /// Let the reload go on.
    pub(crate) async fn let_go(self) {
        let Self(let_go) = self;
        unblock(move || let_go.send(()).unwrap()).await;
    }
}

/// A call to the bus of `member` of `interface` on its object, without arguments.
fn call_of(interface: &str, member: &str) -> Message {
    call_to(PATH, interface, member).build(&()).unwrap()
}

/// The answer of the bus to the call of `member` of `interface` on `path` with `body`, which
/// `conn` makes: its reply, or its error.
async fn call<B>(conn: &Connection, path: &str, interface: &str, member: &str, body: &B) -> Message
where
    B: Serialize + DynamicType,
{
    answer(conn, &call_to(path, interface, member).build(body).unwrap()).await
}

/// The builder of a call to the bus of `member` of `interface` on `path`.
fn call_to<'a>(path: &'a str, interface: &'a str, member: &'a str) -> message::Builder<'a> {
    Message::method_call(path, member)
        .destination(BUS_NAME)
        .interface(interface)
}

/// The answer of the bus to `call`, which `conn` makes: its reply, or its error.
async fn answer(conn: &Connection, call: &Message) -> Message {
    let mut messages = MessageStream::from(conn);
    conn.send(call).await.unwrap();
    let serial = call.primary_header().serial_num();
    loop {
        let msg = messages.next().await.unwrap().unwrap();
        if msg.header().reply_serial() == Some(serial) {
            return msg;
        }
    }
}

/// The next reply or error that comes out of `messages`, skipping the signals.
async fn next_reply(messages: &mut MessageStream) -> anyhow::Result<Message> {
    loop {
        let msg = messages.next().await.context("the stream ended")??;
        if msg.message_type() != message::Type::Signal {
            return Ok(msg);
        }
    }
}

/// The name of the error that `answer` is, if it is one.
fn error_name(answer: &Message) -> Option<String> {
    answer.header().error_name().map(|name| name.to_string())
}

/// How long a test waits to see that nothing comes.
const QUIET: Duration = Duration::from_millis(200);
