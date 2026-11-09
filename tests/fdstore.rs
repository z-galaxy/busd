//! The busd binary, restarted the way that systemd restarts a service with a file descriptor store.
//!
//! The test is the service manager: it owns the notification socket that `$NOTIFY_SOCKET` names,
//! and the listening socket of the socket activation, which busd only hands over under. It keeps
//! the stored FDs, those that busd stores in the manager, and passes them to the next instance of
//! busd, with the listening socket, among the passed FDs, as `$LISTEN_FDS` does. The FDs of a
//! hand-over go by the names that the docs of `src/bus/systemd.rs` give them.
#![cfg(target_os = "linux")]

use std::{
    env::temp_dir,
    fs::{self, File},
    future::IntoFuture,
    io::{self, IoSlice, Read, Seek, SeekFrom, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, BorrowedFd, OwnedFd, RawFd},
        unix::{
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, ExitStatus, Stdio},
    sync::{Mutex, MutexGuard, PoisonError},
    thread,
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use rustix::{
    cmsg_space,
    net::{sendmsg, SendAncillaryBuffer, SendAncillaryMessage, SendFlags},
    process::{getuid, kill_process, Pid, Signal},
};
use zbus::{
    connection, fdo::DBusProxy, message, names::BusName, wire::Fd, Connection, Message,
    MessageStream,
};
use zoter::testing::Manager;
use zruntime::SharedRuntime;

/// A bus that systemd stops hands over to its next instance, which serves its clients on as if
/// nothing happened.
///
/// The first instance stores the state memfd and a queue socket, which holds the connection FDs of
/// its clients, in the store, but not the listening socket, which is systemd's: its socket unit may
/// have any name, even one that starts like those of the stored FDs. The second instance, which is
/// passed them and the listening socket, removes them from the store and tells that it's ready. A
/// client that never reconnected still owns its name, one that connected while no instance ran is
/// served too, and gets the next number, and a client that connects now gets a number after
/// those, and sees who owns the name. The second instance hands over in turn, with all of them, as
/// an instance that took over hands over as well as one that didn't.
#[test]
#[timeout(60000)]
fn restart() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);

    let mut first = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    assert!(systemd.take_removed().is_empty());
    let client = Client::connect(&address, Some(NAME));
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), 1);
    // The client waits in the backlog of the listening socket, and in its handshake, for the next
    // instance.
    let waiting = UnixStream::connect(&socket.0).unwrap();
    let waiting = thread::spawn(move || Client::over(waiting));

    let stored = systemd.names();
    let mut second = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);
    assert_eq!(owner(&client, NAME), client.unique_name);
    // The second instance goes on with the numbers of the first.
    let waiting = waiting.join().unwrap();
    assert_eq!(waiting.id, client.id + 1);
    let late = Client::connect(&address, None);
    assert_eq!(late.id, waiting.id + 1);
    assert_eq!(owner(&late, NAME), client.unique_name);

    // SIGINT would do the same: the bus doesn't tell the two apart.
    hand_over(&mut second, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), 3);
}

/// File descriptors that a client sent ahead of the message that they are for, as the specification
/// allows, which busd read but no message took by the time that it hands over, are stored with the
/// socket of the client, and the next instance passes them on with that message.
#[test]
#[timeout(60000)]
fn restart_with_fds_sent_ahead() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);

    let mut first = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let receiver = Client::connect(&address, None);
    // What goes into one end of the pair comes out of the other, which tells that the end that the
    // receiver gets is the one that the sender sent.
    let (mut sender, mut kept) = {
        let (sent, kept) = UnixStream::pair().unwrap();

        (FdClient::connect(&socket.0, &[sent.as_fd()]).unwrap(), kept)
    };
    hand_over(&mut first, &mut systemd, &socket);
    // The connection FDs: the client sockets of the two clients, and the client-sent FD of the
    // sender.
    assert_eq!(systemd.check_stored(), 3);

    let stored = systemd.names();
    let _second = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);
    // The stream takes what the connection reads from now on: the signal among it.
    let mut messages = MessageStream::from(&receiver.connection);
    sender.send_for_fd(&receiver.unique_name).unwrap();
    ping(&mut messages, &mut kept);
}

/// File descriptors that a client sent ahead, more of them than a datagram of the queue of the
/// store takes, go through the store in more datagrams than one, and the next instance passes each
/// on in its place.
#[test]
#[timeout(60000)]
fn restart_with_more_fds_sent_ahead_than_a_datagram_takes() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);
    // Pairs go first, last, and right after those of the first datagram, and the others are of no
    // consequence.
    let null = File::open("/dev/null").unwrap();
    let paired = [0, SCM_MAX_FD, AHEAD - 1];
    let (sent, mut kept): (Vec<_>, Vec<_>) =
        paired.iter().map(|_| UnixStream::pair().unwrap()).unzip();
    let fds: Vec<_> = (0..AHEAD)
        .map(|n| match paired.iter().position(|&at| at == n) {
            Some(pair) => sent[pair].as_fd(),
            None => null.as_fd(),
        })
        .collect();

    let mut first = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let receiver = Client::connect(&address, None);
    let mut sender = FdClient::connect(&socket.0, &fds).unwrap();
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), 2 + AHEAD);

    let _second = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let mut messages = MessageStream::from(&receiver.connection);
    for n in 0..AHEAD {
        sender.send_for_fd(&receiver.unique_name).unwrap();
        match paired.iter().position(|&at| at == n) {
            Some(pair) => ping(&mut messages, &mut kept[pair]),
            None => {
                let _ = received_fd(&mut messages);
            }
        }
    }
}

/// Client-sent FDs of one open file description, which systemd stores once, as an FD that a client
/// sent twice ahead of the messages that they are for, are stored each as it is, and the next
/// instance passes each on.
#[test]
#[timeout(60000)]
fn restart_with_the_same_fd_sent_ahead_twice() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);

    let mut first = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let receiver = Client::connect(&address, None);
    let (mut sender, mut kept) = {
        let (sent, kept) = UnixStream::pair().unwrap();

        (
            FdClient::connect(&socket.0, &[sent.as_fd(), sent.as_fd()]).unwrap(),
            kept,
        )
    };
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), 4);

    let _second = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let mut messages = MessageStream::from(&receiver.connection);
    for _ in 0..2 {
        sender.send_for_fd(&receiver.unique_name).unwrap();
        ping(&mut messages, &mut kept);
    }
}

/// File descriptors of one file, with the same flags, but of open file descriptions of their own,
/// as two opens of the file make, are each stored as they are, rather than one taken for a
/// duplicate of the other, as systemd may take them where the kernel doesn't tell it whether they
/// are of the same open file description: the next instance passes each on as it was, with an
/// offset of its own.
#[test]
#[timeout(60000)]
fn restart_with_fds_of_one_file_sent_ahead() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);
    let file = Removed(temp_path());
    fs::write(&file.0, b"offsets").unwrap();
    let open_at = |offset| {
        let mut opened = File::open(&file.0).unwrap();
        opened.seek(SeekFrom::Start(offset)).unwrap();

        opened
    };
    let offsets = [1, 4];

    let mut first = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let receiver = Client::connect(&address, None);
    let mut sender = {
        let sent = offsets.map(open_at);

        FdClient::connect(&socket.0, &sent.each_ref().map(AsFd::as_fd)).unwrap()
    };
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), 2 + offsets.len());

    let _second = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let mut messages = MessageStream::from(&receiver.connection);
    for offset in offsets {
        sender.send_for_fd(&receiver.unique_name).unwrap();
        let mut received = File::from(received_fd(&mut messages));
        assert_eq!(received.stream_position().unwrap(), offset);
    }
}

/// A bus that hands over as many clients as the next instance can take: the next instance takes
/// their connection FDs out of the queue sockets, and opens their process FDs, under its hard limit
/// of open FDs, so a bus with that limit low hands over fewer clients than it has, and the others
/// get disconnected, losing their names.
#[test]
#[timeout(60000)]
fn hand_over_fits_the_fd_limit() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);
    let low = Some(LOW_NOFILE);

    let mut first = Busd::spawn_limited(&mut systemd, &address, FDSTORE_MAX, low);
    systemd.until_ready();
    let clients: Vec<_> = (0..CLIENTS)
        .map(|i| {
            let name = format!("{NAME}{i}");

            (Client::connect(&address, Some(&name)), name)
        })
        .collect();
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), FITTING_CLIENTS);

    let removed = systemd.names();
    let _second = Busd::spawn_limited(&mut systemd, &address, FDSTORE_MAX, low);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), removed);
    let late = Client::connect(&address, None);
    assert_eq!(served(&clients, &late), FITTING_CLIENTS);
}

/// A bus that has no FD to spare, as it has as many open as its hard limit takes, still hands
/// over: it keeps some free from the start for the state memfd, the queue socket and the pipe of
/// the barrier, so it stores the state memfd, and the connection FDs of the clients whose client
/// sockets it could duplicate and that fit under that limit. The next instance, under the same
/// limit, serves the clients that were stored.
#[test]
#[timeout(60000)]
fn hand_over_at_the_hard_fd_limit() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);

    let hard = Some(HARD_NOFILE);
    let mut first = Busd::spawn_limited(&mut systemd, &address, FDSTORE_MAX, hard);
    systemd.until_ready();
    // Clients connect until busd has hardly a file descriptor left.
    let mut clients = Vec::new();
    while HARD_NOFILE - first.open_fds() > FREE_FDS {
        let name = format!("{NAME}{}", clients.len());
        clients.push((Client::connect(&address, Some(&name)), name));
    }
    hand_over(&mut first, &mut systemd, &socket);
    let stored = systemd.check_stored();
    assert!(
        stored > 0,
        "busd stored none of its {} clients",
        clients.len()
    );

    let removed = systemd.names();
    let _second = Busd::spawn_limited(&mut systemd, &address, FDSTORE_MAX, hard);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), removed);
    let late = Client::connect(&address, None);
    assert_eq!(served(&clients, &late), stored);
}

/// A next instance that fails to take over, as it can't make sense of the state, starts anyway on
/// the listening socket that systemd passed it, which stays in place: it removes everything from
/// the store, disconnects the clients of the previous instance, and serves new clients at the same
/// address.
#[test]
#[timeout(60000)]
fn failed_take_over() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);

    let mut first = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let client = Client::connect(&address, Some(NAME));
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), 1);
    systemd.spoil_state();

    let stored = systemd.names();
    let _second = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);
    assert!(socket.0.exists());

    // The old client was disconnected, and the new bus doesn't know its name.
    assert!(
        try_owner(&client, NAME).is_err(),
        "the old client is connected"
    );
    let late = Client::connect(&address, None);
    assert!(
        try_owner(&late, NAME).is_err(),
        "the new bus has the old names"
    );
    assert_eq!(owner(&late, "org.freedesktop.DBus"), "org.freedesktop.DBus");
}

/// A next instance that systemd starts with the stored FDs but without the listening socket, as
/// once the socket unit went, doesn't take over: it removes everything from the store, disconnects
/// the clients of the previous instance, and serves new clients at its address, on a listening
/// socket of its own, in place of the one that systemd closed.
#[test]
#[timeout(60000)]
fn no_take_over_without_socket_activation() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);

    let mut first = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let client = Client::connect(&address, Some(NAME));
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(), 1);
    systemd.deactivate();

    let stored = systemd.names();
    let _second = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);

    assert!(
        try_owner(&client, NAME).is_err(),
        "the old client is connected"
    );
    let late = Client::connect(&address, None);
    assert!(
        try_owner(&late, NAME).is_err(),
        "the new bus has the old names"
    );
    assert_eq!(owner(&late, "org.freedesktop.DBus"), "org.freedesktop.DBus");
}

/// A file descriptor store that takes one stored FD is too small to hand over with: the bus stops
/// as it does without systemd. It stores nothing, exits successfully and leaves the file of the
/// listening socket, which is systemd's, where it is.
#[test]
#[timeout(60000)]
fn no_hand_over_with_a_small_store() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::activating(&socket.0);

    let mut busd = Busd::spawn(&mut systemd, &address, 1);
    systemd.until_ready();
    let _client = Client::connect(&address, Some(NAME));

    busd.signal(Signal::TERM);
    let status = busd.wait();
    assert!(status.success(), "busd exited with {status}");
    assert!(socket.0.exists(), "busd removed {}", socket.0.display());
    systemd.assert_silent();
}

/// A bus that listens on a socket of its own, without socket activation, doesn't hand over, however
/// many stored FDs the store takes: it stops as it does without systemd. It stores nothing, exits
/// successfully and removes the file of its listening socket.
#[test]
#[timeout(60000)]
fn no_hand_over_without_socket_activation() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::bind();

    let mut busd = Busd::spawn(&mut systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let _client = Client::connect(&address, Some(NAME));

    busd.signal(Signal::TERM);
    let status = busd.wait();
    assert!(status.success(), "busd exited with {status}");
    assert!(
        !socket.0.exists(),
        "busd left {} behind",
        socket.0.display()
    );
    systemd.assert_silent();
}

/// Keep the tests from running at the same time.
///
/// A process that a test starts holds a copy of every file descriptor of the process between `fork`
/// and `exec`, those that another test is using among them, such as the socket that a bus listens
/// on. A bus that takes the socket file of a previous instance in place only does so if no one
/// listens on the socket anymore, which such a copy keeps from being so for that moment.
fn serialize() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());

    // A test that failed leaves nothing behind that the others depend on.
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stop `busd` with SIGTERM, as systemd does, and take what it stores until it exits.
///
/// It exits successfully, and leaves the file of the listening socket at `socket` where it is.
fn hand_over(busd: &mut Busd, systemd: &mut Systemd, socket: &Removed) {
    busd.signal(Signal::TERM);
    systemd.until_barrier();
    let status = busd.wait();
    assert!(status.success(), "busd exited with {status}");
    assert!(
        socket.0.exists(),
        "busd removed {} as it handed over",
        socket.0.display()
    );
}

/// Wait for the signal among `messages` that comes with a file descriptor, and check that it is an
/// end of the socket pair whose other end is `kept`.
fn ping(messages: &mut MessageStream, kept: &mut UnixStream) {
    // What goes into one end of the pair comes out of the other.
    let mut received = UnixStream::from(received_fd(messages));
    received.write_all(b"ping").unwrap();
    let mut ping = [0; 4];
    kept.read_exact(&mut ping).unwrap();
    assert_eq!(&ping, b"ping");
}

/// Wait for the signal among `messages` that comes with a file descriptor, and give that.
fn received_fd(messages: &mut MessageStream) -> OwnedFd {
    let fd = zruntime::block_on(within(async {
        loop {
            let msg = messages.next().await.unwrap().unwrap();
            if msg.message_type() == message::Type::Signal
                && msg.header().member().is_some_and(|member| member == "Fd")
            {
                return msg.body().deserialize::<zbus::wire::OwnedFd>().unwrap();
            }
        }
    }));

    fd.into()
}

/// The owner of the name `name`, as `client` asks the bus.
fn owner(client: &Client, name: &str) -> String {
    try_owner(client, name).unwrap()
}

/// The owner of the name `name`, as `client` asks the bus, if the bus answers.
fn try_owner(client: &Client, name: &str) -> zbus::Result<String> {
    zruntime::block_on(within(async {
        let dbus = DBusProxy::new(&client.connection).await?;
        let owner = dbus.get_name_owner(BusName::try_from(name)?).await?;

        Ok(owner.to_string())
    }))
}

/// How many of `clients`, each with the name that it owned, the bus serves on, as they and `late`,
/// a client that connected since, ask it: each still owns its name, or else is disconnected, and
/// its name is no one's.
fn served(clients: &[(Client, String)], late: &Client) -> usize {
    let mut served = 0;
    for (client, name) in clients {
        match try_owner(client, name) {
            Ok(owner) => {
                assert_eq!(owner, client.unique_name);
                assert_eq!(try_owner(late, name).unwrap(), client.unique_name);
                served += 1;
            }
            Err(_) => assert!(try_owner(late, name).is_err(), "{name} is still owned"),
        }
    }

    served
}

/// A client of the bus.
struct Client {
    connection: Connection,
    unique_name: String,
    /// The number that the unique name carries.
    id: usize,
}

impl Client {
    /// Connect to the bus at `address`, and own `name` if given.
    fn connect(address: &str, name: Option<&str>) -> Self {
        let builder = connection::Builder::address(address);
        let builder = match name {
            Some(name) => builder.name(name),
            None => builder,
        };

        Self::build(builder)
    }

    /// Say hello to the bus over `stream`, a socket that is connected to it already.
    fn over(stream: UnixStream) -> Self {
        Self::build(connection::Builder::unix_stream(stream))
    }

    /// The client that `builder` connects, once the bus greeted it.
    fn build(builder: connection::Builder<'_>) -> Self {
        let connection = zruntime::block_on(within(builder.build())).unwrap();
        let unique_name = connection.unique_name().unwrap().to_string();
        let id = unique_name
            .strip_prefix(UNIQUE_NAME_PREFIX)
            .and_then(|id| id.parse().ok())
            .unwrap_or_else(|| panic!("`{unique_name}` isn't a unique name of busd"));

        Self {
            connection,
            unique_name,
            id,
        }
    }
}

/// A client that sends a file descriptor ahead of the message that it is for, which it speaks D-Bus
/// on its own for.
struct FdClient(UnixStream);

impl FdClient {
    /// Connect to the bus at `path`, authenticate, agreeing to pass file descriptors, and say
    /// hello, with `fds` along with the call, ahead of the messages that they are for. Then wait
    /// for the reply, which tells that the bus read both the call and `fds`.
    fn connect(path: &Path, fds: &[BorrowedFd<'_>]) -> io::Result<Self> {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(WAIT))?;
        // The user ID goes as the hex of its decimal digits.
        let uid: String = getuid()
            .as_raw()
            .to_string()
            .bytes()
            .map(|digit| format!("{digit:02x}"))
            .collect();
        stream.write_all(format!("\0AUTH EXTERNAL {uid}\r\n").as_bytes())?;
        expect_line(&mut stream, "OK ")?;
        stream.write_all(b"NEGOTIATE_UNIX_FD\r\n")?;
        expect_line(&mut stream, "AGREE_UNIX_FD")?;
        stream.write_all(b"BEGIN\r\n")?;

        let hello = Message::method_call("/org/freedesktop/DBus", "Hello")
            .destination("org.freedesktop.DBus")
            .interface("org.freedesktop.DBus")
            .build(&())
            .unwrap();
        let mut bytes = hello.data().bytes();
        // A message takes no more than `SCM_MAX_FD` file descriptors, so more go in more of them,
        // each with a byte of the call, but for the last, which goes with the rest.
        let mut chunks = fds.chunks(SCM_MAX_FD).peekable();
        while let Some(fds) = chunks.next() {
            let part = match chunks.peek() {
                Some(_) => 1,
                None => bytes.len(),
            };
            let mut space = vec![MaybeUninit::uninit(); cmsg_space!(ScmRights(fds.len()))];
            let mut control = SendAncillaryBuffer::new(&mut space);
            assert!(control.push(SendAncillaryMessage::ScmRights(fds)));
            let sent = sendmsg(
                &stream,
                &[IoSlice::new(&bytes[..part])],
                &mut control,
                SendFlags::empty(),
            )?;
            // A stream that no message was written to yet takes a short one whole.
            assert_eq!(sent, part);
            bytes = &bytes[part..];
        }
        // The reply comes first: the bus sends nothing before it.
        let mut fixed = [0; 16];
        stream.read_exact(&mut fixed)?;
        assert_eq!(fixed[1], 2, "the bus didn't reply to `Hello`");

        Ok(Self(stream))
    }

    /// Send `destination` the signal that the next file descriptor that the client sent ahead is
    /// for.
    fn send_for_fd(&mut self, destination: &str) -> io::Result<()> {
        // The signal is built with a file descriptor, the socket of the client, which its header
        // counts, but goes without it, so that the one sent ahead goes with it.
        let signal = Message::signal("/org/zbus/Fdstore", "org.zbus.Fdstore", "Fd")
            .destination(destination)
            .build(&Fd::from(&self.0))
            .unwrap();

        self.0.write_all(signal.data().bytes())
    }
}

/// Read a line that the bus sends over `stream` as it authenticates a client, and check that it
/// starts with `start`.
fn expect_line(stream: &mut UnixStream, start: &str) -> io::Result<()> {
    let mut line = Vec::new();
    while !line.ends_with(b"\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        line.push(byte[0]);
    }
    if !line.starts_with(start.as_bytes()) {
        let line = String::from_utf8_lossy(&line);

        return Err(io::Error::other(format!(
            "Unexpected `{}`",
            line.trim_end()
        )));
    }

    Ok(())
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

/// A busd process, killed if it's still running when dropped.
struct Busd {
    child: Child,
}

impl Busd {
    /// Starts busd as systemd starts a service, on the socket at `address`, with a file descriptor
    /// store of `store_max` file descriptors, and the sockets of socket activation and what is in
    /// the store of `systemd` as its file descriptors from 3 on.
    fn spawn(systemd: &mut Systemd, address: &str, store_max: usize) -> Self {
        Self::spawn_limited(systemd, address, store_max, None)
    }

    /// Starts busd as `spawn` does, with both limits of its open file descriptors at `nofile`, if
    /// given, so that busd can't raise its soft limit.
    fn spawn_limited(
        systemd: &mut Systemd,
        address: &str,
        store_max: usize,
        nofile: Option<libc::rlim_t>,
    ) -> Self {
        systemd.manager.set_store_max(store_max);
        let mut command = systemd.manager.command(BUSD).unwrap();
        // The bus reads the session configuration of the tests, rather than the one of the
        // system.
        command
            .args(["--config", CONFIG, "--address", address])
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        if let Some(nofile) = nofile {
            // SAFETY: The closure runs in the child, between `fork` and `exec`, where only
            // async-signal-safe functions may be called: it calls `setrlimit`, and `last_os_error`
            // if that fails, which reads `errno` and neither allocates nor takes a lock. It borrows
            // nothing. busd inherits the limits of the shell that execs it.
            unsafe {
                command.pre_exec(move || {
                    let limit = libc::rlimit {
                        rlim_cur: nofile,
                        rlim_max: nofile,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) < 0 {
                        return Err(io::Error::last_os_error());
                    }

                    Ok(())
                });
            }
        }
        let child = command.spawn().unwrap();

        Self { child }
    }

    /// How many file descriptors busd has open.
    fn open_fds(&self) -> libc::rlim_t {
        let fds = fs::read_dir(format!("/proc/{}/fd", self.child.id())).unwrap();

        fds.count() as libc::rlim_t
    }

    /// Sends `signal` to busd.
    fn signal(&self, signal: Signal) {
        kill_process(Pid::from_child(&self.child), signal).unwrap();
    }

    /// Waits a few seconds at most for busd to exit.
    ///
    /// A busd that doesn't exit so fails the test, which then kills it, rather than hang the test
    /// until it times out, which would leave busd running.
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "busd didn't exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Busd {
    fn drop(&mut self) {
        // It has already exited, unless a test failed before it stopped it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The service manager, as busd sees it: the fake one of zoter, which keeps the store as systemd
/// does, on a thread of its own, and starts busd as systemd does.
struct Systemd {
    manager: Manager,
}

impl Systemd {
    /// A notification socket that no one has sent anything to.
    fn bind() -> Self {
        Self {
            manager: Manager::new().unwrap(),
        }
    }

    /// A notification socket, as [`Systemd::bind`] binds, and a listening socket of socket
    /// activation, bound at `path`, which systemd keeps open while busd comes and goes, with its
    /// file in place.
    fn activating(path: &Path) -> Self {
        let mut systemd = Self::bind();
        let activation = UnixListener::bind(path).unwrap();
        systemd.manager.add_socket(ACTIVATION, activation.into());

        systemd
    }

    /// Close the listening socket of the socket activation, as systemd does once its socket unit
    /// goes, and leave its file behind.
    fn deactivate(&mut self) {
        self.manager.take_sockets();
    }

    /// Wait until busd tells that it's ready.
    ///
    /// Nothing stays stored while busd runs: whatever it took from the store, it removed, which it
    /// tells systemd of before that it's ready.
    fn until_ready(&mut self) {
        self.manager.wait_for("READY=1", WAIT).unwrap();

        let left = self.names();
        assert!(left.is_empty(), "busd left {left:?} in the store");
    }

    /// Wait for the barrier that busd ends its hand-over with, which systemd deals with once it
    /// dealt with what busd stored before it.
    fn until_barrier(&mut self) {
        self.manager.wait_for("BARRIER=1", WAIT).unwrap();
    }

    /// The names of what is stored, sorted.
    fn names(&self) -> Vec<String> {
        let mut names = self.manager.stored_names();
        names.sort_unstable();

        names
    }

    /// Replace the stored hand-over of the bus with one that no bus can make sense of.
    fn spoil_state(&mut self) {
        let mut stored = self.manager.stored().unwrap();
        let (_, state) = stored
            .iter_mut()
            .find(|(name, _)| name == "busd/state")
            .unwrap();
        *state = File::open("/dev/null").unwrap().into();
        self.manager.replace_store(stored);
    }

    /// The names that busd removed from the store since this was last called, sorted.
    fn take_removed(&mut self) -> Vec<String> {
        let mut removed = self.manager.take_removed();
        removed.sort_unstable();

        removed
    }

    /// Check that the store holds what a bus hands over, and give how many connection FDs that is.
    ///
    /// That is its memfd first, so that a store too small for all of the stored FDs takes the
    /// one that matters most, and then the queue sockets, which hold the connection FDs: the client
    /// socket of each client, and its client-sent FDs.
    fn check_stored(&mut self) -> usize {
        let names = self.manager.stored_names();
        let Some((state, queues)) = names.split_first() else {
            panic!("busd stored nothing");
        };
        assert_eq!(state, "busd/state", "busd stored {names:?}");
        for name in queues {
            assert_eq!(name, "busd/fds", "busd stored `{name}` among its clients");
        }

        self.manager.stored_fds("busd").unwrap()
    }

    /// Check that busd has sent nothing more than the test waited for.
    ///
    /// For a busd that has exited, as what it sent is in the socket by then, which the manager
    /// deals with before the barrier of the test.
    fn assert_silent(&self) {
        self.manager.notifier().unwrap().barrier(WAIT).unwrap();
        assert_eq!(self.manager.take_notifications(), ["BARRIER=1"]);
    }
}

/// A file that is removed when dropped, if it's there then.
struct Removed(PathBuf);

impl Removed {
    /// The address of a bus that listens at this path.
    fn address(&self) -> String {
        format!("unix:path={}", self.0.display())
    }
}

impl Drop for Removed {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// A path in the temporary directory that no one else uses.
fn temp_path() -> PathBuf {
    temp_dir().join(Alphanumeric.sample_string(&mut rng(), 10))
}

const BUSD: &str = env!("CARGO_BIN_EXE_busd");
const CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/session.conf");
/// The name that the client owns.
const NAME: &str = "org.zbus.Fdstore";
/// What the unique names of the clients of busd start with, before their numbers.
const UNIQUE_NAME_PREFIX: &str = ":busd.";
/// How many file descriptors the service may store: `FileDescriptorStoreMax=` of its unit.
const FDSTORE_MAX: usize = 16;
/// A limit of open FDs under which the next instance takes the fitting clients and no more: the
/// first of the passed FDs, the listening socket, the memfd and the queue sockets, as many as the
/// store takes, and the spare FDs that busd keeps free, besides the clients, each of which takes
/// its client socket and its process FD.
const LOW_NOFILE: libc::rlim_t = (FIRST_FD as libc::rlim_t)
    + 1
    + FDSTORE_MAX as libc::rlim_t
    + 32
    + 2 * FITTING_CLIENTS as libc::rlim_t;
const HARD_NOFILE: libc::rlim_t = 64;
const FREE_FDS: libc::rlim_t = 2;
/// How many clients the test hands over, and how many of them fit under `LOW_NOFILE`.
const CLIENTS: usize = 4;
const FITTING_CLIENTS: usize = 2;
/// The first of the passed FDs, `SD_LISTEN_FDS_START`.
const FIRST_FD: RawFd = 3;
/// How long the test waits for what should come soon, rather than hang if it never does.
const WAIT: Duration = Duration::from_secs(10);
/// The name of the unit of socket activation, which starts like the names of the stored FDs.
const ACTIVATION: &str = "busd-session.socket";
/// How many file descriptors fit in a message: `SCM_MAX_FD` of Linux, as unix(7) tells.
const SCM_MAX_FD: usize = 253;
/// How many client-sent FDs a client sends ahead, which more than one datagram takes.
const AHEAD: usize = SCM_MAX_FD + 50;
