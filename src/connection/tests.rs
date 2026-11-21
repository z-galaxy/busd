use std::{
    fmt::Display,
    future::poll_fn,
    io::{IoSlice, Read, Write},
    os::{
        fd::{BorrowedFd, OwnedFd},
        unix::net,
    },
    slice,
    task::Poll,
};

use futures_util::{future::join, poll, StreamExt};
use ntest::timeout;
use serde_bytes::Bytes;
use zbus::{wire::Fd, Guid, MessageStream};
use zruntime::Task;

use super::*;
use crate::testing::within;

// The reads.

/// The messages that come with one read are taken one after the other, and the reader waits for
/// more once it took the last.
#[test]
#[timeout(30000)]
fn messages_whole_in_one_read() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let mut reader = Reader::new(bus);
        let msgs = [signal(0, 0), signal(100, 1), signal(1000, 2)];
        send_raw(&client, &concat(&msgs), &[]);

        for msg in &msgs {
            assert_eq!(next(&mut reader).await.data().bytes(), msg.data().bytes());
        }
        assert!(poll_once(&mut reader).await.is_pending());
    });
}

/// A message that comes in pieces, the first of which doesn't hold all of its primary header, is
/// taken once its last piece came.
#[test]
#[timeout(30000)]
fn message_across_reads() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let mut reader = Reader::new(bus);
        let msg = signal(100, 1);
        let bytes = msg.data().bytes();

        for piece in [&bytes[..5], &bytes[5..40]] {
            send_raw(&client, piece, &[]);
            assert!(poll_once(&mut reader).await.is_pending());
        }
        send_raw(&client, &bytes[40..], &[]);
        assert_eq!(next(&mut reader).await.data().bytes(), bytes);
    });
}

/// Messages longer than the reader reads at a time are read whole, whatever comes before and
/// after them.
#[test]
#[timeout(30000)]
fn messages_longer_than_the_buffer() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let mut reader = Reader::new(bus);
        let msgs = [
            signal(10, 1),
            signal(10_000, 2),
            signal(10, 3),
            signal(100_000, 4),
            signal(5_000, 5),
        ];
        let bytes = concat(&msgs);

        let reading = async {
            let mut read = Vec::new();
            for _ in &msgs {
                read.push(next(&mut reader).await);
            }

            read
        };
        let ((), read) = join(write_all(&client, &bytes, &[]), reading).await;
        for (read, msg) in read.iter().zip(&msgs) {
            assert_eq!(read.data().bytes(), msg.data().bytes());
        }
    });
}

/// A message takes the file descriptor that comes with it.
#[test]
#[timeout(30000)]
fn fd_with_its_message() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let mut reader = Reader::new(bus);
        let (ours, theirs) = fd_pair();
        let msg = signal_with_fds(&[&theirs]);

        send_raw(&client, msg.data().bytes(), &[theirs.as_fd()]);
        let read = next(&mut reader).await;
        assert_eq!(read.data().bytes(), msg.data().bytes());
        let fds = read.data().fds();
        assert_eq!(fds.len(), 1);
        assert_end_of(fds[0].as_fd(), &ours);
    });
}

/// A message takes as many of the file descriptors that came as its header counts, from the first,
/// in the order that they came in, whichever reads brought them. An FD that came with a message
/// that counts none waits for the next message.
#[test]
#[timeout(30000)]
fn fds_sent_ahead() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let mut reader = Reader::new(bus);
        let [(ours_1, theirs_1), (ours_2, theirs_2)] = [fd_pair(), fd_pair()];
        let first = signal(10, 1);
        let second = signal_with_fds(&[&theirs_1, &theirs_2]);

        send_raw(&client, first.data().bytes(), &[theirs_1.as_fd()]);
        let read = next(&mut reader).await;
        assert_eq!(read.data().bytes(), first.data().bytes());
        assert!(read.data().fds().is_empty());

        send_raw(&client, second.data().bytes(), &[theirs_2.as_fd()]);
        let read = next(&mut reader).await;
        assert_eq!(read.data().bytes(), second.data().bytes());
        let fds = read.data().fds();
        assert_eq!(fds.len(), 2);
        assert_end_of(fds[0].as_fd(), &ours_1);
        assert_end_of(fds[1].as_fd(), &ours_2);
    });
}

/// Up to 3 sets of file descriptors wait for the message that counts them, and a read that brings a
/// fourth ends the input with an error.
///
/// Each set holds the FDs that came with one read. The test writes each piece of the message only
/// once the reader has read the piece before and found the message not whole. So each read brings
/// the FDs of one write, however a platform hands out the FDs of several writes that wait.
#[test]
#[timeout(30000)]
fn fourth_fd_set_is_refused() {
    zruntime::block_on(async {
        for sets in [3, 4] {
            let (bus, client) = pair();
            let mut reader = Reader::new(bus);
            let pairs: Vec<_> = (0..sets).map(|_| fd_pair()).collect();
            let theirs: Vec<_> = pairs.iter().map(|(_, theirs)| theirs).collect();
            let msg = signal_with_fds(&theirs);
            let bytes = msg.data().bytes();
            let pieces: Vec<_> = bytes.chunks(bytes.len().div_ceil(sets)).collect();
            assert_eq!(pieces.len(), sets);

            for (piece, fd) in pieces[..sets - 1].iter().zip(&theirs) {
                send_raw(&client, piece, &[fd.as_fd()]);
                assert!(poll_once(&mut reader).await.is_pending());
            }
            send_raw(&client, pieces[sets - 1], &[theirs[sets - 1].as_fd()]);
            match sets {
                3 => {
                    let read = next(&mut reader).await;
                    assert_eq!(read.data().bytes(), bytes);
                    for (fd, (ours, _)) in read.data().fds().iter().zip(&pairs) {
                        assert_end_of(fd.as_fd(), ours);
                    }
                }
                _ => {
                    let error = reader.next().await.unwrap().unwrap_err();
                    assert!(is_io_error(&error, io::ErrorKind::InvalidData), "{error}");
                    assert!(reader.next().await.is_none());
                }
            }
        }
    });
}

/// Messages that come one after the other with their file descriptors each take their own FDs,
/// even when there are more such messages than sets of FDs may wait.
///
/// The sets that the messages took no longer count, whether a message took all of a set or a part
/// of it. So 3 sets may still wait after them.
#[test]
#[timeout(30000)]
fn fds_of_one_message_after_another() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let mut reader = Reader::new(bus);
        // Messages with an FD each, in their own writes, and then two in one write. That write
        // brings both FDs in one set, and the first message takes a part of it.
        let mut writes: Vec<Vec<_>> = (0..4).map(|_| vec![fd_pair()]).collect();
        writes.push(vec![fd_pair(), fd_pair()]);
        for pairs in &writes {
            let msgs: Vec<_> = pairs
                .iter()
                .map(|(_, theirs)| signal_with_fds(&[theirs]))
                .collect();
            let fds: Vec<_> = pairs.iter().map(|(_, theirs)| theirs.as_fd()).collect();
            send_raw(&client, &concat(&msgs), &fds);
            for (msg, (ours, _)) in msgs.iter().zip(pairs) {
                let read = next(&mut reader).await;
                assert_eq!(read.data().bytes(), msg.data().bytes());
                assert_eq!(read.data().fds().len(), 1);
                assert_end_of(read.data().fds()[0].as_fd(), ours);
            }
        }

        // 3 sets, each sent ahead of the message that counts them all.
        let pairs = [fd_pair(), fd_pair(), fd_pair()];
        for (byte, (_, theirs)) in (1..).zip(&pairs) {
            let msg = signal(10, byte);
            send_raw(&client, msg.data().bytes(), &[theirs.as_fd()]);
            assert_eq!(next(&mut reader).await.data().bytes(), msg.data().bytes());
        }
        let theirs: Vec<_> = pairs.iter().map(|(_, theirs)| theirs).collect();
        send_raw(&client, signal_with_fds(&theirs).data().bytes(), &[]);
        let read = next(&mut reader).await;
        assert_eq!(read.data().fds().len(), pairs.len());
        for (fd, (ours, _)) in read.data().fds().iter().zip(&pairs) {
            assert_end_of(fd.as_fd(), ours);
        }
    });
}

/// EOF at the end of a message ends the stream. EOF in the middle of a message, before its primary
/// header is in or after, ends the stream too, with an error first.
#[test]
#[timeout(30000)]
fn end_of_the_input() {
    zruntime::block_on(async {
        let msg = signal(100, 1);
        let bytes = msg.data().bytes();
        for sent in [bytes.len(), 5, 50] {
            let (bus, client) = pair();
            let mut reader = Reader::new(bus);
            send_raw(&client, &bytes[..sent], &[]);
            client.shutdown(Shutdown::Write).unwrap();

            if sent == bytes.len() {
                assert_eq!(next(&mut reader).await.data().bytes(), bytes);
            } else {
                let error = reader.next().await.unwrap().unwrap_err();
                assert!(is_io_error(&error, io::ErrorKind::UnexpectedEof), "{error}");
            }
            assert!(reader.next().await.is_none());
            assert!(reader.next().await.is_none());
        }
    });
}

/// A primary header that isn't one ends the input with an error, and so does the primary header of
/// a message longer than D-Bus lets a message be.
#[test]
#[timeout(30000)]
fn invalid_headers_end_the_input() {
    zruntime::block_on(async {
        let bytes = signal(0, 0).data().bytes()[..16].to_vec();
        let mut unknown_endian = bytes.clone();
        unknown_endian[0] = b'x';
        // The length of the body, which reads the same in either byte order.
        let mut too_long = bytes;
        too_long[4..8].copy_from_slice(&u32::MAX.to_ne_bytes());

        for bytes in [unknown_endian, too_long] {
            let (bus, client) = pair();
            let mut reader = Reader::new(bus);
            send_raw(&client, &bytes, &[]);

            let error = reader.next().await.unwrap().unwrap_err();
            assert!(
                matches!(
                    error,
                    zbus::Error::IncorrectEndian | zbus::Error::ExcessData
                ),
                "{error}"
            );
            assert!(reader.next().await.is_none());
        }
    });
}

/// A message without the file descriptors that its header counts ends the input with an error.
#[test]
#[timeout(30000)]
fn missing_fds_end_the_input() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let mut reader = Reader::new(bus);
        let (_ours, theirs) = fd_pair();
        let msg = signal_with_fds(&[&theirs]);

        send_raw(&client, msg.data().bytes(), &[]);
        let error = reader.next().await.unwrap().unwrap_err();
        assert!(matches!(error, zbus::Error::MissingParameter(_)), "{error}");
        assert!(reader.next().await.is_none());
    });
}

/// The bus takes everything that the client sent before the bus shut the client's socket down,
/// file descriptors and all, and then EOF.
///
/// This needs Linux, which keeps what a socket holds readable after the shutdown of its own reads.
#[cfg(target_os = "linux")]
#[test]
#[timeout(30000)]
fn what_the_bus_reads_of_a_client_that_it_cut() {
    zruntime::block_on(async {
        let (msgs, read, ours) = cut().await;

        assert_eq!(read.len(), msgs.len(), "{read:?}");
        for (read, msg) in read.iter().zip(&msgs) {
            let read = read.as_ref().unwrap();
            assert_eq!(read.data().bytes(), msg.data().bytes());
            assert_eq!(read.data().fds().len(), msg.data().fds().len());
        }
        assert_end_of(read[1].as_ref().unwrap().data().fds()[0].as_fd(), &ours);
    });
}

/// The bus takes nothing of what the client sent before the bus shut the client's socket down, only
/// EOF. This is because shutting down a socket's reads on macOS discards what the socket holds,
/// file descriptors and all.
#[cfg(not(target_os = "linux"))]
#[test]
#[timeout(30000)]
fn what_the_bus_reads_of_a_client_that_it_cut() {
    zruntime::block_on(async {
        let (_, read, _) = cut().await;

        assert!(
            read.is_empty(),
            "The bus read what the client sent before it shut the socket down: {read:?}"
        );
    });
}

/// The messages that a client sent before the bus shut its socket down, the messages that the bus
/// read after the shutdown, until EOF, and one end of a socket pair.
///
/// The bus shuts a client's socket down once a write to the client failed or timed out. One of the
/// messages carries the other end of the socket pair as a file descriptor.
async fn cut() -> ([Message; 3], Vec<zbus::Result<Message>>, net::UnixStream) {
    let (bus, client) = pair();
    let mut reader = Reader::new(bus.clone());
    let (ours, theirs) = fd_pair();
    let msgs = [signal(10, 1), signal_with_fds(&[&theirs]), signal(20, 2)];
    send_raw(&client, msgs[0].data().bytes(), &[]);
    send_raw(&client, msgs[1].data().bytes(), &[theirs.as_fd()]);
    send_raw(&client, msgs[2].data().bytes(), &[]);

    bus.shutdown(Shutdown::Both).unwrap();
    let mut read = Vec::new();
    while let Some(msg) = within(reader.next()).await {
        read.push(msg);
    }

    (msgs, read, ours)
}

// The writes, through the grouping of what is queued into writes, which can't be seen from the
// other end of a socket.

/// A write takes up to 16 messages, however small they are, as zbus's writes do. The rest of a
/// partly written message counts among them.
#[test]
fn writes_of_16_messages_at_most() {
    let mut outgoing = Outgoing::new();
    for _ in 0..20 {
        outgoing.push(signal(10, 1));
    }

    assert_eq!(outgoing.next_write(), (16, false));
    outgoing.front_written = 5;
    assert_eq!(outgoing.next_write(), (16, false));
    outgoing.chunks.drain(..16);
    outgoing.front_written = 0;
    assert_eq!(outgoing.next_write(), (4, false));
}

/// A write ends with the message that brings it to 16 KiB or more, as zbus's writes end. After a
/// write that ended in the middle of the first message, only what is left of it counts.
#[test]
fn writes_of_16_kib_or_a_message_more() {
    let six_kib = || signal(6 * 1024, 1);
    let mut outgoing = Outgoing::new();
    for _ in 0..4 {
        outgoing.push(six_kib());
    }
    // 12 KiB and some, which the third message brings to 18.
    assert_eq!(outgoing.next_write(), (3, false));

    // A message of 16 KiB or more is the last of its write.
    let mut outgoing = Outgoing::new();
    outgoing.push(signal(20 * 1024, 1));
    outgoing.push(signal(10, 2));
    assert_eq!(outgoing.next_write(), (1, false));
    // Unless its first bytes went with a write before, which leave less than 16 KiB of it.
    outgoing.front_written = 18 * 1024;
    assert_eq!(outgoing.next_write(), (2, false));

    // However long the message that brings a write to 16 KiB is, it goes in that write.
    let mut outgoing = Outgoing::new();
    for _ in 0..10 {
        outgoing.push(signal(30, 1));
    }
    outgoing.push(signal(100 * 1024, 2));
    outgoing.push(signal(30, 3));
    assert_eq!(outgoing.next_write(), (11, false));
}

/// A message with file descriptors goes in its own writes, and the first of those carries the FDs
/// with the message's first byte. The rest of a partly written message goes without FDs.
#[test]
fn messages_with_fds_go_alone() {
    let (_ours, theirs) = fd_pair();
    let mut outgoing = Outgoing::new();
    for msg in [
        signal(10, 1),
        signal_with_fds(&[&theirs]),
        signal(10, 2),
        signal(10, 3),
    ] {
        outgoing.push(msg);
    }

    // The message before it goes without it.
    assert_eq!(outgoing.next_write(), (1, false));
    outgoing.chunks.pop_front();
    // It goes alone, with its FDs.
    assert_eq!(outgoing.next_write(), (1, true));
    // The rest of it too, without them.
    outgoing.front_written = 10;
    assert_eq!(outgoing.next_write(), (1, false));
    // And the messages after it go together.
    outgoing.chunks.pop_front();
    outgoing.front_written = 0;
    assert_eq!(outgoing.next_write(), (2, false));
}

// The writes, through a socket.

/// What a connection writes is read as the messages that it sent, each with its file descriptors.
#[test]
#[timeout(30000)]
fn messages_are_written_with_their_fds() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let connection = connection(bus, LONG_TIMEOUT);
        let mut client = Reader::new(Arc::new(client));
        let pairs = [fd_pair(), fd_pair(), fd_pair()];
        let msgs = [
            signal(10, 1),
            signal_with_fds(&[&pairs[0].1]),
            signal(10, 2),
            signal_with_fds(&[&pairs[1].1, &pairs[2].1]),
            signal(10, 3),
        ];

        assert!(within(connection.send(&msgs)).await.is_empty());
        let mut read = Vec::new();
        for msg in &msgs {
            let msg_read = next(&mut client).await;
            assert_eq!(msg_read.data().bytes(), msg.data().bytes());
            assert_eq!(msg_read.data().fds().len(), msg.data().fds().len());
            read.push(msg_read);
        }
        let fds = read[1].data().fds().iter().chain(read[3].data().fds());
        for (fd, (ours, _)) in fds.zip(&pairs) {
            assert_end_of(fd.as_fd(), ours);
        }
    });
}

/// The messages with file descriptors are given back rather than sent to a client that didn't
/// negotiate passing them, and the others go.
#[test]
#[timeout(30000)]
fn fds_for_a_client_without_fd_passing() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let connection = Connection::new(bus, name(), false, LONG_TIMEOUT);
        let (_ours, theirs) = fd_pair();
        let msgs = [signal(10, 1), signal_with_fds(&[&theirs]), signal(10, 2)];

        let dropped = within(connection.send(&msgs)).await;
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].data().bytes(), msgs[1].data().bytes());
        let sent = concat(&[msgs[0].clone(), msgs[2].clone()]);
        let (read, fds) = recv_exact(&client, sent.len()).await;
        assert_eq!(read, sent);
        assert!(fds.is_empty());
    });
}

/// The messages of each send go out together, in the order of the sends, though the socket takes
/// them a part at a time.
#[test]
#[timeout(30000)]
fn sends_stay_apart_and_in_order() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let filled = fill(&bus);
        let connection = connection(bus, LONG_TIMEOUT);
        // Each message takes several writes, as the socket takes no more than the fill at once.
        let first = vec![signal(filled, b'a'), signal(filled, b'b')];
        let second = vec![signal(filled, b'c'), signal(filled, b'd')];
        let sent = concat(&[first.clone(), second.clone()].concat());

        let sending = [
            start_send(&connection, first).await,
            start_send(&connection, second).await,
        ];
        read_fill(&client, filled).await;
        assert_eq!(read_exact(&client, sent.len()).await, sent);
        for task in sending {
            assert!(within(task).await.unwrap().is_empty());
        }
    });
}

/// A send that the client doesn't take within its time limit disconnects the client. The bus drops
/// what is left to write, writes nothing more, and shuts the socket down. The client reads what was
/// written before, a partly written message included, and then EOF.
#[test]
#[timeout(30000)]
fn timeout_disconnects() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let filled = fill(&bus);
        let connection = connection(bus, Duration::from_millis(200));
        // Room for some of the first message, but not all of it.
        let taken = filled / 2;
        read_fill(&client, taken).await;
        let first = signal(4 * filled, b'a');
        let second = signal(10, b'b');

        let sending = [
            start_send(&connection, vec![first.clone()]).await,
            start_send(&connection, vec![second.clone()]).await,
        ];
        for task in sending {
            assert!(within(task).await.unwrap().is_empty());
        }
        // Nothing more is written.
        assert!(within(connection.send(&[signal(10, b'c')]))
            .await
            .is_empty());
        // `is_disconnecting` and `unwritten_len` exist on Linux alone, because only the hand-over
        // uses them, and Linux alone has the hand-over.
        #[cfg(target_os = "linux")]
        {
            assert!(connection.is_disconnecting());
            assert_eq!(connection.unwritten_len(), 0);
        }

        let read = within(read_to_end(&client)).await;
        let (rest_of_fill, of_first) = read.split_at(filled - taken);
        assert!(rest_of_fill.iter().all(|&byte| byte == FILL));
        assert!(
            of_first.len() < first.data().len(),
            "all of the message went"
        );
        assert_eq!(of_first, &first.data().bytes()[..of_first.len()]);
    });
}

/// A send that fails because the client closed its end disconnects the client too, and the sends
/// after it write nothing.
#[test]
#[timeout(30000)]
fn failed_send_disconnects() {
    zruntime::block_on(async {
        let connection = {
            let (bus, _client) = pair();

            connection(bus, LONG_TIMEOUT)
        };

        for byte in [1, 2] {
            assert!(within(connection.send(&[signal(10, byte)]))
                .await
                .is_empty());
        }
        // `is_disconnecting` and `unwritten_len` exist on Linux alone, because only the hand-over
        // uses them, and Linux alone has the hand-over.
        #[cfg(target_os = "linux")]
        {
            assert!(connection.is_disconnecting());
            assert_eq!(connection.unwritten_len(), 0);
        }
    });
}

/// Sends that wait for room all go on once the client has taken what fills its socket, because the
/// runtime wakes them all.
///
/// The first send that the runtime polls writes what the other sends wait for, and fills the socket
/// again with what comes after. The other sends return at once, instead of waiting for room that
/// they no longer need.
#[test]
#[timeout(30000)]
fn sends_whose_messages_another_send_wrote() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let filled = fill(&bus);
        let connection = connection(bus, LONG_TIMEOUT);
        let msgs = [signal(10, 1), signal(20, 2), signal(4 * filled, 3)];
        let mut sending = Vec::new();
        for msg in &msgs {
            sending.push(start_send(&connection, vec![msg.clone()]).await);
        }
        let last = sending.pop().unwrap();

        read_fill(&client, filled).await;
        for task in sending {
            assert!(within(task).await.unwrap().is_empty());
        }
        assert!(
            !last.is_finished(),
            "the socket took all of the last message"
        );
        let sent = concat(&msgs);
        let (read, rejected) = join(read_exact(&client, sent.len()), within(last)).await;
        assert_eq!(read, sent);
        assert!(rejected.unwrap().is_empty());
    });
}

/// A send that waits for room returns once another waiting send has written its message. The other
/// send was woken by a change, not by the runtime. The first send returns even though that write
/// filled the socket again, so that the runtime finds no room to wake it for.
#[test]
#[timeout(30000)]
fn send_whose_message_another_waiting_send_wrote() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let filled = fill(&bus);
        let connection = connection(bus, LONG_TIMEOUT);
        let small = signal(10, 1);
        let long = signal(4 * filled, 2);
        let mut first = pin!(connection.send(slice::from_ref(&small)));
        assert!(poll!(first.as_mut()).is_pending());
        let mut second = pin!(connection.send(slice::from_ref(&long)));
        assert!(poll!(second.as_mut()).is_pending());

        // A change makes both sends look again. The first finds the socket full, and waits for room
        // afresh.
        connection.changed.notify(usize::MAX);
        assert!(poll!(first.as_mut()).is_pending());
        assert_eq!(connection.outgoing().written, 0);

        // The client takes what fills its socket. When the second send looks again, it writes the
        // first send's message, and as much of its own as fills the socket again, before the
        // runtime looks at the socket.
        read_fill_now(&client, filled);
        assert!(poll!(second.as_mut()).is_pending());
        {
            let outgoing = connection.outgoing();
            assert!(outgoing.written >= small.data().len() as u64);
            assert_eq!(
                outgoing.chunks.len(),
                1,
                "the message of the first send is left"
            );
        }

        assert!(within(first).await.is_empty());
        let sent = concat(&[small.clone(), long.clone()]);
        let (read, rejected) = join(read_exact(&client, sent.len()), within(second)).await;
        assert_eq!(read, sent);
        assert!(rejected.is_empty());
    });
}

/// A send that waits for room returns as soon as a later send has written its message. The later
/// send wrote straight to the socket, not once the runtime found the socket writable. The first
/// send returns even though that write filled the socket again, so that the runtime finds no room
/// to wake it for.
#[test]
#[timeout(30000)]
fn send_whose_message_a_later_send_wrote() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let filled = fill(&bus);
        let connection = connection(bus, LONG_TIMEOUT);
        let waiting = signal(10, 1);
        let first = start_send(&connection, vec![waiting.clone()]).await;

        // The client takes what fills its socket. The next send then writes the first send's
        // message, and as much of its own as fills the socket again, before the runtime looks at
        // the socket.
        read_fill_now(&client, filled);
        let long = signal(4 * filled, 2);
        let mut second = pin!(connection.send(slice::from_ref(&long)));
        assert!(poll!(second.as_mut()).is_pending());
        {
            let outgoing = connection.outgoing();
            assert!(outgoing.written >= waiting.data().len() as u64);
            assert_eq!(
                outgoing.chunks.len(),
                1,
                "the message of the first send is left"
            );
        }

        assert!(within(first).await.unwrap().is_empty());
        let sent = concat(&[waiting.clone(), long.clone()]);
        let (read, rejected) = join(read_exact(&client, sent.len()), within(second)).await;
        assert_eq!(read, sent);
        assert!(rejected.is_empty());
    });
}

// The handshake.

/// The answers to the commands that a client sends while the bus waits for `AUTH`, as zbus 6.4's
/// server gives them.
#[test]
#[timeout(30000)]
fn handshake_waiting_for_auth() {
    let me = id(uid());
    let other = id(uid() + 1);
    let ok = ok();
    check_conversations(&[
        // The client is the user that its socket reports, which `OK` says, and begins.
        (
            &[&format!("AUTH EXTERNAL {me}"), "BEGIN"],
            &[&ok],
            Ends::Begun(false),
        ),
        // The client claims to be another user, and then who it is.
        (
            &[
                &format!("AUTH EXTERNAL {other}"),
                &format!("AUTH EXTERNAL {me}"),
                "BEGIN",
            ],
            &[REJECTED, &ok],
            Ends::Begun(false),
        ),
        // An ID that isn't UTF-8, a user that isn't a number, or a number that a uid can't hold.
        (
            &["AUTH EXTERNAL ff", &format!("AUTH EXTERNAL {me}")],
            &[],
            Ends::Failed,
        ),
        (
            &[&format!("AUTH EXTERNAL {}", id("me")), "BEGIN"],
            &[],
            Ends::Failed,
        ),
        (
            &[&format!("AUTH EXTERNAL {}", id(-1)), "BEGIN"],
            &[],
            Ends::Failed,
        ),
        (
            &[&format!("AUTH EXTERNAL {}", id(1u64 << 32)), "BEGIN"],
            &[],
            Ends::Failed,
        ),
        // `EXTERNAL` without an initial response, which `DATA` asks for.
        (
            &["AUTH EXTERNAL", &format!("DATA {me}"), "BEGIN"],
            &[DATA, &ok],
            Ends::Begun(false),
        ),
        // No mechanism, which asks for the bus's mechanisms, and another mechanism.
        (
            &[
                "AUTH",
                "AUTH ANONYMOUS",
                "AUTH ANONYMOUS 7a627573",
                &format!("AUTH EXTERNAL {me}"),
                "BEGIN",
            ],
            &[REJECTED, REJECTED, REJECTED, &ok],
            Ends::Begun(false),
        ),
        // The client gives up, or tells of an error.
        (
            &[
                "CANCEL",
                "ERROR",
                "ERROR Something went wrong",
                &format!("AUTH EXTERNAL {me}"),
                "BEGIN",
            ],
            &[REJECTED, REJECTED, REJECTED, &ok],
            Ends::Begun(false),
        ),
        // Any other command.
        (
            &[
                "BEGIN",
                "DATA",
                &format!("DATA {me}"),
                "NEGOTIATE_UNIX_FD",
                &format!("OK {GUID}"),
                "REJECTED EXTERNAL",
                "AGREE_UNIX_FD",
                &format!("AUTH EXTERNAL {me}"),
                "BEGIN",
            ],
            &[ERROR, ERROR, ERROR, ERROR, ERROR, ERROR, ERROR, &ok],
            Ends::Begun(false),
        ),
    ]);
}

/// The answers to the commands that a client sends while the bus waits for the `DATA` of `AUTH
/// EXTERNAL`, as zbus 6.4's server gives them.
#[test]
#[timeout(30000)]
fn handshake_waiting_for_data() {
    let me = id(uid());
    let other = id(uid() + 1);
    let ok = ok();
    check_conversations(&[
        // No data: the client is the user that its socket reports.
        (
            &["AUTH EXTERNAL", "DATA", "BEGIN"],
            &[DATA, &ok],
            Ends::Begun(false),
        ),
        // The data is the user, as for an initial response. Another user is rejected, and the
        // client starts over. The user that its socket reports is accepted.
        (
            &[
                "AUTH EXTERNAL",
                &format!("DATA {other}"),
                "AUTH EXTERNAL",
                &format!("DATA {me}"),
                "BEGIN",
            ],
            &[DATA, REJECTED, DATA, &ok],
            Ends::Begun(false),
        ),
        (&["AUTH EXTERNAL", "DATA ff", "DATA"], &[DATA], Ends::Failed),
        // Any other command, `CANCEL` among them.
        (
            &[
                "AUTH EXTERNAL",
                &format!("AUTH EXTERNAL {me}"),
                "CANCEL",
                "ERROR",
                "BEGIN",
                "NEGOTIATE_UNIX_FD",
                "DATA",
                "BEGIN",
            ],
            &[DATA, ERROR, ERROR, ERROR, ERROR, ERROR, &ok],
            Ends::Begun(false),
        ),
    ]);
}

/// The answers to the commands that a client sends once it is authenticated, as zbus 6.4's server
/// gives them.
#[test]
#[timeout(30000)]
fn handshake_waiting_for_begin() {
    let auth = format!("AUTH EXTERNAL {}", id(uid()));
    let ok = ok();
    check_conversations(&[
        // The client asks to pass file descriptors.
        (
            &[&auth, "NEGOTIATE_UNIX_FD", "BEGIN"],
            &[&ok, AGREE_UNIX_FD],
            Ends::Begun(true),
        ),
        // The client gives up, or tells of an error, and starts over.
        (
            &[&auth, "CANCEL", &auth, "ERROR Oops", &auth, "BEGIN"],
            &[&ok, REJECTED, &ok, REJECTED, &ok],
            Ends::Begun(false),
        ),
        // Any other command.
        (
            &[&auth, &auth, "DATA", &format!("OK {GUID}"), "BEGIN"],
            &[&ok, ERROR, ERROR, ERROR],
            Ends::Begun(false),
        ),
    ]);
}

/// The handshake fails, without an answer, on a line that holds no command that zbus knows, such as
/// `AUTH` of a mechanism that zbus doesn't support. It also fails, without an answer, on a line
/// that isn't UTF-8, and at EOF.
#[test]
#[timeout(30000)]
fn handshake_fails_on_what_is_no_command() {
    let me = id(uid());
    check_conversations(&[
        (
            &["HELLO", &format!("AUTH EXTERNAL {me}")],
            &[],
            Ends::Failed,
        ),
        (
            &["AUTH EXTERNAL 3", &format!("AUTH EXTERNAL {me}")],
            &[],
            Ends::Failed,
        ),
        (
            &["AUTH DBUS_COOKIE_SHA1 31303030", "AUTH"],
            &[],
            Ends::Failed,
        ),
        (&["", "AUTH"], &[], Ends::Failed),
        (&["AUTH", "HELLO", "AUTH"], &[REJECTED], Ends::Failed),
        // EOF, after a whole line and in the middle of one.
        (&["AUTH"], &[REJECTED], Ends::Failed),
        (&[], &[], Ends::Failed),
    ]);
    let ends_in_a_line = format!("\0AUTH EXTERNAL {me}");
    let not_utf8 = b"\0AUTH EXTERNAL \xff\r\n";
    for input in [ends_in_a_line.as_bytes(), not_utf8] {
        let (answers, ends) = zruntime::block_on(handshake(input, uid()));
        assert_eq!((answers.as_str(), ends), ("", Ends::Failed), "{input:?}");
    }
}

/// The handshake fails, without an answer, on a first line that doesn't start with the 0 byte that
/// a client sends first. It also fails, without an answer, on a line that doesn't end with `\r\n`,
/// such as a line that starts with `\n`, on which zbus's server panics.
#[test]
#[timeout(30000)]
fn handshake_fails_on_lines_that_are_not_lines() {
    let auth = format!("AUTH EXTERNAL {}", id(uid()));
    let without_nul = format!("{auth}\r\nBEGIN\r\n");
    let lf = format!("\0{auth}\nBEGIN\r\n");
    let lf_first = "\n\0AUTH\r\n".to_string();
    let lf_line = format!("\0{auth}\r\n\nBEGIN\r\n");
    for (input, answers) in [
        (without_nul, String::new()),
        (lf, String::new()),
        (lf_first, String::new()),
        (lf_line, ok()),
    ] {
        let (answered, ends) = zruntime::block_on(handshake(input.as_bytes(), uid()));
        assert_eq!((answered, ends), (answers, Ends::Failed), "{input:?}");
    }
}

/// The handshake fails, without an answer, on a line that doesn't fit in the 4 KiB that the bus
/// reads at a time. It takes the longest line that fits.
#[test]
#[timeout(30000)]
fn handshake_fails_on_lines_longer_than_4_kib() {
    let auth = format!("\0AUTH EXTERNAL {}\r\n", id(uid()));
    // `\r\n` included, and the whitespace after a command ignored.
    let line = |len: usize| format!("{:len$}\r\n", "NEGOTIATE_UNIX_FD", len = len - 2);
    for (len, answers, ends) in [
        (
            4 * 1024,
            format!("{}{AGREE_UNIX_FD}", ok()),
            Ends::Begun(true),
        ),
        (4 * 1024 + 1, ok(), Ends::Failed),
    ] {
        let input = format!("{auth}{}BEGIN\r\n", line(len));
        let (answered, ended) = zruntime::block_on(handshake(input.as_bytes(), uid()));
        assert_eq!((answered, ended), (answers, ends), "{len}");
    }
}

/// The bus reads each line of the handshake as it comes, however the client splits it into writes,
/// and answers each as the client waits for the answer.
#[test]
#[timeout(30000)]
fn handshake_line_by_line() {
    zruntime::block_on(async {
        let (bus, client) = std_pair();
        let auth = format!("\0AUTH EXTERNAL {}\r\n", id(uid()));
        let talking = async {
            // A line in two writes, the second once the bus read the first.
            let (start, end) = auth.as_bytes().split_at(8);
            write_all(&client, start, &[]).await;
            yield_now().await;
            write_all(&client, end, &[]).await;
            assert_eq!(read_line(&client).await, ok());
            write_all(&client, b"NEGOTIATE_UNIX_FD\r\n", &[]).await;
            assert_eq!(read_line(&client).await, AGREE_UNIX_FD);
            write_all(&client, b"BEGIN\r\n", &[]).await;
        };

        let (accepted, ()) = join(
            Connection::accept(bus, &guid(), uid(), name(), LONG_TIMEOUT),
            talking,
        )
        .await;
        let (_reader, connection) = accepted.unwrap();
        assert!(connection.fd_passing);
    });
}

/// `REJECTED` answers a client that claims to be the user that its socket reports, when the bus
/// authenticates another user. A zbus client then gives up.
#[test]
#[timeout(30000)]
fn handshake_of_another_user() {
    zruntime::block_on(async {
        let (bus, client) = net::UnixStream::pair().unwrap();
        let (accepted, connected) = join(
            Connection::accept(bus, &guid(), uid() + 1, name(), LONG_TIMEOUT),
            zbus::connection::Builder::unix_stream(client).p2p().build(),
        )
        .await;

        let error = connected.unwrap_err();
        assert!(matches!(error, zbus::Error::Handshake(_)), "{error}");
        assert!(accepted.is_err());
    });
}

/// What the client sends after `BEGIN`, in the same write, file descriptors included, starts its
/// first message.
#[test]
#[timeout(30000)]
fn bytes_and_fds_after_begin_start_the_first_message() {
    zruntime::block_on(async {
        let (bus, client) = std_pair();
        let (ours, theirs) = fd_pair();
        let first = signal_with_fds(&[&theirs]);
        let second = signal(10, 1);
        let mut input = format!(
            "\0AUTH EXTERNAL {}\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\n",
            id(uid())
        )
        .into_bytes();
        input.extend_from_slice(first.data().bytes());
        send_raw(&client, &input, &[theirs.as_fd()]);

        let (mut reader, connection) =
            Connection::accept(bus, &guid(), uid(), name(), LONG_TIMEOUT)
                .await
                .unwrap();
        assert!(connection.fd_passing);
        let read = next(&mut reader).await;
        assert_eq!(read.data().bytes(), first.data().bytes());
        assert_end_of(read.data().fds()[0].as_fd(), &ours);
        send_raw(&client, second.data().bytes(), &[]);
        assert_eq!(
            next(&mut reader).await.data().bytes(),
            second.data().bytes()
        );
        let answers = format!("{}{AGREE_UNIX_FD}", ok());
        assert_eq!(read_exact(&client, answers.len()).await, answers.as_bytes());
    });
}

/// File descriptors that come with the lines of the handshake, in up to 3 reads, wait for the first
/// message, which takes them. A fourth read that brings some fails the handshake.
///
/// The test writes each line only once the bus has answered the line before, so that each read
/// brings the FDs of one write.
#[test]
#[timeout(30000)]
fn fds_with_the_handshake() {
    zruntime::block_on(async {
        for sets in [3, 4] {
            let (bus, client) = std_pair();
            let pairs: Vec<_> = (0..sets).map(|_| fd_pair()).collect();
            let theirs: Vec<_> = pairs.iter().map(|(_, theirs)| theirs).collect();
            let lines = [
                (format!("\0AUTH EXTERNAL {}\r\n", id(uid())), ok()),
                (
                    "NEGOTIATE_UNIX_FD\r\n".to_string(),
                    AGREE_UNIX_FD.to_string(),
                ),
                (
                    "NEGOTIATE_UNIX_FD\r\n".to_string(),
                    AGREE_UNIX_FD.to_string(),
                ),
            ];
            // The first message counts the FDs of the first 3 lines.
            let msg = signal_with_fds(&theirs[..3]);
            let talking = async {
                for ((line, answer), fd) in lines.iter().zip(&theirs) {
                    send_raw(&client, line.as_bytes(), &[fd.as_fd()]);
                    assert_eq!(&read_line(&client).await, answer);
                }
                // `BEGIN`, with the fourth FD if there is one, and the first message.
                let mut input = b"BEGIN\r\n".to_vec();
                input.extend_from_slice(msg.data().bytes());
                let fourth: Vec<_> = theirs[3..].iter().map(|fd| fd.as_fd()).collect();
                send_raw(&client, &input, &fourth);
            };

            let guid = guid();
            let accepting = Connection::accept(bus, &guid, uid(), name(), LONG_TIMEOUT);
            let (accepted, ()) = join(accepting, talking).await;
            match sets {
                3 => {
                    let (mut reader, _connection) = accepted.unwrap();
                    let read = next(&mut reader).await;
                    assert_eq!(read.data().bytes(), msg.data().bytes());
                    for (fd, (ours, _)) in read.data().fds().iter().zip(&pairs) {
                        assert_end_of(fd.as_fd(), ours);
                    }
                }
                _ => {
                    let error = accepted.unwrap_err();
                    assert!(is_io_error(&error, io::ErrorKind::InvalidData), "{error}");
                }
            }
        }
    });
}

/// A zbus client connects, and its messages and the bus's go through, file descriptors and all.
#[test]
#[timeout(30000)]
fn zbus_client() {
    zruntime::block_on(async {
        let (bus, client) = net::UnixStream::pair().unwrap();
        let (accepted, connected) = join(
            Connection::accept(bus, &guid(), uid(), name(), LONG_TIMEOUT),
            zbus::connection::Builder::unix_stream(client).p2p().build(),
        )
        .await;
        let (mut reader, connection) = accepted.unwrap();
        let client = connected.unwrap();
        assert!(connection.fd_passing);
        let mut stream = MessageStream::from(&client);

        let (ours, theirs) = fd_pair();
        let msg = signal_with_fds(&[&theirs]);
        client.send(&msg).await.unwrap();
        let read = next(&mut reader).await;
        assert_eq!(read.header().member(), msg.header().member());
        assert_end_of(read.data().fds()[0].as_fd(), &ours);

        let (ours, theirs) = fd_pair();
        let msg = signal_with_fds(&[&theirs]);
        assert!(within(connection.send(slice::from_ref(&msg)))
            .await
            .is_empty());
        let received = within(stream.next()).await.unwrap().unwrap();
        assert_eq!(received.data().bytes(), msg.data().bytes());
        assert_end_of(received.data().fds()[0].as_fd(), &ours);
    });
}

/// The tests that need what Linux alone is known to do, or code that only the hand-over uses.
///
/// Linux alone has the hand-over.
#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// A client that closed its end of the socket while bytes that the bus sent it were still
    /// unread in it is read to its end all the same, though Linux reports such a close with
    /// `ECONNRESET`. The reader yields what the client sent, and then EOF, without an error.
    #[test]
    #[timeout(30000)]
    fn client_that_closed_with_bytes_unread() {
        zruntime::block_on(async {
            let msgs = [signal(10, 1), signal(20, 2)];
            let mut reader = {
                let (bus, client) = pair();
                send_raw(&*bus, b"unread", &[]);
                send_raw(&client, &concat(&msgs), &[]);

                Reader::new(bus)
            };

            for msg in &msgs {
                assert_eq!(next(&mut reader).await.data().bytes(), msg.data().bytes());
            }
            assert!(within(reader.next()).await.is_none());
        });
    }

    /// A reader reports that the client shut its writes down, or closed its end, or that the bus
    /// shut the socket down, whatever the socket still holds to read, and not before.
    #[test]
    #[timeout(30000)]
    fn is_closed() {
        zruntime::block_on(async {
            let (bus, client) = pair();
            let mut reader = Reader::new(bus);
            assert!(!reader.is_closed());
            let msg = signal(10, 1);
            send_raw(&client, msg.data().bytes(), &[]);
            assert!(!reader.is_closed());
            client.shutdown(Shutdown::Write).unwrap();
            assert!(reader.is_closed());
            assert_eq!(next(&mut reader).await.data().bytes(), msg.data().bytes());
            assert!(reader.next().await.is_none());

            let reader = {
                let (bus, client) = pair();
                send_raw(&client, msg.data().bytes(), &[]);

                Reader::new(bus)
            };
            assert!(reader.is_closed());

            let (bus, _client) = pair();
            let reader = Reader::new(bus.clone());
            bus.shutdown(Shutdown::Both).unwrap();
            assert!(reader.is_closed());
        });
    }

    /// A pause wakes a send that waits for the client. The send returns at once, and its message
    /// stays queued, for the hand-over. A send during the pause doesn't wait either. It writes what
    /// the socket takes at once, starting with what was left before it.
    #[test]
    #[timeout(30000)]
    fn pause_wakes_a_waiting_send() {
        zruntime::block_on(async {
            let (bus, client) = pair();
            let filled = fill(&bus);
            let connection = connection(bus, LONG_TIMEOUT);
            let msgs = [signal(10, 1), signal(20, 2), signal(30, 3)];
            let sending = start_send(&connection, vec![msgs[0].clone()]).await;

            connection.pause();
            assert!(within(sending).await.unwrap().is_empty());
            assert_eq!(connection.unwritten_len(), msgs[0].data().len());
            assert!(within(connection.send(slice::from_ref(&msgs[1])))
                .await
                .is_empty());
            let unwritten = msgs[0].data().len() + msgs[1].data().len();
            assert_eq!(connection.unwritten_len(), unwritten);

            read_fill(&client, filled).await;
            assert!(within(connection.send(slice::from_ref(&msgs[2])))
                .await
                .is_empty());
            assert_eq!(connection.unwritten_len(), 0);
            let sent = concat(&msgs);
            assert_eq!(read_exact(&client, sent.len()).await, sent);
            assert!(!connection.is_disconnecting());
        });
    }

    /// What a dropped send didn't write stays queued, and a flush writes it within the client's
    /// time limit, as the sends after the dropped send would.
    #[test]
    #[timeout(30000)]
    fn flush_writes_what_a_dropped_send_left() {
        zruntime::block_on(async {
            let (bus, client) = pair();
            let filled = fill(&bus);
            let connection = connection(bus, LONG_TIMEOUT);
            // With nothing to write, the flush returns at once.
            within(connection.flush()).await;
            let msg = signal(10, 1);
            {
                let mut send = pin!(connection.send(slice::from_ref(&msg)));
                assert!(poll!(send.as_mut()).is_pending());
            }
            assert_eq!(connection.unwritten_len(), msg.data().len());

            let reading = async {
                read_fill(&client, filled).await;

                read_exact(&client, msg.data().len()).await
            };
            let (read, ()) = join(reading, within(connection.flush())).await;
            assert_eq!(read, msg.data().bytes());
            assert_eq!(connection.unwritten_len(), 0);
            assert!(!connection.is_disconnecting());
        });
    }

    /// A flush that the client doesn't take within its time limit disconnects the client, as a
    /// send does.
    #[test]
    #[timeout(30000)]
    fn flush_that_times_out_disconnects() {
        zruntime::block_on(async {
            let (bus, _client) = pair();
            fill(&bus);
            let connection = connection(bus, Duration::from_millis(100));
            let msgs = [signal(10, 1)];
            {
                let mut send = pin!(connection.send(&msgs));
                assert!(poll!(send.as_mut()).is_pending());
            }

            within(connection.flush()).await;
            assert!(connection.is_disconnecting());
            assert_eq!(connection.unwritten_len(), 0);
        });
    }
}

/// Whether `error` is an I/O error of `kind`.
fn is_io_error(error: &zbus::Error, kind: io::ErrorKind) -> bool {
    matches!(error, zbus::Error::InputOutput(e) if e.kind() == kind)
}

/// How a handshake ends: with `BEGIN`, and whether the client asked to pass file descriptors, or
/// with a failure.
#[derive(Debug, PartialEq, Eq)]
enum Ends {
    Begun(bool),
    Failed,
}

/// Check that the bus gives the answers that each of `conversations` lists, and that the handshake
/// ends as the conversation says.
///
/// In each conversation, the client sends the lines that the conversation lists, and then shuts
/// its writes down.
fn check_conversations(conversations: &[(&[&str], &[&str], Ends)]) {
    for (lines, answers, ends) in conversations {
        let mut input = vec![0];
        for line in lines.iter() {
            input.extend_from_slice(line.as_bytes());
            input.extend_from_slice(b"\r\n");
        }
        let (answered, ended) = zruntime::block_on(handshake(&input, uid()));
        assert_eq!(
            (answered, &ended),
            (answers.concat(), ends),
            "the answers to {lines:?}"
        );
    }
}

/// What the bus answers a client that sends `input` and then shuts its writes down, and how the
/// handshake ends, when the bus authenticates the user `uid`.
async fn handshake(input: &[u8], uid: u32) -> (String, Ends) {
    let (bus, client) = std_pair();
    let talking = async {
        write_all(&client, input, &[]).await;
        client.shutdown(Shutdown::Write).unwrap();
    };
    let (accepted, ()) = join(
        Connection::accept(bus, &guid(), uid, name(), LONG_TIMEOUT),
        talking,
    )
    .await;
    let ends = match accepted {
        Ok((_reader, connection)) => Ends::Begun(connection.fd_passing),
        Err(_) => Ends::Failed,
    };
    // The bus has let go of the socket, so the client reads EOF after the answers.
    let answers = within(read_to_end(&client)).await;

    (String::from_utf8(answers).unwrap(), ends)
}

/// The answer of a line, read off `client` once it is whole, `\r\n` included.
async fn read_line(client: &UnixStream<Shared>) -> String {
    let mut line = Vec::new();
    while !line.ends_with(b"\r\n") {
        line.extend(read_exact(client, 1).await);
    }

    String::from_utf8(line).unwrap()
}

/// `id` as `EXTERNAL` has it, in decimal, in hex.
fn id(id: impl Display) -> String {
    id.to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The `OK` that the bus answers with.
fn ok() -> String {
    format!("OK {GUID}\r\n")
}

/// The answer to a client that isn't who it claims to be, or that gives up.
///
/// It names the one mechanism of the bus.
const REJECTED: &str = "REJECTED EXTERNAL\r\n";
/// The answer to `AUTH EXTERNAL` without an initial response, which asks for it.
const DATA: &str = "DATA\r\n";
/// The answer to a command that has no place where the client sends it.
const ERROR: &str = "ERROR Unsupported or misplaced command\r\n";
/// The answer to `NEGOTIATE_UNIX_FD`.
const AGREE_UNIX_FD: &str = "AGREE_UNIX_FD\r\n";

/// [`GUID`], as `Connection::accept` takes it.
fn guid() -> OwnedGuid {
    Guid::try_from(GUID).unwrap().into()
}

/// The GUID of the bus of the tests.
const GUID: &str = "0123456789abcdef0123456789abcdef";

/// The uid of this process, which its sockets report.
fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// The bus's end of a pair of connected sockets, and the client's end, both on the runtime that
/// runs the test.
fn pair() -> (Arc<UnixStream<Shared>>, UnixStream<Shared>) {
    let (bus, client) = UnixStream::pair(&SharedRuntime::current().unwrap()).unwrap();

    (Arc::new(bus), client)
}

/// The bus's end of a pair of connected sockets, as `Connection::accept` takes it, and the client's
/// end, on the runtime that runs the test.
fn std_pair() -> (net::UnixStream, UnixStream<Shared>) {
    let (bus, client) = net::UnixStream::pair().unwrap();
    let client = UnixStream::from_std(&SharedRuntime::current().unwrap(), client).unwrap();

    (bus, client)
}

/// A connection to the client at the other end of `bus`, which passes file descriptors, and has
/// `timeout` to take each send.
fn connection(bus: Arc<UnixStream<Shared>>, timeout: Duration) -> Arc<Connection> {
    Arc::new(Connection::new(bus, name(), true, timeout))
}

/// The unique name of the client of the tests.
fn name() -> OwnedUniqueName {
    OwnedUniqueName::try_from(":busd.1").unwrap()
}

/// A signal with `len` times `byte` in its body.
fn signal(len: usize, byte: u8) -> Message {
    Message::signal("/org/busd/Test", "org.busd.Test", "Bytes")
        .build(&(Bytes::new(&vec![byte; len]),))
        .unwrap()
}

/// A signal that carries duplicates of `fds`.
fn signal_with_fds(fds: &[&net::UnixStream]) -> Message {
    let fds: Vec<_> = fds.iter().map(|&fd| Fd::from(fd)).collect();

    Message::signal("/org/busd/Test", "org.busd.Test", "Fds")
        .build(&(fds,))
        .unwrap()
}

/// The bytes of `msgs`, one after the other.
fn concat(msgs: &[Message]) -> Vec<u8> {
    msgs.iter()
        .flat_map(|msg| msg.data().bytes())
        .copied()
        .collect()
}

/// A pair of sockets.
///
/// A test passes the second as a file descriptor, and identifies it by what comes out of the first,
/// which doesn't block.
fn fd_pair() -> (net::UnixStream, net::UnixStream) {
    let (ours, theirs) = net::UnixStream::pair().unwrap();
    ours.set_nonblocking(true).unwrap();

    (ours, theirs)
}

/// Assert that `fd` is the socket whose peer is `ours`, as a byte written to it comes out of
/// `ours`.
fn assert_end_of(fd: BorrowedFd<'_>, ours: &net::UnixStream) {
    let theirs = net::UnixStream::from(fd.try_clone_to_owned().unwrap());
    (&theirs).write_all(b"!").unwrap();
    let mut byte = [0];
    (&*ours).read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"!");
}

/// The next message that `reader` yields, as soon as it is whole.
async fn next(reader: &mut Reader) -> Message {
    within(reader.next()).await.unwrap().unwrap()
}

/// What `reader` yields at once, polled once, whatever share of I/O operations the poll of the
/// test has left.
async fn poll_once(reader: &mut Reader) -> Poll<Option<zbus::Result<Message>>> {
    zruntime::unconstrained(poll_fn(|cx| Poll::Ready(reader.poll_next_unpin(cx)))).await
}

/// Send `msgs` on `connection` in its own task, once the send has queued them, written what the
/// socket took at once, and found that it has to wait for more room.
///
/// The task gives back what the send gives back.
async fn start_send(
    connection: &Arc<Connection>,
    msgs: Vec<Message>,
) -> Task<Vec<Message>, Shared> {
    let connection = connection.clone();
    let mut send = Box::pin(async move { connection.send(&msgs).await });
    assert!(poll!(send.as_mut()).is_pending(), "the send didn't wait");
    let task = zruntime::spawn(send);
    // The task waits from its own first poll on, and this lets the runtime make that poll.
    yield_now().await;

    task
}

/// Let the other tasks of the runtime run before going on.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            return Poll::Ready(());
        }
        yielded = true;
        cx.waker().wake_by_ref();

        Poll::Pending
    })
    .await;
}

/// Fill the socket of `bus` with [`FILL`] until a write would block, and return how many bytes
/// that took.
///
/// The writes go through a non-blocking duplicate of the socket.
fn fill(bus: &UnixStream<Shared>) -> usize {
    // The duplicate shares the mode of the socket, which the runtime set to non-blocking.
    let duplicate = net::UnixStream::from(bus.as_fd().try_clone_to_owned().unwrap());
    let chunk = vec![FILL; 64 * 1024];
    let mut filled = 0;
    // Writes of a byte last, so that not a byte more fits once one would block.
    for len in [chunk.len(), 1] {
        loop {
            match (&duplicate).write(&chunk[..len]) {
                Ok(written) => filled += written,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("Filling the socket failed: {e}"),
            }
        }
    }

    filled
}

/// Read `len` bytes of what [`fill`] wrote off `client`, as they come.
async fn read_fill(client: &UnixStream<Shared>, len: usize) {
    let read = read_exact(client, len).await;
    assert!(read.iter().all(|&byte| byte == FILL));
}

/// Read `len` bytes of what [`fill`] wrote off `client`, which are there to read already, without
/// letting the runtime run.
fn read_fill_now(client: &UnixStream<Shared>, len: usize) {
    let mut read = vec![0; len];
    let mut got = 0;
    while got < len {
        let came = socket::recv(client.as_fd(), &mut read[got..], &mut Vec::new())
            .expect("the bytes of the fill are there to read");
        assert_ne!(came, 0, "the input ended after {got} of {len} bytes");
        got += came;
    }
    assert!(read.iter().all(|&byte| byte == FILL));
}

/// What [`fill`] fills a socket with.
const FILL: u8 = b'#';

/// Write `bytes` to the socket of `client` in one write, with `fds` along with the first byte.
fn send_raw(client: &impl AsFd, bytes: &[u8], fds: &[BorrowedFd<'_>]) {
    let sent = socket::send(client.as_fd(), &[IoSlice::new(bytes)], fds).unwrap();
    assert_eq!(sent, bytes.len(), "the socket took only part of the write");
}

/// Write all of `bytes` to `client`, with `fds` along with the first byte, as the socket takes
/// them.
async fn write_all(client: &UnixStream<Shared>, bytes: &[u8], fds: &[BorrowedFd<'_>]) {
    let mut written = 0;
    let mut fds = fds;
    while written < bytes.len() {
        let rest = &bytes[written..];
        let sent = client
            .write_with(|stream| socket::send(stream.as_fd(), &[IoSlice::new(rest)], fds))
            .await
            .unwrap();
        written += sent;
        fds = &[];
    }
}

/// Read `len` bytes off `client`, as they come.
async fn read_exact(client: &UnixStream<Shared>, len: usize) -> Vec<u8> {
    recv_exact(client, len).await.0
}

/// Read `len` bytes off `client`, as they come, with the file descriptors that come along.
async fn recv_exact(client: &UnixStream<Shared>, len: usize) -> (Vec<u8>, Vec<OwnedFd>) {
    let mut bytes = vec![0; len];
    let mut fds = Vec::new();
    let mut read = 0;
    while read < len {
        let room = &mut bytes[read..];
        let came = client
            .read_with(|stream| socket::recv(stream.as_fd(), room, &mut fds))
            .await
            .unwrap();
        assert_ne!(came, 0, "the input ended after {read} of {len} bytes");
        read += came;
    }

    (bytes, fds)
}

/// Read what `client` reads until EOF.
///
/// A bus can close its end of the socket while what the client sent is still unread in it, as a
/// bus whose handshake with the client failed does. On Linux, the client's read then fails with
/// `ECONNRESET`, which [`socket::recv`] takes for EOF. This takes `ECONNRESET` for EOF on other
/// platforms too.
async fn read_to_end(client: &UnixStream<Shared>) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        let came = client
            .read_with(|stream| socket::recv(stream.as_fd(), &mut buf, &mut Vec::new()))
            .await;
        match came {
            Ok(0) => return bytes,
            Ok(came) => bytes.extend_from_slice(&buf[..came]),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return bytes,
            Err(e) => panic!("Reading what the bus wrote failed: {e}"),
        }
    }
}

/// How long a client gets to take each send, in the tests that don't wait for a time limit to run
/// out.
///
/// It is far longer than the tests wait for what should happen at once, so that a send that waits
/// for no reason fails the test.
const LONG_TIMEOUT: Duration = Duration::from_secs(60);
