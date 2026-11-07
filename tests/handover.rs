//! A bus that hands over to its next instance, which serves its clients on, on the same
//! connections, as if nothing happened.
#![cfg(target_os = "linux")]

use std::{
    collections::HashSet,
    env::temp_dir,
    future::IntoFuture,
    io::{self, IoSlice, Read, Write},
    mem::MaybeUninit,
    net::Shutdown,
    num::{NonZeroU32, NonZeroUsize},
    ops::Range,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
    pin::pin,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Once,
    },
    time::{Duration, Instant},
};

use anyhow::{bail, Context};
use busd::{
    bus::{Bus, Handover, STOP_TIMEOUT},
    peer::SEND_TIMEOUT,
};
use futures_util::{
    future::{select, Either},
    StreamExt,
};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use rustix::{
    cmsg_space,
    net::{sendmsg, SendAncillaryBuffer, SendAncillaryMessage, SendFlags},
};
use serde::Serialize;
use zbus::{
    connection,
    fdo::{DBusProxy, MonitoringProxy, RequestNameFlags},
    message,
    names::BusName,
    proxy::CacheProperties,
    wire::{DynamicType, Fd},
    Connection, MatchRule, Message, MessageStream,
};
use zruntime::{Event, SharedRuntime, Task};

/// The clients keep their unique names and the names that they own, and a client that connects
/// once the next instance runs gets the number after theirs.
#[test]
#[timeout(30000)]
fn smoke() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;

        let service = connection::Builder::address(address.as_str())
            .name(SERVICE)
            .build()
            .await
            .unwrap();
        let client = connect(&address).await;
        let dbus = DBusProxy::new(&client).await.unwrap();
        let owner = dbus
            .get_name_owner(BusName::try_from(SERVICE).unwrap())
            .await
            .unwrap();
        assert_eq!(owner.as_str(), service.unique_name().unwrap().as_str());

        let handover = bus.hand_over().await;
        assert_eq!(handover.clients.len(), 2);
        let bus = Running::take_over(handover).await;

        let owner = dbus
            .get_name_owner(BusName::try_from(SERVICE).unwrap())
            .await
            .unwrap();
        assert_eq!(owner.as_str(), service.unique_name().unwrap().as_str());
        // A client that connects now gets a number after those of the others.
        let late = connect(&address).await;
        assert_eq!(late.unique_name().unwrap().as_str(), ":busd.3");

        bus.finish().await;
    });
}

/// The next instance tells the credentials of a client that it took over, as the previous one did.
#[test]
#[timeout(30000)]
fn credentials_of_a_client_that_was_handed_over() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let client = connect(&address).await;
        let bus = bus.restart().await;

        let asker = connect(&address).await;
        let dbus = DBusProxy::new(&asker).await.unwrap();
        let name = BusName::from(client.unique_name().unwrap().clone());
        let uid = dbus.get_connection_unix_user(name.clone()).await.unwrap();
        assert_eq!(uid, rustix::process::getuid().as_raw());
        let pid = dbus.get_connection_unix_process_id(name).await.unwrap();
        assert_eq!(pid, std::process::id());

        bus.finish().await;
    });
}

/// The next instance looks the credentials of a client that it took over up before it sends the
/// client anything, so that it tells them at once, even while a send to the client waits for the
/// client to read.
#[test]
#[timeout(30000)]
fn credentials_of_a_stalled_client_that_was_handed_over() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let mut stalled = connect(&address).await;
        // Mind the queue of one signal: see `hand_over_does_not_wait_for_a_slow_receiver`.
        let _signals = listen(&stalled, signal_rule("Flood")).await;
        stalled.set_max_queued(NonZeroUsize::MIN);
        let flooder = connect(&address).await;
        let bus = bus.restart().await;

        // The signals fill the stalled client up, and the bus waits to send it the next one.
        emit_chunks(&flooder, "Flood", CHUNKS_EACH).await;
        SharedRuntime::current().unwrap().sleep(FILL_UP).await;
        let asker = connect(&address).await;
        let dbus = DBusProxy::new(&asker).await.unwrap();
        let name = BusName::from(stalled.unique_name().unwrap().clone());
        let started = Instant::now();
        let uid = dbus.get_connection_unix_user(name).await.unwrap();
        let took = started.elapsed();
        assert_eq!(uid, rustix::process::getuid().as_raw());
        assert!(
            took < SEND_TIMEOUT / 2,
            "The bus told the credentials after {took:?}"
        );

        bus.finish().await;
    });
}

/// A method call that is still waiting for its reply as the bus hands over gets the reply from the
/// next instance: the service only answers once the bus has changed hands.
#[test]
#[timeout(30000)]
fn pending_call_gets_its_reply() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let service = connect(&address).await;
        request_name(&service, SERVICE).await;
        let mut calls = MessageStream::from(&service);
        let caller = connect(&address).await;

        // The call has reached the service by the time the bus hands over, so the bus has noted
        // that it waits for a reply.
        let call = zruntime::spawn({
            let caller = caller.clone();
            async move {
                caller
                    .call_method(Some(SERVICE), PATH, Some(INTERFACE), "Ping", &())
                    .await
            }
        });
        let ping = next_message(&mut calls, |msg| is_call(msg, "Ping")).await;
        let bus = bus.restart().await;
        service.reply(&ping.header(), &"Pong").await.unwrap();

        let reply = within(call).await.unwrap().unwrap();
        assert_eq!(reply.body().deserialize::<String>().unwrap(), "Pong");

        bus.finish().await;
    });
}

/// The next instance keeps the clients, and the calls that wait for their replies, as objects of
/// their users, as the instance that handed them over did: with as many objects as those make for
/// their user, it takes no other client of the user, until the call gets its reply.
#[test]
#[timeout(30000)]
fn users_keep_their_objects() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let service = connect(&address).await;
        request_name(&service, SERVICE).await;
        let mut calls = MessageStream::from(&service);
        let caller = connect(&address).await;
        let call = zruntime::spawn({
            let caller = caller.clone();
            async move {
                caller
                    .call_method(Some(SERVICE), PATH, Some(INTERFACE), "Ping", &())
                    .await
            }
        });
        let ping = next_message(&mut calls, |msg| is_call(msg, "Ping")).await;

        // The two clients and the call are all of the objects that the next instance keeps for
        // their user, the user of the tests.
        let mut next = Bus::take_over(bus.hand_over().await, None).await.unwrap();
        next.set_max_objects_per_user(3);
        let bus = Running::new(next);
        let refused = connection::Builder::address(address.as_str()).build().await;
        assert!(refused.is_err(), "the bus took another client of the user");

        service.reply(&ping.header(), &"Pong").await.unwrap();
        within(call).await.unwrap().unwrap();
        let _late = connect(&address).await;

        bus.finish().await;
    });
}

/// The match rules of a client outlive the hand-over: the signals that match the rule of a client
/// reach it from the next instance, and the signals that don't match it still do not.
#[test]
#[timeout(30000)]
fn signals_follow_match_rules() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let receiver = connect(&address).await;
        let mut signals = listen(&receiver, signal_rule("Wanted")).await;
        let emitter = connect(&address).await;
        let bus = bus.restart().await;

        // The signals of one sender arrive in the order that it emits them, so the second is the
        // first that the receiver gets if the bus keeps the first from it, as it should.
        emit(&emitter, "Unwanted").await;
        emit(&emitter, "Wanted").await;
        let signal = next_message(&mut signals, of_interface).await;
        assert_eq!(signal.header().member().unwrap().as_str(), "Wanted");

        bus.finish().await;
    });
}

/// The signals that are on their way as the bus hands over all arrive, in the order that they were
/// emitted, none missing and none twice, and neither the connection of their sender nor that of
/// their receiver breaks: what the sender emits meanwhile waits for the next instance.
#[test]
#[timeout(60000)]
fn traffic_in_flight() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let receiver = connect(&address).await;
        let signals = listen(&receiver, signal_rule("Tick")).await;
        let sender = connect(&address).await;

        let received = Arc::new(AtomicU32::new(0));
        let receiving = zruntime::spawn(receive_ticks(signals, received.clone()));
        let sending = zruntime::spawn(send_ticks(sender.clone(), 0..TICKS, 0));

        // The receiver has to go on reading while the bus hands over, as the bus forwards all that
        // it read of the sender first. Most of the signals are still to come by then.
        wait_until(|| received.load(Ordering::Relaxed) >= TICKS / 5).await;
        let handover = bus.hand_over().await;
        assert!(
            received.load(Ordering::Relaxed) < TICKS,
            "All of the signals came through before the bus handed over"
        );
        let bus = Running::take_over(handover).await;

        within(sending).await.unwrap().unwrap();
        let ticks = within(receiving).await.unwrap().unwrap();
        assert_eq!(ticks, (0..TICKS).collect::<Vec<_>>());
        get_id(&sender).await;
        get_id(&receiver).await;

        bus.finish().await;
    });
}

/// The calls to the bus itself that it took but did not answer when it hands over are answered
/// before it lets go of its clients, and the next instance answers the others: each call gets
/// exactly one reply, and every `NameAcquired` and `NameOwnerChanged` signal that a call causes
/// arrives exactly once.
#[test]
#[timeout(60000)]
fn calls_to_the_bus_get_their_replies() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let caller = connect(&address).await;
        // The caller watches the names that it asks for change owners, besides owning them.
        let messages = listen(&caller, owner_changes_rule()).await;
        let calls: Vec<_> = (0..CALLS).map(request_name_call).collect();
        let serials: HashSet<_> = calls
            .iter()
            .map(|call| call.primary_header().serial_num().get())
            .collect();

        let replied = Arc::new(AtomicU32::new(0));
        let answers = zruntime::spawn(collect_answers(messages, serials.clone(), replied.clone()));
        let sending = zruntime::spawn(send_all(caller.clone(), calls));

        // The bus reads the calls faster than it answers them, so it has a good many to answer
        // once it gets the first replies out.
        wait_until(|| replied.load(Ordering::Relaxed) >= CALLS / 50).await;
        let bus = bus.restart().await;

        within(sending).await.unwrap().unwrap();
        let answers = within(answers).await.unwrap().unwrap();
        let replies = answers
            .replies
            .into_iter()
            .filter(|serial| serials.contains(serial))
            .collect();
        assert_each_once("Replies", replies, serials.into_iter().collect());
        let names: Vec<_> = (0..CALLS).map(name_of).collect();
        assert_each_once("`NameAcquired` signals", answers.acquired, names.clone());
        assert_each_once("`NameOwnerChanged` signals", answers.changed, names);
        get_id(&caller).await;

        bus.finish().await;
    });
}

/// A bus that hands over doesn't wait for its clients to take what it sends them: a receiver that
/// stops taking signals holds no hand-over up, and is handed over with what the bus didn't write to
/// it. It gets the signals all the same, once and in order, the others from the next instance,
/// which writes it what the bus didn't first, as it gets those that come after the next instance
/// took over.
#[test]
#[timeout(30000)]
fn hand_over_does_not_wait_for_a_slow_receiver() {
    init_tracing();

    zruntime::block_on(async {
        // A receiver that doesn't take signals for a while is not to be disconnected meanwhile.
        let mut bus = Bus::for_address(Some(&unix_address())).await.unwrap();
        bus.set_send_timeout(LONG_SEND_TIMEOUT);
        let (bus, address) = serve(bus);
        let mut receiver = connect(&address).await;
        let mut signals = listen(&receiver, signal_rule("Tick")).await;
        // With its queue of one signal, it takes few signals to fill the receiver, once it stops
        // taking them: the queue, its socket and the socket of the bus for it fill up in turn, and
        // the bus has to wait to send the next signal.
        receiver.set_max_queued(NonZeroUsize::MIN);
        let sender = connect(&address).await;

        let sending = zruntime::spawn(send_ticks(sender.clone(), 0..CHUNKS, CHUNK));
        let mut ticks = Vec::new();
        for _ in 0..TAKEN {
            ticks.push(next_tick(&mut signals).await.unwrap());
        }
        // The bus reads all that the sender emits, as it fits in the queue of the sender, but sends
        // no more than the receiver takes.
        within(sending).await.unwrap().unwrap();
        SharedRuntime::current().unwrap().sleep(FILL_UP).await;

        // The receiver takes no signal while the bus hands over, which the bus doesn't wait for.
        let handover = within(bus.hand_over()).await;
        let unwritten = handover
            .clients
            .iter()
            .find(|client| client.id == id_of(&receiver))
            .expect("The receiver wasn't handed over")
            .connection
            .unwritten_len();
        assert!(unwritten > 0, "The bus wrote the receiver all that it sent");
        let bus = Running::take_over(handover).await;

        // The receiver takes the signals from now on, from the next instance.
        for _ in TAKEN..CHUNKS {
            ticks.push(next_tick(&mut signals).await.unwrap());
        }
        // The next instance passes the signals on as well.
        send_ticks(sender.clone(), CHUNKS..CHUNKS + MORE, 0)
            .await
            .unwrap();
        for _ in 0..MORE {
            ticks.push(next_tick(&mut signals).await.unwrap());
        }
        assert_eq!(ticks, (0..CHUNKS + MORE).collect::<Vec<_>>());
        get_id(&sender).await;
        get_id(&receiver).await;

        bus.finish().await;
    });
}

/// The messages that zbus read of a client, and that the bus didn't take by the time that it
/// paused, go to the next instance, which passes them on: the bus is passing one of them on to a
/// receiver that doesn't take it as the hand-over starts, while zbus read the others, none of which
/// is left in the socket of the sender. The receiver gets them all, once and in order, the rest of
/// the one that the bus was passing on first.
#[test]
#[timeout(30000)]
fn messages_that_the_bus_did_not_take_are_handed_over() {
    init_tracing();

    zruntime::block_on(async {
        let mut bus = Bus::for_address(Some(&unix_address())).await.unwrap();
        bus.set_send_timeout(LONG_SEND_TIMEOUT);
        let (bus, address) = serve(bus);
        let mut receiver = connect(&address).await;
        let mut signals = listen(&receiver, signal_rule("Tick")).await;
        // Mind the queue of one signal: see `hand_over_does_not_wait_for_a_slow_receiver`.
        receiver.set_max_queued(NonZeroUsize::MIN);
        let sender = connect(&address).await;

        // The receiver takes none of the signals yet, so the bus waits to pass the first ones on,
        // while zbus reads the others, as they fit in the queue of the sender.
        within(send_ticks(sender.clone(), 0..CHUNKS, CHUNK))
            .await
            .unwrap();
        SharedRuntime::current().unwrap().sleep(FILL_UP).await;

        let handover = within(bus.hand_over()).await;
        let client = handover
            .clients
            .iter()
            .find(|client| client.id == id_of(&sender))
            .expect("The sender wasn't handed over");
        assert!(
            client.connection.unread_len() > 0,
            "The bus took all of the signals"
        );
        let socket = client.connection.fds().next().unwrap();
        let unread = rustix::io::ioctl_fionread(socket).unwrap();
        assert_eq!(unread, 0, "zbus didn't read all that the sender sent");
        let bus = Running::take_over(handover).await;

        let mut ticks = Vec::new();
        for _ in 0..CHUNKS {
            ticks.push(next_tick(&mut signals).await.unwrap());
        }
        assert_eq!(ticks, (0..CHUNKS).collect::<Vec<_>>());
        get_id(&sender).await;
        get_id(&receiver).await;

        bus.finish().await;
    });
}

/// Clients send signals and `BecomeMonitor` in one go, while a receiver of the signals doesn't take
/// them, so that the bus is still passing the signals on when it has made the clients monitors: a
/// hand-over doesn't wait for that, and the receiver gets the signals of each client whole and in
/// order, from the bus that took over, with the monitors still monitoring.
#[test]
#[timeout(30000)]
fn monitor_batch_is_forwarded_before_a_hand_over() {
    init_tracing();

    zruntime::block_on(monitor_batches_are_forwarded(false));
}

/// As with `monitor_batch_is_forwarded_before_a_hand_over`, but the monitors go before the bus
/// hands over, while the bus is still passing their signals on: the receiver gets them all the
/// same.
#[test]
#[timeout(30000)]
fn batch_of_a_monitor_that_went_is_forwarded_before_a_hand_over() {
    init_tracing();

    zruntime::block_on(monitor_batches_are_forwarded(true));
}

/// Have clients send signals and `BecomeMonitor` in one go, while a receiver of the signals doesn't
/// take them, have the monitors disconnect if `leave`, and check that the receiver gets the signals
/// from the next instance, as the hand-over doesn't wait for it to take them.
async fn monitor_batches_are_forwarded(leave: bool) {
    let mut bus = Bus::for_address(Some(&unix_address())).await.unwrap();
    bus.set_send_timeout(LONG_SEND_TIMEOUT);
    let (bus, address) = serve(bus);
    let mut receiver = connect(&address).await;
    let mut signals = listen(&receiver, signal_rule("Tick")).await;
    // Mind the queue of one signal: see `hand_over_does_not_wait_for_a_slow_receiver`.
    receiver.set_max_queued(NonZeroUsize::MIN);
    let sender = connect(&address).await;
    let path = socket_path(&address);
    let runtime = SharedRuntime::current().unwrap();

    // Each client sends signals that are small enough to go in one batch of the bus, along
    // with the call that makes it a monitor, so that the bus is done with the call, and the
    // client is a monitor, while the signals wait to be sent to the receiver. They are too
    // many for the receiver to hold, if it takes none: then the signals of the last clients
    // wait for room in its socket, for as long as the receiver doesn't take the first.
    let mut monitors = Vec::new();
    for i in 0..monitors_to_fill() {
        let mut batch: Vec<_> = (i * BATCHED..(i + 1) * BATCHED)
            .map(|tick| tick_signal(tick, BATCH_PADDING))
            .collect();
        batch.push(
            Message::method_call(BUS_PATH, "BecomeMonitor")
                .destination(BUS_NAME)
                .interface("org.freedesktop.DBus.Monitoring")
                .build(&(["type='signal',member='Check'"].as_slice(), 0u32))
                .unwrap(),
        );
        let path = path.clone();
        let monitor = zruntime::unblock(move || {
            let mut client = BatchClient::connect(&path)?;
            client.become_monitor(&batch)?;

            Ok::<_, io::Error>(client)
        })
        .await
        .unwrap();
        monitors.push(monitor);
    }
    let total = monitors.len() as u32 * BATCHED;
    if leave {
        // The bus removes the monitors as their connections end.
        monitors.clear();
    }
    runtime.sleep(FILL_UP).await;

    // The receiver takes none of the signals while the bus hands over, which the bus doesn't wait
    // for: what it didn't write to the receiver goes on with the connection of the receiver.
    let handover = within(bus.hand_over()).await;
    let bus = Running::take_over(handover).await;
    let mut ticks = within(async {
        let mut ticks = Vec::new();
        for _ in 0..total {
            ticks.push(next_tick(&mut signals).await.unwrap());
        }
        ticks
    })
    .await;

    // The signals of each client came whole and in order, and one client after the other, as
    // the bus sends a batch in one go.
    for run in ticks.chunks(BATCHED as usize) {
        assert_eq!(run, (run[0]..run[0] + BATCHED).collect::<Vec<_>>());
    }
    ticks.sort();
    assert_eq!(ticks, (0..total).collect::<Vec<_>>());

    // The next instance passes the signals on as well.
    send_ticks(sender.clone(), total..total + MORE, 0)
        .await
        .unwrap();
    for tick in total..total + MORE {
        assert_eq!(next_tick(&mut signals).await.unwrap(), tick);
    }
    get_id(&receiver).await;
    get_id(&sender).await;

    // The clients that stayed still are monitors: they get the signals that their rules ask for.
    emit(&sender, "Check").await;
    for mut monitor in monitors {
        zruntime::unblock(move || monitor.receives(b"Check"))
            .await
            .unwrap();
    }

    bus.finish().await;
}

/// A signal of the tests that `send_ticks` would emit, as a message.
fn tick_signal(tick: u32, padding: usize) -> Message {
    Message::signal(PATH, INTERFACE, "Tick")
        .build(&(tick, "x".repeat(padding)))
        .unwrap()
}

/// How many clients `monitor_batches_are_forwarded` has send signals and become monitors: enough
/// for their signals to fill up the socket of the receiver.
fn monitors_to_fill() -> u32 {
    (2 * send_buffer() / (BATCHED * BATCH_PADDING as u32) + 2).min(300)
}

/// How many signals each of those clients sends, and how big each is: all of them, and the call,
/// make less than [`MAX_BATCH_BYTES`](busd::peers::MAX_BATCH_BYTES), and not more than
/// [`MAX_BATCH`](busd::peers::MAX_BATCH) messages.
const BATCHED: u32 = 13;
const BATCH_PADDING: usize = 1000;

/// A bus that was given its socket, as by socket activation, hands over without it: the socket is
/// the activator's, which gives it to the next instance. The clients keep their names and
/// connections, a client that connects later finds the same socket, and the bus leaves the file of
/// the socket be.
#[test]
#[timeout(30000)]
fn bus_on_a_given_socket_hands_over() {
    init_tracing();

    zruntime::block_on(async {
        let path = socket_path(&unix_address());
        let listener = UnixListener::bind(&path).unwrap();
        let activator_copy = OwnedFd::from(listener.try_clone().unwrap());
        let bus = Bus::for_listener(OwnedFd::from(listener)).await.unwrap();
        let (bus, address) = serve(bus);
        let owner = connect(&address).await;
        request_name(&owner, SERVICE).await;
        let client = connect(&address).await;

        let handover = bus.hand_over().await;
        assert!(handover.listener.is_none());
        let bus = Bus::take_over(handover, Some(activator_copy))
            .await
            .unwrap();
        let (bus, _) = serve(bus);

        let dbus = DBusProxy::new(&client).await.unwrap();
        let name = BusName::try_from(SERVICE).unwrap();
        let found = dbus.get_name_owner(name).await.unwrap();
        assert_eq!(found.as_str(), owner.unique_name().unwrap().as_str());
        let late = connect(&address).await;
        assert_eq!(late.unique_name().unwrap().as_str(), ":busd.3");
        get_id(&owner).await;

        bus.finish().await;
        assert!(std::path::Path::new(&path).exists());
        std::fs::remove_file(&path).unwrap();
    });
}

/// Clients that don't take what the bus sends them hold no hand-over up: those that the bus is
/// passing signals on to as it pauses are handed over with what the bus didn't write to them, as
/// are the other clients. The next instance, which serves the others on meanwhile, writes them that
/// first, and disconnects them once they don't take it within the time limit of the bus to send.
#[test]
#[timeout(30000)]
fn stalled_receivers_are_handed_over() {
    init_tracing();

    zruntime::block_on(async {
        // The bus doesn't disconnect the receivers as it hands over: it doesn't wait for them.
        let mut bus = Bus::for_address(Some(&unix_address())).await.unwrap();
        bus.set_send_timeout(LONG_SEND_TIMEOUT);
        let (bus, address) = serve(bus);
        let mut stalled = Vec::new();
        let mut senders = Vec::new();
        for i in 0..STALLED {
            let mut receiver = connect(&address).await;
            // Mind the queue of one signal: see `hand_over_does_not_wait_for_a_slow_receiver`.
            let signals = listen(&receiver, signal_rule(&stalled_member(i))).await;
            receiver.set_max_queued(NonZeroUsize::MIN);
            stalled.push((receiver, signals));
            senders.push(connect(&address).await);
        }
        let watcher = connect(&address).await;
        let mut changes = listen(&watcher, owner_changes_rule()).await;

        // The signals for each receiver, from a sender of its own, fill it up, and the bus waits
        // to send it the next one.
        for (i, sender) in (0..).zip(&senders) {
            emit_chunks(sender, &stalled_member(i), CHUNKS_EACH).await;
        }
        let runtime = SharedRuntime::current().unwrap();
        runtime.sleep(FILL_UP).await;

        let handover = within(bus.hand_over()).await;
        let mut clients: Vec<_> = handover.clients.iter().map(|client| client.id).collect();
        clients.sort();
        let mut expected: Vec<_> = stalled
            .iter()
            .map(|(receiver, _)| receiver)
            .chain(&senders)
            .chain([&watcher])
            .map(id_of)
            .collect();
        expected.sort();
        assert_eq!(clients, expected);
        for (receiver, _) in &stalled {
            let id = id_of(receiver);
            let client = handover.clients.iter().find(|client| client.id == id);
            assert!(
                client.unwrap().connection.unwritten_len() > 0,
                "The bus wrote `:busd.{id}` all that it sent"
            );
        }
        let started = Instant::now();
        let bus = Running::take_over(handover).await;

        // The next instance serves the others meanwhile, and disconnects the receivers once their
        // time is up.
        get_id(&watcher).await;
        let mut waiting: HashSet<_> = stalled
            .iter()
            .map(|(receiver, _)| receiver.unique_name().unwrap().to_string())
            .collect();
        let disconnecting = async {
            while !waiting.is_empty() {
                let signal = changes.next().await.unwrap().unwrap();
                if !is_signal(&signal, "NameOwnerChanged") {
                    continue;
                }
                let (name, _, new_owner): (String, String, String) =
                    signal.body().deserialize().unwrap();
                if new_owner.is_empty() {
                    waiting.remove(&name);
                }
            }
        };
        runtime
            .timeout(SEND_TIMEOUT + WAIT, disconnecting)
            .await
            .expect("The next instance kept the receivers");
        let took = started.elapsed();
        assert!(
            took >= SEND_TIMEOUT,
            "The next instance disconnected the receivers after {took:?}, before their time was up"
        );
        for sender in &senders {
            get_id(sender).await;
        }
        // Without the signals that they don't take, which hold their connections up, the receivers
        // read on, to the ends of their connections.
        for (receiver, signals) in stalled {
            drop(signals);
            within(receiver.closed()).await;
        }

        bus.finish().await;
    });
}

/// A client that goes while the bus hands over, so that its socket is closed by the time the next
/// instance serves it, is cleaned up by that instance as by any bus: the other clients are told
/// that it and the name that it owned lost their owner.
#[test]
#[timeout(30000)]
fn closed_client_is_cleaned_up() {
    init_tracing();

    zruntime::block_on(client_leaves(Leaves::Closes));
}

/// A client that sends a signal and goes while the bus pauses, once the bus takes no more of its
/// messages, as another client holds the pause up with its handshake, has its signal passed on all
/// the same, by the bus that read it, which cleans up after it too, rather than hand it over.
#[test]
#[timeout(30000)]
fn signal_of_a_client_that_goes_while_the_bus_pauses_is_passed_on() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let path = socket_path(&address);
        let receiver = connect(&address).await;
        let mut signals = listen(&receiver, signal_rule("Bye")).await;
        let mut changes = listen(&receiver, owner_changes_rule()).await;
        let mut leaver = {
            let path = path.clone();
            zruntime::unblock(move || BatchClient::connect(&path))
                .await
                .unwrap()
        };
        // The bus numbers its clients in the order that they connect.
        let leaver_id = id_of(&receiver) + 1;
        let _late = zruntime::unblock(move || LateClient::connect(&path))
            .await
            .unwrap();
        let runtime = SharedRuntime::current().unwrap();

        let handing_over = pin!(bus.hand_over());
        let Either::Right((_, handing_over)) =
            select(handing_over, pin!(runtime.sleep(PAUSE))).await
        else {
            panic!("The bus handed over before the handshake of the other client ended");
        };
        // The bus paused the leaver by now. Its connection ends once the bus read the signal.
        let bye = Message::signal(PATH, INTERFACE, "Bye").build(&()).unwrap();
        leaver.send(&[bye]).unwrap();
        leaver.0.shutdown(Shutdown::Write).unwrap();
        let handover = handing_over.await;
        let clients: Vec<_> = handover.clients.iter().map(|client| client.id).collect();
        assert!(
            !clients.contains(&leaver_id),
            "The client that went was handed over"
        );
        let bus = Running::take_over(handover).await;

        next_message(&mut signals, |msg| is_signal(msg, "Bye")).await;
        let leaver_name = format!(":busd.{leaver_id}");
        expect_gone(&mut changes, &leaver_name, &[&leaver_name]).await;

        bus.finish().await;
    });
}

/// A client whose socket is missing from the ones that the bus handed over, as when the service
/// manager that held them let go of the socket of a client that went, is cleaned up by the next
/// instance as by any bus: the other clients are told that it and the name that it owned lost
/// their owner.
#[test]
#[timeout(30000)]
fn missing_client_is_cleaned_up() {
    init_tracing();

    zruntime::block_on(client_leaves(Leaves::Missing));
}

/// The names of clients that went while the bus handed over pass on to the clients that queued for
/// them before the next instance serves anyone: a call to each name that waited in the socket of
/// its caller reaches the client that took the name over.
#[test]
#[timeout(30000)]
fn names_of_missing_clients_pass_on() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let names = [format!("{LEAVER}0"), format!("{LEAVER}1")];
        let mut leavers = Vec::new();
        let mut heirs = Vec::new();
        for name in &names {
            let leaver = connect(&address).await;
            request_name(&leaver, name).await;
            let heir = connect(&address).await;
            request_name(&heir, name).await;
            let calls = MessageStream::from(&heir);
            leavers.push(leaver);
            heirs.push((heir, calls));
        }
        let caller = connect(&address).await;
        let mut replies = MessageStream::from(&caller);

        let mut handover = bus.hand_over().await;
        let gone: Vec<_> = leavers.iter().map(id_of).collect();
        handover.clients.retain(|client| !gone.contains(&client.id));
        // The calls wait in the socket of the caller, which no instance of the bus reads now.
        let mut serials = Vec::new();
        for name in &names {
            let call = Message::method_call(PATH, "Ping")
                .destination(name.as_str())
                .interface(INTERFACE)
                .build(&())
                .unwrap();
            caller.send(&call).await.unwrap();
            serials.push(call.primary_header().serial_num());
        }
        let bus = Running::take_over(handover).await;

        for ((heir, calls), serial) in heirs.iter_mut().zip(serials) {
            let ping = next_message(calls, |msg| is_call(msg, "Ping")).await;
            heir.reply(&ping.header(), &"Pong").await.unwrap();
            let reply = next_message(&mut replies, |msg| {
                msg.header().reply_serial() == Some(serial)
            })
            .await;
            assert_eq!(reply.message_type(), message::Type::MethodReturn);
        }

        bus.finish().await;
    });
}

/// A monitor is still one after the hand-over: it gets the messages that others send each other,
/// whether they are signals, calls or replies.
#[test]
#[timeout(30000)]
fn monitor_keeps_monitoring() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let monitor = connect(&address).await;
        let mut monitored = MessageStream::from(&monitor);
        MonitoringProxy::builder(&monitor)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .unwrap()
            .become_monitor(&[], 0)
            .await
            .unwrap();
        let client = connect(&address).await;
        let bus = bus.restart().await;

        emit(&client, "Monitored").await;
        let reply = client
            .call_method(Some(BUS_NAME), BUS_PATH, Some(BUS_NAME), "GetId", &())
            .await
            .unwrap();
        let serial = reply.header().reply_serial().unwrap();

        // The client sent the signal before it made the call, so the monitor gets them in that
        // order.
        next_message(&mut monitored, |msg| is_signal(msg, "Monitored")).await;
        let call = next_message(&mut monitored, |msg| is_call(msg, "GetId")).await;
        assert_eq!(call.primary_header().serial_num(), serial);
        next_message(&mut monitored, |msg| {
            msg.message_type() == message::Type::MethodReturn
                && msg.header().reply_serial() == Some(serial)
        })
        .await;

        bus.finish().await;
    });
}

/// A client in the middle of a message as the bus hands over doesn't hold the hand-over up, which
/// takes less than [`STOP_TIMEOUT`], the time that a client in the middle of its handshake has: the
/// bus hands the client over with the part of the message that it read, and the next instance
/// answers the message once the client sends the rest of it.
#[test]
#[timeout(30000)]
fn client_in_the_middle_of_a_message_is_handed_over() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let bystander = connect(&address).await;
        let path = socket_path(&address);
        let stuck = zruntime::unblock(move || StuckClient::connect(&path))
            .await
            .unwrap();
        // The bus numbers its clients in the order that they connect.
        let stuck_id = id_of(&bystander) + 1;

        let started = Instant::now();
        let handover = bus.hand_over().await;
        let took = started.elapsed();
        assert!(took < STOP_TIMEOUT, "The bus handed over after {took:?}");
        let mut clients: Vec<_> = handover.clients.iter().map(|client| client.id).collect();
        clients.sort();
        assert_eq!(clients, [id_of(&bystander), stuck_id]);
        let bus = Running::take_over(handover).await;

        zruntime::unblock(move || stuck.finish()).await.unwrap();
        get_id(&bystander).await;

        bus.finish().await;
    });
}

/// File descriptors that a client sends ahead of the message that they are for, as the
/// specification allows, which the bus read but no message took by the time that it hands over,
/// go to the next instance, which passes them on with that message.
#[test]
#[timeout(30000)]
fn fds_sent_ahead_are_handed_over() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let receiver = connect(&address).await;
        let mut messages = MessageStream::from(&receiver);
        let destination = receiver.unique_name().unwrap().to_string();
        let path = socket_path(&address);
        // What goes into one end of the pair comes out of the other, which tells that the end that
        // the receiver gets is the one that the sender sent.
        let (sent, mut kept) = UnixStream::pair().unwrap();
        let mut sender = zruntime::unblock(move || FdClient::connect(&path, sent))
            .await
            .unwrap();
        let sender_id = id_of(&receiver) + 1;

        let handover = bus.hand_over().await;
        let client = handover
            .clients
            .iter()
            .find(|client| client.id == sender_id)
            .expect("The sender wasn't handed over");
        // Its socket, and the file descriptor that it sent ahead.
        assert_eq!(client.connection.fds().len(), 2);
        let bus = Running::take_over(handover).await;

        zruntime::unblock(move || sender.send_for_fd(&destination))
            .await
            .unwrap();
        let signal = next_message(&mut messages, |msg| is_signal(msg, "Fd")).await;
        let fd: zbus::wire::OwnedFd = signal.body().deserialize().unwrap();
        let mut received = UnixStream::from(OwnedFd::from(fd));
        received.write_all(b"ping").unwrap();
        let mut ping = [0; 4];
        kept.read_exact(&mut ping).unwrap();
        assert_eq!(&ping, b"ping");

        bus.finish().await;
    });
}

/// A client that ends its handshake while the bus pauses, with its `Hello` in the same write,
/// pauses as soon as it does, rather than once the time for handshakes is up, and is handed over
/// with its `Hello`: the next instance answers that, and gives the client its unique name.
#[test]
#[timeout(30000)]
fn client_whose_handshake_ends_while_the_bus_pauses_is_handed_over() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let watcher = connect(&address).await;
        let path = socket_path(&address);
        let late = zruntime::unblock(move || LateClient::connect(&path))
            .await
            .unwrap();
        // The bus numbers its clients in the order that they connect.
        let late_id = id_of(&watcher) + 1;
        // Keeps the connection of the client open once its handshake is over.
        let _late_socket = late.0.try_clone().unwrap();
        let runtime = SharedRuntime::current().unwrap();

        let started = Instant::now();
        let handing_over = pin!(bus.hand_over());
        let Either::Right((_, handing_over)) =
            select(handing_over, pin!(runtime.sleep(PAUSE))).await
        else {
            panic!("The bus handed over before the handshake of the client ended");
        };
        let ending = zruntime::unblock(move || late.finish());
        let handover = handing_over.await;
        let took = started.elapsed();
        assert!(
            took < STOP_TIMEOUT,
            "The bus handed over after {took:?}, once the time for handshakes was up"
        );
        let clients: Vec<_> = handover.clients.iter().map(|client| client.id).collect();
        assert!(
            clients.contains(&late_id),
            "The client wasn't handed over: {clients:?}"
        );
        let bus = Running::take_over(handover).await;

        ending.await.unwrap();
        let late_name = format!(":busd.{late_id}");
        let owner = DBusProxy::new(&watcher)
            .await
            .unwrap()
            .get_name_owner(late_name.as_str().try_into().unwrap())
            .await
            .unwrap();
        assert_eq!(owner.as_str(), late_name);

        bus.finish().await;
    });
}

/// A client still in the middle of its handshake once the time for that is up, as the bus hands
/// over, gets disconnected then, before it has a name, rather than handed over: the end of its
/// handshake, with its `Hello` in the same write, finds it disconnected, and the next instance
/// knows nothing of it, nor does any other client.
#[test]
#[timeout(30000)]
fn client_whose_handshake_ends_too_late_is_disconnected() {
    init_tracing();

    zruntime::block_on(async {
        let (bus, address) = start().await;
        let watcher = connect(&address).await;
        let mut changes = listen(&watcher, owner_changes_rule()).await;
        let path = socket_path(&address);
        let late = zruntime::unblock(move || LateClient::connect(&path))
            .await
            .unwrap();
        // The bus numbers its clients in the order that they connect.
        let late_name = format!(":busd.{}", id_of(&watcher) + 1);

        let started = Instant::now();
        let handover = bus.hand_over().await;
        let took = started.elapsed();
        assert!(
            took >= STOP_TIMEOUT,
            "The bus handed over after {took:?}, before the time for handshakes was up"
        );
        let error = zruntime::unblock(move || late.finish())
            .await
            .expect_err("The bus answered the client");
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
            ),
            "The client wasn't disconnected: {error}"
        );
        let clients: Vec<_> = handover.clients.iter().map(|client| client.id).collect();
        assert_eq!(clients, [id_of(&watcher)]);
        let bus = Running::take_over(handover).await;

        // The next instance knows nothing of the client, and tells nothing of it.
        let owner = DBusProxy::new(&watcher)
            .await
            .unwrap()
            .get_name_owner(late_name.as_str().try_into().unwrap())
            .await;
        assert!(owner.is_err(), "The client owns its name: {owner:?}");
        let more = SharedRuntime::current().unwrap().timeout(
            QUIET,
            next_message(&mut changes, |msg| is_signal(msg, "NameOwnerChanged")),
        );
        if let Ok(signal) = more.await {
            panic!("An owner changed: {signal:?}");
        }

        bus.finish().await;
    });
}

/// How many bytes the socket of a client holds of what the bus sends it, at the most: the default
/// size of the send buffer of a socket.
fn send_buffer() -> u32 {
    std::fs::read_to_string("/proc/sys/net/core/wmem_default")
        .ok()
        .and_then(|size| size.trim().parse().ok())
        .unwrap_or(212_992)
}

/// A client that owns a name leaves while the bus hands over, as `how` says, and the next instance
/// tells another client that it and its name lost their owner.
async fn client_leaves(how: Leaves) {
    let (bus, address) = start().await;
    let watcher = connect(&address).await;
    let mut changes = listen(&watcher, owner_changes_rule()).await;
    let leaver = connect(&address).await;
    request_name(&leaver, LEAVER).await;
    let owner = leaver.unique_name().unwrap().to_string();

    let mut handover = bus.hand_over().await;
    match how {
        // This ends the connection of the client, whatever copies of its socket the bus holds.
        Leaves::Closes => leaver.clone().close().await.unwrap(),
        Leaves::Missing => handover
            .clients
            .retain(|client| client.id != id_of(&leaver)),
    }
    let bus = Running::take_over(handover).await;

    expect_gone(&mut changes, &owner, &[LEAVER, &owner]).await;
    // No socket of the client is left open, so it finds that the connection ended.
    within(leaver.closed()).await;

    bus.finish().await;
}

/// How the client of [`client_leaves`] goes.
enum Leaves {
    /// It closes its end of the connection.
    Closes,
    /// The bus hands its socket over to no one.
    Missing,
}

/// Wait until `changes` tell that `owner` lost each of `names`.
async fn expect_gone(changes: &mut MessageStream, owner: &str, names: &[&str]) {
    let mut waiting: HashSet<_> = names.iter().copied().collect();
    while !waiting.is_empty() {
        let signal = next_message(changes, |msg| is_signal(msg, "NameOwnerChanged")).await;
        let (name, old_owner, new_owner): (String, String, String) =
            signal.body().deserialize().unwrap();
        if old_owner == owner && new_owner.is_empty() {
            waiting.remove(name.as_str());
        }
    }
}

/// Emit the signal `member` from `conn`, `count` times, each with a big body.
async fn emit_chunks(conn: &Connection, member: &str, count: u32) {
    let filling = "x".repeat(CHUNK);
    for _ in 0..count {
        within(conn.emit_signal(None::<&str>, PATH, INTERFACE, member, &filling))
            .await
            .unwrap();
    }
}

/// The signal that the `i`th stalled receiver of `stalled_receivers_are_handed_over` asks for.
fn stalled_member(i: u32) -> String {
    format!("Stalled{i}")
}

/// How many receivers of `stalled_receivers_are_handed_over` stop taking signals, and how many big
/// signals a sender emits for each: enough to fill the receiver up.
const STALLED: u32 = 3;
const CHUNKS_EACH: u32 = 5;

/// Collect the sequence numbers of the signals that `send_ticks` sends, as they come out of
/// `signals`, and count the ones that came in `received`. There must be no more than [`TICKS`].
async fn receive_ticks(
    mut signals: MessageStream,
    received: Arc<AtomicU32>,
) -> anyhow::Result<Vec<u32>> {
    let runtime = SharedRuntime::current()?;
    let mut ticks = Vec::new();
    while ticks.len() < TICKS as usize {
        ticks.push(next_tick(&mut signals).await?);
        received.store(ticks.len() as u32, Ordering::Relaxed);
    }
    // Nothing comes after the last one.
    if let Ok(tick) = runtime.timeout(QUIET, next_tick(&mut signals)).await {
        bail!("Another tick came after the last one: {}", tick?);
    }

    Ok(ticks)
}

/// The sequence number of the next signal of `send_ticks` that comes out of `signals`.
async fn next_tick(signals: &mut MessageStream) -> anyhow::Result<u32> {
    loop {
        let msg = signals
            .next()
            .await
            .context("The connection of the receiver ended")??;
        if of_interface(&msg) {
            let (tick, _): (u32, String) = msg.body().deserialize()?;

            return Ok(tick);
        }
    }
}

/// Emit the signals numbered `ticks` from `sender`, each with its sequence number and `padding`
/// bytes of filling.
async fn send_ticks(sender: Connection, ticks: Range<u32>, padding: usize) -> zbus::Result<()> {
    let filling = "x".repeat(padding);
    for tick in ticks {
        sender
            .emit_signal(None::<&str>, PATH, INTERFACE, "Tick", &(tick, &filling))
            .await?;
    }

    Ok(())
}

/// How many signals `traffic_in_flight` has the bus pass on.
const TICKS: u32 = 5000;

/// How many signals `hand_over_does_not_wait_for_a_slow_receiver` has the bus pass on, and how big
/// they are: more than a receiver that doesn't take any holds, and not so many that the bus can't
/// read them all. The receiver takes a few of them, and the bus passes on a few more after it took
/// over.
const CHUNKS: u32 = 30;
const CHUNK: usize = 256 * 1024;
const TAKEN: u32 = 3;
const MORE: u32 = 5;

/// Collect what the bus sends in answer to the calls of `calls_to_the_bus_get_their_replies`, as
/// it comes out of `messages`, and count the replies in `replied`.
///
/// It is done once everything that `serials`, the serials of the calls, calls for has come, and
/// nothing more for a while; or when nothing comes for a good while, and something is missing.
async fn collect_answers(
    mut messages: MessageStream,
    serials: HashSet<u32>,
    replied: Arc<AtomicU32>,
) -> anyhow::Result<Answers> {
    let runtime = SharedRuntime::current()?;
    let mut answers = Answers::default();
    loop {
        let wait = if answers.is_complete(&serials) {
            QUIET
        } else {
            SILENCE
        };
        let Ok(next) = runtime.timeout(wait, messages.next()).await else {
            return Ok(answers);
        };
        let msg = next.context("The connection of the caller ended")??;
        answers.add(&msg);
        replied.store(answers.replies.len() as u32, Ordering::Relaxed);
    }
}

/// What the bus sent a client in answer to its calls to request names.
#[derive(Default)]
struct Answers {
    /// The serials of the calls that the replies are to.
    replies: Vec<u32>,
    /// The names that the `NameAcquired` signals are of.
    acquired: Vec<String>,
    /// The names that the `NameOwnerChanged` signals are of.
    changed: Vec<String>,
}

impl Answers {
    /// Note `msg`, if it is one of the answers.
    fn add(&mut self, msg: &Message) {
        if msg.message_type() == message::Type::MethodReturn {
            let serial = msg.header().reply_serial();
            self.replies.extend(serial.map(NonZeroU32::get));
        } else if is_signal(msg, "NameAcquired") {
            let name: String = msg.body().deserialize().unwrap();
            if name.starts_with(NAMES) {
                self.acquired.push(name);
            }
        } else if is_signal(msg, "NameOwnerChanged") {
            let (name, ..): (String, String, String) = msg.body().deserialize().unwrap();
            if name.starts_with(NAMES) {
                self.changed.push(name);
            }
        }
    }

    /// Whether there are the replies to the calls with `serials`, and the signals of as many names.
    fn is_complete(&self, serials: &HashSet<u32>) -> bool {
        let replies = self
            .replies
            .iter()
            .filter(|serial| serials.contains(serial))
            .count();

        replies >= serials.len()
            && self.acquired.len() >= serials.len()
            && self.changed.len() >= serials.len()
    }
}

/// Send `calls` to the bus from `conn`, one after the other, without waiting for any reply.
async fn send_all(conn: Connection, calls: Vec<Message>) -> zbus::Result<()> {
    for call in &calls {
        conn.send(call).await?;
    }

    Ok(())
}

/// The call to the bus to give the caller the `i`th of the names of the test.
fn request_name_call(i: u32) -> Message {
    call_bus("RequestName", &(name_of(i), 0_u32))
}

/// The `i`th of the names that `calls_to_the_bus_get_their_replies` asks for.
fn name_of(i: u32) -> String {
    format!("{NAMES}{i}")
}

/// Check that `got` and `expected` hold the same, each once, whatever the order.
fn assert_each_once<T>(what: &str, mut got: Vec<T>, mut expected: Vec<T>)
where
    T: Ord,
{
    got.sort();
    expected.sort();
    assert!(
        got == expected,
        "{what}: {} came, and {} were due, each once",
        got.len(),
        expected.len()
    );
}

/// How many names `calls_to_the_bus_get_their_replies` asks for, and the start of each.
const CALLS: u32 = 500;
const NAMES: &str = "org.zbus.Handover.Name";

/// A client that connects to the bus and is in the middle of a message from then on.
struct StuckClient {
    stream: UnixStream,
    /// The rest of the message.
    rest: Vec<u8>,
}

impl StuckClient {
    /// Connect to the bus at `path`, authenticate and say hello. Then start a second message, a
    /// call to the bus, and leave it at that: all but its last byte.
    fn connect(path: &str) -> io::Result<Self> {
        let mut stream = authenticate(path, false)?;

        let hello = call_bus("Hello", &());
        let unfinished = call_bus("GetId", &());
        let unfinished = unfinished.data().bytes();
        let (start, rest) = unfinished.split_at(unfinished.len() - 1);
        // The bus reads what is in its socket as far as its buffer goes, so it has the start of the
        // second message along with the first if they go in one write.
        stream.write_all(&[hello.data().bytes(), start].concat())?;
        // The reply tells that the bus has read what was written, and that it waits for the rest of
        // the second message.
        reply(&mut stream)?;

        Ok(Self {
            stream,
            rest: rest.to_vec(),
        })
    }

    /// Send the rest of the message, and wait for the reply to it.
    fn finish(mut self) -> io::Result<()> {
        self.stream.write_all(&self.rest)?;

        reply(&mut self.stream)
    }
}

/// A client that sends a file descriptor ahead of the message that it is for.
struct FdClient(UnixStream);

impl FdClient {
    /// Connect to the bus at `path`, authenticate, agreeing to pass file descriptors, and say
    /// hello, with `fd` along with the call, ahead of the message that it is for. Then wait for
    /// the reply, which tells that the bus read both the call and `fd`.
    fn connect(path: &str, fd: impl AsFd) -> io::Result<Self> {
        let mut stream = authenticate(path, true)?;
        let hello = call_bus("Hello", &());
        send_with_fd(&stream, hello.data().bytes(), fd.as_fd())?;
        reply(&mut stream)?;

        Ok(Self(stream))
    }

    /// Send `destination` the signal that the file descriptor that the client sent ahead is for.
    fn send_for_fd(&mut self, destination: &str) -> io::Result<()> {
        // The signal is built with a file descriptor, the socket of the client, which its header
        // counts, but goes without it, so that the one sent ahead goes with it.
        let signal = Message::signal(PATH, INTERFACE, "Fd")
            .destination(destination)
            .build(&Fd::from(&self.0))
            .unwrap();

        self.0.write_all(signal.data().bytes())
    }
}

/// Send `bytes` over `stream` in one write, with `fd`.
fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: BorrowedFd<'_>) -> io::Result<()> {
    let fds = [fd];
    let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
    let sent = sendmsg(
        stream,
        &[IoSlice::new(bytes)],
        &mut control,
        SendFlags::empty(),
    )?;
    // A stream that nothing was written to yet takes a short message whole.
    assert_eq!(sent, bytes.len());

    Ok(())
}

/// A client that is in the middle of its handshake, until it ends it.
struct LateClient(UnixStream);

impl LateClient {
    /// Connect to the bus at `path` and authenticate, but leave the handshake at that.
    fn connect(path: &str) -> io::Result<Self> {
        start_handshake(path).map(Self)
    }

    /// End the handshake, and say hello in the same write. Then wait for the reply.
    fn finish(mut self) -> io::Result<()> {
        let hello = call_bus("Hello", &());
        self.0
            .write_all(&[b"BEGIN\r\n", hello.data().bytes()].concat())?;

        reply(&mut self.0)
    }
}

/// A client that sends the messages it has in one write, to have the bus read them in one go.
struct BatchClient(UnixStream);

impl BatchClient {
    /// Connect to the bus at `path`, authenticate and say hello, and wait for the reply.
    fn connect(path: &str) -> io::Result<Self> {
        let mut stream = authenticate(path, false)?;
        stream.write_all(call_bus("Hello", &()).data().bytes())?;
        reply(&mut stream)?;

        Ok(Self(stream))
    }

    /// Send `msgs`, which end with the call that makes the client a monitor, in one write, and wait
    /// for the reply to the call.
    fn become_monitor(&mut self, msgs: &[Message]) -> io::Result<()> {
        self.send(msgs)?;

        reply(&mut self.0)
    }

    /// Send `msgs` in one write.
    fn send(&mut self, msgs: &[Message]) -> io::Result<()> {
        let bytes: Vec<u8> = msgs
            .iter()
            .flat_map(|msg| msg.data().bytes().to_vec())
            .collect();

        self.0.write_all(&bytes)
    }

    /// Read what the bus sends until `needle` comes up in it.
    fn receives(&mut self, needle: &[u8]) -> io::Result<()> {
        let mut got = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let n = self.0.read(&mut chunk)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            got.extend_from_slice(&chunk[..n]);
            if got.windows(needle.len()).any(|window| window == needle) {
                return Ok(());
            }
        }
    }
}

/// Connect to the bus at `path` and authenticate, with a read timeout of [`WAIT`], agreeing to pass
/// file descriptors if `fds`. The stream is ready for the first message, which has to be `Hello`.
fn authenticate(path: &str, fds: bool) -> io::Result<UnixStream> {
    let mut stream = start_handshake(path)?;
    if fds {
        stream.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        let line = read_line(&mut stream)?;
        if line != "AGREE_UNIX_FD" {
            return Err(io::Error::other(format!("Unexpected `{line}`")));
        }
    }
    stream.write_all(b"BEGIN\r\n")?;

    Ok(stream)
}

/// Connect to the bus at `path` and authenticate, with a read timeout of [`WAIT`]. The handshake
/// goes on from there.
fn start_handshake(path: &str) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(WAIT))?;
    // The user ID goes as the hex of its decimal digits.
    let uid: String = rustix::process::getuid()
        .as_raw()
        .to_string()
        .bytes()
        .map(|digit| format!("{digit:02x}"))
        .collect();
    stream.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())?;
    let line = read_line(&mut stream)?;
    if !line.starts_with("OK ") {
        return Err(io::Error::other(format!("Unexpected `{line}`")));
    }

    Ok(stream)
}

/// Wait for the reply to the call that `stream` made last, skipping the signals before it.
fn reply(stream: &mut UnixStream) -> io::Result<()> {
    loop {
        // The fixed part of a message tells how long the rest is: the length of the body, and of
        // the header fields, which the body follows from the next multiple of 8. They are in the
        // byte order of the message, which its first byte tells: that of its sender.
        let mut fixed = [0; 16];
        stream.read_exact(&mut fixed)?;
        let from_bytes: fn([u8; 4]) -> u32 = match fixed[0] {
            b'l' => u32::from_le_bytes,
            b'B' => u32::from_be_bytes,
            order => return Err(io::Error::other(format!("Unknown byte order `{order}`"))),
        };
        let word = |at: usize| from_bytes(fixed[at..at + 4].try_into().unwrap());
        let (body, fields) = (word(4) as usize, word(12) as usize);
        stream.read_exact(&mut vec![0; fields.next_multiple_of(8) + body])?;
        match fixed[1] {
            // A method return.
            2 => return Ok(()),
            // An error.
            3 => return Err(io::Error::other("The bus replied with an error")),
            _ => (),
        }
    }
}

/// A line that the bus sends over `stream` as it authenticates a client, without its ending.
fn read_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    while !line.ends_with(b"\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    line.truncate(line.len() - 2);

    String::from_utf8(line).map_err(io::Error::other)
}

/// A call to `method` of the bus, with `body`.
fn call_bus<B>(method: &str, body: &B) -> Message
where
    B: Serialize + DynamicType,
{
    Message::method_call(BUS_PATH, method)
        .destination(BUS_NAME)
        .interface(BUS_NAME)
        .build(body)
        .unwrap()
}

/// Have `conn` ask the bus for the signals that match `rule`, and return the messages that `conn`
/// gets from then on.
///
/// The stream is of all the messages of the connection, rather than just those that match `rule`,
/// so that it also shows signals that the bus should not have sent.
async fn listen(conn: &Connection, rule: MatchRule<'_>) -> MessageStream {
    let messages = MessageStream::from(conn);
    DBusProxy::new(conn)
        .await
        .unwrap()
        .add_match_rule(rule)
        .await
        .unwrap();

    messages
}

/// The rule for the signals of the tests that are named `member`.
fn signal_rule(member: &str) -> MatchRule<'_> {
    MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(INTERFACE)
        .member(member)
        .build()
        .unwrap()
}

/// The rule for the signals of the bus that tell that a name changed its owner.
fn owner_changes_rule() -> MatchRule<'static> {
    MatchRule::builder()
        .msg_type(message::Type::Signal)
        .sender(BUS_NAME)
        .interface(BUS_NAME)
        .member("NameOwnerChanged")
        .build()
        .unwrap()
}

/// Emit the signal `member` of the tests from `conn`, to whoever asked for it.
async fn emit(conn: &Connection, member: &str) {
    conn.emit_signal(None::<&str>, PATH, INTERFACE, member, &())
        .await
        .unwrap();
}

/// Have `conn` own the well-known `name`.
async fn request_name(conn: &Connection, name: &str) {
    DBusProxy::new(conn)
        .await
        .unwrap()
        .request_name(name.try_into().unwrap(), RequestNameFlags::empty())
        .await
        .unwrap();
}

/// Check that the bus answers `conn`.
async fn get_id(conn: &Connection) {
    DBusProxy::new(conn).await.unwrap().get_id().await.unwrap();
}

/// The number of the client `conn`, which its unique name carries.
fn id_of(conn: &Connection) -> usize {
    let name = conn.unique_name().unwrap();

    name.as_str()
        .strip_prefix(":busd.")
        .unwrap()
        .parse()
        .unwrap()
}

/// The next message of `stream` that `wanted` picks. The others are skipped: the bus sends clients
/// messages of its own too, such as `NameAcquired`.
async fn next_message<P>(stream: &mut MessageStream, wanted: P) -> Message
where
    P: Fn(&Message) -> bool,
{
    within(async {
        loop {
            let msg = stream
                .next()
                .await
                .expect("The stream ended")
                .expect("The connection failed");
            if wanted(&msg) {
                return msg;
            }
        }
    })
    .await
}

/// Whether `msg` is a call to `member`.
fn is_call(msg: &Message, member: &str) -> bool {
    msg.message_type() == message::Type::MethodCall && has_member(msg, member)
}

/// Whether `msg` is the signal `member`.
fn is_signal(msg: &Message, member: &str) -> bool {
    msg.message_type() == message::Type::Signal && has_member(msg, member)
}

/// Whether the header of `msg` names `member`.
fn has_member(msg: &Message, member: &str) -> bool {
    msg.header()
        .member()
        .is_some_and(|name| name.as_str() == member)
}

/// Whether `msg` is of the interface of the tests.
fn of_interface(msg: &Message) -> bool {
    msg.header()
        .interface()
        .is_some_and(|interface| interface.as_str() == INTERFACE)
}

/// Wait until `ready` holds.
async fn wait_until<P>(ready: P)
where
    P: Fn() -> bool,
{
    let runtime = SharedRuntime::current().unwrap();
    within(async {
        while !ready() {
            runtime.sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
}

/// Wait for `future`, for as long as a test can wait for what should come soon, rather than hang
/// if it never does.
async fn within<F>(future: F) -> F::Output
where
    F: IntoFuture,
{
    SharedRuntime::current()
        .unwrap()
        .timeout(WAIT, future)
        .await
        .expect("Timed out")
}

/// The file of the UNIX socket that `address` is of.
fn socket_path(address: &str) -> String {
    let path = address.strip_prefix("unix:path=").unwrap();

    path.split(',').next().unwrap().to_string()
}

/// Connect to the bus at `address`.
async fn connect(address: &str) -> Connection {
    connection::Builder::address(address).build().await.unwrap()
}

/// Set the tracing subscriber up, once for all the tests of this file, which run in one process.
fn init_tracing() {
    static INIT: Once = Once::new();
    INIT.call_once(busd::tracing_subscriber::init);
}

/// Run a bus on a UNIX socket of a random name, and return the address that its clients connect
/// to.
async fn start() -> (Running, String) {
    serve(Bus::for_address(Some(&unix_address())).await.unwrap())
}

/// Run `bus`, and return the address that its clients connect to.
fn serve(bus: Bus) -> (Running, String) {
    let address = bus.address().to_string();

    (Running::new(bus), address)
}

/// The address of a UNIX socket of a random name.
fn unix_address() -> String {
    let name = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(name);

    format!("unix:path={}", path.display())
}

/// A bus that runs until it is stopped.
struct Running {
    stop: Event,
    task: Task<Bus, zruntime::Shared>,
}

impl Running {
    /// Run `bus`.
    fn new(mut bus: Bus) -> Self {
        let stop = Event::new();
        let stopped = stop.listen();
        let task = zruntime::spawn(async move {
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

        Self { stop, task }
    }

    /// Hand the bus over to its next instance, and run that.
    async fn restart(self) -> Self {
        Self::take_over(self.hand_over().await).await
    }

    /// Stop the bus and hand it over.
    async fn hand_over(self) -> Handover {
        self.stop().await.hand_over().await.unwrap()
    }

    /// Run the instance of the bus that takes over from the one that handed `handover` over.
    async fn take_over(handover: Handover) -> Self {
        Self::new(Bus::take_over(handover, None).await.unwrap())
    }

    /// Stop the bus and clean up after it.
    async fn finish(self) {
        self.stop().await.cleanup().await.unwrap();
        let _ = SharedRuntime::current()
            .unwrap()
            .sleep(Duration::ZERO)
            .await;
    }

    /// Stop the bus, and take it back.
    async fn stop(self) -> Bus {
        self.stop.notify(usize::MAX);

        self.task.await.unwrap()
    }
}

/// The name that the service of the tests owns, and the path and the interface that it serves.
const SERVICE: &str = "org.zbus.Handover";
const PATH: &str = "/org/zbus/Handover";
const INTERFACE: &str = "org.zbus.Handover";

/// The name that the client of `client_leaves` owns.
const LEAVER: &str = "org.zbus.Handover.Leaver";

/// The name, interface and path of the bus.
const BUS_NAME: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";

/// How long a test waits for what should come soon, such as a message that is on its way.
const WAIT: Duration = Duration::from_secs(10);

/// How long a test waits to see that nothing more comes.
const QUIET: Duration = Duration::from_millis(200);

/// How long the bus is given to send a message in the tests where a client doesn't take it for a
/// while.
const LONG_SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the tests give the bus to fill up a receiver that doesn't take what the bus sends it,
/// and how long they give a bus that waits for a handshake as it hands over to be done, at least.
const FILL_UP: Duration = Duration::from_millis(100);
const PAUSE: Duration = Duration::from_millis(400);

/// How long a test waits for what it still lacks to come, when it has had nothing for a while.
const SILENCE: Duration = Duration::from_secs(3);
