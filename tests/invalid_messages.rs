use std::{env::temp_dir, future::Future, pin::pin, sync::Once};

use anyhow::ensure;
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
    connection, fdo::DBusProxy, message, names::OwnedUniqueName, Connection, MatchRule, Message,
    MessageStream,
};
use zruntime::Event;

const PATH: &str = "/org/zbus/InvalidMessages";
const INTERFACE: &str = "org.zbus.InvalidMessages";

/// A client that sends a message whose body doesn't hold what its signature says gets
/// disconnected, and the message goes nowhere.
#[test]
#[instrument]
#[timeout(30000)]
fn invalid_body() {
    init_tracing();

    zruntime::block_on(with_bus(invalid_body_));
}

/// A client that sends a message without a field that its type requires gets disconnected.
#[test]
#[instrument]
#[timeout(30000)]
fn missing_field() {
    init_tracing();

    zruntime::block_on(with_bus(missing_field_));
}

/// A client that sends a signal with the interface that the specification reserves for local use
/// gets disconnected, and the signal goes nowhere.
#[test]
#[instrument]
#[timeout(30000)]
fn local_interface() {
    init_tracing();

    zruntime::block_on(with_bus(local_interface_));
}

/// A client that sends a signal with the path that the specification reserves for local use gets
/// disconnected, and the signal goes nowhere.
#[test]
#[instrument]
#[timeout(30000)]
fn local_path() {
    init_tracing();

    zruntime::block_on(with_bus(local_path_));
}

/// A client that sends a method call with the interface that the specification reserves for local
/// use gets disconnected, and the call goes nowhere.
#[test]
#[instrument]
#[timeout(30000)]
fn local_method_call() {
    init_tracing();

    zruntime::block_on(with_bus(local_method_call_));
}

async fn invalid_body_(address: String) -> anyhow::Result<()> {
    let recipient = connect(&address).await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    let mut signals = MessageStream::for_match_rule(rule, &recipient, None).await?;
    let sender = connect(&address).await?;

    // A boolean of 1 gets to the recipient, and one of 2 has the bus disconnect its sender.
    sender.send(&signal("Valid", &[1, 0, 0, 0])?).await?;
    sender.send(&signal("Invalid", &[2, 0, 0, 0])?).await?;
    sender.closed().await;
    let other = connect(&address).await?;
    other.send(&signal("Next", &[0, 0, 0, 0])?).await?;
    for member in ["Valid", "Next"] {
        let msg = signals.try_next().await?.unwrap();
        let header = msg.header();
        let got = header.member().unwrap();
        ensure!(
            got == member,
            "the recipient got `{got}` rather than `{member}`"
        );
    }
    DBusProxy::new(&recipient).await?.get_id().await?;

    Ok(())
}

async fn missing_field_(address: String) -> anyhow::Result<()> {
    let recipient = connect(&address).await?;
    let sender = connect(&address).await?;

    // A reply without the serial of the call that it replies to.
    let call = Message::method_call(PATH, "Call")
        .sender(recipient.unique_name().unwrap())
        .build(&())?;
    let reply = Message::method_return(&call.header())
        .reply_serial(None)
        .build(&())?;
    sender.send(&reply).await?;
    sender.closed().await;
    DBusProxy::new(&recipient).await?.get_id().await?;

    Ok(())
}

async fn local_interface_(address: String) -> anyhow::Result<()> {
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .path(PATH)
        .build()?;
    let local = Message::signal(PATH, LOCAL_INTERFACE, "Disconnected").build(&())?;

    local_signal(&address, rule, &local).await
}

async fn local_path_(address: String) -> anyhow::Result<()> {
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .build()?;
    let local = Message::signal(LOCAL_PATH, INTERFACE, "Disconnected").build(&())?;

    local_signal(&address, rule, &local).await
}

async fn local_method_call_(address: String) -> anyhow::Result<()> {
    let recipient = connect(&address).await?;
    let mut messages = MessageStream::from(&recipient);
    let sender = connect(&address).await?;
    let name = recipient.unique_name().unwrap();

    sender.send(&valid_call(name, "Valid")?).await?;
    let local = Message::method_call(PATH, "Call")
        .destination(name)
        .interface(LOCAL_INTERFACE)
        .with_flags(message::Flags::NO_REPLY_EXPECTED)
        .build(&())?;
    sender.send(&local).await?;
    sender.closed().await;
    let other = connect(&address).await?;
    other.send(&valid_call(name, "Next")?).await?;
    for member in ["Valid", "Next"] {
        let msg = loop {
            let msg = messages.try_next().await?.unwrap();
            if msg.message_type() == message::Type::MethodCall {
                break msg;
            }
        };
        let header = msg.header();
        let got = header.member().unwrap();
        ensure!(
            got == member,
            "the recipient got the call `{got}` rather than `{member}`"
        );
    }
    DBusProxy::new(&recipient).await?.get_id().await?;

    Ok(())
}

/// A recipient that matches `rule` gets the signals that a client sends before `local`, which the
/// bus disconnects the client for, and those that another one sends after it, and nothing between.
async fn local_signal(
    address: &str,
    rule: MatchRule<'static>,
    local: &Message,
) -> anyhow::Result<()> {
    let recipient = connect(address).await?;
    let mut signals = MessageStream::for_match_rule(rule, &recipient, None).await?;
    let sender = connect(address).await?;

    sender.send(&signal("Valid", &[1, 0, 0, 0])?).await?;
    sender.send(local).await?;
    sender.closed().await;
    let other = connect(address).await?;
    other.send(&signal("Next", &[0, 0, 0, 0])?).await?;
    for member in ["Valid", "Next"] {
        let msg = signals.try_next().await?.unwrap();
        let header = msg.header();
        let got = header.member().unwrap();
        ensure!(
            got == member,
            "the recipient got `{got}` rather than `{member}`"
        );
    }
    DBusProxy::new(&recipient).await?.get_id().await?;

    Ok(())
}

/// A call of `member` to `name` that expects no reply.
fn valid_call(name: &OwnedUniqueName, member: &str) -> zbus::Result<Message> {
    Message::method_call(PATH, member)
        .destination(name)
        .interface(INTERFACE)
        .with_flags(message::Flags::NO_REPLY_EXPECTED)
        .build(&())
}

/// A signal for the recipients of the test, of `member`, whose body is a boolean of `bytes`.
fn signal(member: &str, bytes: &[u8]) -> zbus::Result<Message> {
    let builder = Message::signal(PATH, INTERFACE, member);
    // SAFETY: The body carries no file descriptors, and the bus is to check it.
    unsafe { builder.build_raw_body(bytes, "b", vec![]) }
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

/// The interface and the path that the specification reserves for the `Disconnected` signal that a
/// D-Bus library raises locally, which clients are not to send.
const LOCAL_INTERFACE: &str = "org.freedesktop.DBus.Local";
const LOCAL_PATH: &str = "/org/freedesktop/DBus/Local";
