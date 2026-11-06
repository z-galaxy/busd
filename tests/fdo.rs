use std::{
    collections::{HashMap, HashSet},
    env::temp_dir,
    future::Future,
    pin::pin,
    sync::Once,
    time::Duration,
};

use anyhow::{ensure, Context};
use busd::bus::Bus;
use futures_util::{
    future::{select, try_join, Either},
    stream::StreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::{
    connection,
    fdo::{self, DBusProxy, ReleaseNameReply, RequestNameFlags, RequestNameReply},
    names::{BusName, WellKnownName},
    proxy::CacheProperties,
    Connection, Message, MessageStream,
};
use zruntime::{Event, SharedRuntime};

#[test]
#[instrument]
#[timeout(30000)]
fn name_ownership_changes() {
    init_tracing();

    zruntime::block_on(async {
        let s = Alphanumeric.sample_string(&mut rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        with_bus(&address, name_ownership_changes_client).await;
    });
}

/// Every method of the bus, among many other calls to the bus that a client sends at once, gets its
/// reply, whether it asks about the bus itself, the client or another one, as do the other calls,
/// and the bus goes on serving everyone, a client that becomes a monitor meanwhile included.
///
/// The bus answers its calls one at a time, so a method that waited for anything that waits for the
/// bus to take the calls after it would hold the bus up for good.
#[test]
#[timeout(30000)]
fn every_call_to_the_bus_among_calls() {
    init_tracing();

    zruntime::block_on(async {
        let s = Alphanumeric.sample_string(&mut rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        with_bus(&address, every_call_to_the_bus_among_calls_client).await;
    });
}

/// Run `test` against a bus at `address`.
async fn with_bus<F, T>(address: &str, test: F)
where
    F: FnOnce(String) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let mut bus = Bus::for_address(Some(address)).await.unwrap();
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

    let ret = test(address.to_owned()).await;
    stop.notify(usize::MAX);
    let bus = handle.await.unwrap();
    bus.cleanup().await.unwrap();
    ret.unwrap();
}

#[instrument]
async fn name_ownership_changes_client(address: String) -> anyhow::Result<()> {
    let conn = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let conn_unique_name = conn.unique_name().unwrap().to_owned();
    let dbus_proxy = DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let name: WellKnownName = "org.blah".try_into()?;

    let mut name_changed_stream = dbus_proxy.receive_name_owner_changed().await?;
    let mut name_acquired_stream = dbus_proxy.receive_name_acquired().await?;
    // This should work.
    let ret = dbus_proxy
        .request_name(name.clone(), RequestNameFlags::ALLOW_REPLACEMENT)
        .await?;
    ensure!(
        ret == RequestNameReply::PrimaryOwner,
        "expected to become primary owner"
    );
    // Ensure signals were emitted.
    let mut changed = name_changed_stream.next().await.unwrap();
    if *changed.args()?.name() == *conn_unique_name {
        // In case we do happen to get the signal for our unique name, well-known name signal should
        // be next.
        changed = name_changed_stream.next().await.unwrap();
    }
    ensure!(
        *changed.args()?.name() == name,
        "expected name owner changed signal for our name"
    );
    ensure!(
        changed.args()?.old_owner.is_none(),
        "expected no old owner for our name"
    );
    ensure!(
        changed.args()?.new_owner.as_ref().unwrap() == conn.unique_name().unwrap(),
        "expected new owner to be us"
    );
    ensure!(
        changed.message().header().destination().is_none(),
        "expected no destination for our signal",
    );
    let acquired = name_acquired_stream.next().await.unwrap();
    ensure!(
        *acquired.args()?.name() == name,
        "expected name acquired signal for our name"
    );
    ensure!(
        *acquired.message().header().destination().unwrap()
            == BusName::from(conn.unique_name().unwrap()),
        "expected name acquired signal to be unicasted to the acquiring connection",
    );

    // This shouldn't and we should be told we already own the name.
    let ret = dbus_proxy
        .request_name(name.clone(), RequestNameFlags::ALLOW_REPLACEMENT)
        .await?;
    ensure!(
        ret == RequestNameReply::AlreadyOwner,
        "expected to be already primary owner"
    );

    // Now we try with another connection and we should be queued.
    let conn2 = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let conn2_unique_name = conn2.unique_name().unwrap().to_owned();
    let changed = name_changed_stream.next().await.unwrap();
    ensure!(
        *changed.args()?.name() == *conn2_unique_name,
        "expected name owner changed signal for the new connections gaining unique name"
    );
    ensure!(
        changed.args()?.old_owner.is_none(),
        "expected no old owner for the unique name of the second connection"
    );
    ensure!(
        *changed.args()?.new_owner.as_ref().unwrap() == conn2_unique_name,
        "expected new owner of the unique name of the second connection to be itself"
    );
    let dbus_proxy2 = DBusProxy::builder(&conn2)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let ret = dbus_proxy2
        .request_name(name.clone(), RequestNameFlags::empty())
        .await?;

    // Check that first client is the primary owner before it releases the name.
    ensure!(ret == RequestNameReply::InQueue, "expected to be in queue");
    let owner = dbus_proxy.get_name_owner(name.clone().into()).await?;
    let unique_name = conn.unique_name().unwrap().clone();
    ensure!(owner == unique_name, "unexpected owner");
    let owner = dbus_proxy
        .get_name_owner(unique_name.clone().into())
        .await?;
    ensure!(owner == unique_name, "unexpected owner");
    let res = dbus_proxy.get_name_owner(":1.3333".try_into()?).await;
    ensure!(
        matches!(res, Err(fdo::Error::NameHasNoOwner(_))),
        "expected error"
    );

    let mut name_acquired_stream = dbus_proxy2.receive_name_acquired().await?;
    let mut name_lost_stream = dbus_proxy.receive_name_lost().await?;
    // Now the first client releases name.
    let ret = dbus_proxy.release_name(name.clone()).await?;
    ensure!(
        ret == ReleaseNameReply::Released,
        "expected name to be released"
    );
    // Ensure signals were emitted.
    let changed = name_changed_stream.next().await.unwrap();
    ensure!(
        *changed.args()?.name() == name,
        "expected name owner changed signal for our name"
    );
    ensure!(
        changed.args()?.old_owner.as_ref().unwrap() == conn.unique_name().unwrap(),
        "expected old owner to be our first connection"
    );
    ensure!(
        changed.args()?.new_owner.as_ref().unwrap() == conn2.unique_name().unwrap(),
        "expected new owner to be our second connection"
    );
    ensure!(
        changed.message().header().destination().is_none(),
        "expected no destination for our signal",
    );
    let lost = name_lost_stream.next().await.unwrap();
    ensure!(
        *lost.args()?.name() == name,
        "expected name lost signal for our name"
    );
    ensure!(
        *lost.message().header().destination().unwrap()
            == BusName::from(conn.unique_name().unwrap()),
        "expected name lost signal to be unicasted to the loosing connection",
    );
    let acquired = name_acquired_stream.next().await.unwrap();
    ensure!(
        *acquired.args()?.name() == name,
        "expected name acquired signal for our name"
    );
    ensure!(
        *acquired.message().header().destination().unwrap()
            == BusName::from(conn2.unique_name().unwrap()),
        "expected name acquired signal to be unicasted to the acquiring connection",
    );

    // Now the second client should be the primary owner.
    let owner = dbus_proxy.get_name_owner(name.clone().into()).await?;
    ensure!(owner == *conn2.unique_name().unwrap(), "unexpected owner");

    drop(name_acquired_stream);
    drop(dbus_proxy2);
    drop(conn2);

    let mut unique_name_signaled = false;
    let mut well_known_name_signaled = false;
    while !unique_name_signaled || !well_known_name_signaled {
        let changed = name_changed_stream.next().await.unwrap();
        if *changed.args()?.name() == *conn2_unique_name {
            ensure!(
                changed.args()?.new_owner.is_none(),
                "expected no new owner for our unique name"
            );
            ensure!(
                *changed.args()?.old_owner.as_ref().unwrap() == conn2_unique_name,
                "expected old owner to be us"
            );
            unique_name_signaled = true;
        } else if *changed.args()?.name() == name {
            ensure!(
                changed.args()?.new_owner.is_none(),
                "expected no new owner for our name"
            );
            ensure!(
                *changed.args()?.old_owner.as_ref().unwrap() == conn2_unique_name,
                "expected old owner to be us"
            );
            well_known_name_signaled = true;
        } else {
            panic!("unexpected name owner changed signal");
        }
    }

    Ok(())
}

async fn every_call_to_the_bus_among_calls_client(address: String) -> anyhow::Result<()> {
    let conn = connect(&address).await?;
    let other = connect(&address).await?;
    DBusProxy::new(&other)
        .await?
        .request_name(OTHER_NAME.try_into()?, RequestNameFlags::empty())
        .await?;
    let monitor = connect(&address).await?;
    let own_name = conn.unique_name().context("no unique name")?.to_string();
    let other_name = other.unique_name().context("no unique name")?.to_string();
    let names = [BUS_NAME, &own_name, &other_name, OTHER_NAME];
    let mut calls = Vec::new();
    for _ in 0..ROUNDS {
        for name in names {
            for method in [
                "GetNameOwner",
                "NameHasOwner",
                "GetConnectionCredentials",
                "GetConnectionUnixUser",
                "GetConnectionUnixProcessID",
                "GetConnectionSELinuxSecurityContext",
                "GetAdtAuditSessionData",
            ] {
                calls.push(call_bus(method, &name)?);
            }
        }
        calls.push(call_bus("Hello", &())?);
        calls.push(call_bus("RequestName", &(OWN_NAME, 0u32))?);
        calls.push(call_bus("ListQueuedOwners", &OWN_NAME)?);
        calls.push(call_bus("ReleaseName", &OWN_NAME)?);
        calls.push(call_bus("AddMatch", &RULE)?);
        calls.push(call_bus("RemoveMatch", &RULE)?);
        calls.push(call_bus("StartServiceByName", &(OTHER_NAME, 0u32))?);
        let environment = HashMap::<&str, &str>::new();
        calls.push(call_bus("UpdateActivationEnvironment", &environment)?);
        for method in ["ListNames", "ListActivatableNames", "ReloadConfig", "Dune"] {
            calls.push(call_bus(method, &())?);
        }
        calls.push(call(
            "org.freedesktop.DBus.Properties",
            "GetAll",
            &BUS_NAME,
        )?);
        calls.push(call(
            "org.freedesktop.DBus.Introspectable",
            "Introspect",
            &(),
        )?);
        calls.push(call("org.freedesktop.DBus.Peer", "Ping", &())?);
        for _ in 0..GET_IDS {
            calls.push(call_bus("GetId", &())?);
        }
    }
    let becoming_monitor = async {
        let rules: Vec<&str> = Vec::new();
        monitor
            .call_method(
                Some(BUS_NAME),
                PATH,
                Some("org.freedesktop.DBus.Monitoring"),
                "BecomeMonitor",
                &(rules, 0u32),
            )
            .await?;

        anyhow::Ok(())
    };

    let (replies, ()) = try_join(burst(&conn, &calls), becoming_monitor).await?;
    // Some of the methods fail by design, but none for being called wrongly.
    for reply in replies {
        let error = reply.header().error_name().map(|name| name.to_string());
        ensure!(
            !error
                .as_deref()
                .is_some_and(|name| name.ends_with(".UnknownMethod")
                    || name.ends_with(".UnknownInterface")
                    || name.ends_with(".InvalidArgs")),
            "the test called a method wrongly: {reply:?}"
        );
    }
    DBusProxy::new(&connect(&address).await?)
        .await?
        .get_id()
        .await?;

    Ok(())
}

/// Send `calls` from `conn`, one after the other, without waiting for any reply, and the replies to
/// them, which come in meanwhile: a client that didn't read them would hold the bus up itself.
async fn burst(conn: &Connection, calls: &[Message]) -> anyhow::Result<Vec<Message>> {
    let mut messages = MessageStream::from(conn);
    let mut waiting: HashSet<_> = calls
        .iter()
        .map(|call| call.primary_header().serial_num())
        .collect();
    let mut replies = Vec::with_capacity(calls.len());
    let sending = async {
        for call in calls {
            conn.send(call).await?;
        }

        anyhow::Ok(())
    };
    let receiving = async {
        while !waiting.is_empty() {
            let msg = messages.next().await.context("the stream ended")??;
            if msg
                .header()
                .reply_serial()
                .is_some_and(|serial| waiting.remove(&serial))
            {
                replies.push(msg);
            }
        }

        anyhow::Ok(())
    };
    SharedRuntime::current()?
        .timeout(BURST_TIMEOUT, try_join(sending, receiving))
        .await
        .context("the bus held up its replies")??;

    Ok(replies)
}

async fn connect(address: &str) -> zbus::Result<Connection> {
    connection::Builder::address(address).build().await
}

/// A call to the bus, of `method` with `body`.
fn call_bus<B>(method: &str, body: &B) -> zbus::Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    call(BUS_NAME, method, body)
}

/// A call to the bus, of `method` of `interface` with `body`.
fn call<B>(interface: &str, method: &str, body: &B) -> zbus::Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    Message::method_call(PATH, method)
        .destination(BUS_NAME)
        .interface(interface)
        .build(body)
}

fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}

/// The name of the bus, and of its interface.
const BUS_NAME: &str = "org.freedesktop.DBus";
/// The path of the object of the bus.
const PATH: &str = "/org/freedesktop/DBus";
/// How many times the client of `every_call_to_the_bus_among_calls` calls each method, and how many
/// calls of `GetId` it makes each time, of the hundreds that it makes at once.
const ROUNDS: usize = 10;
const GET_IDS: usize = 20;
/// The names that the client of `every_call_to_the_bus_among_calls` asks for, and that another
/// client owns.
const OWN_NAME: &str = "org.zbus.Burst";
const OTHER_NAME: &str = "org.zbus.Burst.Other";
/// The match rule that the client of `every_call_to_the_bus_among_calls` adds and removes.
const RULE: &str = "type='signal',interface='org.zbus.Burst'";
/// How long the bus may take to answer all of the calls that a client makes at once.
const BURST_TIMEOUT: Duration = Duration::from_secs(10);
