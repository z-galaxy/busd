#![cfg(feature = "testing")]

use anyhow::ensure;
use busd::testing::{init_tracing, with_bus};
use futures_util::TryStreamExt;
use ntest::timeout;
use tracing::instrument;
use zbus::{
    connection,
    fdo::{DBusProxy, MonitoringProxy},
    message,
    proxy::CacheProperties,
    MatchRule, MessageStream,
};

/// Broadcast signals, which have no destination, reach the match rules that they match, and
/// leave the bus running.
#[test]
#[instrument]
#[timeout(30000)]
fn broadcast_signals() {
    init_tracing();

    busd::testing::block_on(with_bus(|address| async move {
        broadcast_signals_(&address).await
    }));
}

#[instrument]
async fn broadcast_signals_(address: &str) -> anyhow::Result<()> {
    // A monitor of signals alone, which has a match rule, unlike a monitor of every message.
    let monitor = connection::Builder::address(address).build().await?;
    let mut monitored = MessageStream::from(&monitor);
    let signals = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .build()?;
    MonitoringProxy::builder(&monitor)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .become_monitor(&[signals], 0)
        .await?;

    // A client that wants the signals that are sent to it.
    let client = connection::Builder::address(address).build().await?;
    let client_name = client.unique_name().unwrap().to_owned();
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.zbus.Broadcast")
        .destination(client_name.as_str())
        .build()?;
    let mut received = MessageStream::for_match_rule(rule, &client, None).await?;

    // A broadcast signal, then one that is sent to the client.
    let emitter = connection::Builder::address(address).build().await?;
    for (destination, member) in [(None, "Everyone"), (Some(client_name.as_str()), "Client")] {
        emitter
            .emit_signal(
                destination,
                "/org/zbus/Broadcast",
                "org.zbus.Broadcast",
                member,
                &(),
            )
            .await?;
    }

    // The rule of the client matches both, as with dbus-broker and zbus's own matching.
    for expected in ["Everyone", "Client"] {
        let msg = received.try_next().await?.unwrap();
        let header = msg.header();
        let member = header.member().unwrap();
        ensure!(
            member == expected,
            "the client got `{member}` rather than `{expected}`"
        );
    }

    // The monitor gets the broadcast signal, among others.
    loop {
        let msg = monitored.try_next().await?.unwrap();
        let header = msg.header();
        if header
            .member()
            .is_some_and(|member| member.as_str() == "Everyone")
        {
            break;
        }
    }

    // And the bus still answers.
    DBusProxy::new(&emitter).await?.get_id().await?;

    Ok(())
}
