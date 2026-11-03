use std::{
    env::temp_dir,
    fs::File,
    future::Future,
    io::{self, Read, Write},
    net::Shutdown,
    num::NonZeroUsize,
    os::{
        fd::{BorrowedFd, OwnedFd},
        unix::net::UnixStream,
    },
    pin::pin,
    sync::{Arc, Once},
    time::{Duration, Instant},
};

use anyhow::{ensure, Context};
use busd::bus::Bus;
use futures_util::{
    future::{join, select, try_join_all, Either},
    stream::StreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use tracing::instrument;
use zbus::{
    connection::{
        self,
        socket::{ReadHalf, Socket, Split, WriteHalf},
    },
    export::async_trait::async_trait,
    fdo::{self, DBusProxy, MonitoringProxy, RequestNameFlags, RequestNameReply},
    message,
    names::{BusName, UniqueName, WellKnownName},
    proxy::CacheProperties,
    wire::Fd,
    Connection, MatchRule, Message, MessageStream,
};
use zruntime::{AsyncIo, Event, Shared, SharedRuntime};
#[cfg(target_os = "linux")]
use {
    busd::peer::MAX_MESSAGE_FDS,
    rustix::net::{
        recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
    },
    std::{
        io::{IoSlice, IoSliceMut},
        mem::MaybeUninit,
        os::fd::AsFd,
    },
    zbus::names::OwnedUniqueName,
};

/// A client that stops reading its socket, while it is subscribed to signals, holds up those who
/// send to it for no longer than the time limit of the bus, which disconnects it then, and says
/// so. Its senders stay, the clients that read get every signal, in order, and the bus goes on
/// answering everyone else.
#[test]
#[instrument]
#[timeout(30000)]
fn stalled_client() {
    init_tracing();

    zruntime::block_on(with_bus(SHORT_TIMEOUT, stalled_client_));
}

/// While a sender waits on a client that stops reading, it holds up no one else: the bus lets
/// clients connect, and forwards the messages of the others, meanwhile.
#[test]
#[instrument]
#[timeout(30000)]
fn stall_holds_up_no_one_else() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, stall_holds_up_no_one_else_));
}

/// A client that stops reading for a while, but reads again before the time limit, holds up
/// those who send to it until then, and gets every message, in order.
#[test]
#[instrument]
#[timeout(30000)]
fn slow_client() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, slow_client_));
}

/// The bus removes a client that it disconnects, as it does any client that goes: it releases
/// the names that the client owns, takes it out of the queues for others, and says so.
#[test]
#[instrument]
#[timeout(30000)]
fn stalled_owner() {
    init_tracing();

    zruntime::block_on(with_bus(SHORT_TIMEOUT, stalled_owner_));
}

/// The bus disconnects a monitor that stops reading, as it does a peer, while the monitors that
/// read get every message, in order.
#[test]
#[instrument]
#[timeout(30000)]
fn stalled_monitor() {
    init_tracing();

    zruntime::block_on(with_bus(SHORT_TIMEOUT, stalled_monitor_));
}

/// Sends to a client that stops reading, from several senders at once, time out together, and the
/// bus disconnects the client once, whether it is a peer or a monitor, and none of the senders.
#[test]
#[instrument]
#[timeout(30000)]
fn concurrent_timeouts() {
    init_tracing();

    zruntime::block_on(with_bus(SHORT_TIMEOUT, concurrent_timeouts_));
}

/// Clients that stop reading, which the same signals go to, hold those signals up for the time
/// limit of the bus, all together, rather than for the time limit each: the bus waits on them at
/// once.
#[test]
#[instrument]
#[timeout(30000)]
fn stalled_subscribers() {
    init_tracing();

    zruntime::block_on(with_bus(SHORT_TIMEOUT, stalled_subscribers_));
}

/// A client that goes while the bus waits for it to read is removed once, and the bus carries on.
#[test]
#[instrument]
#[timeout(30000)]
fn leaving_client() {
    init_tracing();

    zruntime::block_on(with_bus(SHORT_TIMEOUT, leaving_client_));
}

/// The messages of each sender get to each of their recipients in the order that the sender sent
/// them, whether the recipient is a peer or a monitor, and whether the sender is a client or the
/// bus itself.
#[test]
#[instrument]
#[timeout(30000)]
fn ordering() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, ordering_));
}

/// A message that a client can't take, as it has file descriptors and the client didn't negotiate
/// passing them, doesn't hold up the messages after it.
#[test]
#[instrument]
#[timeout(30000)]
fn client_without_fds() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, client_without_fds_));
}

/// A call that a client can't take, as it has a file descriptor and the client didn't negotiate
/// passing them, gets an error from the bus, rather than wait for a reply that can't come.
#[test]
#[instrument]
#[timeout(30000)]
fn call_to_client_without_fds() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, call_to_client_without_fds_));
}

/// Messages that a client can't take, as they have file descriptors and the client didn't
/// negotiate passing them, don't hold up those that the bus sends the client along with them.
#[test]
#[instrument]
#[timeout(30000)]
fn client_without_fds_in_a_batch() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, client_without_fds_in_a_batch_));
}

/// A client that the bus can't write to anymore gets disconnected, even while the bus can still
/// read from it.
#[cfg(target_os = "linux")]
#[test]
#[instrument]
#[timeout(30000)]
fn unwritable_client() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, unwritable_client_));
}

/// A client that sends a message with more file descriptors than the bus can pass on gets
/// disconnected, rather than the recipient of the message.
///
/// The limit is Linux's, so the test runs on Linux only.
#[cfg(target_os = "linux")]
#[test]
#[instrument]
#[timeout(30000)]
fn too_many_fds() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, too_many_fds_));
}

/// A message that brings more file descriptors than its header declares gets to its recipient with
/// only those that it declares.
///
/// The recipient reads its socket itself, as a client of zbus would only take as many file
/// descriptors as the message declares, whatever comes along with it.
#[cfg(target_os = "linux")]
#[test]
#[instrument]
#[timeout(30000)]
fn undeclared_fds() {
    init_tracing();

    zruntime::block_on(with_bus(LONG_TIMEOUT, undeclared_fds_));
}

/// A client that the bus wrote part of a message to, when the time limit came, gets nothing more
/// on its socket: neither the rest of the message, nor the messages that waited behind it.
#[cfg(target_os = "linux")]
#[test]
#[instrument]
#[timeout(30000)]
fn partial_write() {
    init_tracing();

    zruntime::block_on(with_bus(PARTIAL_WRITE_TIMEOUT, partial_write_));
}

async fn stalled_client_(address: String) -> anyhow::Result<()> {
    let (stalled, _signals) = stalled_subscriber(&address).await?;
    let stalled_name = stalled.unique_name().unwrap().to_owned();
    let observer = connect(&address).await?;
    let mut owner_changes = owner_changes(&observer, &stalled_name).await?;
    let reader = connect(&address).await?;
    let mut floods = floods(&reader).await?;

    // Far more signals than the sockets between the bus and the stalled client can take.
    let flooder = connect(&address).await?;
    let started = Instant::now();
    flood(&flooder, FLOOD_SIGNALS).await?;

    // The bus disconnects the stalled client, once it has waited on it for the time limit.
    disconnected(&mut owner_changes, &stalled_name).await?;
    let waited = started.elapsed();
    ensure!(
        waited >= SHORT_TIMEOUT,
        "the stalled client went after {waited:?}"
    );

    // The client that reads gets every signal, in order.
    in_order(&mut floods, FLOOD_SIGNALS).await?;

    // The bus still answers everyone else, the flooder and a client that connects now included.
    DBusProxy::new(&flooder).await?.get_id().await?;
    let client = connect(&address).await?;
    let dbus = DBusProxy::new(&client).await?;
    let name = WellKnownName::try_from("org.zbus.Stalled")?;
    for _ in 0..10 {
        dbus.request_name(name.clone(), RequestNameFlags::empty())
            .await?;
        dbus.release_name(name.clone()).await?;
    }
    let newcomer = connect(&address).await?;
    DBusProxy::new(&newcomer).await?.get_id().await?;

    Ok(())
}

async fn stall_holds_up_no_one_else_(address: String) -> anyhow::Result<()> {
    // A client that stops reading the flood, and the bus waits on for far longer than this test
    // takes, along with the flooder that waits on it.
    let stalled = connect(&address).await?;
    let stalled_floods =
        MessageStream::for_match_rule(flood_rule()?, &stalled, Some(NonZeroUsize::MIN)).await?;
    let flooder = connect(&address).await?;
    let flooding = zruntime::spawn(async move {
        flood(&flooder, FLOOD_SIGNALS).await?;

        anyhow::Ok(flooder)
    });
    SharedRuntime::current()?.sleep(SHORT_TIMEOUT).await;
    ensure!(!flooding.is_finished(), "the flood didn't wait");

    // Meanwhile, which takes each lock of the bus to change what it holds: a client connects, asks
    // for a name and releases it, and gets a signal from another one, and a third one becomes a
    // monitor.
    let runtime = SharedRuntime::current()?;
    runtime
        .timeout(LONG_TIMEOUT / 4, async {
            let newcomer = connect(&address).await?;
            let dbus = DBusProxy::new(&newcomer).await?;
            let name = WellKnownName::try_from("org.zbus.Others")?;
            dbus.request_name(name.clone(), RequestNameFlags::empty())
                .await?;
            dbus.release_name(name).await?;
            let rule = MatchRule::builder()
                .msg_type(message::Type::Signal)
                .interface("org.zbus.Others")
                .build()?;
            let mut signals = MessageStream::for_match_rule(rule, &newcomer, None).await?;
            let other = connect(&address).await?;
            other
                .emit_signal(
                    newcomer.unique_name().map(|name| name.as_str()),
                    "/org/zbus/Others",
                    "org.zbus.Others",
                    "Hi",
                    &(),
                )
                .await?;
            signals.next().await.context("the stream ended")??;
            let (_monitor, _) = monitor(&address, &[]).await?;

            anyhow::Ok(())
        })
        .await
        .context("the bus held up the others")??;

    // Once the stalled client goes, the flood goes on, and the flooder stays.
    ensure!(!flooding.is_finished(), "the flood didn't wait");
    drop((stalled, stalled_floods));
    let flooder = flooding.await??;
    DBusProxy::new(&flooder).await?.get_id().await?;

    Ok(())
}

async fn slow_client_(address: String) -> anyhow::Result<()> {
    // The client takes one signal into its stream, and reads nothing more from its socket until
    // the stream is read.
    let slow = connect(&address).await?;
    let mut floods =
        MessageStream::for_match_rule(flood_rule()?, &slow, Some(NonZeroUsize::MIN)).await?;
    let flooder = connect(&address).await?;
    let flooding = zruntime::spawn(async move {
        let started = Instant::now();
        flood(&flooder, FLOOD_SIGNALS).await?;

        anyhow::Ok((flooder, started.elapsed()))
    });

    // The client reads again after a pause, well within the time limit.
    SharedRuntime::current()?.sleep(PAUSE).await;
    in_order(&mut floods, FLOOD_SIGNALS).await?;
    let (flooder, took) = flooding.await??;
    // The sockets can't take the flood, so the flooder had to wait for the client to read again.
    ensure!(took >= PAUSE / 2, "the flood took only {took:?}");

    // Neither the client nor the flooder got disconnected.
    DBusProxy::new(&slow).await?.get_id().await?;
    DBusProxy::new(&flooder).await?.get_id().await?;

    Ok(())
}

async fn stalled_owner_(address: String) -> anyhow::Result<()> {
    let observer = connect(&address).await?;
    let dbus = DBusProxy::new(&observer).await?;
    let queued = WellKnownName::try_from("org.zbus.Queued")?;
    dbus.request_name(queued.clone(), RequestNameFlags::empty())
        .await?;
    let mut changes = all_owner_changes(&observer).await?;

    // A client that owns two names, and waits in the queue for a third, stops reading.
    let stalled = connect(&address).await?;
    let stalled_name = stalled.unique_name().unwrap().to_owned();
    let owned = ["org.zbus.Owned1", "org.zbus.Owned2"];
    let stalled_dbus = DBusProxy::new(&stalled).await?;
    for name in owned {
        let reply = stalled_dbus
            .request_name(name.try_into()?, RequestNameFlags::empty())
            .await?;
        ensure!(
            reply == RequestNameReply::PrimaryOwner,
            "asking for `{name}` got {reply:?}"
        );
    }
    let reply = stalled_dbus
        .request_name(queued.clone(), RequestNameFlags::empty())
        .await?;
    ensure!(
        reply == RequestNameReply::InQueue,
        "asking for `{queued}` got {reply:?}"
    );
    let queue = dbus.list_queued_owners(queued.clone()).await?;
    ensure!(
        queue.contains(&stalled_name),
        "the queue for `{queued}` is {queue:?}"
    );
    let _signals = stall(&stalled).await?;
    let flooder = connect(&address).await?;
    flood(&flooder, FLOOD_SIGNALS).await?;

    // The bus releases the names of the client, and then its unique name.
    let mut released = vec![];
    loop {
        let (name, old_owner, new_owner) = next_owner_change(&mut changes).await?;
        if old_owner != stalled_name.as_str() {
            continue;
        }
        ensure!(new_owner.is_empty(), "`{name}` went to `{new_owner}`");
        if name == stalled_name.as_str() {
            break;
        }
        released.push(name);
    }
    released.sort();
    ensure!(released == owned, "the bus released {released:?}");

    // Nothing is left of the client: not its names, nor its place in the queue.
    for name in owned {
        ensure!(
            !dbus.name_has_owner(name.try_into()?).await?,
            "`{name}` still has an owner"
        );
    }
    let queue = dbus.list_queued_owners(queued.clone()).await?;
    ensure!(
        !queue.contains(&stalled_name),
        "the queue for `{queued}` is {queue:?}"
    );
    ensure!(
        !dbus.name_has_owner(BusName::from(&stalled_name)).await?,
        "`{stalled_name}` is still there"
    );
    let names = dbus.list_names().await?;
    ensure!(
        !names
            .iter()
            .any(|name| name.as_str() == stalled_name.as_str()),
        "`{stalled_name}` is still listed"
    );

    Ok(())
}

async fn stalled_monitor_(address: String) -> anyhow::Result<()> {
    // A monitor of every message, whose stream takes one: once a message is in it, the monitor
    // reads nothing more from its socket.
    let stalled = connection::Builder::address(address.as_str())
        .max_queued(NonZeroUsize::MIN)
        .build()
        .await?;
    become_monitor(&stalled, &[]).await?;
    let stalled_messages = MessageStream::from(&stalled);
    // A monitor of the flood, which reads all of it.
    let (_monitor, mut monitored) = monitor(&address, &[flood_rule()?]).await?;

    let flooder = connect(&address).await?;
    flood(&flooder, FLOOD_SIGNALS).await?;

    // The monitor that reads gets every signal, in order.
    in_order(&mut monitored, FLOOD_SIGNALS).await?;

    // The bus disconnected the stalled monitor: once the monitor reads again, it finds its
    // connection closed.
    drop(stalled_messages);
    closed(&stalled).await?;
    DBusProxy::new(&flooder).await?.get_id().await?;

    Ok(())
}

async fn concurrent_timeouts_(address: String) -> anyhow::Result<()> {
    let observer = connect(&address).await?;
    let mut changes = all_owner_changes(&observer).await?;

    // A peer that owns a name stops reading, and so does a monitor.
    let stalled = connect(&address).await?;
    let stalled_name = stalled.unique_name().unwrap().to_owned();
    let name = WellKnownName::try_from("org.zbus.Concurrent")?;
    DBusProxy::new(&stalled)
        .await?
        .request_name(name.clone(), RequestNameFlags::empty())
        .await?;
    let _signals = stall(&stalled).await?;
    let stalled_monitor = connection::Builder::address(address.as_str())
        .max_queued(NonZeroUsize::MIN)
        .build()
        .await?;
    become_monitor(&stalled_monitor, &[flood_rule()?]).await?;
    let stalled_messages = MessageStream::from(&stalled_monitor);

    // Two flooders, at once.
    let mut flooders = vec![];
    for _ in 0..2 {
        flooders.push(connect(&address).await?);
    }
    try_join_all(flooders.iter().map(|flooder| flood(flooder, FLOOD_SIGNALS))).await?;

    // The peer goes once: its name, and then itself.
    let mut gone = vec![];
    while gone.len() < 2 {
        let (changed, old_owner, new_owner) = next_owner_change(&mut changes).await?;
        if old_owner == stalled_name.as_str() {
            ensure!(new_owner.is_empty(), "`{changed}` went to `{new_owner}`");
            gone.push(changed);
        }
    }
    ensure!(
        gone == [name.as_str(), stalled_name.as_str()],
        "the bus said {gone:?} went"
    );
    no_more_changes_of(&mut changes, &stalled_name).await?;

    // So does the monitor, and neither flooder.
    drop(stalled_messages);
    closed(&stalled_monitor).await?;
    for flooder in &flooders {
        DBusProxy::new(flooder).await?.get_id().await?;
    }

    Ok(())
}

async fn stalled_subscribers_(address: String) -> anyhow::Result<()> {
    let observer = connect(&address).await?;
    let mut changes = all_owner_changes(&observer).await?;
    let mut stalled = vec![];
    for _ in 0..STALLED_SUBSCRIBERS {
        stalled.push(stalled_subscriber(&address).await?);
    }
    let mut left: Vec<_> = stalled
        .iter()
        .map(|(conn, _)| conn.unique_name().unwrap().to_string())
        .collect();

    let flooder = connect(&address).await?;
    let started = Instant::now();
    // The bus disconnects the stalled clients once it has waited on them for the time limit. They
    // are timed as they go, while the rest of the flood still goes through the bus, which takes a
    // while longer.
    let gone = async {
        while !left.is_empty() {
            let (changed, old_owner, new_owner) = next_owner_change(&mut changes).await?;
            if old_owner == changed && new_owner.is_empty() {
                left.retain(|name| *name != changed);
            }
        }

        anyhow::Ok(started.elapsed())
    };
    let (waited, flooded) = join(gone, flood(&flooder, FLOOD_SIGNALS)).await;
    flooded?;
    let waited = waited?;
    ensure!(
        waited >= SHORT_TIMEOUT,
        "the stalled clients went after {waited:?}"
    );
    // The bus waits at once on those that stop taking the flood at the same signal, while waiting
    // on them in turn would take the time limit as many times as there are of them. The sockets of
    // those that connected first hold a few more of the bus's signals from before the flood, about
    // the clients after them, so they can stop taking it a signal or so sooner than the others,
    // which the bus then waits on for a time limit of their own.
    let in_turn = SHORT_TIMEOUT * STALLED_SUBSCRIBERS as u32;
    ensure!(
        waited < in_turn - SHORT_TIMEOUT,
        "the stalled clients went after {waited:?}, as if the bus waited on them in turn"
    );

    // The flooder stays.
    DBusProxy::new(&flooder).await?.get_id().await?;

    Ok(())
}

async fn leaving_client_(address: String) -> anyhow::Result<()> {
    let observer = connect(&address).await?;
    let mut changes = all_owner_changes(&observer).await?;
    let (stalled, signals) = stalled_subscriber(&address).await?;
    let stalled_name = stalled.unique_name().unwrap().to_owned();

    // The client goes while the bus waits for it to read, before the time limit.
    let flooder = connect(&address).await?;
    let flooding = zruntime::spawn(async move {
        flood(&flooder, FLOOD_SIGNALS).await?;

        anyhow::Ok(flooder)
    });
    SharedRuntime::current()?.sleep(SHORT_TIMEOUT / 2).await;
    drop((stalled, signals));

    // The bus says once that it went.
    loop {
        let (changed, old_owner, new_owner) = next_owner_change(&mut changes).await?;
        if old_owner == stalled_name.as_str() {
            ensure!(
                changed == stalled_name.as_str() && new_owner.is_empty(),
                "`{changed}` went from `{old_owner}` to `{new_owner}`"
            );
            break;
        }
    }
    let flooder = flooding.await??;
    no_more_changes_of(&mut changes, &stalled_name).await?;

    // And it carries on.
    DBusProxy::new(&flooder).await?.get_id().await?;
    let newcomer = connect(&address).await?;
    DBusProxy::new(&newcomer).await?.get_id().await?;

    Ok(())
}

async fn ordering_(address: String) -> anyhow::Result<()> {
    // A client that gets the signals that the sender broadcasts as well as those that the sender
    // sends to it, one that gets the broadcasts alone, and a monitor of both, and of the owners of
    // a name.
    let both = connect(&address).await?;
    let both_name = both.unique_name().unwrap().to_owned();
    let mut both_got = sequence(&both).await?;
    let broadcasts = connect(&address).await?;
    let mut broadcasts_got = sequence(&broadcasts).await?;
    let name = "org.zbus.Order";
    let (_monitor, mut monitored) =
        monitor(&address, &[sequence_rule()?, owner_rule(name)?]).await?;

    let sender = connect(&address).await?;
    for i in 0..SEQUENCE {
        let destination = (i % 2 == 1).then_some(both_name.as_str());
        sender
            .emit_signal(destination, "/org/zbus/Order", "org.zbus.Order", "Seq", &i)
            .await?;
    }
    for i in 0..SEQUENCE {
        next_in_sequence(&mut both_got, i).await?;
        next_in_sequence(&mut monitored, i).await?;
    }
    for i in (0..SEQUENCE).step_by(2) {
        next_in_sequence(&mut broadcasts_got, i).await?;
    }

    // The bus's own messages, as a client asks for the name and releases it, time after time.
    let owner = connection::Builder::address(address.as_str())
        .max_queued(NonZeroUsize::new(1024).unwrap())
        .build()
        .await?;
    let owner_name = owner.unique_name().unwrap().to_owned();
    let mut owner_got = MessageStream::from(&owner);
    let watcher = connect(&address).await?;
    let mut owner_changes = owner_changes(&watcher, name).await?;
    let mut calls = vec![];
    for _ in 0..NAME_ROUNDS {
        let reply = call_bus(&owner, "RequestName", &(name, 0u32)).await?;
        calls.push((reply.header().reply_serial().unwrap(), "NameAcquired"));
        let reply = call_bus(&owner, "ReleaseName", &name).await?;
        calls.push((reply.header().reply_serial().unwrap(), "NameLost"));
    }

    // Each of them reaches those it is for in the order that the bus sent it: the owner gets the
    // signal that it acquired or lost the name before the reply to its call, and the others get
    // the changes of the owner of the name in turn.
    for (serial, signal) in calls {
        let mut signalled = false;
        loop {
            let msg = owner_got.next().await.context("the stream ended")??;
            let header = msg.header();
            if header.reply_serial() == Some(serial) {
                break;
            }
            if header
                .member()
                .is_some_and(|member| member.as_str() == signal)
                && msg.body().deserialize::<&str>()? == name
            {
                signalled = true;
            }
        }
        ensure!(signalled, "the reply came before `{signal}`");
    }
    for _ in 0..NAME_ROUNDS {
        for stream in [&mut owner_changes, &mut monitored] {
            for (old_owner, new_owner) in [("", owner_name.as_str()), (owner_name.as_str(), "")] {
                let (changed, old, new) = next_owner_change(stream).await?;
                ensure!(
                    (changed.as_str(), old.as_str(), new.as_str()) == (name, old_owner, new_owner),
                    "`{changed}` went from `{old}` to `{new}`"
                );
            }
        }
    }

    Ok(())
}

async fn client_without_fds_(address: String) -> anyhow::Result<()> {
    let (client, _) = client_on_socket(&address).await?;
    let client_name = client.unique_name().unwrap().to_owned();

    // A signal with a file descriptor for the client, then a call to the bus, which returns once
    // the bus has handled the signal.
    let sender = connect(&address).await?;
    let file = File::open("/dev/null")?;
    sender
        .emit_signal(
            Some(client_name.as_str()),
            "/org/zbus/Fds",
            "org.zbus.Fds",
            "WithFd",
            &Fd::from(&file),
        )
        .await?;
    DBusProxy::new(&sender).await?.get_id().await?;

    // The reply to a call of the client comes after the signal, and still gets to it.
    DBusProxy::new(&client).await?.get_id().await?;

    Ok(())
}

async fn call_to_client_without_fds_(address: String) -> anyhow::Result<()> {
    let (client, _) = client_on_socket(&address).await?;
    let client_name = client.unique_name().unwrap().to_owned();
    let caller = connect(&address).await?;
    let file = File::open("/dev/null")?;

    let reply = caller
        .call_method(
            Some(client_name.as_str()),
            "/org/zbus/Fds",
            Some("org.zbus.Fds"),
            "WithFd",
            &Fd::from(&file),
        )
        .await;
    ensure!(
        matches!(reply, Err(zbus::Error::FDO(ref e)) if matches!(**e, fdo::Error::Failed(_))),
        "the caller got {reply:?}"
    );

    Ok(())
}

async fn client_without_fds_in_a_batch_(address: String) -> anyhow::Result<()> {
    let (client, _) = client_on_socket(&address).await?;
    let client_name = client.unique_name().unwrap().to_owned();
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.zbus.Fds")
        .build()?;
    let mut signals = MessageStream::for_match_rule(rule, &client, None).await?;

    // Signals for the client, one with a file descriptor between two without, which the sender
    // sends at once, for the bus to forward them together.
    let sender = connect(&address).await?;
    let file = File::open("/dev/null")?;
    let signal =
        |member| Message::signal("/org/zbus/Fds", "org.zbus.Fds", member).destination(&client_name);
    let msgs = [
        signal("Before").build(&())?,
        signal("WithFd").build(&Fd::from(&file))?,
        signal("After").build(&())?,
    ];
    sender.send_all(&msgs).await?;

    // The client gets the two without.
    for expected in ["Before", "After"] {
        let msg = signals.next().await.context("the stream ended")??;
        let header = msg.header();
        let member = header.member().map(|member| member.as_str());
        ensure!(
            member == Some(expected),
            "got {member:?} rather than `{expected}`"
        );
    }

    Ok(())
}

#[cfg(target_os = "linux")]
async fn unwritable_client_(address: String) -> anyhow::Result<()> {
    let (client, socket) = client_on_socket(&address).await?;
    let client_name = client.unique_name().unwrap().to_owned();
    let observer = connect(&address).await?;
    let mut owner_changes = owner_changes(&observer, &client_name).await?;

    // On Linux, writing to a socket fails once its peer shuts it down for reading.
    socket.0.get_ref().shutdown(Shutdown::Read)?;
    let sender = connect(&address).await?;
    sender
        .emit_signal(
            Some(client_name.as_str()),
            "/org/zbus/Unwritable",
            "org.zbus.Unwritable",
            "Unwritable",
            &(),
        )
        .await?;

    // The bus disconnects the client, and says so.
    disconnected(&mut owner_changes, &client_name).await
}

#[cfg(target_os = "linux")]
async fn too_many_fds_(address: String) -> anyhow::Result<()> {
    let recipient = connect(&address).await?;
    let recipient_name = recipient.unique_name().unwrap().to_owned();
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.zbus.Fds")
        .member("WithFds")
        .build()?;
    let mut signals = MessageStream::for_match_rule(rule, &recipient, None).await?;
    let path = address.strip_prefix("unix:path=").unwrap().to_owned();
    let (sender, _) = zruntime::unblock(move || RawClient::connect(&path)).await?;
    let files = (0..=MAX_MESSAGE_FDS)
        .map(|_| File::open("/dev/null"))
        .collect::<io::Result<Vec<_>>>()?;

    // A message with as many file descriptors as the bus can pass on gets to the recipient.
    let signal = with_fds(&recipient_name, &files[..MAX_MESSAGE_FDS])?;
    let sender = zruntime::unblock(move || sender.send(&signal).map(|()| sender)).await?;
    let msg = signals.next().await.unwrap()?;
    let fds = msg.data().fds().len();
    ensure!(
        fds == MAX_MESSAGE_FDS,
        "the signal came with {fds} file descriptors"
    );

    // With one more, the bus disconnects the sender, and the recipient stays.
    let signal = with_fds(&recipient_name, &files)?;
    zruntime::unblock(move || sender.send(&signal).and_then(|()| sender.disconnected()))
        .await
        .context("the bus didn't disconnect the sender")?;
    DBusProxy::new(&recipient).await?.get_id().await?;

    Ok(())
}

#[cfg(target_os = "linux")]
async fn undeclared_fds_(address: String) -> anyhow::Result<()> {
    let path = address.strip_prefix("unix:path=").unwrap().to_owned();
    let files = (0..3)
        .map(|_| File::open("/dev/null"))
        .collect::<io::Result<Vec<_>>>()?;
    let (msg, fds) = zruntime::unblock(move || {
        let (recipient, recipient_name) = RawClient::connect(&path)?;
        // The `NameAcquired` signal of its unique name.
        recipient.read_message()?;
        let (sender, _) = RawClient::connect(&path)?;

        // A signal that declares one file descriptor, and brings all three.
        let signal = with_fds(&recipient_name, &files[..1])?;
        let fds: Vec<_> = files.iter().map(AsFd::as_fd).collect();
        sender.send_with_fds(signal.data(), &fds)?;

        anyhow::Ok(recipient.read_message_with_fds()?)
    })
    .await?;
    let member = b"WithFds";
    ensure!(
        msg.windows(member.len()).any(|bytes| bytes == member),
        "the recipient got another message"
    );
    ensure!(
        fds.len() == 1,
        "the signal came with {} file descriptors",
        fds.len()
    );

    Ok(())
}

#[cfg(target_os = "linux")]
async fn partial_write_(address: String) -> anyhow::Result<()> {
    // A client that reads what the bus sends it as it says hello, and then stops reading.
    let path = address.strip_prefix("unix:path=").unwrap().to_owned();
    let (client, client_name) = zruntime::unblock(move || {
        let (client, name) = RawClient::connect(&path)?;
        // The `NameAcquired` signal of its unique name.
        client.read_message()?;

        anyhow::Ok((client, name))
    })
    .await?;
    let observer = connect(&address).await?;
    let mut owner_changes = owner_changes(&observer, &client_name).await?;

    // A signal far bigger than the socket of the client can take, and while the bus waits to write
    // the rest of it, another one, which has to wait behind it.
    let sender = connect(&address).await?;
    let big = "x".repeat(BIG_SIGNAL);
    sender
        .emit_signal(
            Some(client_name.as_str()),
            "/org/zbus/Partial",
            "org.zbus.Partial",
            "Big",
            &big.as_str(),
        )
        .await?;
    SharedRuntime::current()?
        .sleep(PARTIAL_WRITE_TIMEOUT / 5)
        .await;
    let other = connect(&address).await?;
    other
        .emit_signal(
            Some(client_name.as_str()),
            "/org/zbus/Partial",
            "org.zbus.Partial",
            "Small",
            &"small",
        )
        .await?;

    // The bus disconnects the client, which then reads what it has on its socket: the start of the
    // big signal, and nothing after it. It may have the small signal before it.
    disconnected(&mut owner_changes, &client_name).await?;
    let received = zruntime::unblock(move || client.disconnected()).await?;
    let mut rest = &received[..];
    while rest.len() >= 16 && message_len(rest) <= rest.len() {
        let len = message_len(rest);
        ensure!(len < BIG_SIGNAL, "the client got the whole big signal");
        rest = &rest[len..];
    }
    ensure!(
        rest.len() >= 16,
        "the client got no start of the big signal"
    );
    ensure!(
        rest.len() < message_len(rest),
        "the client got more than the start of the big signal"
    );
    let body = &rest[body_start(rest)..];
    ensure!(
        body.len() > 4 && body[4..].iter().all(|&byte| byte == b'x'),
        "the client got something other than the big signal after its start"
    );

    // Neither sender got disconnected.
    DBusProxy::new(&sender).await?.get_id().await?;
    DBusProxy::new(&other).await?.get_id().await?;

    Ok(())
}

/// How long the bus waits for a client to read in the tests where a client stops reading for
/// good: short, so that the tests don't take long.
const SHORT_TIMEOUT: Duration = Duration::from_millis(200);

/// How long the bus waits for a client to read in the tests where no client must get
/// disconnected: long, so that a busy machine doesn't have the bus disconnect one all the same.
const LONG_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the client of `slow_client` doesn't read.
const PAUSE: Duration = Duration::from_millis(300);

/// How many clients of `stalled_subscribers` stop reading.
const STALLED_SUBSCRIBERS: usize = 5;

/// How long the bus waits for the client of `partial_write` to read: long enough for the second
/// signal to come while the bus waits.
#[cfg(target_os = "linux")]
const PARTIAL_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// The size of the big signal of `partial_write`.
#[cfg(target_os = "linux")]
const BIG_SIGNAL: usize = 4 * 1024 * 1024;

/// How many signals `flood` sends, and how big each one is: far more than the sockets between
/// the bus and a client can take, and than the bus reads ahead from its clients.
const FLOOD_SIGNALS: u32 = 512;
const FLOOD_PAYLOAD: usize = 4096;

/// How many signals the sender of `ordering` sends, and how many times its owner of a name asks
/// for it and releases it.
const SEQUENCE: u32 = 200;
const NAME_ROUNDS: usize = 20;

/// Set the tracing subscriber up, once for all the tests of this file, which run in one process.
fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}

/// Run `test` against a bus of its own, at an address that it is given, which waits for each
/// client to take a message for `send_timeout`.
async fn with_bus<F, T>(send_timeout: Duration, test: F)
where
    F: FnOnce(String) -> T,
    T: Future<Output = anyhow::Result<()>>,
{
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    let mut bus = Bus::for_address(Some(&address)).await.unwrap();
    bus.set_send_timeout(send_timeout);
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

async fn connect(address: &str) -> zbus::Result<Connection> {
    connection::Builder::address(address).build().await
}

/// A client that is subscribed to every signal, and reads none, as [`stall`] has it.
async fn stalled_subscriber(address: &str) -> anyhow::Result<(Connection, MessageStream)> {
    let conn = connect(address).await?;
    let signals = stall(&conn).await?;

    Ok((conn, signals))
}

/// Subscribe `conn` to every signal, through a stream that holds one, for it to stop reading.
///
/// Once a signal is in the stream, which is never read, the client reads nothing more from its
/// socket.
async fn stall(conn: &Connection) -> anyhow::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .build()?;

    Ok(MessageStream::for_match_rule(rule, conn, Some(NonZeroUsize::MIN)).await?)
}

/// A client on a socket that zbus doesn't negotiate passing file descriptors over.
async fn client_on_socket(address: &str) -> anyhow::Result<(Connection, ClientSocket)> {
    let path = address.strip_prefix("unix:path=").unwrap().to_owned();
    let stream = zruntime::unblock(move || UnixStream::connect(path)).await?;
    let socket = ClientSocket(Arc::new(AsyncIo::new(&SharedRuntime::current()?, stream)?));
    let conn = connection::Builder::socket(socket.clone())
        // A reply that never comes fails the call, rather than have the test time out.
        .method_timeout(Duration::from_secs(5))
        .build()
        .await?;

    Ok((conn, socket))
}

/// A monitor of what `rules` match, every message if there are none, which reads all of it.
async fn monitor(
    address: &str,
    rules: &[MatchRule<'_>],
) -> anyhow::Result<(Connection, MessageStream)> {
    let conn = connection::Builder::address(address)
        .max_queued(NonZeroUsize::new(4 * FLOOD_SIGNALS as usize).unwrap())
        .build()
        .await?;
    let stream = MessageStream::from(&conn);
    become_monitor(&conn, rules).await?;

    Ok((conn, stream))
}

async fn become_monitor(conn: &Connection, rules: &[MatchRule<'_>]) -> zbus::Result<()> {
    MonitoringProxy::builder(conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?
        .become_monitor(rules, 0)
        .await?;

    Ok(())
}

/// Wait for the connection of a client that the bus disconnects to close.
async fn closed(conn: &Connection) -> anyhow::Result<()> {
    SharedRuntime::current()?
        .timeout(LONG_TIMEOUT, conn.closed())
        .await
        .context("the bus didn't disconnect the client")
}

/// Make a call to the bus, and return the reply.
async fn call_bus<B>(conn: &Connection, method: &str, body: &B) -> zbus::Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        method,
        body,
    )
    .await
}

/// The `NameOwnerChanged` signals for `name`, as `conn` gets them.
async fn owner_changes(conn: &Connection, name: &str) -> anyhow::Result<MessageStream> {
    let rule = owner_rule(name)?;

    Ok(MessageStream::for_match_rule(rule, conn, NonZeroUsize::new(1024)).await?)
}

/// The `NameOwnerChanged` signals for any name, as `conn` gets them.
async fn all_owner_changes(conn: &Connection) -> anyhow::Result<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .build()?;

    Ok(MessageStream::for_match_rule(rule, conn, NonZeroUsize::new(1024)).await?)
}

fn owner_rule(name: &str) -> zbus::Result<MatchRule<'_>> {
    MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .interface("org.freedesktop.DBus")
        .member("NameOwnerChanged")
        .arg(0, name)
        .build()
}

/// The next `NameOwnerChanged` signal in `stream`: the name, and its old and new owners.
async fn next_owner_change(stream: &mut MessageStream) -> anyhow::Result<(String, String, String)> {
    loop {
        let msg = stream.next().await.context("the stream ended")??;
        if msg
            .header()
            .member()
            .is_some_and(|member| member.as_str() == "NameOwnerChanged")
        {
            return Ok(msg.body().deserialize()?);
        }
    }
}

/// Wait for the bus to say that the client of the unique name `name` is gone.
async fn disconnected(
    owner_changes: &mut MessageStream,
    name: &UniqueName<'_>,
) -> anyhow::Result<()> {
    loop {
        let (changed, old_owner, new_owner) = next_owner_change(owner_changes).await?;
        // The bus may only now say that the client came.
        if old_owner.is_empty() {
            continue;
        }
        ensure!(changed == name.as_str(), "unexpected name `{changed}`");
        ensure!(new_owner.is_empty(), "`{name}` went to `{new_owner}`");

        return Ok(());
    }
}

/// Check that the bus doesn't say again that the client of the unique name `name` went, or
/// anything that it owned did, for a while.
async fn no_more_changes_of(
    changes: &mut MessageStream,
    name: &UniqueName<'_>,
) -> anyhow::Result<()> {
    let runtime = SharedRuntime::current()?;
    while let Ok(change) = runtime
        .timeout(3 * SHORT_TIMEOUT, next_owner_change(changes))
        .await
    {
        let (changed, old_owner, _) = change?;
        ensure!(
            old_owner != name.as_str(),
            "the bus said again that `{changed}` went"
        );
    }

    Ok(())
}

fn flood_rule() -> zbus::Result<MatchRule<'static>> {
    MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.zbus.Flood")
        .member("Flood")
        .build()
}

/// The signals of [`flood`], as `conn` gets them, in a stream that holds them all.
async fn floods(conn: &Connection) -> anyhow::Result<MessageStream> {
    let max_queued = NonZeroUsize::new(2 * FLOOD_SIGNALS as usize);

    Ok(MessageStream::for_match_rule(flood_rule()?, conn, max_queued).await?)
}

/// Broadcast `count` signals of [`FLOOD_PAYLOAD`] bytes, each with its number.
async fn flood(conn: &Connection, count: u32) -> zbus::Result<()> {
    let payload = "x".repeat(FLOOD_PAYLOAD);
    for i in 0..count {
        conn.emit_signal(
            None::<&str>,
            "/org/zbus/Flood",
            "org.zbus.Flood",
            "Flood",
            &(i, payload.as_str()),
        )
        .await?;
    }

    Ok(())
}

/// Read the first `count` signals of [`flood`] from `stream`, which must come in order.
async fn in_order(stream: &mut MessageStream, count: u32) -> anyhow::Result<()> {
    let mut expected = 0;
    while expected < count {
        let msg = stream.next().await.context("the stream ended")??;
        if msg
            .header()
            .member()
            .is_none_or(|member| member.as_str() != "Flood")
        {
            continue;
        }
        let (got, _): (u32, &str) = msg.body().deserialize()?;
        ensure!(got == expected, "got signal {got} rather than {expected}");
        expected += 1;
    }

    Ok(())
}

fn sequence_rule() -> zbus::Result<MatchRule<'static>> {
    MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface("org.zbus.Order")
        .member("Seq")
        .build()
}

/// The signals of the sender of `ordering`, as `conn` gets them, in a stream that holds them all.
async fn sequence(conn: &Connection) -> anyhow::Result<MessageStream> {
    let max_queued = NonZeroUsize::new(2 * SEQUENCE as usize);

    Ok(MessageStream::for_match_rule(sequence_rule()?, conn, max_queued).await?)
}

/// Read the next signal of the sender of `ordering` from `stream`, which must be number `i`.
async fn next_in_sequence(stream: &mut MessageStream, i: u32) -> anyhow::Result<()> {
    loop {
        let msg = stream.next().await.context("the stream ended")??;
        if msg
            .header()
            .member()
            .is_some_and(|member| member.as_str() == "Seq")
        {
            let got: u32 = msg.body().deserialize()?;
            ensure!(got == i, "got signal {got} rather than {i}");

            return Ok(());
        }
    }
}

/// A client's Unix socket, wrapped so that zbus doesn't see it as one.
///
/// So zbus doesn't negotiate passing file descriptors over it, and a test can reach the socket
/// under the client's connection.
#[derive(Debug, Clone)]
struct ClientSocket(Arc<AsyncIo<UnixStream, Shared>>);

impl Socket for ClientSocket {
    type ReadHalf = Self;
    type WriteHalf = Self;

    fn split(self) -> Split<Self, Self> {
        Split::new(self.clone(), self)
    }
}

#[async_trait]
impl ReadHalf for ClientSocket {
    async fn recvmsg(&mut self, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
        let len = self.0.read_with(|mut stream| stream.read(buf)).await?;

        Ok((len, vec![]))
    }
}

#[async_trait]
impl WriteHalf for ClientSocket {
    async fn sendmsg(&mut self, buf: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<usize> {
        assert!(
            fds.is_empty(),
            "file descriptors to send over a socket that can't"
        );

        self.0.write_with(|mut stream| stream.write(buf)).await
    }

    async fn close(&mut self) -> io::Result<()> {
        self.0.get_ref().shutdown(Shutdown::Both)
    }
}

/// A signal for `destination`, with a file descriptor of each of `files`.
#[cfg(target_os = "linux")]
fn with_fds(destination: &UniqueName<'_>, files: &[File]) -> zbus::Result<Message> {
    let fds: Vec<_> = files.iter().map(Fd::from).collect();

    Message::signal("/org/zbus/Fds", "org.zbus.Fds", "WithFds")
        .destination(destination)
        .build(&fds)
}

/// A client on a socket of its own, which passes the file descriptors of a message over as many
/// writes as Linux needs, where zbus passes them all with the first write.
#[cfg(target_os = "linux")]
struct RawClient(UnixStream);

#[cfg(target_os = "linux")]
impl RawClient {
    /// Connect to the bus on `path`, negotiate passing file descriptors, and say hello.
    ///
    /// Returns the client, and the unique name that the bus gave it.
    fn connect(path: &str) -> anyhow::Result<(Self, OwnedUniqueName)> {
        let mut stream = UnixStream::connect(path)?;
        let uid = rustix::process::getuid().as_raw().to_string();
        let uid: String = uid.bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())?;
        read_line(&stream, "OK ")?;
        stream.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        read_line(&stream, "AGREE_UNIX_FD")?;
        stream.write_all(b"BEGIN\r\n")?;
        let hello = Message::method_call("/org/freedesktop/DBus", "Hello")
            .destination("org.freedesktop.DBus")
            .interface("org.freedesktop.DBus")
            .build(&())?;
        stream.write_all(hello.data())?;
        // Wait for the reply, as clients do, since zbus takes the file descriptors that it reads
        // along with the handshake for those of the first message after it.
        let client = Self(stream);
        let reply = client.read_message()?;
        // The body is the unique name, a string: its length, and then its bytes.
        let body = &reply[body_start(&reply)..];
        let name = &body[4..4 + u32_at(body, 0)];
        let name = OwnedUniqueName::try_from(std::str::from_utf8(name)?)?;

        Ok((client, name))
    }

    /// Send `msg`, with at most [`MAX_MESSAGE_FDS`] of its file descriptors per write.
    fn send(&self, msg: &Message) -> io::Result<()> {
        let data = msg.data();
        let fds: Vec<_> = data.fds().iter().map(AsFd::as_fd).collect();

        self.send_with_fds(data, &fds)
    }

    /// Send the message of `data` with `fds`, whatever its header declares, with at most
    /// [`MAX_MESSAGE_FDS`] of them per write.
    fn send_with_fds(&self, data: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        let chunks: Vec<_> = fds.chunks(MAX_MESSAGE_FDS).collect();
        let mut sent = 0;
        for (i, chunk) in chunks.iter().enumerate() {
            // A byte goes with each write of file descriptors but the last, which takes the rest.
            let end = if i + 1 < chunks.len() {
                sent + 1
            } else {
                data.len()
            };
            let mut space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_MESSAGE_FDS))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            control.push(SendAncillaryMessage::ScmRights(chunk));
            let iov = [IoSlice::new(&data[sent..end])];
            sent += sendmsg(&self.0, &iov, &mut control, SendFlags::empty())?;
        }

        (&self.0).write_all(&data[sent..])
    }

    /// Read the next message that the bus sends.
    fn read_message(&self) -> io::Result<Vec<u8>> {
        self.read_message_with_fds().map(|(msg, _)| msg)
    }

    /// Read the next message that the bus sends, and the file descriptors that come with it.
    ///
    /// The reads take no more than the bytes of the message, so the file descriptors are those
    /// that the bus sends with any of them.
    fn read_message_with_fds(&self) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
        let mut msg = vec![0; 16];
        let mut fds = vec![];
        self.read_exact_with_fds(&mut msg, &mut fds)?;
        msg.resize(message_len(&msg), 0);
        self.read_exact_with_fds(&mut msg[16..], &mut fds)?;

        Ok((msg, fds))
    }

    /// Fill `buf` from the socket, and add the file descriptors that come with it to `fds`.
    fn read_exact_with_fds(&self, mut buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<()> {
        while !buf.is_empty() {
            let mut space =
                [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_MESSAGE_FDS))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let mut iov = [IoSliceMut::new(buf)];
            let read = recvmsg(&self.0, &mut iov, &mut control, RecvFlags::CMSG_CLOEXEC)?.bytes;
            for msg in control.drain() {
                if let RecvAncillaryMessage::ScmRights(received) = msg {
                    fds.extend(received);
                }
            }
            if read == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            buf = &mut buf[read..];
        }

        Ok(())
    }

    /// Wait for the bus to disconnect the client, and return what it sends until then.
    fn disconnected(&self) -> io::Result<Vec<u8>> {
        self.0.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut received = vec![];
        (&self.0).read_to_end(&mut received)?;

        Ok(received)
    }
}

/// Read a line of the handshake, which must start with `expected`.
#[cfg(target_os = "linux")]
fn read_line(mut stream: &UnixStream, expected: &str) -> anyhow::Result<()> {
    let mut line = vec![];
    let mut byte = [0];
    while !line.ends_with(b"\r\n") {
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&line);
    ensure!(
        line.starts_with(expected),
        "unexpected `{}`",
        line.trim_end()
    );

    Ok(())
}

/// The length of the message that `bytes` start with, from the 16 bytes that every message starts
/// with. The bus sends messages in the byte order of the machine.
#[cfg(target_os = "linux")]
fn message_len(bytes: &[u8]) -> usize {
    body_start(bytes) + u32_at(bytes, 4)
}

/// Where the body of the message that `bytes` start with starts: after the header fields, the
/// length of which is in the first 16 bytes, padded to 8 bytes.
#[cfg(target_os = "linux")]
fn body_start(bytes: &[u8]) -> usize {
    (16 + u32_at(bytes, 12)).next_multiple_of(8)
}

#[cfg(target_os = "linux")]
fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}
