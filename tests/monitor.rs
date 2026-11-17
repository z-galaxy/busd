use std::collections::HashMap;

use anyhow::{ensure, Context};
use busd::{
    bus::Bus,
    peers::Peers,
    testing::{connect_as, init_tracing, unix_address, with, with_bus},
};
use futures_util::{StreamExt, TryStreamExt};
use ntest::timeout;
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
    MessageStream, OwnedGuid,
};

#[test]
#[instrument]
#[timeout(30000)]
fn become_monitor() {
    init_tracing();

    zruntime::block_on(with_bus(|address| async move {
        become_monitor_client(&address).await
    }));
}

/// A name of a peer that becomes a monitor passes on to the peer that queued for it, and both the
/// signal about its owner and the one to its new owner say so.
#[test]
#[instrument]
#[timeout(30000)]
fn name_of_a_monitor_passes_on() {
    init_tracing();

    zruntime::block_on(with_bus(|address| async move {
        name_of_a_monitor_passes_on_client(&address).await
    }));
}

/// Only the peers of root and of the user of the bus may become monitors, or update the activation
/// environment; any other peer is refused, and stays an ordinary peer.
#[test]
#[instrument]
#[timeout(30000)]
fn unprivileged() {
    init_tracing();

    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&unix_address())).await.unwrap();
        with(bus, |bus| async move {
            unprivileged_clients(bus.address(), bus.peers(), bus.guid()).await
        })
        .await;
    });
}

#[instrument(skip(peers, guid))]
async fn unprivileged_clients(
    address: &str,
    peers: &Peers,
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

async fn name_of_a_monitor_passes_on_client(address: &str) -> anyhow::Result<()> {
    let name = "org.z-galaxy.MonitorTest.Queued";
    let owner = connection::Builder::address(address).build().await?;
    let reply = DBusProxy::new(&owner)
        .await?
        .request_name(name.try_into()?, RequestNameFlags::empty())
        .await?;
    ensure!(reply == RequestNameReply::PrimaryOwner, "got {reply:?}");
    let queued = connection::Builder::address(address).build().await?;
    let queued_dbus = DBusProxy::builder(&queued)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let reply = queued_dbus
        .request_name(name.try_into()?, RequestNameFlags::empty())
        .await?;
    ensure!(reply == RequestNameReply::InQueue, "got {reply:?}");
    let mut changes = queued_dbus.receive_name_owner_changed().await?;
    let mut msgs = MessageStream::from(&queued);

    MonitoringProxy::builder(&owner)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .become_monitor(&[], 0)
        .await?;

    let changed = changes.next().await.context("no signal")?;
    let args = changed.args()?;
    let owner_name = owner.unique_name().context("no unique name")?.as_str();
    let queued_name = queued.unique_name().context("no unique name")?;
    ensure!(
        *args.name() == name
            && args.old_owner().as_deref() == Some(owner_name)
            && args.new_owner().as_deref() == Some(queued_name.as_str()),
        "unexpected change of owner: {args:?}"
    );
    loop {
        let msg = msgs.try_next().await?.context("no message")?;
        let Some(acquired) = NameAcquired::from_message(msg) else {
            continue;
        };
        ensure!(
            *acquired.args()?.name() == name,
            "`NameAcquired` of another name"
        );
        break;
    }
    let now = queued_dbus.get_name_owner(name.try_into()?).await?;
    ensure!(now == *queued_name, "the owner is {now}");

    Ok(())
}
