use anyhow::ensure;
use busd::testing::{init_tracing, with_bus};
use futures_util::TryStreamExt;
use ntest::timeout;
use tracing::instrument;
use zbus::{
    connection,
    fdo::{DBusProxy, RequestNameFlags},
    message, MatchRule, MessageStream,
};

const NAME: &str = "org.zbus.MatchRules";
const PATH: &str = "/org/zbus/MatchRules";
const INTERFACE: &str = "org.zbus.MatchRules";

/// A signal sent to a well-known name goes to the owner of the name, whatever its match rules.
#[test]
#[instrument]
#[timeout(30000)]
fn signals_to_well_known_names() {
    init_tracing();

    zruntime::block_on(with_bus(signals_to_well_known_names_));
}

async fn signals_to_well_known_names_(address: String) -> anyhow::Result<()> {
    // The owner of a name, with no match rule for the signals of the test.
    let owner = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let owner_name = owner.unique_name().unwrap().to_owned();
    DBusProxy::new(&owner)
        .await?
        .request_name(NAME.try_into()?, RequestNameFlags::empty())
        .await?;
    let mut received = MessageStream::from(&owner);

    // A signal sent to the name, then one sent to the unique name of its owner.
    let emitter = connection::Builder::address(address.as_str())
        .build()
        .await?;
    for (destination, member) in [
        (NAME, "ToWellKnownName"),
        (owner_name.as_str(), "ToUniqueName"),
    ] {
        emitter
            .emit_signal(Some(destination), PATH, INTERFACE, member, &())
            .await?;
    }

    // The owner gets both, in that order.
    let mut members = vec![];
    while members.last().map(String::as_str) != Some("ToUniqueName") {
        let msg = received.try_next().await?.unwrap();
        let header = msg.header();
        if header.interface().is_some_and(|i| i.as_str() == INTERFACE) {
            members.push(header.member().unwrap().to_string());
        }
    }
    ensure!(
        members == ["ToWellKnownName", "ToUniqueName"],
        "the owner got {members:?}"
    );

    Ok(())
}

/// A signal with a destination goes to no one else, whatever their match rules.
#[test]
#[instrument]
#[timeout(30000)]
fn signals_to_others() {
    init_tracing();

    zruntime::block_on(with_bus(signals_to_others_));
}

async fn signals_to_others_(address: String) -> anyhow::Result<()> {
    // A client with a match rule that matches every signal of the test.
    let snooper = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    let mut snooped = MessageStream::for_match_rule(rule, &snooper, None).await?;

    // And one that owns a name.
    let owner = connection::Builder::address(address.as_str())
        .build()
        .await?;
    let owner_name = owner.unique_name().unwrap().to_owned();
    DBusProxy::new(&owner)
        .await?
        .request_name(NAME.try_into()?, RequestNameFlags::empty())
        .await?;

    // A signal sent to the name, one sent to the unique name of its owner, and a broadcast one.
    let emitter = connection::Builder::address(address.as_str())
        .build()
        .await?;
    for (destination, member) in [
        (Some(NAME), "ToWellKnownName"),
        (Some(owner_name.as_str()), "ToUniqueName"),
        (None, "Broadcast"),
    ] {
        emitter
            .emit_signal(destination, PATH, INTERFACE, member, &())
            .await?;
    }

    // The snooper only gets the broadcast signal, which came last.
    let msg = snooped.try_next().await?.unwrap();
    let header = msg.header();
    let member = header.member().unwrap();
    ensure!(member == "Broadcast", "the snooper got `{member}` first");

    Ok(())
}
