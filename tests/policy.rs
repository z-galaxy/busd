use std::{
    env::temp_dir,
    fmt::Debug,
    future::Future,
    os::unix::net::UnixStream,
    pin::pin,
    sync::{Arc, Once},
};

use anyhow::{bail, ensure, Context};
use busd::{
    bus::Bus,
    config::Config,
    peer::SEND_TIMEOUT,
    peers::Peers,
    policy::{Ids, Policy},
};
use futures_util::{
    future::{join, select, Either},
    TryStreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::{
    connection,
    fdo::{DBusProxy, RequestNameFlags, RequestNameReply, StatsProxy},
    message,
    names::BusName,
    proxy::CacheProperties,
    Connection, DBusError, MatchRule, Message, MessageStream, OwnedGuid,
};
use zruntime::Event;

/// A peer may own the names that the policy lets it own, by name or by prefix, and is refused the
/// others.
#[test]
#[instrument]
#[timeout(30000)]
fn own() {
    init_tracing();

    let policies = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <deny own="*"/>
            <allow own="org.busd.Policy.Allowed"/>
            <allow own_prefix="org.busd.Policy.Prefix"/>
        </policy>"#
    );
    zruntime::block_on(with_bus(&policies, own_));
}

/// A call that the policy keeps its caller from sending to a name gets an error, whether it goes
/// to the name or to the unique name of its owner, or goes nowhere if it expects no reply; the
/// calls that the policy lets through reach the owner.
#[test]
#[instrument]
#[timeout(30000)]
fn send_to_a_name() {
    init_tracing();

    let policies = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <allow own="*"/>
            <deny send_destination="{SERVICE}" send_interface="org.busd.Denied"/>
        </policy>"#
    );
    zruntime::block_on(with_bus(&policies, send_to_a_name_));
}

/// The policy refuses a message by its member, its path and its type, and lets the others
/// through.
#[test]
#[instrument]
#[timeout(30000)]
fn send_attributes() {
    init_tracing();

    let policies = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <deny send_member="Forbidden"/>
            <deny send_path="/org/busd/Policy/Forbidden"/>
            <deny send_type="signal"/>
        </policy>"#
    );
    zruntime::block_on(with_bus(&policies, send_attributes_));
}

/// A peer that the policy keeps from receiving a message from a sender doesn't get it: a call gets
/// an error, and a broadcast goes to the other subscribers alone.
#[test]
#[instrument]
#[timeout(30000)]
fn receive() {
    init_tracing();

    let policies = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <allow own="*"/>
        </policy>
        <policy user="{}">
            <deny receive_sender="{SERVICE}" receive_interface="org.busd.Denied"/>
        </policy>"#,
        other_uid(),
    );
    zruntime::block_on(with_bus(&policies, receive_));
}

/// The calls to the bus itself are subject to the policy too.
#[test]
#[instrument]
#[timeout(30000)]
fn calls_to_the_bus() {
    init_tracing();

    let policies = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <deny send_destination="{BUS_NAME}" send_interface="org.freedesktop.DBus.Debug.Stats"/>
        </policy>"#
    );
    zruntime::block_on(with_bus(&policies, calls_to_the_bus_));
}

/// A reply goes to the call that waits for it, though no rule lets its sender send it.
#[test]
#[instrument]
#[timeout(30000)]
fn replies_are_not_checked() {
    init_tracing();

    let policies = format!(
        r#"<policy context="default">
            <allow eavesdrop="true"/>
            <allow own="*"/>
            <deny send_type="method_call"/>
            <allow send_destination="{BUS_NAME}"/>
            <allow send_destination="{SERVICE}"/>
        </policy>"#
    );
    zruntime::block_on(with_bus(&policies, replies_are_not_checked_));
}

/// A rule of the mandatory context outranks one of a user, which outranks one of the default
/// context, wherever they stand in the configuration.
#[test]
#[instrument]
#[timeout(30000)]
fn contexts() {
    init_tracing();

    let policies = format!(
        r#"<policy context="mandatory">
            <deny own="org.busd.Policy.Mandatory"/>
        </policy>
        <policy user="{}">
            <allow own="org.busd.Policy.User"/>
            <allow own="org.busd.Policy.Mandatory"/>
        </policy>
        <policy context="default">
            {SEND_AND_RECEIVE}
            <deny own="*"/>
        </policy>"#,
        other_uid(),
    );
    zruntime::block_on(with_bus(&policies, contexts_));
}

/// A client that the policy doesn't let connect can't, and one that it lets connect can.
#[test]
#[instrument]
#[timeout(30000)]
fn connect() {
    init_tracing();

    let denied = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <deny user="*"/>
        </policy>"#
    );
    let allowed = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <deny user="*"/>
            <allow user="{}"/>
        </policy>"#,
        our_uid(),
    );
    zruntime::block_on(async {
        with_bus(&denied, |bus| async move {
            let result = connection::Builder::address(bus.address.as_str())
                .build()
                .await;
            ensure!(result.is_err(), "the client connected: {result:?}");

            Ok(())
        })
        .await;
        with_bus(&allowed, |bus| async move {
            let conn = bus.connect().await?;
            dbus_proxy(&conn).await?.get_id().await?;

            Ok(())
        })
        .await;
    });
}

/// The peers that a bus has go by the policy that it is given.
#[test]
#[instrument]
#[timeout(30000)]
fn set_policy_applies_to_connected_peers() {
    init_tracing();

    zruntime::block_on(with_bus("", set_policy_applies_to_connected_peers_));
}

/// The signals of the bus are subject to the policy of their receivers.
#[test]
#[instrument]
#[timeout(30000)]
fn bus_signals() {
    init_tracing();

    let policies = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <allow own="*"/>
        </policy>
        <policy user="{}">
            <deny receive_sender="{BUS_NAME}" receive_member="NameOwnerChanged"/>
        </policy>"#,
        other_uid(),
    );
    zruntime::block_on(with_bus(&policies, bus_signals_));
}

/// Under the stock system.conf, clients connect, say hello, get the signals of the bus and the
/// broadcasts of each other, and talk to the bus, but an ordinary user can't own names, call
/// other peers or use the statistics of the bus.
#[test]
#[instrument]
#[timeout(30000)]
fn stock_system_configuration() {
    init_tracing();

    zruntime::block_on(async {
        let config = Config::read_file("tests/data/system.conf").unwrap();
        let ids = Ids::parse("root:x:0:0:::", "");
        let policy = Policy::new(&config, &ids, our_uid());

        with_policy(policy, stock_system_configuration_).await;
    });
}

/// A system bus whose configuration has no policy lets the clients of its user connect, and
/// refuses them everything from there on, their `Hello` included.
#[test]
#[instrument]
#[timeout(30000)]
fn system_bus_without_a_policy() {
    init_tracing();

    zruntime::block_on(async {
        let config = Config::parse(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
            "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
            <busconfig><type>system</type></busconfig>"#,
        )
        .unwrap();
        let policy = Policy::new(&config, &Ids::default(), our_uid());

        with_policy(policy, system_bus_without_a_policy_).await;
    });
}

async fn own_(bus: TestBus) -> anyhow::Result<()> {
    let conn = bus.connect().await?;
    let dbus = dbus_proxy(&conn).await?;

    for name in ["org.busd.Policy.Allowed", "org.busd.Policy.Prefix.Sub"] {
        let reply = request_name(&dbus, name).await?;
        ensure!(
            reply == RequestNameReply::PrimaryOwner,
            "expected `PrimaryOwner` for {name}, got {reply:?}"
        );
    }
    for name in ["org.busd.Policy.Other", "org.busd.Policy.PrefixNot"] {
        ensure_denied(request_name(&dbus, name).await, OWN_REFUSED).context(name)?;
    }

    Ok(())
}

async fn send_to_a_name_(bus: TestBus) -> anyhow::Result<()> {
    let service = bus.connect().await?;
    request_name(&dbus_proxy(&service).await?, SERVICE).await?;
    let mut calls = MessageStream::from(&service);
    let caller = bus.connect().await?;
    let unique_name = service.unique_name().unwrap().as_str();

    for destination in [SERVICE, unique_name] {
        let result = call(&caller, destination, PATH, "org.busd.Denied", "Do").await;
        ensure_denied(result, SEND_REFUSED)
            .with_context(|| format!("a call to `{destination}`"))?;
    }
    let mut replies = MessageStream::from(&caller);
    caller
        .send(&no_reply(SERVICE, PATH, "org.busd.Denied", "Quiet")?)
        .await?;
    caller
        .send(&no_reply(SERVICE, PATH, INTERFACE, "Allowed")?)
        .await?;
    ensure_member(&mut calls, "Allowed").await?;
    // An error for the call that expects no reply would come before the reply to this one.
    dbus_proxy(&caller).await?.get_id().await?;
    let msg = loop {
        let msg = replies.try_next().await?.unwrap();
        if msg.message_type() != message::Type::Signal {
            break msg;
        }
    };
    ensure!(
        msg.message_type() == message::Type::MethodReturn,
        "the caller got {msg:?} rather than the reply to `GetId`"
    );

    Ok(())
}

async fn send_attributes_(bus: TestBus) -> anyhow::Result<()> {
    let receiver = bus.connect().await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .path(PATH)
        .build()?;
    dbus_proxy(&receiver).await?.add_match_rule(rule).await?;
    let mut msgs = MessageStream::from(&receiver);
    let sender = bus.connect().await?;
    let name = receiver.unique_name().unwrap().as_str();

    let result = call(&sender, name, PATH, INTERFACE, "Forbidden").await;
    ensure_denied(result, SEND_REFUSED).context("by member")?;
    sender
        .send(&no_reply(name, PATH, INTERFACE, "Allowed")?)
        .await?;
    ensure_member(&mut msgs, "Allowed").await?;

    let forbidden = "/org/busd/Policy/Forbidden";
    let result = call(&sender, name, forbidden, INTERFACE, "OnForbiddenPath").await;
    ensure_denied(result, SEND_REFUSED).context("by path")?;
    sender
        .send(&no_reply(name, PATH, INTERFACE, "OnAllowedPath")?)
        .await?;
    ensure_member(&mut msgs, "OnAllowedPath").await?;

    sender
        .emit_signal(None::<&str>, PATH, INTERFACE, "Signal", &())
        .await?;
    sender
        .send(&no_reply(name, PATH, INTERFACE, "Call")?)
        .await?;
    ensure_member(&mut msgs, "Call").await?;

    Ok(())
}

async fn receive_(bus: TestBus) -> anyhow::Result<()> {
    let sender = bus.connect().await?;
    request_name(&dbus_proxy(&sender).await?, SERVICE).await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .path(PATH)
        .build()?;
    // The rule of the policy is for the one subscriber, and not for the other.
    let restricted = bus.connect_as(1_000_001, other_uid()).await?;
    dbus_proxy(&restricted)
        .await?
        .add_match_rule(rule.clone())
        .await?;
    let mut restricted_msgs = MessageStream::from(&restricted);
    let other = bus.connect().await?;
    dbus_proxy(&other).await?.add_match_rule(rule).await?;
    let mut other_msgs = MessageStream::from(&other);

    let name = restricted.unique_name().unwrap().as_str();
    let result = call(&sender, name, PATH, "org.busd.Denied", "Do").await;
    ensure_denied(result, RECEIVE_REFUSED)?;
    sender
        .emit_signal(None::<&str>, PATH, "org.busd.Denied", "First", &())
        .await?;
    sender
        .emit_signal(None::<&str>, PATH, INTERFACE, "Second", &())
        .await?;
    ensure_member(&mut restricted_msgs, "Second").await?;
    ensure_member(&mut other_msgs, "First").await?;
    ensure_member(&mut other_msgs, "Second").await?;

    Ok(())
}

async fn calls_to_the_bus_(bus: TestBus) -> anyhow::Result<()> {
    let conn = bus.connect().await?;
    ensure_stats_denied(&conn).await?;
    dbus_proxy(&conn).await?.get_id().await?;

    Ok(())
}

async fn replies_are_not_checked_(bus: TestBus) -> anyhow::Result<()> {
    let service = bus.connect().await?;
    request_name(&dbus_proxy(&service).await?, SERVICE).await?;
    let mut calls = MessageStream::from(&service);
    let caller = bus.connect().await?;
    // No rule lets the service send the caller anything.
    let caller_name = caller.unique_name().unwrap().as_str();
    let result = call(&service, caller_name, PATH, INTERFACE, "Do").await;
    ensure_denied(result, SEND_REFUSED)?;

    let answering = async {
        let call = next_call(&mut calls).await?;
        let reply = Message::method_return(&call.header()).build(&"Pong")?;
        service.send(&reply).await?;

        anyhow::Ok(())
    };
    let (answered, reply) = join(answering, call(&caller, SERVICE, PATH, INTERFACE, "Ping")).await;
    answered?;
    let reply: String = reply?.body().deserialize()?;
    ensure!(
        reply == "Pong",
        "the caller got `{reply}` rather than `Pong`"
    );

    Ok(())
}

async fn contexts_(bus: TestBus) -> anyhow::Result<()> {
    let conn = bus.connect_as(1_000_001, other_uid()).await?;
    let dbus = dbus_proxy(&conn).await?;

    let reply = request_name(&dbus, "org.busd.Policy.User").await?;
    ensure!(
        reply == RequestNameReply::PrimaryOwner,
        "expected `PrimaryOwner`, got {reply:?}"
    );
    for name in ["org.busd.Policy.Default", "org.busd.Policy.Mandatory"] {
        ensure_denied(request_name(&dbus, name).await, OWN_REFUSED).context(name)?;
    }

    Ok(())
}

async fn set_policy_applies_to_connected_peers_(bus: TestBus) -> anyhow::Result<()> {
    let conn = bus.connect().await?;
    let dbus = dbus_proxy(&conn).await?;
    let reply = request_name(&dbus, "org.busd.Policy.Before").await?;
    ensure!(
        reply == RequestNameReply::PrimaryOwner,
        "expected `PrimaryOwner`, got {reply:?}"
    );

    let policies = format!(
        r#"<policy context="default">
            {SEND_AND_RECEIVE}
            <deny own="*"/>
        </policy>"#
    );
    bus.peers.set_policy(Arc::new(policy(&policies)?)).await;
    let result = request_name(&dbus, "org.busd.Policy.After").await;
    ensure_denied(result, OWN_REFUSED)?;

    Ok(())
}

async fn bus_signals_(bus: TestBus) -> anyhow::Result<()> {
    let owner_changes = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender(BUS_NAME)
        .member("NameOwnerChanged")
        .build()?;
    let ours = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    // The rule of the policy is for the one client, and not for the other.
    let restricted = bus.connect_as(1_000_001, other_uid()).await?;
    let other = bus.connect().await?;
    let mut streams = Vec::new();
    for conn in [&restricted, &other] {
        let dbus = dbus_proxy(conn).await?;
        dbus.add_match_rule(owner_changes.clone()).await?;
        dbus.add_match_rule(ours.clone()).await?;
        streams.push(MessageStream::from(conn));
    }
    let [mut restricted_msgs, mut other_msgs] = <[_; 2]>::try_from(streams).unwrap();

    // The bus signals the change of owner before it answers the call.
    let name = "org.busd.Policy.Signal";
    let client = bus.connect().await?;
    request_name(&dbus_proxy(&client).await?, name).await?;
    client
        .emit_signal(None::<&str>, PATH, INTERFACE, "Marker", &())
        .await?;
    let msg = loop {
        let msg = restricted_msgs.try_next().await?.unwrap();
        let header = msg.header();
        if header
            .member()
            .is_some_and(|member| ["NameOwnerChanged", "Marker"].contains(&member.as_str()))
        {
            break msg;
        }
    };
    let header = msg.header();
    let got = header.member().unwrap();
    ensure!(got == "Marker", "the restricted client got `{got}`");
    next_owner_change(&mut other_msgs, name).await?;
    ensure_member(&mut other_msgs, "Marker").await?;

    Ok(())
}

async fn system_bus_without_a_policy_(bus: TestBus) -> anyhow::Result<()> {
    ensure_denied(bus.connect().await, SEND_REFUSED).context("`Hello`")
}

async fn stock_system_configuration_(bus: TestBus) -> anyhow::Result<()> {
    let mut msgs = connection::Builder::address(bus.address.as_str())
        .build_message_stream()
        .await?;
    let conn = Connection::from(&msgs);
    let unique_name = conn.unique_name().context("no unique name")?.to_string();
    loop {
        let msg = msgs.try_next().await?.unwrap();
        let header = msg.header();
        if header
            .member()
            .is_some_and(|member| member == "NameAcquired")
        {
            let name: String = msg.body().deserialize()?;
            ensure!(name == unique_name, "`NameAcquired` of `{name}`");
            break;
        }
    }
    dbus_proxy(&conn).await?.get_id().await?;

    // A client that is neither of root nor of the user of the bus.
    let ordinary = bus.connect_as(1_000_001, other_uid()).await?;
    let dbus = dbus_proxy(&ordinary).await?;
    ensure_denied(
        request_name(&dbus, "org.busd.Policy.Ordinary").await,
        OWN_REFUSED,
    )?;
    let result = call(&ordinary, &unique_name, PATH, INTERFACE, "Do").await;
    ensure_denied(result, SEND_REFUSED).context("a call to another peer")?;
    ensure_stats_denied(&ordinary).await?;
    dbus.get_id().await?;

    let owner_changes = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender(BUS_NAME)
        .member("NameOwnerChanged")
        .build()?;
    let ours = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    dbus.add_match_rule(owner_changes).await?;
    dbus.add_match_rule(ours).await?;
    let mut ordinary_msgs = MessageStream::from(&ordinary);
    conn.emit_signal(None::<&str>, PATH, INTERFACE, "Broadcast", &())
        .await?;
    ensure_member(&mut ordinary_msgs, "Broadcast").await?;
    let later = bus.connect().await?;
    next_owner_change(&mut ordinary_msgs, later.unique_name().unwrap()).await?;

    Ok(())
}

/// A bus that a test runs against.
struct TestBus {
    address: String,
    peers: Arc<Peers>,
    guid: OwnedGuid,
}

impl TestBus {
    /// A client that connects to the bus through its socket.
    async fn connect(&self) -> zbus::Result<Connection> {
        connection::Builder::address(self.address.as_str())
            .build()
            .await
    }

    /// A client of the bus numbered `id`, of the user of `uid`, whatever the user that runs the
    /// test.
    async fn connect_as(&self, id: usize, uid: u32) -> anyhow::Result<Connection> {
        connect_as(&self.peers, &self.guid, id, uid).await
    }
}

/// A client of the bus numbered `id`, of the user of `uid`, whatever the user that runs the test.
async fn connect_as(
    peers: &Arc<Peers>,
    guid: &OwnedGuid,
    id: usize,
    uid: u32,
) -> anyhow::Result<Connection> {
    let (ours, theirs) = UnixStream::pair()?;
    let adding = peers.add(
        guid,
        id,
        connection::Builder::unix_stream(theirs),
        SEND_TIMEOUT,
        uid,
    );
    let connecting = connection::Builder::unix_stream(ours).build();
    let (added, conn) = join(adding, connecting).await;
    added?;

    Ok(conn?)
}

async fn dbus_proxy(conn: &Connection) -> zbus::Result<DBusProxy<'_>> {
    DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// Request `name`, which no one is to own or queue for.
async fn request_name(dbus: &DBusProxy<'_>, name: &str) -> zbus::fdo::Result<RequestNameReply> {
    dbus.request_name(name.try_into()?, RequestNameFlags::DO_NOT_QUEUE)
        .await
}

/// Call `member` of `interface` on `path` of `destination`, and wait for the reply.
async fn call(
    conn: &Connection,
    destination: &str,
    path: &str,
    interface: &str,
    member: &str,
) -> zbus::Result<Message> {
    conn.call_method(Some(destination), path, Some(interface), member, &())
        .await
}

/// A call of `member` of `interface` on `path` of `destination` that expects no reply.
fn no_reply(destination: &str, path: &str, interface: &str, member: &str) -> zbus::Result<Message> {
    Message::method_call(path, member)
        .destination(destination)
        .interface(interface)
        .with_flags(message::Flags::NO_REPLY_EXPECTED)
        .build(&())
}

/// Fail unless the bus refuses `conn` each method of its `org.freedesktop.DBus.Debug.Stats`
/// interface, which tells about the bus and about the other connections.
async fn ensure_stats_denied(conn: &Connection) -> anyhow::Result<()> {
    let stats = StatsProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    ensure_denied(stats.get_stats().await, SEND_REFUSED).context("`GetStats`")?;
    let own_name = BusName::from(conn.unique_name().context("no unique name")?.clone());
    ensure_denied(stats.get_connection_stats(own_name).await, SEND_REFUSED)
        .context("`GetConnectionStats`")?;
    ensure_denied(stats.get_all_match_rules().await, SEND_REFUSED).context("`GetAllMatchRules`")
}

/// Fail unless `result` is the `AccessDenied` error of the bus, of `description`.
fn ensure_denied<T, E>(result: Result<T, E>, description: &str) -> anyhow::Result<()>
where
    T: Debug,
    E: Into<zbus::Error>,
{
    let (name, got) = match result.map_err(Into::into) {
        Err(zbus::Error::MethodError(name, got, _)) => (name.to_string(), got),
        Err(zbus::Error::FDO(e)) => (e.name().to_string(), e.description().map(Into::into)),
        other => bail!("expected `AccessDenied`, got {other:?}"),
    };
    ensure!(
        name == "org.freedesktop.DBus.Error.AccessDenied" && got.as_deref() == Some(description),
        "expected `AccessDenied` of `{description}`, got `{name}` of {got:?}"
    );

    Ok(())
}

/// Fail unless the next method call or signal that is not of the bus in `msgs` is of `member`.
async fn ensure_member(msgs: &mut MessageStream, member: &str) -> anyhow::Result<()> {
    let msg = loop {
        let msg = msgs.try_next().await?.unwrap();
        let from_bus = msg
            .header()
            .sender()
            .is_some_and(|sender| sender == BUS_NAME);
        let is_reply = matches!(
            msg.message_type(),
            message::Type::MethodReturn | message::Type::Error
        );
        if !from_bus && !is_reply {
            break msg;
        }
    };
    let header = msg.header();
    let got = header.member().map(|member| member.as_str());
    ensure!(
        got == Some(member),
        "expected `{member}`, got {:?} {got:?}",
        msg.message_type()
    );

    Ok(())
}

/// The next method call in `msgs`.
async fn next_call(msgs: &mut MessageStream) -> anyhow::Result<Message> {
    loop {
        let msg = msgs.try_next().await?.unwrap();
        if msg.message_type() == message::Type::MethodCall {
            return Ok(msg);
        }
    }
}

/// Wait for the `NameOwnerChanged` of `name` in `msgs`, and fail if a method call or a signal of
/// a client comes first.
async fn next_owner_change(msgs: &mut MessageStream, name: &str) -> anyhow::Result<()> {
    loop {
        let msg = msgs.try_next().await?.unwrap();
        let header = msg.header();
        match header.member().map(|member| member.as_str()) {
            Some("NameOwnerChanged") => {
                let (changed, _, _): (String, String, String) = msg.body().deserialize()?;
                if changed == name {
                    return Ok(());
                }
            }
            Some(member) if msg.message_type() != message::Type::MethodReturn => {
                let from_bus = header.sender().is_some_and(|sender| sender == BUS_NAME);
                ensure!(
                    from_bus,
                    "expected `NameOwnerChanged` of `{name}`, got `{member}`"
                );
            }
            _ => (),
        }
    }
}

/// The uid of the user that runs the test, which the bus runs as too.
fn our_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// A uid that is neither that of root nor that of the user that runs the test.
fn other_uid() -> u32 {
    our_uid().wrapping_add(1).max(1)
}

/// The policy of the `<policy>` elements of `policies`, for a bus of the user that runs the test.
fn policy(policies: &str) -> anyhow::Result<Policy> {
    let config = Config::parse(&format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>{policies}</busconfig>"#
    ))?;

    Ok(Policy::new(&config, &Ids::default(), our_uid()))
}

/// Set the tracing subscriber up, once for all the tests of this file, which run in one process.
fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}

/// Run `test` against a bus of its own, which goes by the `<policy>` elements of `policies`.
async fn with_bus<F, T>(policies: &str, test: F)
where
    F: FnOnce(TestBus) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    with_policy(policy(policies).unwrap(), test).await;
}

/// Run `test` against a bus of its own, which goes by `policy`.
async fn with_policy<F, T>(policy: Policy, test: F)
where
    F: FnOnce(TestBus) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    let mut bus = Bus::for_address(Some(&address)).await.unwrap();
    // Before the bus serves anyone.
    bus.set_policy(Arc::new(policy)).await;
    let test_bus = TestBus {
        address,
        peers: bus.peers().clone(),
        guid: bus.guid().clone(),
    };
    let stop = Event::new();
    let stopped = stop.listen();

    let handle = zruntime::spawn(async move {
        {
            let running = pin!(bus.run());
            match select(stopped, running).await {
                Either::Left(_) => (),
                Either::Right((Ok(()), _)) => panic!("Bus exited unexpectedly"),
                Either::Right((Err(e), _)) => panic!("Bus exited with an error: {e}"),
            }
        }

        bus
    });

    let ret = test(test_bus).await;
    stop.notify(usize::MAX);
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

const PATH: &str = "/org/busd/Policy";
const INTERFACE: &str = "org.busd.Policy";
const SERVICE: &str = "org.busd.Policy.Service";
const BUS_NAME: &str = "org.freedesktop.DBus";
/// The rules that let everyone send and receive everything.
const SEND_AND_RECEIVE: &str = r#"<allow send_destination="*"/> <allow eavesdrop="true"/>"#;
const OWN_REFUSED: &str = "Request to own name refused by policy";
const SEND_REFUSED: &str = "Sender is not authorized to send message";
const RECEIVE_REFUSED: &str = "Receiver is not authorized to receive message";
