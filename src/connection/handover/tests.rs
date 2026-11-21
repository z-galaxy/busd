use std::{
    env,
    fs::File,
    io::{self, Read},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        unix::{fs::MetadataExt, net},
    },
    process::Command,
    sync::Arc,
    time::Duration,
};

use futures_util::future::join;
use ntest::timeout;
use rustix::process::{getrlimit, setrlimit, Resource, Rlimit};
use zruntime::SharedRuntime;

use super::*;
use crate::{
    connection::{
        tests::{
            concat, connection, fd_pair, fill, name, next, pair, poll_once, read_exact, read_fill,
            read_to_end, recv_exact, send_raw, signal, signal_with_fds, start_send, write_all,
            LONG_TIMEOUT,
        },
        Connection, Reader,
    },
    testing::{within, WAIT},
};

/// A hand-over goes through its record and its file descriptors whole: the socket, whether the
/// client passes FDs, the unread bytes with their FDs, and what is left to write. The unread bytes
/// run from the start of a message. What is left to write is the rest of a message, and whole
/// messages with their FDs.
#[test]
fn records_go_through_whole() {
    let pairs: Vec<_> = (0..4).map(|_| fd_pair()).collect();
    let theirs = |i: usize| &pairs[i].1;
    let long = signal(10_000, 1);
    let mut partly = concat(&[signal(10, 2), signal_with_fds(&[theirs(0)])]);
    partly.extend_from_slice(&signal(100, 3).data().bytes()[..30]);
    let cases = [
        // Nothing but the socket.
        (false, Vec::new(), 0, Vec::new(), Vec::new()),
        // Unread bytes: whole messages and the start of one, with the FD that one of the messages
        // carries, and an FD for a message to come.
        (true, partly.clone(), 2, Vec::new(), Vec::new()),
        // The start of a message longer than a reader reads at a time.
        (
            true,
            long.data().bytes()[..6000].to_vec(),
            0,
            Vec::new(),
            Vec::new(),
        ),
        // What is left to write: the rest of a message, and whole ones, with FDs.
        (
            true,
            Vec::new(),
            0,
            long.data().bytes()[3..].to_vec(),
            vec![
                signal_with_fds(&[theirs(1)]),
                signal(10, 4),
                signal_with_fds(&[theirs(2), theirs(3)]),
            ],
        ),
        // A message alone, whose length is a multiple of 8, so that no zeros follow it.
        (true, Vec::new(), 0, Vec::new(), vec![signal(4, 5)]),
        // All of those.
        (
            true,
            partly,
            1,
            long.data().bytes()[5000..].to_vec(),
            vec![signal(20, 6), signal_with_fds(&[theirs(1), theirs(3)])],
        ),
    ];
    for (fd_passing, unread, unread_fds, rest, kept) in cases {
        let (socket, _peer) = net::UnixStream::pair().unwrap();
        let handover = Handover {
            socket: socket.into(),
            fd_passing,
            unread,
            unread_fds: (0..unread_fds).map(|i| owned(theirs(i))).collect(),
            rest,
            kept,
        };

        let (record, fds) = saved(&handover);
        assert_eq!(record.len() % 8, 0);
        assert_eq!(&record[..8], b"busd-cnx");
        let restored = Handover::restore(&record, fds).unwrap();

        assert_same(&restored, &handover);
    }
}

/// A hand-over isn't saved into memory of another length than its record, and the memory is left
/// as it was.
#[test]
fn records_are_saved_into_their_length_alone() {
    let (socket, _peer) = net::UnixStream::pair().unwrap();
    let handover = Handover {
        socket: socket.into(),
        fd_passing: true,
        unread: vec![1; 10],
        unread_fds: Vec::new(),
        rest: Vec::new(),
        kept: vec![signal(10, 1)],
    };
    let len = handover.saved_len().unwrap();

    for len in [len - 1, len + 1] {
        let mut dest = vec![0xa5; len];
        let error = io::Error::from(handover.save(&mut dest).unwrap_err());
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(dest.iter().all(|byte| *byte == 0xa5));
    }
}

/// Restoring a record that no hand-over saved, or a record that comes with other file descriptors
/// than it counts, fails rather than panicking, and the file descriptors close. The cases include a
/// record of a later version, which this version can't read, and spoiled records of every other
/// kind.
#[test]
fn malformed_records_are_refused() {
    let pairs: Vec<_> = (0..3).map(|_| fd_pair()).collect();
    let (socket, _peer) = net::UnixStream::pair().unwrap();
    let kept = [signal_with_fds(&[&pairs[0].1]), signal(100, 1)];
    let handover = Handover {
        socket: socket.into(),
        fd_passing: true,
        // Not a multiple of 8 long, so that zeros follow it.
        unread: concat(&[signal(5, 2)])[..21].to_vec(),
        unread_fds: vec![owned(&pairs[1].1), owned(&pairs[2].1)],
        rest: vec![7; 13],
        kept: kept.to_vec(),
    };
    let (record, fds) = saved(&handover);
    assert!(Handover::restore(&record, duplicates(&fds)).is_ok());
    let unread_end = HEADER + 2 * ENTRY + 24;
    let first_kept = unread_end + 13;
    let u16_at = |record: &mut Vec<u8>, at: usize, value: u16| {
        record[at..at + 2].copy_from_slice(&value.to_le_bytes())
    };
    let u32_at = |record: &mut Vec<u8>, at: usize, value: u32| {
        record[at..at + 4].copy_from_slice(&value.to_le_bytes())
    };
    let u64_at = |record: &mut Vec<u8>, at: usize, value: u64| {
        record[at..at + 8].copy_from_slice(&value.to_le_bytes())
    };
    let kept_len = |i: usize| kept[i].data().len() as u32;

    // What is spoiled, how many file descriptors come with the record, and the kind of error.
    type Spoil<'a> = (
        &'a str,
        Box<dyn Fn(&mut Vec<u8>) + 'a>,
        usize,
        io::ErrorKind,
    );
    let invalid = io::ErrorKind::InvalidData;
    let spoils: Vec<Spoil<'_>> = vec![
        ("magic", Box::new(|r| r[0] = b'x'), 4, invalid),
        (
            "cut in the header",
            Box::new(|r| r.truncate(20)),
            4,
            invalid,
        ),
        ("version 0", Box::new(|r| u16_at(r, 8, 0)), 4, invalid),
        (
            "version 2",
            Box::new(|r| u16_at(r, 8, 2)),
            4,
            io::ErrorKind::Unsupported,
        ),
        ("unknown flag", Box::new(|r| u16_at(r, 10, 3)), 4, invalid),
        ("not 0", Box::new(|r| r[38] = 1), 4, invalid),
        (
            "one message more",
            Box::new(|r| u32_at(r, 12, 3)),
            4,
            invalid,
        ),
        (
            "too many messages",
            Box::new(|r| u32_at(r, 12, u32::MAX)),
            4,
            invalid,
        ),
        ("read: longer", Box::new(|r| u64_at(r, 16, 29)), 4, invalid),
        (
            "read: longer than memory",
            Box::new(|r| u64_at(r, 16, u64::MAX)),
            4,
            invalid,
        ),
        ("rest: longer", Box::new(|r| u64_at(r, 24, 21)), 4, invalid),
        (
            "rest: longer than memory",
            Box::new(|r| u64_at(r, 24, u64::MAX)),
            4,
            invalid,
        ),
        (
            "descriptors: one more",
            Box::new(|r| u32_at(r, 32, 3)),
            4,
            invalid,
        ),
        ("descriptors: one fewer", Box::new(|_| ()), 3, invalid),
        ("descriptors: one too many", Box::new(|_| ()), 5, invalid),
        (
            "message: the length of the other",
            Box::new(|r| {
                u32_at(r, HEADER, kept_len(1));
                u32_at(r, HEADER + ENTRY, kept_len(0));
            }),
            4,
            invalid,
        ),
        (
            "message: a descriptor that it doesn't carry",
            Box::new(|r| {
                u32_at(r, 32, 1);
                u32_at(r, HEADER + ENTRY + 4, 1);
            }),
            4,
            invalid,
        ),
        (
            "message: its descriptor with the other",
            Box::new(|r| {
                u32_at(r, HEADER + 4, 0);
                u32_at(r, HEADER + ENTRY + 4, 1);
            }),
            4,
            invalid,
        ),
        (
            "message: not one",
            Box::new(|r| r[first_kept] = b'x'),
            4,
            invalid,
        ),
        (
            "message: empty",
            Box::new(|r| {
                u32_at(r, HEADER, 0);
                u64_at(r, 24, 13 + u64::from(kept_len(0)));
            }),
            4,
            invalid,
        ),
        (
            "message: shorter than a primary header",
            Box::new(|r| {
                u32_at(r, HEADER, 11);
                u32_at(r, HEADER + ENTRY, kept_len(0) - 11 + kept_len(1));
            }),
            4,
            invalid,
        ),
        (
            "padding after what was read",
            Box::new(|r| r[unread_end - 1] = 1),
            4,
            invalid,
        ),
        (
            "padding at the end",
            Box::new(|r| *r.last_mut().unwrap() = 1),
            4,
            invalid,
        ),
        (
            "cut at the end",
            Box::new(|r| r.truncate(r.len() - 8)),
            4,
            invalid,
        ),
        (
            "zeros at the end",
            Box::new(|r| r.extend([0; 8])),
            4,
            invalid,
        ),
        ("empty", Box::new(Vec::clear), 4, invalid),
    ];
    // The padding that the cases spoil is there.
    assert_eq!(record[unread_end - 3..unread_end], [0; 3]);
    assert_eq!(*record.last().unwrap(), 0);
    for (what, spoil, fd_count, kind) in spoils {
        let mut spoiled = record.clone();
        spoil(&mut spoiled);
        // The far ends of sockets. The test keeps no duplicate of them, so each near end reads EOF
        // once its far end closes.
        let (ours, theirs): (Vec<_>, Vec<_>) = (0..fd_count)
            .map(|_| net::UnixStream::pair().unwrap())
            .unzip();
        let fds = theirs.into_iter().map(OwnedFd::from).collect();

        let error = Handover::restore(&spoiled, fds).unwrap_err();
        assert_eq!(error.kind(), kind, "{what}: {error}");
        for ours in ours {
            // The read waits for EOF, because a child process that another test spawns holds
            // duplicates of this process's FDs until it execs.
            ours.set_read_timeout(Some(WAIT)).unwrap();
            let read = (&ours).read(&mut [0]);
            assert!(
                matches!(read, Ok(0)),
                "{what}: a descriptor stayed open: {read:?}"
            );
        }
    }
}

/// The connection of a client that the bus is disconnecting goes nowhere, and neither does what is
/// left to write to the client.
#[test]
#[timeout(30000)]
fn connections_of_clients_that_the_bus_disconnects_are_not_handed_over() {
    zruntime::block_on(async {
        let (bus, _client) = pair();
        fill(&bus);
        let reader = Reader::new(bus.clone());
        let connection = connection(bus, Duration::from_millis(100));

        within(connection.send(&[signal(10, 1)])).await;
        assert!(connection.is_disconnecting());
        assert!(alone(connection).hand_over(reader).is_none());
    });
}

/// A pause ends a send that waits for the client. The hand-over of the connection takes what the
/// send didn't write, and the bus doesn't disconnect the client.
#[test]
#[timeout(30000)]
fn hand_overs_take_what_the_paused_sends_left() {
    zruntime::block_on(async {
        let (bus, _client) = pair();
        fill(&bus);
        let reader = Reader::new(bus.clone());
        let connection = connection(bus, LONG_TIMEOUT);
        let msg = signal(10, 1);
        let sending = start_send(&connection, vec![msg.clone()]).await;

        connection.pause();
        assert!(within(sending).await.unwrap().is_empty());
        assert!(!connection.is_disconnecting());
        let handover = alone(connection).hand_over(reader).unwrap();
        assert!(handover.rest.is_empty());
        assert_eq!(handover.kept.len(), 1);
        assert_eq!(handover.kept[0].data().bytes(), msg.data().bytes());
    });
}

/// A connection goes nowhere if the bus fails to duplicate its socket, because the process has as
/// many file descriptors open as it may. The bus then disconnects the client: what was left to
/// write is dropped, and the socket closes with the connection. The client reads what its socket
/// took before, and then EOF.
#[test]
#[timeout(30000)]
fn clients_that_fail_to_be_handed_over_are_disconnected() {
    if !in_own_process("clients_that_fail_to_be_handed_over_are_disconnected") {
        return;
    }
    zruntime::block_on(async {
        let (bus, client) = pair();
        let filled = fill(&bus);
        let reader = Reader::new(bus.clone());
        let connection = connection(bus, LONG_TIMEOUT);
        let sending = start_send(&connection, vec![signal(10, 1)]).await;
        connection.pause();
        assert!(within(sending).await.unwrap().is_empty());
        let connection = alone(connection);

        assert!(at_the_fd_limit(|| connection.hand_over(reader)).is_none());
        read_fill(&client, filled).await;
        assert_eq!(within(read_to_end(&client)).await, b"");
    });
}

/// Closing a connection disconnects its client. A send that waits returns, what was left to write
/// is dropped, and nothing more is written. Closing the connection again does nothing. The client
/// reads what its socket took before, and then EOF.
#[test]
#[timeout(30000)]
fn closed_connections_disconnect_their_clients() {
    zruntime::block_on(async {
        let (bus, client) = pair();
        let filled = fill(&bus);
        let connection = connection(bus, LONG_TIMEOUT);
        let sending = start_send(&connection, vec![signal(10, 1)]).await;

        connection.close();
        assert!(within(sending).await.unwrap().is_empty());
        assert!(connection.is_disconnecting());
        assert_eq!(connection.unwritten_len(), 0);
        assert!(within(connection.send(&[signal(20, 2)])).await.is_empty());
        assert_eq!(connection.unwritten_len(), 0);
        connection.close();
        assert!(connection.is_disconnecting());
        read_fill(&client, filled).await;
        assert_eq!(within(read_to_end(&client)).await, b"");
    });
}

/// A connection under traffic both ways goes to the next instance, and from there to the next one.
///
/// The client is in the middle of sending a message, and the bus in the middle of writing one. The
/// instances run in the same process, and the hand-overs go through their records, as to another
/// process, or as they are. The bus gets every message of the client once, in order, with its file
/// descriptors, and the client gets every message of the bus.
#[test]
#[timeout(30000)]
fn connections_go_on_where_they_stopped() {
    for through_the_record in [true, false] {
        zruntime::block_on(go_on_where_it_stopped(through_the_record));
    }
}

/// The test of [`connections_go_on_where_they_stopped`], whose hand-over goes through its record if
/// `through_the_record`, and as it is otherwise.
async fn go_on_where_it_stopped(through_the_record: bool) {
    let (bus, client) = pair();
    // The bus sends to a client that doesn't read until its socket is full, and then reads what
    // filled it.
    let filled = fill(&bus);
    let mut reader = Reader::new(bus.clone());
    let connection = connection(bus, LONG_TIMEOUT);
    let pairs: Vec<_> = (0..4).map(|_| fd_pair()).collect();
    let theirs = |i: usize| &pairs[i].1;
    // The client sends, in one write, the messages that the bus takes before it hands over, the
    // messages that it reads and doesn't take, and the start of a message. Two of the messages
    // carry FDs.
    let sent = [
        signal(100, 0),
        signal_with_fds(&[theirs(0)]),
        signal(300, 2),
        signal(200, 3),
        signal_with_fds(&[theirs(1)]),
        signal(1000, 5),
        signal(10, 6),
        signal(2000, 7),
    ];
    let mut first = concat(&sent[..7]);
    let started = sent[7].data().bytes();
    first.extend_from_slice(&started[..500]);
    send_raw(&client, &first, &[theirs(0).as_fd(), theirs(1).as_fd()]);
    let mut took = Vec::new();
    for _ in 0..3 {
        took.push(next(&mut reader).await);
    }
    // The socket takes as much as fits, including the start of a message longer than the socket can
    // hold. The rest of that message, and what comes after it, are left to write.
    let to_client = [
        signal_with_fds(&[theirs(2)]),
        signal(4 * filled, 1),
        signal(10, 2),
    ];
    let sending = start_send(&connection, to_client.to_vec()).await;
    read_fill(&client, filled).await;
    let unwritten = |msgs: &[Message]| msgs.iter().map(|msg| msg.data().len()).sum::<usize>();
    let left = unwritten(&to_client[1..]);
    within(async {
        while connection.unwritten_len() >= left {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    connection.pause();
    assert!(within(sending).await.unwrap().is_empty());

    let handover = alone(connection).hand_over(reader).unwrap();
    assert_eq!(handover.unread.len(), first.len() - unwritten(&sent[..3]));
    assert_eq!(handover.unread_fds.len(), 1);
    assert!(!handover.rest.is_empty());
    assert_eq!(handover.kept.len(), 1);
    let unread = handover.unread.clone();
    let long = to_client[1].data().bytes();
    let went = long.len() - handover.rest.len();
    // While the bus hands over, the client sends the rest of the message that it started, and
    // three more messages. Those bytes wait in the socket.
    let after = [
        signal(20, 8),
        signal_with_fds(&[theirs(3)]),
        signal(5000, 10),
    ];
    let mut second = started[500..].to_vec();
    second.extend(concat(&after));
    write_all(&client, &second, &[theirs(3).as_fd()]).await;
    // The hand-over took the connection and the reader, and the socket closed with them. The
    // duplicate in the hand-over keeps the client connected.
    let handover = carried(handover, through_the_record);
    let (reader, connection) = Connection::taken_over(handover, name(), LONG_TIMEOUT).unwrap();

    // The next instance hands the connection over in turn, before it has read anything. It does
    // so once it has written some of the rest of the long message while paused. The socket took
    // those bytes once the client read some of what came before.
    connection.pause();
    let (bytes, fds) = recv_exact(&client, to_client[0].data().len()).await;
    assert_eq!(bytes, to_client[0].data().bytes());
    assert_eq!(fds.len(), 1);
    assert_eq!(identity(fds[0].as_fd()), identity(theirs(2).as_fd()));
    let read = went / 2;
    assert_eq!(read_exact(&client, read).await, long[..read]);
    let unwritten = connection.unwritten_len();
    within(connection.flush()).await;
    assert!(
        connection.unwritten_len() < unwritten,
        "the socket took nothing"
    );
    let handover = connection.hand_over(reader).unwrap();
    assert_eq!(handover.unread, unread);
    assert_eq!(handover.unread_fds.len(), 1);
    assert!(handover.rest.len() < long.len() - went);
    assert_eq!(handover.rest, long[long.len() - handover.rest.len()..]);
    assert_eq!(handover.kept.len(), 1);
    let handover = carried(handover, through_the_record);
    let (mut reader, connection) = Connection::taken_over(handover, name(), LONG_TIMEOUT).unwrap();

    for msg in sent[3..].iter().chain(&after) {
        took.push(next(&mut reader).await);
        assert_eq!(took.last().unwrap().data().bytes(), msg.data().bytes());
    }
    assert!(poll_once(&mut reader).await.is_pending());
    let fds: Vec<_> = took
        .iter()
        .flat_map(|msg| msg.data().fds())
        .map(|fd| identity(fd.as_fd()))
        .collect();
    let expected: Vec<_> = [0, 1, 3].map(|i| identity(theirs(i).as_fd())).into();
    assert_eq!(fds, expected);

    let more = [signal(30, 3), signal(40, 4)];
    let writing = async {
        within(connection.flush()).await;
        within(connection.send(&more)).await
    };
    let reading = async {
        let mut rest = long[read..].to_vec();
        rest.extend(concat(&[&to_client[2..], &more[..]].concat()));
        assert_eq!(read_exact(&client, rest.len()).await, rest);
    };
    let (dropped, ()) = join(writing, reading).await;
    assert!(dropped.is_empty());
    assert_eq!(connection.unwritten_len(), 0);
    assert!(!connection.is_disconnecting());
}

/// `connection` by value, as a hand-over takes it, now that nothing else holds it.
fn alone(connection: Arc<Connection>) -> Connection {
    Arc::into_inner(connection).expect("something else holds the connection")
}

/// `handover`, as the next instance takes it over.
///
/// If `through_the_record`, it goes through its record and duplicates of its file descriptors, as
/// in another process. Otherwise it goes as it is.
fn carried(handover: Handover, through_the_record: bool) -> Handover {
    if !through_the_record {
        return handover;
    }
    let (record, fds) = saved(&handover);

    Handover::restore(&record, fds).unwrap()
}

/// Check that `restored` holds what `handover` does: the same bytes, and the same files for its
/// file descriptors.
fn assert_same(restored: &Handover, handover: &Handover) {
    let identities = |fds: &[OwnedFd]| {
        fds.iter()
            .map(|fd| identity(fd.as_fd()))
            .collect::<Vec<_>>()
    };
    assert_eq!(identity(restored.socket()), identity(handover.socket()));
    assert_eq!(restored.fd_passing, handover.fd_passing);
    assert_eq!(restored.unread, handover.unread);
    assert_eq!(
        identities(&restored.unread_fds),
        identities(&handover.unread_fds)
    );
    assert_eq!(restored.rest, handover.rest);
    assert_eq!(restored.kept.len(), handover.kept.len());
    for (restored, kept) in restored.kept.iter().zip(&handover.kept) {
        assert_eq!(restored.data().bytes(), kept.data().bytes());
        let fds = |msg: &Message| {
            msg.data()
                .fds()
                .iter()
                .map(|fd| identity(fd.as_fd()))
                .collect::<Vec<_>>()
        };
        assert_eq!(fds(restored), fds(kept));
    }
    assert_eq!(restored.unread_len(), handover.unread_len());
    assert_eq!(restored.unwritten_len(), handover.unwritten_len());
}

/// The record of `item`, and duplicates of its file descriptors, as zoter gives them to the next
/// instance in another process.
fn saved(item: &impl Save) -> (Vec<u8>, Vec<OwnedFd>) {
    let mut record = vec![0; item.saved_len().unwrap()];
    item.save(&mut record).unwrap();
    let fds = item
        .fds()
        .into_iter()
        .map(|fd| fd.try_clone_to_owned().unwrap())
        .collect();

    (record, fds)
}

/// Duplicates of `fds`.
fn duplicates(fds: &[OwnedFd]) -> Vec<OwnedFd> {
    fds.iter().map(|fd| fd.try_clone().unwrap()).collect()
}

/// Which open file `fd` is: its device and inode, which its duplicates share.
fn identity(fd: BorrowedFd<'_>) -> (u64, u64) {
    let metadata = File::from(owned(&fd)).metadata().unwrap();

    (metadata.dev(), metadata.ino())
}

/// A duplicate of `fd`, owned.
fn owned(fd: &impl AsFd) -> OwnedFd {
    fd.as_fd().try_clone_to_owned().unwrap()
}

/// Wait for `duration`.
async fn sleep(duration: Duration) {
    SharedRuntime::current().unwrap().sleep(duration).await;
}

/// Whether `test`, a test of this module, runs in this process on its own.
///
/// A test that changes what the whole process may do, such as how many file descriptors it may
/// have open, can't run among the threads of the other tests. In the test's own process, which
/// runs that test alone, this is true, and the test goes on. Anywhere else, this runs the test in
/// its own process and checks that it passed, and the test is done.
fn in_own_process(test: &str) -> bool {
    if env::var_os(OWN_PROCESS).is_some() {
        return true;
    }
    // A test goes by its path in the crate, which leaves the name of the crate out.
    let (_, module) = module_path!()
        .split_once("::")
        .expect("the module is in the crate");
    let test = format!("{module}::{test}");
    let output = Command::new(env::current_exe().unwrap())
        .args(["--exact", &test])
        .env(OWN_PROCESS, "1")
        .output()
        .unwrap();
    // A process that finds no test of that name succeeds all the same.
    let passed = String::from_utf8_lossy(&output.stdout).contains(&format!("test {test} ... ok"));
    assert!(output.status.success() && passed, "{output:?}");

    false
}

/// What `f` gives, called while the process has as many file descriptors open as it may, so that
/// opening or duplicating an FD in `f` fails.
fn at_the_fd_limit<F, T>(f: F) -> T
where
    F: FnOnce() -> T,
{
    let limit = getrlimit(Resource::Nofile);
    // A duplicate takes the first FD that isn't open, and this one closes at once. With the limit
    // at that FD, every FD that the process may have is open.
    let first_free = io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .unwrap()
        .as_raw_fd();
    let at_the_limit = Rlimit {
        current: Some(u64::try_from(first_free).unwrap()),
        ..limit
    };
    setrlimit(Resource::Nofile, at_the_limit).unwrap();
    let given = f();
    setrlimit(Resource::Nofile, limit).unwrap();

    given
}

/// The environment variable that tells a test that it runs in its own process.
const OWN_PROCESS: &str = "BUSD_TEST_OWN_PROCESS";
