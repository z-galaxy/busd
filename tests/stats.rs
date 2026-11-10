use std::{
    collections::{HashMap, HashSet},
    env::temp_dir,
    future::Future,
    os::unix::net::UnixStream,
    pin::pin,
    sync::Once,
};

use anyhow::{ensure, Context};
use busd::bus::Bus;
use futures_util::{
    future::{select, Either},
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
    fdo::{
        self, DBusProxy, IntrospectableProxy, NameOwnerChanged, ReleaseNameReply, RequestNameFlags,
        RequestNameReply, StatsProxy,
    },
    message,
    names::{BusName, OwnedUniqueName, WellKnownName},
    proxy::CacheProperties,
    Connection, MatchRule, MessageStream, OwnedMatchRule, OwnedVariant,
};
use zruntime::Event;

/// `GetStats` counts the connections, the match rules and the names, and their peaks, as clients
/// come and go, and its serial goes up with each call.
#[test]
#[instrument]
#[timeout(30000)]
fn get_stats() {
    init_tracing();

    zruntime::block_on(with_bus(get_stats_));
}

/// A client that authenticated but did not call `Hello`, and one that did not even authenticate,
/// are incomplete connections, and no longer are once they go.
#[test]
#[instrument]
#[timeout(30000)]
fn incomplete_connections() {
    init_tracing();

    zruntime::block_on(with_bus(incomplete_connections_));
}

/// A client that becomes a monitor stays an active connection, and its match rules, and those that
/// it gave to `BecomeMonitor`, are not counted. It is no connection for `GetConnectionStats` to
/// tell about, nor for `GetAllMatchRules` to list.
#[test]
#[instrument]
#[timeout(30000)]
fn becoming_a_monitor() {
    init_tracing();

    zruntime::block_on(with_bus(becoming_a_monitor_));
}

/// `GetConnectionStats` counts the match rules and the names of one connection, and their peaks,
/// asked for by its unique name or by a name that it owns, and its serial goes up with each call
/// that it answers. It fails for the bus itself, and for a name without an owner.
#[test]
#[instrument]
#[timeout(30000)]
fn get_connection_stats() {
    init_tracing();

    zruntime::block_on(with_bus(get_connection_stats_));
}

/// `GetAllMatchRules` lists the match rules of each connection, as many as `GetStats` counts, in a
/// form that reads back as the rules that the connection added, and a connection without rules
/// with none.
#[test]
#[instrument]
#[timeout(30000)]
fn get_all_match_rules() {
    init_tracing();

    zruntime::block_on(with_bus(get_all_match_rules_));
}

/// The bus introspects the three methods of the interface with the signatures of the
/// specification.
#[test]
#[instrument]
#[timeout(30000)]
fn introspection() {
    init_tracing();

    zruntime::block_on(with_bus(introspection_));
}

async fn get_stats_(address: String) -> anyhow::Result<()> {
    let a = connect(&address).await?;
    let a_stats = stats_proxy(&a).await?;
    let a_dbus = dbus_proxy(&a).await?;
    let serial = expect(&a_stats, [1, 0, 0, 0, 0, 0, 0, 0]).await?;
    ensure!(serial == 1, "unexpected serial: {serial}");
    let serial = expect(&a_stats, [1, 0, 0, 0, 0, 0, 0, 0]).await?;
    ensure!(serial == 2, "unexpected serial: {serial}");

    let owner_changes = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender(BUS_NAME)
        .interface(BUS_NAME)
        .member("NameOwnerChanged")
        .build()?;
    let other = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.busd.Stats")
        .build()?;
    a_dbus.add_match_rule(owner_changes.clone()).await?;
    a_dbus.add_match_rule(other).await?;
    // A rule that is there already does not count twice.
    a_dbus.add_match_rule(owner_changes).await?;
    let mut a_messages = MessageStream::from(&a);

    let b = connect(&address).await?;
    let b_unique_name = b.unique_name().context("no unique name")?.to_owned();
    let b_stats = stats_proxy(&b).await?;
    let b_dbus = dbus_proxy(&b).await?;
    let b_rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.busd.Stats.B")
        .build()?;
    b_dbus.add_match_rule(b_rule.clone()).await?;
    let first_name = WellKnownName::try_from(FIRST_NAME)?;
    let second_name = WellKnownName::try_from(SECOND_NAME)?;
    for name in [&first_name, &second_name] {
        let reply = b_dbus
            .request_name(name.clone(), RequestNameFlags::empty())
            .await?;
        ensure!(reply == RequestNameReply::PrimaryOwner, "got {reply:?}");
    }
    expect(&a_stats, [2, 0, 3, 3, 2, 2, 2, 2]).await?;

    // A name that is queued for has no new owner.
    let reply = a_dbus
        .request_name(first_name.clone(), RequestNameFlags::empty())
        .await?;
    ensure!(reply == RequestNameReply::InQueue, "got {reply:?}");
    expect(&a_stats, [2, 0, 3, 3, 2, 2, 2, 2]).await?;

    // The queued client gets the name.
    b_dbus.remove_match_rule(b_rule).await?;
    let reply = b_dbus.release_name(first_name.clone()).await?;
    ensure!(reply == ReleaseNameReply::Released, "got {reply:?}");
    expect(&b_stats, [2, 0, 2, 3, 2, 2, 2, 2]).await?;

    drop(b_stats);
    drop(b_dbus);
    drop(b);
    loop {
        let msg = a_messages.try_next().await?.context("stream ended")?;
        let Some(changed) = NameOwnerChanged::from_message(msg) else {
            continue;
        };
        let args = changed.args()?;
        if *args.name() == *b_unique_name && args.new_owner().is_none() {
            break;
        }
    }
    expect(&a_stats, [1, 0, 2, 3, 2, 1, 2, 2]).await?;

    Ok(())
}

async fn incomplete_connections_(address: String) -> anyhow::Result<()> {
    let client = connect(&address).await?;
    let stats = stats_proxy(&client).await?;
    let greeting_never_comes = connection::Builder::address(address.as_str())
        .p2p()
        .build()
        .await?;
    let counts = loop {
        let counts = stats.get_stats().await?;
        if counts.incomplete_connections() == Some(1) {
            break counts;
        }
    };
    ensure!(
        counts.active_connections() == Some(1),
        "unexpected counts: {counts:?}"
    );

    let path = address
        .strip_prefix("unix:path=")
        .context("not a path address")?;
    let silent = UnixStream::connect(path)?;
    while stats.get_stats().await?.incomplete_connections() != Some(2) {}

    drop(silent);
    drop(greeting_never_comes);
    while stats.get_stats().await?.incomplete_connections() != Some(0) {}

    Ok(())
}

async fn becoming_a_monitor_(address: String) -> anyhow::Result<()> {
    let monitor = connect(&address).await?;
    let monitor_unique_name = monitor.unique_name().context("no unique name")?.clone();
    let other = connect(&address).await?;
    let other_unique_name = other.unique_name().context("no unique name")?.clone();
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.busd.Stats")
        .build()?;
    dbus_proxy(&monitor).await?.add_match_rule(rule).await?;
    let rules = vec!["type='signal',interface='org.busd.Stats.Monitor'"];
    monitor
        .call_method(
            Some(BUS_NAME),
            PATH,
            Some("org.freedesktop.DBus.Monitoring"),
            "BecomeMonitor",
            &(rules, 0u32),
        )
        .await?;
    let stats = stats_proxy(&other).await?;
    expect(&stats, [2, 0, 0, 1, 1, 0, 0, 0]).await?;

    let result = stats
        .get_connection_stats(BusName::try_from(monitor_unique_name.as_str())?)
        .await;
    ensure!(
        matches!(result, Err(fdo::Error::NameHasNoOwner(_))),
        "unexpected result: {result:?}"
    );
    let listed = all_match_rules(&stats).await?;
    let expected = HashMap::from([(other_unique_name, HashSet::new())]);
    ensure!(listed == expected, "unexpected rules: {listed:?}");

    Ok(())
}

async fn get_connection_stats_(address: String) -> anyhow::Result<()> {
    let a = connect(&address).await?;
    let a_unique_name = a.unique_name().context("no unique name")?.clone();
    let a_stats = stats_proxy(&a).await?;
    let a_dbus = dbus_proxy(&a).await?;
    let b = connect(&address).await?;
    let b_unique_name = b.unique_name().context("no unique name")?.clone();
    let b_dbus = dbus_proxy(&b).await?;
    let serial = expect_connection(
        &a_stats,
        a_unique_name.as_str(),
        &a_unique_name,
        [0, 0, 0, 0],
    )
    .await?;
    ensure!(serial == 1, "unexpected serial: {serial}");

    // Two rules and two names, of which one each goes again.
    let first_rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.busd.Stats")
        .build()?;
    let second_rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.busd.Stats.Other")
        .build()?;
    a_dbus.add_match_rule(first_rule.clone()).await?;
    a_dbus.add_match_rule(second_rule).await?;
    a_dbus.remove_match_rule(first_rule).await?;
    let first_name = WellKnownName::try_from(FIRST_NAME)?;
    let second_name = WellKnownName::try_from(SECOND_NAME)?;
    for name in [&first_name, &second_name] {
        let reply = a_dbus
            .request_name(name.clone(), RequestNameFlags::empty())
            .await?;
        ensure!(reply == RequestNameReply::PrimaryOwner, "got {reply:?}");
    }
    let reply = a_dbus.release_name(second_name).await?;
    ensure!(reply == ReleaseNameReply::Released, "got {reply:?}");
    // A name that a connection queues for is not one that it has.
    let reply = b_dbus
        .request_name(first_name.clone(), RequestNameFlags::empty())
        .await?;
    ensure!(reply == RequestNameReply::InQueue, "got {reply:?}");

    // By its unique name, and by a name that it owns. The serial is apart from that of `GetStats`.
    let serial = expect_connection(
        &a_stats,
        a_unique_name.as_str(),
        &a_unique_name,
        [1, 2, 1, 2],
    )
    .await?;
    ensure!(serial == 2, "unexpected serial: {serial}");
    let serial = a_stats.get_stats().await?.serial();
    ensure!(
        serial == Some(1),
        "unexpected serial of `GetStats`: {serial:?}"
    );
    let serial = expect_connection(&a_stats, FIRST_NAME, &a_unique_name, [1, 2, 1, 2]).await?;
    ensure!(serial == 3, "unexpected serial: {serial}");
    expect_connection(
        &a_stats,
        b_unique_name.as_str(),
        &b_unique_name,
        [0, 0, 0, 0],
    )
    .await?;

    // The name passes on to the connection that queued for it.
    let reply = a_dbus.release_name(first_name).await?;
    ensure!(reply == ReleaseNameReply::Released, "got {reply:?}");
    expect_connection(
        &a_stats,
        a_unique_name.as_str(),
        &a_unique_name,
        [1, 2, 0, 2],
    )
    .await?;
    expect_connection(&a_stats, FIRST_NAME, &b_unique_name, [0, 0, 1, 1]).await?;

    // The bus itself cannot be queried, and neither can an unowned name.
    let result = a_stats
        .get_connection_stats(BusName::try_from(BUS_NAME)?)
        .await;
    ensure!(
        matches!(result, Err(fdo::Error::InvalidArgs(_))),
        "unexpected result: {result:?}"
    );
    for name in [SECOND_NAME, ":busd.nobody"] {
        let result = a_stats.get_connection_stats(BusName::try_from(name)?).await;
        ensure!(
            matches!(result, Err(fdo::Error::NameHasNoOwner(_))),
            "unexpected result for `{name}`: {result:?}"
        );
    }
    // A call that failed took no serial.
    let serial = expect_connection(
        &a_stats,
        a_unique_name.as_str(),
        &a_unique_name,
        [1, 2, 0, 2],
    )
    .await?;
    ensure!(serial == 7, "unexpected serial: {serial}");

    // The reply has the entries that the bus has something to count for, and no others.
    let reply = a
        .call_method(
            Some(BUS_NAME),
            PATH,
            Some(STATS_INTERFACE),
            "GetConnectionStats",
            &a_unique_name.as_str(),
        )
        .await?;
    let entries: HashMap<String, OwnedVariant> = reply.body().deserialize()?;
    let mut keys: Vec<_> = entries.keys().map(String::as_str).collect();
    keys.sort_unstable();
    ensure!(keys == CONNECTION_ENTRIES, "unexpected entries: {keys:?}");

    Ok(())
}

async fn get_all_match_rules_(address: String) -> anyhow::Result<()> {
    let a = connect(&address).await?;
    let a_unique_name = a.unique_name().context("no unique name")?.clone();
    let a_stats = stats_proxy(&a).await?;
    let a_dbus = dbus_proxy(&a).await?;
    let b = connect(&address).await?;
    let b_unique_name = b.unique_name().context("no unique name")?.clone();
    let b_dbus = dbus_proxy(&b).await?;

    // The two connections, without a rule so far, and not the bus itself.
    let listed = all_match_rules(&a_stats).await?;
    let expected = HashMap::from([
        (a_unique_name.clone(), HashSet::new()),
        (b_unique_name.clone(), HashSet::new()),
    ]);
    ensure!(listed == expected, "unexpected rules: {listed:?}");

    let plain = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.busd.Stats")
        .build()?;
    // A rule with most of the keys that there are, and with values that take quoting.
    let quoted = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender("org.busd.Stats.Sender")
        .interface("org.busd.Stats")
        .member("Changed")
        .path_namespace("/org/busd")
        .destination(":busd.42")
        .arg(0, "it's")
        .arg(1, "a,b='c'")
        .arg_path(2, "/org/busd/Stats")
        .build()?;
    // A rule that a connection adds twice is one rule.
    for rule in [&plain, &quoted, &plain] {
        a_dbus.add_match_rule(rule.clone()).await?;
    }
    let listed = all_match_rules(&a_stats).await?;
    let expected = HashMap::from([
        (
            a_unique_name.clone(),
            HashSet::from([plain.clone().into(), quoted.clone().into()]),
        ),
        (b_unique_name.clone(), HashSet::new()),
    ]);
    ensure!(listed == expected, "unexpected rules: {listed:?}");

    // Each connection has its own rules, and one that is removed is listed no more.
    b_dbus.add_match_rule(plain.clone()).await?;
    a_dbus.remove_match_rule(plain.clone()).await?;
    let listed = all_match_rules(&a_stats).await?;
    let expected = HashMap::from([
        (a_unique_name, HashSet::from([quoted.into()])),
        (b_unique_name, HashSet::from([plain.into()])),
    ]);
    ensure!(listed == expected, "unexpected rules: {listed:?}");

    // As many rules as `GetStats` counts.
    let counted = a_stats.get_stats().await?.match_rules();
    let listed: usize = listed.values().map(HashSet::len).sum();
    ensure!(
        counted == u32::try_from(listed).ok(),
        "{counted:?} rules counted, {listed} listed"
    );

    Ok(())
}

async fn introspection_(address: String) -> anyhow::Result<()> {
    let conn = connect(&address).await?;
    let xml = IntrospectableProxy::builder(&conn)
        .destination(BUS_NAME)
        .path(PATH)
        .build()
        .await?
        .introspect()
        .await?;
    let start = xml
        .find(&format!("<interface name=\"{STATS_INTERFACE}\">"))
        .context("the bus does not introspect the interface")?;
    let interface = &xml[start..];
    let interface = &interface[..interface.find("</interface>").context("unclosed")?];
    for (method, args) in [
        ("GetStats", &[("a{sv}", "out")][..]),
        ("GetConnectionStats", &[("s", "in"), ("a{sv}", "out")]),
        ("GetAllMatchRules", &[("a{sas}", "out")]),
    ] {
        let start = interface
            .find(&format!("<method name=\"{method}\">"))
            .with_context(|| format!("no `{method}` in {interface}"))?;
        let method_xml = &interface[start..];
        let method_xml = &method_xml[..method_xml.find("</method>").context("unclosed")?];
        let found: Vec<_> = method_xml
            .lines()
            .filter(|line| line.contains("<arg "))
            .collect();
        ensure!(
            found.len() == args.len(),
            "unexpected arguments of `{method}`: {method_xml}"
        );
        for (line, (signature, direction)) in found.into_iter().zip(args) {
            ensure!(
                line.contains(&format!("type=\"{signature}\""))
                    && line.contains(&format!("direction=\"{direction}\"")),
                "unexpected argument of `{method}`: {line}"
            );
        }
    }

    Ok(())
}

/// Call `GetStats` on `proxy`, and check the counts in the reply, in this order: the active and the
/// incomplete connections, the match rules and their peak and their peak for one connection, the
/// names and their peak and their peak for one connection. All nine entries must be there.
///
/// The serial of the reply.
async fn expect(proxy: &StatsProxy<'_>, expected: [u32; 8]) -> anyhow::Result<u32> {
    let stats = proxy.get_stats().await?;
    let got = [
        stats.active_connections(),
        stats.incomplete_connections(),
        stats.match_rules(),
        stats.peak_match_rules(),
        stats.peak_match_rules_per_connection(),
        stats.bus_names(),
        stats.peak_bus_names(),
        stats.peak_bus_names_per_connection(),
    ];
    ensure!(
        got == expected.map(Some),
        "expected {expected:?}, got {stats:?}"
    );

    stats
        .serial()
        .with_context(|| format!("no serial in {stats:?}"))
}

/// Call `GetConnectionStats` for `name` on `proxy`, and check that the reply is about the
/// connection `unique_name`, and its counts, in this order: the match rules and their peak, the
/// names and their peak.
///
/// The serial of the reply.
async fn expect_connection(
    proxy: &StatsProxy<'_>,
    name: &str,
    unique_name: &OwnedUniqueName,
    expected: [u32; 4],
) -> anyhow::Result<u32> {
    let stats = proxy.get_connection_stats(BusName::try_from(name)?).await?;
    let got = [
        stats.match_rules(),
        stats.peak_match_rules(),
        stats.bus_names(),
        stats.peak_bus_names(),
    ];
    ensure!(
        stats.unique_name() == Some(unique_name) && got == expected.map(Some),
        "expected {expected:?} of {unique_name}, got {stats:?}"
    );

    stats
        .serial()
        .with_context(|| format!("no serial in {stats:?}"))
}

/// Call `GetAllMatchRules` on `proxy`, which reads each rule back from the form that the bus lists
/// it in.
///
/// The rules of each connection, as a set: it fails if the bus lists one twice.
async fn all_match_rules(
    proxy: &StatsProxy<'_>,
) -> anyhow::Result<HashMap<OwnedUniqueName, HashSet<OwnedMatchRule>>> {
    let mut all = HashMap::new();
    for (name, rules) in proxy.get_all_match_rules().await? {
        let listed = rules.len();
        let rules: HashSet<_> = rules.into_iter().collect();
        ensure!(rules.len() == listed, "a rule of {name} is listed twice");
        all.insert(name, rules);
    }

    Ok(all)
}

async fn stats_proxy(conn: &Connection) -> zbus::Result<StatsProxy<'_>> {
    StatsProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn dbus_proxy(conn: &Connection) -> zbus::Result<DBusProxy<'_>> {
    DBusProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

async fn connect(address: &str) -> zbus::Result<Connection> {
    connection::Builder::address(address).build().await
}

/// Set the tracing subscriber up, once for all the tests of this file, which run in one process.
fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}

/// Run `test` against a bus of its own, at an address that it is given.
async fn with_bus<F, T>(test: F)
where
    F: FnOnce(String) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    let mut bus = Bus::for_address(Some(&address)).await.unwrap();
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

    let ret = test(address).await;
    stop.notify(usize::MAX);
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

const BUS_NAME: &str = "org.freedesktop.DBus";
const PATH: &str = "/org/freedesktop/DBus";
const STATS_INTERFACE: &str = "org.freedesktop.DBus.Debug.Stats";
const FIRST_NAME: &str = "org.busd.Stats.First";
const SECOND_NAME: &str = "org.busd.Stats.Second";
/// The entries of the reply of `GetConnectionStats`, sorted.
const CONNECTION_ENTRIES: [&str; 6] = [
    "BusNames",
    "MatchRules",
    "PeakBusNames",
    "PeakMatchRules",
    "Serial",
    "UniqueName",
];
