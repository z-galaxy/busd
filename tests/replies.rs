use std::{
    env::temp_dir,
    future::Future,
    num::{NonZeroU32, NonZeroUsize},
    pin::pin,
    sync::Once,
};

use anyhow::{ensure, Context};
use busd::bus::Bus;
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
    fdo::{self, DBusProxy, MonitoringProxy},
    message, Connection, MatchRule, Message, MessageStream,
};
use zruntime::Event;

const PATH: &str = "/org/zbus/Replies";
const INTERFACE: &str = "org.zbus.Replies";
/// How many objects the buses of the tests keep for each user at most: few, so that the tests reach
/// the limit quickly. The clients of the tests are all of the same user.
const OBJECTS_PER_USER: usize = 16;

/// A reply that a peer sends to a caller, for a call that the caller made of another peer, doesn't
/// get to the caller, and the reply of the callee does.
#[test]
#[instrument]
#[timeout(30000)]
fn forged_reply() {
    init_tracing();

    zruntime::block_on(with_bus(forged_reply_));
}

/// A call that waits on a peer that goes gets a `NoReply` error from the bus.
#[test]
#[instrument]
#[timeout(30000)]
fn callee_leaves() {
    init_tracing();

    zruntime::block_on(with_bus(callee_leaves_));
}

/// A call that waits on a peer that becomes a monitor gets a `NoReply` error from the bus.
#[test]
#[instrument]
#[timeout(30000)]
fn callee_becomes_monitor() {
    init_tracing();

    zruntime::block_on(with_bus(callee_becomes_monitor_));
}

/// A call with the serial of another of its caller that waits on the same callee is refused, and
/// the callee only answers the first. A call that expects no reply doesn't wait on its callee. The
/// monitors get the call that the bus refuses all the same, and the error of the bus to it, and the
/// second answer of the callee, which goes nowhere else.
#[test]
#[instrument]
#[timeout(30000)]
fn same_serial() {
    init_tracing();

    zruntime::block_on(with_bus(same_serial_));
}

/// A call to a name that no peer has gets an error from the bus: `ServiceUnknown`, as the bus
/// starts no services, or `NameHasNoOwner` if the call asks for none to be started. A call that
/// expects no reply gets none. The monitors get each of the calls all the same.
#[test]
#[instrument]
#[timeout(30000)]
fn absent_destination() {
    init_tracing();

    zruntime::block_on(with_bus(absent_destination_));
}

/// The bus keeps no more than [`OBJECTS_PER_USER`] objects for a user: its clients, and the calls
/// to them that wait for replies. It answers a call beyond them with a `LimitsExceeded` error, and
/// passes it on to none of the callees, whichever the callee and whichever client of the user makes
/// the call, and refuses another client of the user. A reply to one of the calls that wait makes
/// room for another, and so does a client that goes, along with its calls that wait, but not one
/// that becomes a monitor.
#[test]
#[instrument]
#[timeout(30000)]
fn quota_exceeded() {
    init_tracing();

    zruntime::block_on(with_bus(quota_exceeded_));
}

async fn forged_reply_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let (callee, mut calls) = callee(&address).await?;
    let forger = connect(&address).await?;

    let calling = caller.call_method(callee.unique_name(), PATH, Some(INTERFACE), "Call", &());
    let answering = async {
        let call = calls.try_next().await?.context("no call")?;
        // The forger knows the serial, as one that guesses it right does.
        let forged = Message::method_return(&call.header()).build(&"forged")?;
        forger.send(&forged).await?;
        // By the time that the bus answers the forger, it has passed the forged reply on, if it
        // does at all.
        DBusProxy::new(&forger).await?.get_id().await?;
        let reply = Message::method_return(&call.header()).build(&"genuine")?;
        callee.send(&reply).await?;

        anyhow::Ok(())
    };
    let (reply, answered) = join(calling, answering).await;
    answered?;
    let reply: String = reply?.body().deserialize()?;
    ensure!(reply == "genuine", "the caller got the `{reply}` reply");

    Ok(())
}

async fn callee_leaves_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let (callee, mut calls) = callee(&address).await?;

    let calling = caller.call_method(callee.unique_name(), PATH, Some(INTERFACE), "Call", &());
    let leaving = async {
        calls.try_next().await?.context("no call")?;
        drop(calls);
        callee.clone().close().await?;

        anyhow::Ok(())
    };
    let (reply, left) = join(calling, leaving).await;
    left?;
    ensure!(
        matches!(reply, Err(zbus::Error::FDO(ref e)) if matches!(**e, fdo::Error::NoReply(_))),
        "the caller got {reply:?}"
    );

    Ok(())
}

async fn callee_becomes_monitor_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let (callee, mut calls) = callee(&address).await?;

    let calling = caller.call_method(callee.unique_name(), PATH, Some(INTERFACE), "Call", &());
    let becoming = async {
        calls.try_next().await?.context("no call")?;
        MonitoringProxy::new(&callee)
            .await?
            .become_monitor(&[], 0)
            .await?;

        anyhow::Ok(())
    };
    let (reply, became) = join(calling, becoming).await;
    became?;
    ensure!(
        matches!(reply, Err(zbus::Error::FDO(ref e)) if matches!(**e, fdo::Error::NoReply(_))),
        "the caller got {reply:?}"
    );

    Ok(())
}

async fn same_serial_(address: String) -> anyhow::Result<()> {
    let monitor = connect(&address).await?;
    let mut monitored = MessageStream::from(&monitor);
    MonitoringProxy::new(&monitor)
        .await?
        .become_monitor(&[], 0)
        .await?;
    let caller = connect(&address).await?;
    let mut replies = MessageStream::from(&caller);
    let (callee, mut calls) = callee(&address).await?;
    let callee_name = callee.unique_name().unwrap();
    let serial = NonZeroU32::new(7).unwrap();
    let call = |member| {
        Message::method_call(PATH, member)
            .destination(callee_name)
            .interface(INTERFACE)
            .serial(serial)
    };

    // Two calls that expect no reply, then two that do, all with the same serial.
    for member in ["First", "Second"] {
        let msg = call(member)
            .with_flags(message::Flags::NO_REPLY_EXPECTED)
            .build(&())?;
        caller.send(&msg).await?;
    }
    for member in ["Third", "Fourth"] {
        caller.send(&call(member).build(&())?).await?;
    }

    // The callee gets all but the last, which the bus refuses.
    for member in ["First", "Second", "Third"] {
        let msg = calls.try_next().await?.context("no call")?;
        let header = msg.header();
        let got = header.member().unwrap();
        ensure!(
            got == member,
            "the callee got `{got}` rather than `{member}`"
        );
    }
    let refused = next_reply(&mut replies).await?;
    let header = refused.header();
    ensure!(
        header.message_type() == message::Type::Error
            && header.error_name().unwrap() == "org.freedesktop.DBus.Error.AccessDenied"
            && header.sender().unwrap() == busd::fdo::BUS_NAME
            && header.reply_serial() == Some(serial),
        "the caller got {refused:?}"
    );

    // The monitor got all four calls, and the error of the bus to the last after it.
    for member in ["First", "Second", "Third", "Fourth"] {
        let msg = loop {
            let msg = monitored.try_next().await?.context("no message")?;
            if msg.message_type() == message::Type::MethodCall
                && msg.header().interface().is_some_and(|i| i == INTERFACE)
            {
                break msg;
            }
        };
        let header = msg.header();
        let got = header.member().unwrap();
        ensure!(
            got == member,
            "the monitor got `{got}` rather than `{member}`"
        );
    }
    let error = loop {
        let msg = monitored.try_next().await?.context("no message")?;
        if msg.message_type() == message::Type::Error {
            break msg;
        }
    };
    ensure!(
        error.header().reply_serial() == Some(serial),
        "the monitor got {error:?}"
    );

    // The callee answers the third, which the caller gets, and that one only.
    let third = Message::method_call(PATH, "Third")
        .sender(caller.unique_name().unwrap())
        .serial(serial)
        .build(&())?;
    for answer in ["first", "second"] {
        let reply = Message::method_return(&third.header()).build(&answer)?;
        callee.send(&reply).await?;
    }
    let reply = next_reply(&mut replies).await?;
    let answer: String = reply.body().deserialize()?;
    ensure!(answer == "first", "the caller got the `{answer}` reply");
    for answer in ["first", "second"] {
        let reply = loop {
            let msg = monitored.try_next().await?.context("no message")?;
            if msg.message_type() == message::Type::MethodReturn
                && msg.header().sender() == Some(callee_name)
            {
                break msg;
            }
        };
        let got: String = reply.body().deserialize()?;
        ensure!(
            got == answer,
            "the monitor got the `{got}` reply rather than `{answer}`"
        );
    }
    // By the time that the bus answers the callee, it has passed the second reply on, if it does at
    // all, and the caller gets the reply of the bus to its own call after it.
    DBusProxy::new(&callee).await?.get_id().await?;
    DBusProxy::new(&caller).await?.get_id().await?;
    let reply = next_reply(&mut replies).await?;
    let header = reply.header();
    ensure!(
        header.sender().unwrap() == busd::fdo::BUS_NAME,
        "the caller got {reply:?}"
    );

    Ok(())
}

async fn absent_destination_(address: String) -> anyhow::Result<()> {
    let monitor = connect(&address).await?;
    let mut monitored = MessageStream::from(&monitor);
    MonitoringProxy::new(&monitor)
        .await?
        .become_monitor(&[], 0)
        .await?;
    let caller = connect(&address).await?;
    let mut replies = MessageStream::from(&caller);
    // No client of the bus ever gets this unique name, and no one owns the well-known one.
    let destinations = [":busd.4294967295", "org.zbus.Replies.Absent"];
    let call = |destination, flags| {
        Message::method_call(PATH, "Call")
            .destination(destination)
            .interface(INTERFACE)
            .with_flags(flags)
            .build(&())
    };

    for destination in destinations {
        // A call that expects no reply goes first, so that an error to it would come before the
        // errors to the calls after it, which the bus sends in the order of the calls.
        let msg = call(destination, message::Flags::NO_REPLY_EXPECTED)?;
        caller.send(&msg).await?;
        for (flags, expected) in [
            (
                message::Flags::empty(),
                "org.freedesktop.DBus.Error.ServiceUnknown",
            ),
            (
                message::Flags::NO_AUTO_START,
                "org.freedesktop.DBus.Error.NameHasNoOwner",
            ),
        ] {
            let msg = call(destination, flags)?;
            caller.send(&msg).await?;
            let error = next_reply(&mut replies).await?;
            let header = error.header();
            ensure!(
                header.message_type() == message::Type::Error
                    && header.error_name().unwrap() == expected
                    && header.sender().unwrap() == busd::fdo::BUS_NAME
                    && header.reply_serial() == Some(msg.primary_header().serial_num()),
                "the call to `{destination}` with {flags:?} got {error:?}"
            );
        }
    }

    // The monitor got the three calls to each destination.
    for destination in destinations {
        for _ in 0..3 {
            let msg = loop {
                let msg = monitored.try_next().await?.context("no message")?;
                if msg.message_type() == message::Type::MethodCall
                    && msg.header().member().is_some_and(|member| member == "Call")
                {
                    break msg;
                }
            };
            let header = msg.header();
            let got = header.destination().map(ToString::to_string);
            ensure!(
                got.as_deref() == Some(destination),
                "the monitor got a call to {got:?} rather than to `{destination}`"
            );
        }
    }

    Ok(())
}

async fn quota_exceeded_(address: String) -> anyhow::Result<()> {
    let caller = connect(&address).await?;
    let mut replies = MessageStream::from(&caller);
    let other = connect(&address).await?;
    let mut other_replies = MessageStream::from(&other);
    let (other_callee, mut other_calls) = callee(&address).await?;
    let mut owner_changes = owner_changes(&other_callee).await?;
    let (callee, mut calls) = callee(&address).await?;
    let call = |callee: &Connection, member, flags| {
        Message::method_call(PATH, member)
            .destination(callee.unique_name().unwrap())
            .interface(INTERFACE)
            .with_flags(flags)
            .build(&())
    };
    // The four clients, all of the same user, take four of its objects, and the calls that wait
    // for replies the others.
    let room = OBJECTS_PER_USER - 4;

    // The callee takes the calls as they come, and answers none of them.
    let calling = async {
        for _ in 0..room {
            let msg = call(&callee, "Waits", message::Flags::empty())?;
            caller.send(&msg).await?;
        }

        anyhow::Ok(())
    };
    let taking = async {
        let first = calls.try_next().await?.context("no call")?;
        for _ in 1..room {
            calls.try_next().await?.context("no call")?;
        }

        anyhow::Ok(first)
    };
    let (called, first) = join(calling, taking).await;
    called?;
    let first = first?;

    // One more call, to either callee, from either client of the user, gets an error from the bus.
    for callee in [&callee, &other_callee] {
        let msg = call(callee, "Beyond", message::Flags::empty())?;
        exceeds_quota(&caller, &mut replies, msg).await?;
    }
    let msg = call(&callee, "Other", message::Flags::empty())?;
    exceeds_quota(&other, &mut other_replies, msg).await?;
    // Nor does the user get another connection.
    ensure!(
        connect(&address).await.is_err(),
        "the bus took another connection of the user"
    );

    // The reply to a call that waits gets to the caller, and makes room for another call, which
    // goes on to its callee, as those that the bus refused before it didn't.
    callee
        .send(&Message::method_return(&first.header()).build(&())?)
        .await?;
    let reply = next_reply(&mut replies).await?;
    ensure!(
        reply.message_type() == message::Type::MethodReturn,
        "the caller got {reply:?}"
    );
    caller
        .send(&call(&callee, "Again", message::Flags::empty())?)
        .await?;
    expect_call(&mut calls, "Again").await?;
    // A call that expects no reply takes no room.
    caller
        .send(&call(
            &other_callee,
            "NoReply",
            message::Flags::NO_REPLY_EXPECTED,
        )?)
        .await?;
    expect_call(&mut other_calls, "NoReply").await?;

    // A client that goes takes its object with it, and makes room for another client.
    let other_name = other.unique_name().unwrap().to_string();
    drop(other_replies);
    other.close().await?;
    gone(&mut owner_changes, &other_name).await?;
    let newcomer = connect(&address).await?;
    // A client that becomes a monitor stays an object of its user.
    MonitoringProxy::new(&newcomer)
        .await?
        .become_monitor(&[], 0)
        .await?;
    ensure!(
        connect(&address).await.is_err(),
        "the bus took another connection of the user"
    );

    // A caller that goes takes its calls that wait with it, as well as its object, and makes room
    // for a client to make as many calls again, but no more.
    let caller_name = caller.unique_name().unwrap().to_string();
    drop(replies);
    caller.close().await?;
    gone(&mut owner_changes, &caller_name).await?;
    let late = connect(&address).await?;
    let mut late_replies = MessageStream::from(&late);
    for _ in 0..room {
        late.send(&call(&callee, "Late", message::Flags::empty())?)
            .await?;
        expect_call(&mut calls, "Late").await?;
    }
    let msg = call(&callee, "Beyond", message::Flags::empty())?;
    exceeds_quota(&late, &mut late_replies, msg).await?;

    Ok(())
}

/// The `NameOwnerChanged` signals of the bus, as `conn` gets them.
async fn owner_changes(conn: &Connection) -> anyhow::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .build()?;

    Ok(MessageStream::for_match_rule(rule, conn, NonZeroUsize::new(1024)).await?)
}

/// Wait for the bus to say, in `owner_changes`, that the client of the unique name `name` went.
async fn gone(owner_changes: &mut MessageStream, name: &str) -> anyhow::Result<()> {
    loop {
        let msg = owner_changes.try_next().await?.context("no signal")?;
        let body = msg.body();
        let (changed, _, new_owner): (&str, &str, &str) = body.deserialize()?;
        if changed == name && new_owner.is_empty() {
            return Ok(());
        }
    }
}

/// Have `conn` send `msg`, which the bus refuses, as the quota of the user of `conn` is exceeded,
/// and take the error of the bus to it out of `replies`.
async fn exceeds_quota(
    conn: &Connection,
    replies: &mut MessageStream,
    msg: Message,
) -> anyhow::Result<()> {
    conn.send(&msg).await?;
    let error = next_reply(replies).await?;
    let header = error.header();
    ensure!(
        header.message_type() == message::Type::Error
            && header.error_name().unwrap() == "org.freedesktop.DBus.Error.LimitsExceeded"
            && header.sender().unwrap() == busd::fdo::BUS_NAME
            && header.reply_serial() == Some(msg.primary_header().serial_num()),
        "the call beyond the quota got {error:?}"
    );

    Ok(())
}

/// Take the next call out of `calls`, which must be of `member`.
async fn expect_call(calls: &mut MessageStream, member: &str) -> anyhow::Result<()> {
    let msg = calls.try_next().await?.context("no call")?;
    let header = msg.header();
    let got = header.member().unwrap();
    ensure!(
        got == member,
        "the callee got `{got}` rather than `{member}`"
    );

    Ok(())
}

/// A connection that gets the calls of the tests, which it doesn't answer of its own accord.
async fn callee(address: &str) -> anyhow::Result<(Connection, MessageStream)> {
    let callee = connect(address).await?;
    let rule = MatchRule::builder()
        .msg_type(message::Type::MethodCall)
        .interface(INTERFACE)
        .build()?;
    let calls = MessageStream::for_match_rule(rule, &callee, None).await?;

    Ok((callee, calls))
}

/// The next reply, or error, in `msgs`.
async fn next_reply(msgs: &mut MessageStream) -> anyhow::Result<Message> {
    loop {
        let msg = msgs.try_next().await?.context("no reply")?;
        if matches!(
            msg.message_type(),
            message::Type::MethodReturn | message::Type::Error
        ) {
            return Ok(msg);
        }
    }
}

async fn connect(address: &str) -> zbus::Result<Connection> {
    connection::Builder::address(address).build().await
}

/// Set the tracing subscriber up, once for all the tests of this file, which run in one process.
fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}

/// Run `test` against a bus of its own, at an address that it is given, which keeps
/// [`OBJECTS_PER_USER`] objects for each user at most.
async fn with_bus<F, T>(test: F)
where
    F: FnOnce(String) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    let mut bus = Bus::for_address(Some(&address)).await.unwrap();
    bus.set_max_objects_per_user(OBJECTS_PER_USER);
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
