use std::{
    future::poll_fn,
    io::{self, Read, Write},
    mem,
    net::Shutdown,
    os::unix::net::UnixStream,
    task::Poll,
};

use futures_util::{future::join, FutureExt, StreamExt};
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use zbus::{
    connection, fdo::ConnectionCredentials, message, Connection, MatchRule, Message, MessageStream,
    OwnedGuid,
};
use zoter::{Restore, Save};
use zruntime::unblock;

use super::*;
use crate::{
    bus::{Bus, STOP_TIMEOUT},
    fdo::{
        self,
        hook::{FOLLOWING_INTERFACE, FOLLOWING_PATH, GATED_INTERFACE, GATED_PATH},
        tests::BlockedReload,
        QUEUED,
    },
    match_rules::MatchRules,
    peer::SEND_TIMEOUT,
    user::Charge,
};

/// A client that sends a last signal and shuts its writes down, once the bus settled, but before it
/// hands the connection over, is handed over with the signal and the end of what it sent, which are
/// still in its socket: the next instance passes the signal on, and then cleans up after the
/// client.
#[test]
#[timeout(30000)]
fn last_signal_of_a_client_that_closes_its_end_is_handed_over() {
    crate::testing::block_on(async {
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
        let handed_over = HandedOver::new(bus);
        assert!(
            handed_over.clients.iter().any(|client| client.id == 2),
            "the client that closed its end was left out"
        );

        let _next = handed_over.take_over(Carry::AsItIs).await;
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
/// disconnecting it, and is cleaned up after: the bus hands over no client that it disconnects,
/// so it serves the client to the end of its input, as any client that went.
#[test]
#[timeout(30000)]
fn last_signal_of_a_client_that_a_send_fails_to_is_passed_on() {
    crate::testing::block_on(async {
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
        // `x` goes once the bus took the reader of `y` back, to serve it on, and before the
        // chore that serves it forwards the signal of `y` to `x`: the runtime runs one task at
        // a time, and `join` polls this each time that it is polled, so this runs as soon as
        // the bus waits for that chore, before the chore runs.
        let x_goes = poll_fn(|_| {
            let taken = peers
                .inner
                .routing
                .try_borrow_mut()
                .is_ok_and(|mut routing| {
                    routing
                        .peers
                        .get_mut(y_name.as_str())
                        .is_none_or(|y| y.client_mut().paused().is_none())
                });
            if !taken {
                return Poll::Pending;
            }
            x_socket.shutdown(Shutdown::Both).unwrap();

            Poll::Ready(())
        });
        join(peers.settle(), x_goes).await;
        let handed_over = HandedOver::new(bus);

        let _next = handed_over.take_over(Carry::AsItIs).await;
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

/// The bus doesn't settle, to hand over, while it has a call under way. It settles once it has
/// answered the call. The caller gets the reply once, whichever instance writes it to the caller,
/// since the caller's connection keeps what the caller didn't take for the next instance.
#[test]
#[timeout(30000)]
fn bus_hands_over_once_the_calls_under_way_are_answered() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (caller, _socket) = connect(&bus, 1).await;
        let mut messages = MessageStream::from(&caller);
        let pass = Message::method_call(GATED_PATH, "Pass")
            .interface(GATED_INTERFACE)
            .destination(fdo::BUS_NAME)
            .build(&0u32)
            .unwrap();

        caller.send(&pass).await.unwrap();
        peers.hook().gate.reached().await;
        peers.pause(STOP_TIMEOUT).await.unwrap();
        let runtime = runtime::local().unwrap();
        let mut settling = pin!(peers.settle());
        let settled = runtime
            .timeout(Duration::from_millis(200), settling.as_mut())
            .await;
        assert!(settled.is_err(), "the bus settled with a call under way");
        peers.hook().gate.open();
        settling.await;
        let handed_over = HandedOver::new(bus);

        let _next = handed_over.take_over(Carry::AsItIs).await;
        let ping = call_bus("GetId").build(&()).unwrap();
        caller.send(&ping).await.unwrap();
        let replies = replies_until(&mut messages, &pass, &ping).await;
        assert_eq!(replies, [&pass, &ping].map(serial));
    });
}

/// The signal that follows the reply to a call is part of the call. The bus doesn't settle, to
/// hand over, until it has sent the signal too. The caller gets the signal once, after the reply,
/// and before the replies to the calls that it makes once the next instance has taken over.
#[test]
#[timeout(30000)]
fn signal_that_follows_a_reply_is_part_of_the_call() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (caller, _socket) = connect(&bus, 1).await;
        add_match(&caller, FOLLOWING_INTERFACE).await;
        let mut messages = MessageStream::from(&caller);
        let follow = Message::method_call(FOLLOWING_PATH, "Follow")
            .interface(FOLLOWING_INTERFACE)
            .destination(fdo::BUS_NAME)
            .build(&())
            .unwrap();

        caller.send(&follow).await.unwrap();
        // The bus sent the reply, and the signal waits at the gate.
        peers.hook().follow.reached().await;
        peers.pause(STOP_TIMEOUT).await.unwrap();
        let runtime = runtime::local().unwrap();
        let mut settling = pin!(peers.settle());
        let settled = runtime
            .timeout(Duration::from_millis(200), settling.as_mut())
            .await;
        assert!(
            settled.is_err(),
            "the bus settled with a signal yet to follow a reply"
        );
        peers.hook().follow.open();
        settling.await;
        let handed_over = HandedOver::new(bus);

        let _next = handed_over.take_over(Carry::AsItIs).await;
        let ping = call_bus("GetId").build(&()).unwrap();
        caller.send(&ping).await.unwrap();
        let mut got = Vec::new();
        loop {
            let msg = messages.next().await.unwrap().unwrap();
            let header = msg.header();
            match msg.message_type() {
                // Not the reply to the call of `add_match`, which may still come out of the
                // stream: see `replies_until`.
                message::Type::MethodReturn
                    if header
                        .reply_serial()
                        .is_some_and(|replied| replied < serial(&follow)) => {}
                message::Type::MethodReturn => {
                    let replied = header.reply_serial();
                    got.push(format!("reply to {}", replied.unwrap()));
                    if replied == Some(serial(&ping)) {
                        break;
                    }
                }
                message::Type::Signal => got.push(header.member().unwrap().to_string()),
                _ => (),
            }
        }
        let expected = [
            format!("reply to {}", serial(&follow)),
            "Followed".to_string(),
            format!("reply to {}", serial(&ping)),
        ];
        assert_eq!(got, expected);
    });
}

/// Each call to the bus gets one reply, in the order of the calls, across a hand-over, and so do
/// the signals that the calls make the bus send. The instance that hands over answers the calls
/// that it took: the call under way, which waits at the gate, and the calls queued behind it. The
/// next instance answers the calls that the previous instance didn't take. Those calls go to it
/// with their caller's connection, as it is or through its record, which is how a bus that systemd
/// restarts stores the connection.
#[test]
#[timeout(30000)]
fn calls_to_the_bus_get_one_reply_each_across_a_hand_over() {
    for carry in [Carry::AsItIs, Carry::Records] {
        crate::testing::block_on(one_reply_each_across_a_hand_over(carry));
    }
}

/// Have a bus hand over with calls to it under way, queued, and left to the next instance, and
/// check that each call gets one reply, in order, and that each signal comes once, in order.
///
/// The next instance gets the caller's connection as `carry` says.
async fn one_reply_each_across_a_hand_over(carry: Carry) {
    // More than the queue holds, and than the task that serves the caller takes at once.
    const MANY: u32 = 150;

    let bus = Bus::for_address(Some(&address())).await.unwrap();
    let peers = bus.peers().clone();
    let (caller, _socket) = connect(&bus, 1).await;
    add_match(&caller, GATED_INTERFACE).await;
    let mut messages = MessageStream::from(&caller);
    let pass = Message::method_call(GATED_PATH, "Pass")
        .interface(GATED_INTERFACE)
        .destination(fdo::BUS_NAME)
        .build(&MANY)
        .unwrap();
    caller.send(&pass).await.unwrap();
    peers.hook().gate.reached().await;
    let calls: Vec<_> = (0..MANY)
        .map(|_| call_bus("GetId").build(&()).unwrap())
        .collect();
    caller.send_all(&calls).await.unwrap();
    // The queue is full, and a call waits for room in it, besides the call under way. Each of
    // these calls is a chore of the bus.
    until(|| peers.inner.chores.running() >= QUEUED.get() + 2).await;

    // The task that serves the caller waits for room in the queue, so the bus is only done pausing
    // once the call under way is answered.
    let opening = async { peers.hook().gate.open() };
    let (paused, ()) = join(peers.pause(STOP_TIMEOUT), opening).await;
    paused.unwrap();
    peers.settle().await;
    let handed_over = HandedOver::new(bus);
    // The calls that the bus didn't take are in the unread bytes of the caller's connection, or
    // still in the caller's socket, which goes to the next instance with the connection.
    let connection = &handed_over.clients[0].connection;
    let unread = rustix::io::ioctl_fionread(connection.socket()).unwrap();
    assert!(
        connection.unread_len() > 0 || unread > 0,
        "the bus took every call, rather than leave some to the next instance"
    );

    let _next = handed_over.take_over(carry).await;
    // And nothing twice: the reply to a call made now, and a signal emitted now, come right after
    // the others.
    let ping = call_bus("GetId").build(&()).unwrap();
    caller.send(&ping).await.unwrap();
    caller
        .emit_signal(None::<()>, GATED_PATH, GATED_INTERFACE, "Passed", &MANY)
        .await
        .unwrap();
    let expected: Vec<_> = [&pass]
        .into_iter()
        .chain(&calls)
        .chain([&ping])
        .map(serial)
        .collect();
    let mut replies = Vec::new();
    let mut passed = Vec::new();
    while replies.len() < expected.len() || passed.len() <= MANY as usize {
        let msg = messages.next().await.unwrap().unwrap();
        match msg.message_type() {
            message::Type::MethodReturn => {
                // Not the reply to the call of `add_match`, which may still come out of the
                // stream: see `replies_until`.
                let replied = msg.header().reply_serial().unwrap();
                if replied >= serial(&pass) {
                    replies.push(replied);
                }
            }
            message::Type::Signal => passed.push(msg.body().deserialize::<u32>().unwrap()),
            _ => (),
        }
    }
    assert_eq!(replies, expected, "{carry:?}");
    assert_eq!(passed, (0..=MANY).collect::<Vec<_>>(), "{carry:?}");
}

/// A reload of the configuration that is under way holds up a hand-over, as any call to the bus
/// under way does. The bus settles once the reload is done and it has answered the calls after
/// it, in order.
#[test]
#[timeout(30000)]
fn reload_under_way_holds_a_hand_over_up() {
    crate::testing::block_on(async {
        let mut bus = Bus::for_address(Some(&address())).await.unwrap();
        let (reload, blocked) = BlockedReload::new();
        bus.set_reload(reload);
        let peers = bus.peers().clone();
        let (caller, _socket) = connect(&bus, 1).await;
        let mut messages = MessageStream::from(&caller);
        let reload = call_bus("ReloadConfig").build(&()).unwrap();
        let get_id = call_bus("GetId").build(&()).unwrap();

        caller.send(&reload).await.unwrap();
        caller.send(&get_id).await.unwrap();
        let reloading = blocked.started().await;
        peers.pause(STOP_TIMEOUT).await.unwrap();
        let runtime = runtime::local().unwrap();
        let mut settling = pin!(peers.settle());
        let settled = runtime
            .timeout(Duration::from_millis(200), settling.as_mut())
            .await;
        assert!(settled.is_err(), "the bus settled with a reload under way");
        reloading.let_go().await;
        settling.await;
        let handed_over = HandedOver::new(bus);

        let _next = handed_over.take_over(Carry::AsItIs).await;
        let replies = replies_until(&mut messages, &reload, &get_id).await;
        assert_eq!(replies, [&reload, &get_id].map(serial));
    });
}

/// The bus is done pausing as soon as the tasks that serve its clients gave the readers of
/// their messages back, rather than at a later look: a pause takes no longer than its clients
/// take to pause.
#[test]
#[timeout(30000)]
fn pause_ends_once_the_clients_paused() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let _first = connect(&bus, 1).await;
        let _second = connect(&bus, 2).await;

        let mut pausing = pin!(peers.pause(STOP_TIMEOUT));
        // The tasks that serve the clients only give their readers back once they run, after
        // this yields.
        assert!(
            pausing.as_mut().now_or_never().is_none(),
            "the clients paused before their tasks ran"
        );
        until(|| {
            peers
                .inner
                .routing
                .try_borrow_mut()
                .is_ok_and(|mut routing| paused(&mut routing))
        })
        .await;
        assert!(
            matches!(pausing.now_or_never(), Some(Ok(()))),
            "the bus went on pausing once its clients paused"
        );
    });
}

/// A peer that goes while the bus pauses no longer holds up the pause once it is gone.
///
/// The task that serves the peer removes it, instead of giving the reader of its messages back. The
/// bus finds every other client paused first, and is done pausing as soon as the peer is gone.
/// Before the bus pauses, the peer's task takes a call to the bus and the peer's EOF in one batch.
/// It then waits for room to queue the call: a call waits at the gate, and the queue behind it is
/// full. So once the gate opens, the task goes on to the end of the batch, instead of seeing the
/// pause first.
#[test]
#[timeout(30000)]
fn peer_that_goes_while_the_bus_pauses_holds_it_up_no_longer() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (caller, _caller_socket) = connect(&bus, 1).await;
        let caller_name = caller.unique_name().unwrap().clone();
        let (peer, mut socket) = connect(&bus, 2).await;
        let name = peer.unique_name().unwrap().clone();
        // The bus answers a call that waits at the gate, and the caller's calls fill the queue
        // behind it, with none left to wait for room. So the task that serves the caller can pause.
        let pass = Message::method_call(GATED_PATH, "Pass")
            .interface(GATED_INTERFACE)
            .destination(fdo::BUS_NAME)
            .build(&0u32)
            .unwrap();
        caller.send(&pass).await.unwrap();
        peers.hook().gate.reached().await;
        let calls: Vec<_> = (0..QUEUED.get())
            .map(|_| call_bus("GetId").build(&()).unwrap())
            .collect();
        caller.send_all(&calls).await.unwrap();
        until(|| peers.inner.chores.running() == QUEUED.get() + 1).await;
        // The peer calls the bus, and goes, before the task that serves it runs. The task takes
        // the call and the peer's EOF in one batch, and waits for room in the queue.
        let get_id = call_bus("GetId").build(&()).unwrap();
        socket.write_all(get_id.data().bytes()).unwrap();
        socket.shutdown(Shutdown::Both).unwrap();
        until(|| peers.inner.chores.running() == QUEUED.get() + 2).await;

        // The bus pauses its clients when it starts to pause. Each time that a client's task gives
        // the client's reader back, or removes the client, the bus looks again at whether all of
        // its clients paused.
        let mut pausing = pin!(peers.pause(STOP_TIMEOUT));
        assert!(
            pausing.as_mut().now_or_never().is_none(),
            "the bus was done pausing before the tasks that serve its clients ran"
        );
        until(|| {
            peers
                .inner
                .routing
                .try_borrow_mut()
                .is_ok_and(|mut routing| {
                    routing
                        .peers
                        .get_mut(caller_name.as_str())
                        .is_some_and(|caller| caller.client_mut().paused().is_some())
                })
        })
        .await;
        // The look that follows the pause of the caller finds the peer still there, and unpaused.
        assert!(
            pausing.as_mut().now_or_never().is_none(),
            "the bus was done pausing with the peer yet to go"
        );
        // Once the bus has answered the call at the gate and taken the next call out of the queue,
        // the peer's task queues the peer's call, finishes the batch, and removes the peer.
        peers.hook().gate.open();
        until(|| {
            peers
                .inner
                .routing
                .try_borrow_mut()
                .is_ok_and(|mut routing| {
                    routing
                        .peers
                        .get_mut(name.as_str())
                        .is_none_or(|peer| peer.client_mut().paused().is_some())
                })
        })
        .await;
        assert!(
            !peers.routing().has_peer(name.as_str()),
            "the task that served the peer gave the reader back, rather than remove the peer"
        );
        assert!(
            matches!(pausing.now_or_never(), Some(Ok(()))),
            "the bus went on pausing once the peer went"
        );
    });
}

/// A monitor that goes while the bus pauses no longer holds up the pause.
///
/// The task that serves the monitor sees the pause before the monitor's EOF, and gives the reader
/// of its messages back. While the bus settles, it serves the monitor up to that EOF.
#[test]
#[timeout(30000)]
fn monitor_that_goes_while_the_bus_pauses_holds_it_up_no_longer() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (client, socket) = connect(&bus, 1).await;
        let name = client.unique_name().unwrap().clone();
        assert!(peers.make_monitor(&name, MatchRules::default()).await);
        // The bus is done with what it sent as the client connected, and became a monitor, and,
        // once the runtime ran what was ready, the task that serves the monitor waits for it to
        // send something.
        peers.inner.chores.done().await;
        let mut yielded = false;
        until(|| mem::replace(&mut yielded, true)).await;

        // The monitor goes, and the bus pauses, before the task runs.
        socket.shutdown(Shutdown::Both).unwrap();
        let mut pausing = pin!(peers.pause(STOP_TIMEOUT));
        assert!(
            pausing.as_mut().now_or_never().is_none(),
            "the bus was done pausing before the task that serves the monitor ran"
        );
        until(|| {
            peers
                .inner
                .routing
                .try_borrow_mut()
                .is_ok_and(|mut routing| paused(&mut routing))
        })
        .await;
        assert!(
            peers.routing().monitors.contains_key(&name),
            "the task that served the monitor removed it, rather than give the reader back"
        );
        assert!(
            matches!(pausing.now_or_never(), Some(Ok(()))),
            "the bus went on pausing once the monitor gave the reader back"
        );

        // As it settles, the bus finds that the monitor went, and serves it to the end.
        peers.settle().await;
        assert!(
            !peers.routing().monitors.contains_key(&name),
            "the bus settled with the monitor that went"
        );
    });
}

/// A client still in the middle of its handshake once the bus is done pausing, as it hands
/// over, gets disconnected then, rather than once its handshake ends, which may be never: the
/// bus lets go of its socket, and of the objects charged to its user, at once.
#[test]
#[timeout(30000)]
fn a_handshake_under_way_once_the_bus_paused_is_cut_off() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        // The client is the one object of its user.
        peers.users().set_max_objects(1);
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let uid = rustix::process::getuid().as_raw();
        let user = peers.users().user(uid);
        let credentials = ConnectionCredentials::from_socket(&theirs).unwrap();
        let adding = peers.add(bus.guid(), 1, theirs, credentials, SEND_TIMEOUT, uid);
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

/// A client that the bus disconnects once it has paused, because a send to the client fails, isn't
/// handed over. The bus leaves it out, for the next instance to clean up after it as after a client
/// that went.
#[test]
#[timeout(30000)]
fn client_that_the_bus_disconnects_once_it_paused_is_left_out() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (receiver, socket) = connect(&bus, 1).await;
        let name = receiver.unique_name().unwrap().to_string();
        peers.pause(STOP_TIMEOUT).await.unwrap();
        peers.settle().await;
        {
            // The test lets go of the recipient once the send is done, as a send of the bus does.
            let recipient = peers
                .routing()
                .peers
                .get(name.as_str())
                .map(|peer| peer.client().recipient().clone())
                .unwrap();

            // The receiver shut its end down, so the send to it fails.
            socket.shutdown(Shutdown::Both).unwrap();
            let signal = Message::signal("/org/zbus/Test", "org.zbus.Test", "Gone")
                .build(&())
                .unwrap();
            recipient.send(&[signal]).await;
        }
        let (snapshot, clients) = peers.hand_over();
        assert!(
            clients.iter().all(|client| client.id != 1),
            "the client was handed over, though the bus was disconnecting it"
        );
        assert!(snapshot.peers.iter().any(|peer| peer.id == 1));
    });
}

/// The bus disconnects a client whose connection is still held elsewhere when the bus hands over.
///
/// The hand-over takes the connection, so such a connection can't go to the next instance. The bus
/// closes the connection instead, which disconnects the client. Whoever holds the connection then
/// sends nothing more to the client. The bus leaves the client out, and the next instance cleans up
/// after it as after a client that went. Nothing in the bus but the client holds its connection
/// once the bus is paused and settled, since the bus sends nothing then and has let go of what it
/// sent.
#[test]
#[timeout(30000)]
fn client_whose_connection_is_still_held_is_disconnected() {
    crate::testing::block_on(async {
        let bus = Bus::for_address(Some(&address())).await.unwrap();
        let peers = bus.peers().clone();
        let (receiver, mut socket) = connect(&bus, 1).await;
        let name = receiver.unique_name().unwrap().to_string();
        peers.pause(STOP_TIMEOUT).await.unwrap();
        peers.settle().await;
        // The clone holds the client's connection, as a send to the client does while it lasts.
        let _recipient = peers
            .routing()
            .peers
            .get(name.as_str())
            .map(|peer| peer.client().recipient().clone())
            .unwrap();

        let (snapshot, clients) = peers.hand_over();
        assert!(
            clients.iter().all(|client| client.id != 1),
            "the client was handed over, though something still held its connection"
        );
        assert!(snapshot.peers.iter().any(|peer| peer.id == 1));
        // The bus shut its end of the socket down as it closed the connection, so the client reads
        // what its socket still holds, and then EOF, without waiting. A read that would wait finds
        // the connection still open.
        socket.set_nonblocking(true).unwrap();
        if let Err(e) = socket.read_to_end(&mut Vec::new()) {
            panic!("the connection outlived the hand-over: {e}");
        }
    });
}

/// What a bus hands over, as [`HandedOver::new`] takes it out of the bus.
struct HandedOver {
    guid: OwnedGuid,
    snapshot: Snapshot,
    clients: Vec<ClientHandover>,
}

/// How the connections of the clients of a bus get to its next instance.
#[derive(Clone, Copy, Debug)]
enum Carry {
    /// As they are, as to a next instance in the same process.
    AsItIs,
    /// Through their records, as to a next instance in another process.
    Records,
}

impl HandedOver {
    /// Have `bus`, which is paused and settled, hand over, as [`Bus::hand_over`] does once it is:
    /// the connections of its clients. It lets go of its clients without disconnecting them, and
    /// of `bus` itself, as its process does as it exits.
    fn new(bus: Bus) -> Self {
        let peers = bus.peers().clone();
        let (snapshot, clients) = peers.hand_over();
        let guid = bus.guid().clone();

        Self {
            guid,
            snapshot,
            clients,
        }
    }

    /// The peers of the next instance, which takes over from this hand-over. The connections of
    /// the clients get there as `carry` says.
    async fn take_over(self, carry: Carry) -> Peers {
        let Self {
            guid,
            snapshot,
            clients,
        } = self;
        let clients = clients
            .into_iter()
            .map(|ClientHandover { id, connection }| {
                let connection = match carry {
                    Carry::AsItIs => connection,
                    Carry::Records => stored(&connection),
                };

                (id, ClientHandover { id, connection })
            })
            .collect();
        let peers = Peers::new(guid);
        peers
            .restore(snapshot, clients, SEND_TIMEOUT)
            .await
            .unwrap();

        peers
    }
}

/// `handover`, as zoter gives it back to a next instance in another process: restored from its
/// record and its file descriptors.
fn stored(handover: &Handover) -> Handover {
    let fds = handover
        .fds()
        .into_iter()
        .map(|fd| fd.try_clone_to_owned().unwrap())
        .collect();

    Handover::restore(&record_of(handover), fds).unwrap()
}

/// The record that `handover` saves.
fn record_of(handover: &Handover) -> Vec<u8> {
    let mut record = vec![0; handover.saved_len().unwrap()];
    handover.save(&mut record).unwrap();

    record
}

/// Add a match rule for the signals of `interface` to the rules of `conn`.
async fn add_match(conn: &Connection, interface: &str) {
    let rule = MatchRule::builder()
        .msg_type(message::Type::Signal)
        .interface(interface)
        .build()
        .unwrap();
    conn.call_method(
        Some(fdo::BUS_NAME),
        fdo::PATH,
        Some(fdo::BUS_NAME),
        "AddMatch",
        &rule.to_string(),
    )
    .await
    .unwrap();
}

/// The serials of the calls that the replies from `messages` answer, from the reply to `first` up
/// to the reply to `last`.
///
/// The replies to the calls made before `first` are left out. A stream made after a call returned
/// can still yield the reply to that call, such as the reply to the `Hello` that the client sent as
/// it connected. zbus hands a reply to its caller before it hands it to the streams. When the
/// connection reads its socket on another thread, the caller can go on and make the stream in
/// between. zbus numbers the messages of a process in the order that they are built, so such a
/// reply answers a lower serial than `first` has.
async fn replies_until(
    messages: &mut MessageStream,
    first: &Message,
    last: &Message,
) -> Vec<NonZeroU32> {
    let mut replies = Vec::new();
    loop {
        let msg = messages.next().await.unwrap().unwrap();
        if msg.message_type() != message::Type::MethodReturn {
            continue;
        }
        let replied = msg.header().reply_serial().unwrap();
        if replied < serial(first) {
            continue;
        }
        replies.push(replied);
        if replied == serial(last) {
            return replies;
        }
    }
}

/// A call to the bus of `member` of its interface, to build with its body.
fn call_bus(member: &str) -> message::Builder<'_> {
    Message::method_call(fdo::PATH, member)
        .interface(fdo::BUS_NAME)
        .destination(fdo::BUS_NAME)
}

/// The serial of `msg`.
fn serial(msg: &Message) -> NonZeroU32 {
    msg.primary_header().serial_num()
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
    let credentials = ConnectionCredentials::from_socket(&theirs).unwrap();
    let adding = bus
        .peers()
        .add(bus.guid(), id, theirs, credentials, SEND_TIMEOUT, uid);
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
