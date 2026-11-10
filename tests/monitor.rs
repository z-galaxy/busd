use std::{
    collections::HashMap,
    env::temp_dir,
    os::unix::net::UnixStream,
    pin::pin,
    sync::{Arc, Once},
};

use anyhow::{ensure, Context};
use busd::{bus::Bus, peer::SEND_TIMEOUT, peers::Peers};
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
    fdo::{
        self, DBusProxy, MonitoringProxy, NameAcquired, NameLost, NameOwnerChanged,
        RequestNameFlags, RequestNameReply,
    },
    message,
    names::BusName,
    proxy::CacheProperties,
    Connection, MessageStream, OwnedGuid,
};
use zruntime::Event;

#[test]
#[instrument]
#[timeout(30000)]
fn become_monitor() {
    init_tracing();

    zruntime::block_on(async {
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

        let ret = become_monitor_client(&address).await;
        stop.notify(usize::MAX);
        let bus = handle.await.unwrap();
        bus.cleanup().await.unwrap();
        ret.unwrap();
    });
}

/// Only the peers of root and of the user of the bus may become monitors, or update the activation
/// environment; any other peer is refused, and stays an ordinary peer.
#[test]
#[instrument]
#[timeout(30000)]
fn unprivileged() {
    init_tracing();

    zruntime::block_on(async {
        let s = Alphanumeric.sample_string(&mut rng(), 10);
        let path = temp_dir().join(s);
        let address = format!("unix:path={}", path.display());
        let mut bus = Bus::for_address(Some(&address)).await.unwrap();
        let peers = bus.peers().clone();
        let guid = bus.guid().clone();
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

        let ret = unprivileged_clients(&address, &peers, &guid).await;
        stop.notify(usize::MAX);
        let bus = handle.await.unwrap();
        bus.cleanup().await.unwrap();
        ret.unwrap();
    });
}

#[instrument(skip(peers, guid))]
async fn unprivileged_clients(
    address: &str,
    peers: &Arc<Peers>,
    guid: &OwnedGuid,
) -> anyhow::Result<()> {
    // A client of a user that is neither root nor the user of the bus.
    let our_uid = rustix::process::getuid().as_raw();
    let conn = connect_as(peers, guid, 1_000_001, our_uid + 1).await?;
    let monitoring = MonitoringProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let result = monitoring.become_monitor(&[], 0).await;
    ensure!(
        matches!(result, Err(fdo::Error::AccessDenied(_))),
        "expected `AccessDenied` from `BecomeMonitor`, got {result:?}"
    );
    let dbus = DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let result = dbus.update_activation_environment(HashMap::new()).await;
    ensure!(
        matches!(result, Err(fdo::Error::AccessDenied(_))),
        "expected `AccessDenied` from `UpdateActivationEnvironment`, got {result:?}"
    );
    // It is still an ordinary peer, which a monitor is not.
    dbus.get_id().await.context("`GetId` after the refusals")?;
    let reply = dbus
        .request_name(
            "org.z-galaxy.UnprivilegedTest".try_into()?,
            RequestNameFlags::DO_NOT_QUEUE,
        )
        .await?;
    ensure!(
        reply == RequestNameReply::PrimaryOwner,
        "expected `PrimaryOwner`, got {reply:?}"
    );

    // A client of root may become a monitor.
    let conn = connect_as(peers, guid, 1_000_002, 0).await?;
    let result = MonitoringProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .become_monitor(&[], 0)
        .await;
    ensure!(
        result.is_ok(),
        "expected root to become a monitor, got {result:?}"
    );

    // A client of the user of the bus passes the check, and fails for want of service activation.
    let conn = connection::Builder::address(address).build().await?;
    let result = DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .update_activation_environment(HashMap::new())
        .await;
    ensure!(
        matches!(result, Err(fdo::Error::Failed(_))),
        "expected `Failed` from `UpdateActivationEnvironment`, got {result:?}"
    );

    Ok(())
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

#[instrument]
async fn become_monitor_client(address: &str) -> anyhow::Result<()> {
    // Create a monitor that wants all messages.
    let conn = connection::Builder::address(address).build().await?;
    let mut msg_stream = MessageStream::from(&conn);
    MonitoringProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .become_monitor(&[], 0)
        .await?;
    let unique_name = BusName::from(conn.unique_name().unwrap().clone());
    drop(conn);

    // Signals for the monitor loosing its unique name.
    let signal = loop {
        let msg = msg_stream.try_next().await?.unwrap();
        // Ignore other messages (e.g `BecomeMonitor` method & reply)
        if let Some(signal) = NameOwnerChanged::from_message(msg) {
            break signal;
        }
    };
    let args = signal.args()?;
    ensure!(
        *args.name() == unique_name,
        "expected NameOwnerChanged signal for monitor's unique_name"
    );
    let signal = loop {
        let msg = msg_stream.try_next().await?.unwrap();
        if let Some(signal) = NameLost::from_message(msg) {
            break signal;
        }
    };
    let args = signal.args()?;
    ensure!(
        *args.name() == unique_name,
        "expected NameLost signal for monitor's unique_name"
    );

    // Now a client that calls a method that triggers a signal.
    let conn = connection::Builder::address(address).build().await?;
    let name = "org.z-galaxy.MonitorTest";
    DBusProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .request_name(
            name.try_into()?,
            RequestNameFlags::REPLACE_EXISTING | RequestNameFlags::DO_NOT_QUEUE,
        )
        .await?;

    // Now monitor should have received all messages.
    let mut num_received = 0;
    let mut hello_serial = None;
    let mut request_name_serial = None;
    while num_received < 8 {
        let msg = msg_stream.try_next().await?.unwrap();
        let header = msg.header();
        let member = header.member();

        match msg.message_type() {
            message::Type::MethodCall => match member.unwrap().as_str() {
                "Hello" => {
                    hello_serial = Some(msg.primary_header().serial_num());
                }
                "RequestName" => {
                    request_name_serial = Some(msg.primary_header().serial_num());
                }
                method => panic!("unexpected method call: {method}"),
            },
            message::Type::MethodReturn => {
                let serial = header.reply_serial();
                if serial == hello_serial {
                    hello_serial = None;
                } else if serial == request_name_serial {
                    request_name_serial = None;
                } else {
                    panic!("unexpected method return: {}", serial.unwrap());
                }
            }
            message::Type::Signal => {
                if let Some(signal) = NameOwnerChanged::from_message(msg.clone()) {
                    let args = signal.args()?;
                    ensure!(
                        *args.name() == BusName::from(conn.unique_name().unwrap())
                            || *args.name() == name,
                        "expected NameOwnerChanged signal for one of client's names"
                    );
                } else if let Some(signal) = NameAcquired::from_message(msg) {
                    let args = signal.args()?;
                    ensure!(
                        *args.name() == BusName::from(conn.unique_name().unwrap())
                            || *args.name() == name,
                        "expected NameAcquired signal for one of client's names"
                    );
                }
            }
            _ => panic!("unexpected message type: {:?}", msg.message_type()),
        }

        num_received += 1;
    }

    Ok(())
}

/// Set the tracing subscriber up, once for all the tests of this file, which run in one process.
fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}
