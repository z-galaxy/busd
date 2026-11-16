use std::{
    future::poll_fn,
    io::{self, Read, Write},
    mem,
    net::Shutdown,
    num::NonZeroUsize,
    os::unix::net::UnixStream,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

use futures_util::{future::join, FutureExt, StreamExt};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use zbus::{
    connection::{self, socket::ChannelHandover},
    message,
    object_server::{ResponseDispatchNotifier, SignalEmitter},
    MatchRule, Message, OwnedGuid,
};
use zoter::{Restore, Save};
use zruntime::{unblock, Event};

use super::*;
use crate::{
    bus::{Bus, STOP_TIMEOUT},
    match_rules::MatchRules,
    peer::SEND_TIMEOUT,
    user::Charge,
};

/// A client that sends a last signal and closes its end of the connection, once the bus took
/// the last of its messages that it forwards itself, but before it hands the connection over,
/// as zbus read the signal and the end of what the client sent by then, is handed over with
/// the signal: the next instance passes the signal on, and then cleans up after the client.
#[test]
#[timeout(30000)]
fn last_signal_of_a_client_that_closes_its_end_is_handed_over() {
    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (receiver, _) = connect(&bus, 1).await;
        let rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .build()
            .unwrap();
        let mut signals = MessageStream::for_match_rule(rule, &receiver, None)
            .await
            .unwrap();
        let (sender, socket) = connect(&bus, 2).await;
        let sender_name = sender.unique_name().unwrap().to_string();

        peers.pause(STOP_TIMEOUT).await.unwrap();
        peers.settle().await;
        let bye = Message::signal("/org/zbus/Test", "org.zbus.Test", "Bye")
            .build(&())
            .unwrap();
        sender.send(&bye).await.unwrap();
        socket.shutdown(Shutdown::Write).unwrap();
        let conn = peers
            .routing()
            .await
            .peers
            .get(sender_name.as_str())
            .map(|peer| peer.conn().clone())
            .unwrap();
        conn.closed().await;
        let handed_over = hand_over(bus).await;
        assert!(
            handed_over.clients.iter().any(|client| client.id == 2),
            "the client that closed its end was left out"
        );

        let _next = take_over(handed_over, Carry::AsItIs).await;
        let mut passed_on = false;
        loop {
            let signal = signals.next().await.unwrap().unwrap();
            let header = signal.header();
            match header.member().map(|member| member.as_str()) {
                Some("Bye") => passed_on = true,
                Some("NameOwnerChanged") => {
                    let (name, _, new_owner): (String, String, String) =
                        signal.body().deserialize().unwrap();
                    if name == sender_name && new_owner.is_empty() {
                        break;
                    }
                }
                _ => (),
            }
        }
        assert!(
            passed_on,
            "the client was cleaned up after before its signal was passed on"
        );
    });
}

/// A client that sends a last signal and goes while the bus settles, and that a send of the
/// bus fails to then, as it went, has its signal passed on all the same, though the bus is
/// disconnecting it, and is cleaned up after: zbus hands over no connection that the bus
/// closed, so the bus serves the client to the end of its stream, as any client that went.
#[test]
#[timeout(30000)]
fn last_signal_of_a_client_that_a_send_fails_to_is_passed_on() {
    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .build()
            .unwrap();
        let (receiver, _) = connect(&bus, 1).await;
        let mut signals = MessageStream::for_match_rule(rule.clone(), &receiver, None)
            .await
            .unwrap();
        // The bus forwards the signals to `x` too, so it sends `x` the signal of `y`.
        let (x, x_socket) = connect(&bus, 2).await;
        let _x_signals = MessageStream::for_match_rule(rule, &x, None).await.unwrap();
        let (y, y_socket) = connect(&bus, 3).await;
        let x_name = x.unique_name().unwrap().to_string();
        let y_name = y.unique_name().unwrap().to_string();

        peers.pause(STOP_TIMEOUT).await.unwrap();
        let bye = Message::signal("/org/zbus/Test", "org.zbus.Test", "Bye")
            .build(&())
            .unwrap();
        x.send(&bye).await.unwrap();
        y.send(&bye).await.unwrap();
        y_socket.shutdown(Shutdown::Both).unwrap();
        let y_conn = peers
            .routing()
            .await
            .peers
            .get(y_name.as_str())
            .map(|peer| peer.conn().clone())
            .unwrap();
        y_conn.closed().await;
        // `x` goes once the bus took the stream of `y` back, to serve it on, and before the
        // chore that serves it forwards the signal of `y` to `x`: the runtime runs one task at
        // a time, and `join` polls this each time that it is polled, so this runs as soon as
        // the bus waits for that chore, before the chore runs.
        let x_goes = poll_fn(|_| {
            let taken = peers.inner.routing.try_write().is_some_and(|mut routing| {
                routing
                    .peers
                    .get_mut(y_name.as_str())
                    .is_none_or(|y| y.paused().is_none())
            });
            if !taken {
                return Poll::Pending;
            }
            x_socket.shutdown(Shutdown::Both).unwrap();

            Poll::Ready(())
        });
        join(peers.settle(), x_goes).await;
        let handed_over = hand_over(bus).await;

        let _next = take_over(handed_over, Carry::AsItIs).await;
        let mut passed_on = Vec::new();
        let mut cleaned_up = Vec::new();
        while cleaned_up.len() < 2 {
            let signal = signals.next().await.unwrap().unwrap();
            let header = signal.header();
            match header.member().map(|member| member.as_str()) {
                Some("Bye") => passed_on.push(header.sender().unwrap().to_string()),
                Some("NameOwnerChanged") => {
                    let (name, _, new_owner): (String, String, String) =
                        signal.body().deserialize().unwrap();
                    if (name == x_name || name == y_name) && new_owner.is_empty() {
                        assert!(
                            passed_on.contains(&name),
                            "`{name}` was cleaned up after before its signal was passed on"
                        );
                        cleaned_up.push(name);
                    }
                }
                _ => (),
            }
        }
    });
}

/// The bus doesn't hand over while it has a call under way, whether it answers the call itself
/// or in a task of its own, as zbus has those of the interfaces that it adds to the bus
/// answered. It hands over once the call is answered, and doesn't pass the reply on: the next
/// instance does, once.
#[test]
#[timeout(30000)]
fn bus_hands_over_once_the_calls_under_way_are_answered() {
    for inline in [false, true] {
        zruntime::block_on(async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            let gate = add_gated(&peers, inline).await;
            let (caller, _socket) = connect(&bus, 1).await;

            let mut calling = pin!(caller.call_method(
                Some(fdo::BUS_NAME),
                GATED_PATH,
                Some(GATED_INTERFACE),
                "Pass",
                &0u32,
            ));
            assert!(
                calling.as_mut().now_or_never().is_none(),
                "the call was answered before its method returned"
            );
            gate.reached().await;
            peers.pause(STOP_TIMEOUT).await.unwrap();
            peers.settle().await;
            let runtime = SharedRuntime::current().unwrap();
            let mut handing_over = pin!(hand_over(bus));
            let handed_over = runtime
                .timeout(Duration::from_millis(200), handing_over.as_mut())
                .await;
            assert!(
                handed_over.is_err(),
                "the bus handed over with a call under way"
            );
            gate.open();
            let handed_over = handing_over.await;
            let answered = runtime
                .timeout(Duration::from_millis(200), calling.as_mut())
                .await;
            assert!(answered.is_err(), "the bus passed the reply on once paused");

            let _next = take_over(handed_over, Carry::AsItIs).await;
            calling.await.unwrap();
        });
    }
}

/// The signal that follows the reply to a call is part of the call, also of one that the bus
/// took once it settled: the bus doesn't hand over until it sent the signal, which the next
/// instance passes on.
#[test]
#[timeout(30000)]
fn signal_that_follows_a_reply_goes_to_the_next_instance() {
    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let follow = Arc::new(Follow::default());
        let service = peers.bus_conn().clone().unwrap();
        let was = service
            .object_server()
            .at(FOLLOWING_PATH, Following(follow.clone()))
            .await;
        assert!(was.unwrap().is_none(), "the bus has the interface already");
        let (caller, _socket) = connect(&bus, 1).await;
        let rule = MatchRule::builder()
            .msg_type(message::Type::Signal)
            .interface(FOLLOWING_INTERFACE)
            .build()
            .unwrap();
        caller
            .call_method(
                Some(fdo::BUS_NAME),
                fdo::DBus::PATH,
                Some(fdo::DBus::INTERFACE),
                "AddMatch",
                &rule.to_string(),
            )
            .await
            .unwrap();
        let mut messages = MessageStream::from(&caller);
        let bus_peer = peers
            .routing()
            .await
            .peers
            .get(fdo::BUS_NAME)
            .map(|bus| bus.conn().clone())
            .unwrap();
        let follow_call = Message::method_call(FOLLOWING_PATH, "Follow")
            .interface(FOLLOWING_INTERFACE)
            .destination(fdo::BUS_NAME)
            .sender(caller.unique_name().unwrap())
            .build(&())
            .unwrap();

        peers.pause(STOP_TIMEOUT).await.unwrap();
        peers.settle().await;
        // The call gets to the bus as one that it forwarded before it paused does, and the
        // bus answers it: the signal that follows the reply waits at the gate.
        bus_peer.send(&follow_call).await.unwrap();
        follow.gate.reached().await;
        let runtime = SharedRuntime::current().unwrap();
        let mut handing_over = pin!(hand_over(bus));
        let handed_over = runtime
            .timeout(Duration::from_millis(200), handing_over.as_mut())
            .await;
        assert!(
            handed_over.is_err(),
            "the bus handed over with a signal yet to follow a reply"
        );
        follow.gate.open();
        let handed_over = handing_over.await;
        let sent = follow.sent.lock().unwrap().take();
        sent.expect("the bus handed over ahead of the signal")
            .unwrap();

        let _next = take_over(handed_over, Carry::AsItIs).await;
        // The next instance passes the signal on, ahead of the reply to a call made now.
        let ping = Message::method_call(fdo::DBus::PATH, "GetId")
            .interface(fdo::DBus::INTERFACE)
            .destination(fdo::BUS_NAME)
            .build(&())
            .unwrap();
        caller.send(&ping).await.unwrap();
        let mut signals = Vec::new();
        loop {
            let msg = messages.next().await.unwrap().unwrap();
            let header = msg.header();
            match msg.message_type() {
                message::Type::MethodReturn
                    if header.reply_serial() == Some(ping.primary_header().serial_num()) =>
                {
                    break;
                }
                message::Type::Signal => signals.push(header.member().unwrap().to_string()),
                _ => (),
            }
        }
        assert_eq!(signals, ["Followed"]);
    });
}

/// What is on its way between the bus and its clients as the bus hands over goes to the next
/// instance, on the connection on which the bus serves the D-Bus API, as it is or through its
/// record, as a bus that systemd restarts stores it: the calls that the bus didn't take, as it
/// was answering another one, and the reply and the signals that it sent once it paused, more
/// of each than the connection holds on its way, so that its ends keep some to write. The next
/// instance answers the calls, and passes the reply and the signals on, each once, in order.
#[test]
#[timeout(30000)]
fn what_is_on_its_way_to_and_from_the_bus_goes_to_the_next_instance() {
    for carry in [Carry::AsItIs, Carry::Records] {
        zruntime::block_on(what_is_on_its_way_goes_to_the_next_instance(carry));
    }
}

/// Have a bus hand over with calls to it that it didn't take, and a reply and signals that it
/// sent once it paused, and check that the next instance, which gets the connection of the
/// bus as `carry` says, answers the calls and passes the rest on, each once, in order.
async fn what_is_on_its_way_goes_to_the_next_instance(carry: Carry) {
    // More than the two ends of the connection of the bus hold between them.
    const MANY: u32 = 150;

    let bus = Bus::for_address(Some(&address())).await.unwrap();
    let peers = bus.peers().clone();
    // The bus answers the calls of the interface itself, one at a time, as those to
    // `org.freedesktop.DBus`, so it takes no call while the method waits at the gate.
    let gate = add_gated(&peers, true).await;
    let (caller, _socket) = connect(&bus, 1).await;
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(GATED_INTERFACE)
        .build()
        .unwrap();
    let call_bus = |member: &'static str| {
        Message::method_call(fdo::DBus::PATH, member)
            .interface(fdo::DBus::INTERFACE)
            .destination(fdo::BUS_NAME)
    };
    caller
        .call_method(
            Some(fdo::BUS_NAME),
            fdo::DBus::PATH,
            Some(fdo::DBus::INTERFACE),
            "AddMatch",
            &rule.to_string(),
        )
        .await
        .unwrap();
    let mut messages = MessageStream::from(&caller);

    let pass = Message::method_call(GATED_PATH, "Pass")
        .interface(GATED_INTERFACE)
        .destination(fdo::BUS_NAME)
        .build(&MANY)
        .unwrap();
    caller.send(&pass).await.unwrap();
    gate.reached().await;
    // The bus forwarded the first call. The others get to the bus as those that it forwards
    // do, the first of them before it pauses, and wait for it to be done with the first call.
    let bus_peer = peers
        .routing()
        .await
        .peers
        .get(fdo::BUS_NAME)
        .map(|bus| bus.conn().clone())
        .unwrap();
    let forwarded: Vec<_> = (0..MANY)
        .map(|_| {
            call_bus("GetId")
                .sender(caller.unique_name().unwrap())
                .build(&())
                .unwrap()
        })
        .collect();
    bus_peer.send(&forwarded[0]).await.unwrap();
    peers.pause(STOP_TIMEOUT).await.unwrap();
    bus_peer.send_all(&forwarded[1..]).await.unwrap();
    join(peers.settle(), async { gate.open() }).await;
    let handed_over = hand_over(bus).await;
    let own = &handed_over.own;
    assert_ne!(
        own.service.unread_len(),
        0,
        "the bus took the calls that it got while it answered another"
    );
    assert_ne!(
        own.peer.unread_len(),
        0,
        "the bus passed on what it sent once it paused"
    );
    assert_ne!(
        own.service.unwritten_len(),
        0,
        "the bus wrote more than its connection holds"
    );
    assert_ne!(
        own.peer.unwritten_len(),
        0,
        "the peer of the bus wrote more than its connection holds"
    );

    let _next = take_over(handed_over, carry).await;
    // The reply to the first call and the signals of its method, from the instance that handed
    // over, and the replies to the other calls, from the next instance, in order, with nothing
    // more sent to the bus, so that it writes what the two ends kept on its own.
    let serial = |msg: &Message| msg.primary_header().serial_num();
    let mut expected: Vec<_> = [&pass].into_iter().chain(&forwarded).map(serial).collect();
    let mut answered = Vec::new();
    let mut passed = Vec::new();
    let mut collect = async |answers: usize, signals: usize| {
        while answered.len() < answers || passed.len() < signals {
            let msg = messages.next().await.unwrap().unwrap();
            match msg.message_type() {
                message::Type::MethodReturn => {
                    answered.push(msg.header().reply_serial().unwrap());
                }
                message::Type::Signal => {
                    passed.push(msg.body().deserialize::<u32>().unwrap());
                }
                _ => (),
            }
        }
    };
    collect(expected.len(), MANY as usize).await;
    // And nothing twice: the reply to a call made now, and a signal emitted now, come right
    // after them.
    let ping = call_bus("GetId").build(&()).unwrap();
    caller.send(&ping).await.unwrap();
    caller
        .emit_signal(None::<()>, GATED_PATH, GATED_INTERFACE, "Passed", &MANY)
        .await
        .unwrap();
    expected.push(serial(&ping));
    collect(expected.len(), MANY as usize + 1).await;
    assert_eq!(answered, expected, "{carry:?}");
    assert_eq!(passed, (0..=MANY).collect::<Vec<_>>(), "{carry:?}");
}

/// A bus takes over from one that handed it no connection to serve the D-Bus API on, as a bus
/// of the version before this one, which made sure that nothing was on its way on that
/// connection as it stopped: it serves the D-Bus API on a new one, to the clients that it took
/// over as to any other.
#[test]
#[timeout(30000)]
fn bus_that_was_handed_no_connection_of_its_own_serves_on_a_new_one() {
    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (client, _socket) = connect(&bus, 1).await;
        peers.pause(STOP_TIMEOUT).await.unwrap();
        peers.settle().await;
        let handed_over = hand_over(bus).await;

        let _next = take_over(handed_over, Carry::NotAtAll).await;
        let has_owner: bool = client
            .call_method(
                Some(fdo::BUS_NAME),
                fdo::DBus::PATH,
                Some(fdo::DBus::INTERFACE),
                "NameHasOwner",
                &client.unique_name().unwrap().as_str(),
            )
            .await
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        assert!(has_owner, "the next instance doesn't know the client");
    });
}

/// The connection on which a bus serves the D-Bus API gets to a next instance in another
/// process as its two ends together, off the channel that joined them, through one record: the
/// ends of two connections don't go together, and the two that do come back as they went.
#[test]
#[timeout(30000)]
fn connection_of_the_bus_goes_as_both_of_its_ends() {
    zruntime::block_on(async {
        let paused = || async {
            let bus = Bus::for_address(Some(&address())).await.unwrap();
            let peers = bus.peers().clone();
            peers.pause(STOP_TIMEOUT).await.unwrap();
            peers.settle().await;

            hand_over(bus).await.own
        };
        let (one, other) = (paused().await, paused().await);
        assert!(ChannelHandover::new(one.service, other.peer).is_err());

        let own = paused().await;
        let channel = ChannelHandover::new(own.service, own.peer).unwrap();
        let restored = stored(&channel);
        assert_eq!(record_of(&restored), record_of(&channel));
        let OwnHandover { service, peer } = OwnHandover::from(restored);
        assert_eq!(
            (service.unread_len(), peer.unread_len()),
            (0, 0),
            "a settled bus left something unread"
        );
    });
}

/// The bus is done pausing as soon as the tasks that serve its clients gave the streams of
/// their messages back, rather than at a later look: a pause takes no longer than its clients
/// take to pause.
#[test]
#[timeout(30000)]
fn pause_ends_once_the_clients_paused() {
    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let _first = connect(&bus, 1).await;
        let _second = connect(&bus, 2).await;

        let mut pausing = pin!(peers.pause(STOP_TIMEOUT));
        // The tasks that serve the clients only give their streams back once they run, after
        // this yields.
        assert!(
            pausing.as_mut().now_or_never().is_none(),
            "the clients paused before their tasks ran"
        );
        until(|| {
            peers
                .inner
                .routing
                .try_write()
                .is_some_and(|mut routing| paused(&mut routing))
        })
        .await;
        assert!(
            matches!(pausing.now_or_never(), Some(Ok(()))),
            "the bus went on pausing once its clients paused"
        );
    });
}

/// A client that goes while the bus pauses, as the task that serves it removes it rather than
/// give the stream of its messages back, holds the pause up no longer: the bus is done pausing
/// as soon as the client is gone.
#[test]
#[timeout(30000)]
fn peer_that_goes_while_the_bus_pauses_holds_it_up_no_longer() {
    zruntime::block_on(goes_while_the_bus_pauses(false));
}

/// As with `peer_that_goes_while_the_bus_pauses_holds_it_up_no_longer`, for a monitor.
#[test]
#[timeout(30000)]
fn monitor_that_goes_while_the_bus_pauses_holds_it_up_no_longer() {
    zruntime::block_on(goes_while_the_bus_pauses(true));
}

/// A client still in the middle of its handshake once the bus is done pausing, as it hands
/// over, gets disconnected then, rather than once its handshake ends, which may be never: the
/// bus lets go of its socket, and of the objects charged to its user, at once.
#[test]
#[timeout(30000)]
fn a_handshake_under_way_once_the_bus_paused_is_cut_off() {
    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        // The client is the one object of its user.
        peers.users().set_max_objects(1);
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let uid = rustix::process::getuid().as_raw();
        let user = peers.users().user(uid);
        let server = connection::Builder::unix_stream(theirs);
        let adding = peers.add(bus.guid(), 1, server, SEND_TIMEOUT, uid);
        let cutting_off = async {
            // The client authenticates, but never begins.
            let mut ours = unblock(move || {
                let uid: String = uid
                    .to_string()
                    .bytes()
                    .map(|digit| format!("{digit:02x}"))
                    .collect();
                ours.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())
                    .unwrap();
                let mut line = Vec::new();
                while !line.ends_with(b"\r\n") {
                    let mut byte = [0];
                    ours.read_exact(&mut byte).unwrap();
                    line.push(byte[0]);
                }
                assert!(line.starts_with(b"OK "), "the bus refused the client");

                ours
            })
            .await;
            assert!(Charge::new(&user, &user).is_none());
            peers.pause(Duration::from_millis(100)).await.unwrap();
            // A bus that still had the client would answer this, and keep the connection.
            let (answer, ended) = unblock(move || {
                // The write fails if the bus closed the connection already.
                let _ = ours.write_all(b"CANCEL\r\n");
                let mut answer = Vec::new();
                // The read ends with the connection, or else with the timeout. A connection
                // that the bus closed with the cancel unread is reset.
                let ended = match ours.read_to_end(&mut answer) {
                    Ok(_) => true,
                    Err(e) => e.kind() == io::ErrorKind::ConnectionReset,
                };

                (String::from_utf8_lossy(&answer).into_owned(), ended)
            })
            .await;
            assert!(
                answer.is_empty(),
                "the bus answered `{answer}` once it was done pausing"
            );
            assert!(ended, "the connection outlived the pause");
        };

        let (added, ()) = join(adding, cutting_off).await;
        added.unwrap();
        assert!(
            Charge::new(&user, &user).is_some(),
            "the client is still charged to its user"
        );
    });
}

/// A client that the bus starts disconnecting while the hand-over of its connection waits for
/// its turn to write, as when a send to the client that holds the turn fails, isn't handed
/// over, even where the hand-over gets the connection before the closing of the connection
/// does, as with a lock that goes to the first to wait for it.
#[test]
#[timeout(30000)]
fn client_that_the_bus_disconnects_as_its_hand_over_waits_is_left_out() {
    const CHUNK: usize = 64 * 1024;
    const CHUNKS: usize = 16;

    zruntime::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (mut receiver, _socket) = connect(&bus, 1).await;
        let name = receiver.unique_name().unwrap().to_string();
        // The receiver takes no more of what the bus sends it once a message waits in its
        // queue.
        let _messages = MessageStream::from(&receiver);
        receiver.set_max_queued(NonZeroUsize::MIN);
        peers.pause(STOP_TIMEOUT).await.unwrap();
        peers.settle().await;
        let (conn, recipient) = peers
            .routing()
            .await
            .peers
            .get(name.as_str())
            .map(|peer| (peer.conn().clone(), peer.recipient().clone()))
            .unwrap();
        // The connection keeps what the socket of the receiver doesn't take, and a flush holds
        // the connection as it waits for the receiver to take that.
        let chunk = "x".repeat(CHUNK);
        let chunks: Vec<_> = (0..CHUNKS)
            .map(|_| {
                Message::signal("/org/zbus/Test", "org.zbus.Test", "Chunk")
                    .build(&chunk)
                    .unwrap()
            })
            .collect();
        recipient.send(&chunks).await;
        let mut flushing = Box::pin(conn.flush());
        assert!(
            flushing.as_mut().now_or_never().is_none(),
            "the flush didn't wait for the receiver"
        );
        let mut handing_over = Box::pin(peers.snapshot());
        assert!(
            handing_over.as_mut().now_or_never().is_none(),
            "the hand-over didn't wait for the flush"
        );

        // The bus flags the client before the flush lets go of the connection, as it flags one
        // that a send fails to, and nothing closes the connection before the hand-over gets it.
        recipient.hand_over();
        drop(flushing);
        let (_, clients) = handing_over.await.unwrap();
        assert!(
            clients.iter().all(|client| client.id != 1),
            "the client was handed over, though the bus was disconnecting it"
        );
    });
}

/// Have a client of a bus, a monitor if `monitor`, go while the bus pauses, once the pause took
/// the lock of the routing ahead of the task that serves the client, which waits for the lock
/// to remove the client, and check that the bus is done pausing as soon as the task removed it.
async fn goes_while_the_bus_pauses(monitor: bool) {
    let bus = Bus::for_address(Some(&address())).await.unwrap();
    let peers = bus.peers().clone();
    let (client, socket) = connect(&bus, 1).await;
    let name = client.unique_name().unwrap().clone();
    if monitor {
        assert!(peers.make_monitor(&name, MatchRules::default()).await);
    }
    // The bus is done with what it sent as the client connected, and became a monitor, and,
    // once the runtime ran what was ready, the task that serves the client waits for the client
    // to send something.
    peers.inner.chores.done().await;
    let mut yielded = false;
    until(|| mem::replace(&mut yielded, true)).await;

    {
        // The task that serves the client, which goes, waits for this to remove the client.
        let _reading = peers.inner.routing.try_read().unwrap();
        socket.shutdown(Shutdown::Both).unwrap();
        // A writer that waits for the lock, the task, keeps new readers out.
        until(|| peers.inner.routing.try_read().is_none()).await;
    }
    // The pause takes the lock, which is free, ahead of the task, which only runs again once
    // this yields: the pause doesn't find the client gone.
    let mut pausing = pin!(peers.pause(STOP_TIMEOUT));
    assert!(
        pausing.as_mut().now_or_never().is_none(),
        "the bus was done pausing before the client went"
    );
    until(|| {
        peers.inner.routing.try_write().is_some_and(|routing| {
            !routing.peers.contains_key(&name) && !routing.monitors.contains_key(&name)
        })
    })
    .await;
    assert!(
        matches!(pausing.now_or_never(), Some(Ok(()))),
        "the bus went on pausing once the client went"
    );
}

/// A gate that the method of [`Gated`] waits at, until the test opens it.
#[derive(Default)]
struct Gate {
    reached: AtomicBool,
    open: AtomicBool,
    changed: Event,
}

impl Gate {
    /// Wait at the gate until it is open, and tell that it was reached.
    async fn pass(&self) {
        self.reached.store(true, Ordering::SeqCst);
        self.changed.notify(usize::MAX);
        self.wait_for(&self.open).await;
    }

    /// Wait for a method to have reached the gate.
    async fn reached(&self) {
        self.wait_for(&self.reached).await;
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.changed.notify(usize::MAX);
    }

    async fn wait_for(&self, flag: &AtomicBool) {
        loop {
            // The listener comes first, so that a change right after the check isn't missed.
            let listener = self.changed.listen();
            if flag.load(Ordering::SeqCst) {
                return;
            }
            listener.await;
        }
    }
}

/// Defines an interface whose method waits at a gate, and emits signals once it is through:
/// in a task of its own for each call, as those of the interfaces that zbus adds to the bus
/// run, or not, as those of the interfaces of the bus itself run, as `spawn` says.
macro_rules! gated {
    ($name:ident, $spawn:literal) => {
        struct $name(Arc<Gate>);

        #[zbus::interface(name = "org.busd.Test.Gated", spawn = $spawn)]
        impl $name {
            async fn pass(
                &self,
                signals: u32,
                #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
            ) -> zbus::fdo::Result<()> {
                self.0.pass().await;
                for n in 0..signals {
                    Self::passed(&emitter, n).await?;
                }

                Ok(())
            }

            #[zbus(signal)]
            async fn passed(emitter: &SignalEmitter<'_>, n: u32) -> zbus::Result<()>;
        }
    };
}

gated!(Gated, true);
gated!(InlineGated, false);

/// An interface whose method has a signal follow its reply, once the test lets it.
struct Following(Arc<Follow>);

/// What a test knows of the signal that follows a reply of [`Following`].
#[derive(Default)]
struct Follow {
    /// What the signal waits at, once the reply was sent.
    gate: Gate,
    /// How sending the signal went, once it was sent, or failed to be.
    sent: std::sync::Mutex<Option<zbus::Result<()>>>,
}

#[zbus::interface(name = "org.busd.Test.Following", spawn = false)]
impl Following {
    /// Answers, and has the signal follow the response, once the test opened the gate.
    fn follow(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> ResponseDispatchNotifier<()> {
        let (follow, owned) = (self.0.clone(), emitter.to_owned());

        ResponseDispatchNotifier::followed_by((), emitter.connection(), async move {
            follow.gate.pass().await;
            let sent = Self::followed(&owned).await;
            *follow.sent.lock().unwrap() = Some(sent);
        })
    }

    #[zbus(signal)]
    async fn followed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Add an interface whose method waits at a gate to the bus of `peers`, which answers its
/// calls itself if `inline`, and in tasks of their own otherwise, and return the gate.
async fn add_gated(peers: &Peers, inline: bool) -> Arc<Gate> {
    let gate = Arc::new(Gate::default());
    let service = peers.bus_conn().clone().unwrap();
    let server = service.object_server();
    // What was there before, if anything.
    let free = match inline {
        false => server
            .at(GATED_PATH, Gated(gate.clone()))
            .await
            .map(|was| was.is_none()),
        true => server
            .at(GATED_PATH, InlineGated(gate.clone()))
            .await
            .map(|was| was.is_none()),
    };
    assert!(free.unwrap(), "the bus has the interface already");

    gate
}

/// What a bus hands over, as [`hand_over`] takes it out of the bus.
struct HandedOver {
    guid: OwnedGuid,
    snapshot: Snapshot,
    clients: Vec<ClientHandover>,
    own: OwnHandover,
}

/// How the connection on which a bus serves the D-Bus API gets to its next instance.
#[derive(Clone, Copy, Debug)]
enum Carry {
    /// As it is, as to a next instance in the same process.
    AsItIs,
    /// Through the record of its two ends together, as to a next instance in another process.
    Records,
    /// Not at all, as from a bus of the version before this one, which handed none over.
    NotAtAll,
}

/// Have `bus`, which is paused and settled, hand over, as [`Bus::hand_over`] does once it is:
/// its own connection, then those of its clients. It lets go of its clients without
/// disconnecting them, and of `bus` itself, as its process does as it exits.
async fn hand_over(bus: Bus) -> HandedOver {
    let peers = bus.peers().clone();
    let own = peers.hand_over_us().await.unwrap();
    let (snapshot, clients) = peers.snapshot().await.unwrap();
    let guid = bus.guid().clone();
    peers.retire().await;

    HandedOver {
        guid,
        snapshot,
        clients,
        own,
    }
}

/// The peers of the next instance of the bus that handed `handed_over` over, which gets the
/// connection on which the bus serves the D-Bus API as `carry` says, and the end of that
/// connection on which it answers the calls to it.
async fn take_over(handed_over: HandedOver, carry: Carry) -> (Peers, Connection) {
    let HandedOver {
        guid,
        snapshot,
        clients,
        own,
    } = handed_over;
    let own = match carry {
        Carry::AsItIs => Some(own),
        Carry::Records => {
            let channel = ChannelHandover::new(own.service, own.peer).unwrap();

            Some(OwnHandover::from(stored(&channel)))
        }
        Carry::NotAtAll => None,
    };
    let peers = Peers::new();
    let us = Bus::us(&peers, &guid, own).await.unwrap();
    let clients = clients
        .into_iter()
        .map(|client| (client.id, client))
        .collect();
    let service = peers
        .restore(snapshot, clients, SEND_TIMEOUT, us)
        .await
        .unwrap();

    (peers, service)
}

/// `channel`, the two ends of the connection of a bus off their channel, as zoter gives them
/// back to a next instance in another process: restored from their record and their file
/// descriptors.
fn stored(channel: &ChannelHandover) -> ChannelHandover {
    let fds = channel
        .fds()
        .into_iter()
        .map(|fd| fd.try_clone_to_owned().unwrap())
        .collect();

    ChannelHandover::restore(&record_of(channel), fds).unwrap()
}

/// The record that `channel` saves.
fn record_of(channel: &ChannelHandover) -> Vec<u8> {
    let mut record = vec![0; channel.saved_len().unwrap()];
    channel.save(&mut record).unwrap();

    record
}

/// Wait until `ready`, which is checked again each time that the runtime has run the tasks that
/// are ready to run.
async fn until<F>(mut ready: F)
where
    F: FnMut() -> bool,
{
    poll_fn(|cx| {
        if ready() {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();

        Poll::Pending
    })
    .await
}

/// A client of `bus`, numbered `id`, over a socket pair, and its end of the pair.
async fn connect(bus: &Bus, id: usize) -> (Connection, UnixStream) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let socket = ours.try_clone().unwrap();
    let uid = rustix::process::getuid().as_raw();
    let server = connection::Builder::unix_stream(theirs);
    let adding = bus.peers().add(bus.guid(), id, server, SEND_TIMEOUT, uid);
    let connecting = connection::Builder::unix_stream(ours).build();
    let (added, conn) = join(adding, connecting).await;
    added.unwrap();

    (conn.unwrap(), socket)
}

/// The address of an abstract socket that no other bus listens on.
fn address() -> String {
    format!(
        "unix:abstract={}",
        Alphanumeric.sample_string(&mut rng(), 10)
    )
}

const GATED_PATH: &str = "/org/busd/Test/Gated";
const GATED_INTERFACE: &str = "org.busd.Test.Gated";
const FOLLOWING_PATH: &str = "/org/busd/Test/Following";
const FOLLOWING_INTERFACE: &str = "org.busd.Test.Following";
