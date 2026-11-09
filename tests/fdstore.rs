//! The busd binary, restarted the way that systemd restarts a service with a file descriptor store.
//!
//! The test is the service manager: it owns the notification socket that `$NOTIFY_SOCKET` names,
//! keeps the stored FDs, those that busd stores in the manager, and passes them to the next
//! instance of busd, among the passed FDs, as `$LISTEN_FDS` does. The FDs of a hand-over go by the
//! names that the docs of `src/bus/systemd.rs` give them.
#![cfg(target_os = "linux")]

use std::{
    env::temp_dir,
    fs::{self, File},
    future::IntoFuture,
    io::{self, IoSlice, IoSliceMut, Read, Seek, SeekFrom, Write},
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::{
            net::{UnixDatagram, UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
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
    io::{fcntl_dupfd_cloexec, Errno},
    net::{
        recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags,
        SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
    },
    process::{getpid, getuid, kill_process, Pid, Signal},
};
use zbus::{
    connection, fdo::DBusProxy, message, names::BusName, wire::Fd, Connection, Message,
    MessageStream,
};
use zruntime::SharedRuntime;

/// A bus that systemd stops hands over to its next instance, which serves its clients on as if
/// nothing happened.
///
/// The first instance stores the state memfd, its listening socket and a queue socket, which holds
/// the connection FDs of its clients, in the store, and exits without removing the file of its
/// listening socket. The second one, which is passed them, removes them from the store and tells
/// that it's ready. A client that never reconnected still owns its name, and a client that connects
/// now gets a number after those of the others, and sees who owns the name. The second instance
/// hands over in turn, with both clients, as an instance that took over hands over as well as one
/// that didn't.
#[test]
#[timeout(60000)]
fn restart() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::bind();

    let mut first = Busd::spawn(&systemd, &address, &[], FDSTORE_MAX);
    systemd.until_ready();
    let client = Client::connect(&address, Some(NAME));
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(true), 1);

    let stored = systemd.names();
    let mut second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);
    assert_eq!(owner(&client, NAME), client.unique_name);
    let late = Client::connect(&address, None);
    // The second instance goes on with the numbers of the first.
    assert_eq!(late.id, client.id + 1);
    assert_eq!(owner(&late, NAME), client.unique_name);

    // SIGINT would do the same: the bus doesn't tell the two apart.
    hand_over(&mut second, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(true), 2);
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
    let mut systemd = Systemd::bind();

    let mut first = Busd::spawn(&systemd, &address, &[], FDSTORE_MAX);
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
    assert_eq!(systemd.check_stored(true), 3);

    let stored = systemd.names();
    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
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
    let mut systemd = Systemd::bind();
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

    let mut first = Busd::spawn(&systemd, &address, &[], FDSTORE_MAX);
    systemd.until_ready();
    let receiver = Client::connect(&address, None);
    let mut sender = FdClient::connect(&socket.0, &fds).unwrap();
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(true), 2 + AHEAD);

    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
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
    let mut systemd = Systemd::bind();

    let mut first = Busd::spawn(&systemd, &address, &[], FDSTORE_MAX);
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
    assert_eq!(systemd.check_stored(true), 4);

    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
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
    let mut systemd = Systemd::bind();
    let file = Removed(temp_path());
    fs::write(&file.0, b"offsets").unwrap();
    let open_at = |offset| {
        let mut opened = File::open(&file.0).unwrap();
        opened.seek(SeekFrom::Start(offset)).unwrap();

        opened
    };
    let offsets = [1, 4];

    let mut first = Busd::spawn(&systemd, &address, &[], FDSTORE_MAX);
    systemd.until_ready();
    let receiver = Client::connect(&address, None);
    let mut sender = {
        let sent = offsets.map(open_at);

        FdClient::connect(&socket.0, &sent.each_ref().map(AsFd::as_fd)).unwrap()
    };
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(true), 2 + offsets.len());

    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
    systemd.until_ready();
    let mut messages = MessageStream::from(&receiver.connection);
    for offset in offsets {
        sender.send_for_fd(&receiver.unique_name).unwrap();
        let mut received = File::from(received_fd(&mut messages));
        assert_eq!(received.stream_position().unwrap(), offset);
    }
}

/// A bus that socket activation started hands over its clients, but not its listening socket,
/// which the activation owns: the unit may have any name, even one that starts like those of the
/// stored FDs, and the next instance is passed the listening socket along with the stored FDs.
#[test]
#[timeout(60000)]
fn restart_under_socket_activation() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    // The socket is systemd's, which keeps it open and its file in place as busd comes and goes.
    let listener = UnixListener::bind(&socket.0).unwrap();
    let mut systemd = Systemd::bind();

    let activation = [(ACTIVATION, listener.as_fd())];
    let mut first = Busd::spawn(&systemd, &address, &activation, FDSTORE_MAX);
    systemd.until_ready();
    assert!(systemd.take_removed().is_empty());
    let client = Client::connect(&address, Some(NAME));
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(false), 1);

    let stored = systemd.names();
    let passed = systemd.passed(Some(&listener));
    let _second = Busd::spawn(&systemd, &address, &passed, FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);
    assert_eq!(owner(&client, NAME), client.unique_name);
    let late = Client::connect(&address, None);
    assert_eq!(owner(&late, NAME), client.unique_name);
}

/// A next instance that fails to take over, as it can't make sense of the state, starts anyway: it
/// removes everything from the store, disconnects the clients of the previous instance, and serves
/// new clients on the same address, on the socket file of the previous instance.
#[test]
#[timeout(60000)]
fn failed_take_over() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::bind();

    let mut first = Busd::spawn(&systemd, &address, &[], FDSTORE_MAX);
    systemd.until_ready();
    let client = Client::connect(&address, Some(NAME));
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(true), 1);
    systemd.spoil_state();

    let stored = systemd.names();
    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
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
    let mut systemd = Systemd::bind();
    let low = Some(LOW_NOFILE);

    let mut first = Busd::spawn_limited(&systemd, &address, &[], FDSTORE_MAX, low);
    systemd.until_ready();
    let clients: Vec<_> = (0..CLIENTS)
        .map(|i| {
            let name = format!("{NAME}{i}");

            (Client::connect(&address, Some(&name)), name)
        })
        .collect();
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(true), FITTING_CLIENTS);

    let removed = systemd.names();
    let passed = systemd.passed(None);
    let _second = Busd::spawn_limited(&systemd, &address, &passed, FDSTORE_MAX, low);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), removed);
    let late = Client::connect(&address, None);
    assert_eq!(served(&clients, &late), FITTING_CLIENTS);
}

/// A bus that has no FD to spare, as it has as many open as its hard limit takes, still hands
/// over: it keeps some free from the start for the duplicate of its listening socket, the state
/// memfd, the queue socket and the pipe of the barrier, so it stores the state memfd and its
/// listening socket, and the connection FDs of the clients whose client sockets it could duplicate
/// and that fit under that limit. The next instance, under the same limit, serves the clients that
/// were stored.
#[test]
#[timeout(60000)]
fn hand_over_at_the_hard_fd_limit() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::bind();

    let hard = Some(HARD_NOFILE);
    let mut first = Busd::spawn_limited(&systemd, &address, &[], FDSTORE_MAX, hard);
    systemd.until_ready();
    // Clients connect until busd has hardly a file descriptor left.
    let mut clients = Vec::new();
    while HARD_NOFILE - first.open_fds() > FREE_FDS {
        let name = format!("{NAME}{}", clients.len());
        clients.push((Client::connect(&address, Some(&name)), name));
    }
    hand_over(&mut first, &mut systemd, &socket);
    let stored = systemd.check_stored(true);
    assert!(
        stored > 0,
        "busd stored none of its {} clients",
        clients.len()
    );

    let removed = systemd.names();
    let passed = systemd.passed(None);
    let _second = Busd::spawn_limited(&systemd, &address, &passed, FDSTORE_MAX, hard);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), removed);
    let late = Client::connect(&address, None);
    assert_eq!(served(&clients, &late), stored);
}

/// A next instance that fails to take over under socket activation starts on the socket that
/// systemd passed it, which stays in place: it serves new clients at the same address, and
/// disconnects the clients of the previous instance.
#[test]
#[timeout(60000)]
fn failed_take_over_under_socket_activation() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let listener = UnixListener::bind(&socket.0).unwrap();
    let mut systemd = Systemd::bind();

    let activation = [(ACTIVATION, listener.as_fd())];
    let mut first = Busd::spawn(&systemd, &address, &activation, FDSTORE_MAX);
    systemd.until_ready();
    let client = Client::connect(&address, Some(NAME));
    hand_over(&mut first, &mut systemd, &socket);
    assert_eq!(systemd.check_stored(false), 1);
    systemd.spoil_state();

    let stored = systemd.names();
    let passed = systemd.passed(Some(&listener));
    let _second = Busd::spawn(&systemd, &address, &passed, FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);
    assert!(socket.0.exists());

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
/// as it does without systemd. It stores nothing, exits successfully and removes the file of its
/// listening socket.
#[test]
#[timeout(60000)]
fn no_hand_over_with_a_small_store() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::bind();

    let mut busd = Busd::spawn(&systemd, &address, &[], 1);
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
/// It exits successfully, and leaves the file of its socket at `socket` where it is.
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
        let connection = zruntime::block_on(within(async {
            let builder = connection::Builder::address(address);
            let builder = match name {
                Some(name) => builder.name(name),
                None => builder,
            };

            builder.build().await
        }))
        .unwrap();
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
    /// store of `store_max` file descriptors, and `passed` as its file descriptors from 3 on.
    fn spawn(
        systemd: &Systemd,
        address: &str,
        passed: &[(&str, BorrowedFd<'_>)],
        store_max: usize,
    ) -> Self {
        Self::spawn_limited(systemd, address, passed, store_max, None)
    }

    /// Starts busd as `spawn` does, with both limits of its open file descriptors at `nofile`, if
    /// given, so that busd can't raise its soft limit.
    fn spawn_limited(
        systemd: &Systemd,
        address: &str,
        passed: &[(&str, BorrowedFd<'_>)],
        store_max: usize,
        nofile: Option<libc::rlim_t>,
    ) -> Self {
        let count = RawFd::try_from(passed.len()).unwrap();
        // The file descriptors may be among those they are to be passed as, so that passing one
        // would close another before it's passed. Copies out of that way, which close on exec, are
        // what is passed instead, and the caller keeps its own. The copies close as this returns,
        // once busd has its own.
        let copies: Vec<OwnedFd> = passed
            .iter()
            .map(|(_, fd)| fcntl_dupfd_cloexec(fd, FIRST_FD + count).unwrap())
            .collect();
        let sources: Vec<RawFd> = copies.iter().map(AsRawFd::as_raw_fd).collect();

        let mut command = if passed.is_empty() {
            Command::new(BUSD)
        } else {
            // The shell runs busd as itself, so that `$LISTEN_PID` is the PID of busd, which the
            // process can't know before it starts: systemd sets it for the process it execs.
            let mut command = Command::new("sh");
            command.args(["-c", r#"LISTEN_PID=$$ exec "$0" "$@""#, BUSD]);

            command
        };
        // What the tests are run with must not tell busd of file descriptors, or of anything
        // else of systemd's.
        for var in [
            "LISTEN_PID",
            "LISTEN_PIDFDID",
            "LISTEN_FDS",
            "LISTEN_FDNAMES",
        ] {
            command.env_remove(var);
        }
        if !passed.is_empty() {
            let names: Vec<_> = passed.iter().map(|(name, _)| *name).collect();
            command
                .env("LISTEN_FDS", count.to_string())
                .env("LISTEN_FDNAMES", names.join(":"));
        }
        // The bus reads the session configuration of the tests, rather than the one of the
        // system.
        command
            .args(["--config", CONFIG, "--address", address])
            .env("NOTIFY_SOCKET", systemd.path())
            .env("FDSTORE", store_max.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        // SAFETY: The closure runs in the child, between `fork` and `exec`, where only
        // async-signal-safe functions may be called: it calls `setrlimit` and `dup2`, and
        // `last_os_error` if one fails, which reads `errno` and neither allocates nor takes a lock.
        // It owns the numbers of the file descriptors, and borrows nothing. The sources are open in
        // the child, as `copies` holds them until `spawn` returns. They are all `FIRST_FD +
        // count` or above, so none is among the targets, `FIRST_FD` up to that, and no `dup2`
        // closes a source that another has yet to copy. A target that is open is closed, but only
        // in the child, whose copy nothing uses but for `std`'s pipe for reporting a failed `exec`:
        // if that were one, the test would fail for want of a busd that is ready, rather than for
        // the error. `dup2` clears the close-on-exec flag of the target, which keeps it open across
        // `exec`. The copies stay close-on-exec, so they close then.
        unsafe {
            command.pre_exec(move || {
                if let Some(nofile) = nofile {
                    let limit = libc::rlimit {
                        rlim_cur: nofile,
                        rlim_max: nofile,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                for (target, source) in (FIRST_FD..).zip(&sources) {
                    if libc::dup2(*source, target) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }

                Ok(())
            });
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

/// The service manager, as busd sees it.
struct Systemd {
    /// The notification socket, `$NOTIFY_SOCKET`.
    socket: UnixDatagram,
    path: Removed,
    /// The file descriptor store of the service: the stored FDs that busd didn't remove, with the
    /// names it stored them under.
    store: Vec<(String, OwnedFd)>,
    /// The names that busd removed from the store since the test last asked.
    removed: Vec<String>,
}

impl Systemd {
    /// A notification socket that no one has sent anything to.
    fn bind() -> Self {
        let path = Removed(temp_path());
        let socket = UnixDatagram::bind(&path.0).unwrap();
        // A bus that doesn't send what the test waits for fails the test, which then kills it,
        // rather than hang the test until it times out, which would leave the bus running.
        socket.set_read_timeout(Some(WAIT)).unwrap();

        Self {
            socket,
            path,
            store: Vec::new(),
            removed: Vec::new(),
        }
    }

    /// The path of the notification socket.
    fn path(&self) -> &Path {
        &self.path.0
    }

    /// The passed FDs, to pass to busd as it starts: the listening socket of the socket
    /// activation, if any, and the stored FDs, with their names.
    fn passed<'a>(
        &'a self,
        activation: Option<&'a UnixListener>,
    ) -> Vec<(&'a str, BorrowedFd<'a>)> {
        let activation = activation.map(|listener| (ACTIVATION, listener.as_fd()));
        let stored = self
            .store
            .iter()
            .map(|(name, fd)| (name.as_str(), fd.as_fd()));

        activation.into_iter().chain(stored).collect()
    }

    /// Deal with what busd sends, until it tells that it's ready.
    ///
    /// Nothing stays stored while busd runs: whatever it took from the store, it removed.
    fn until_ready(&mut self) {
        while self.process() != "READY=1" {}

        let left = self.names();
        assert!(left.is_empty(), "busd left {left:?} in the store");
    }

    /// Deal with what busd sends as it hands over, until its barrier.
    fn until_barrier(&mut self) {
        while self.process() != "BARRIER=1" {}
    }

    /// Deal with the next notification, as systemd does, and give its text.
    ///
    /// It keeps the stored FDs, and closes those that busd removes. It closes the pipe that comes
    /// with a barrier, which lets busd go on, as it gets to the message only once it has dealt with
    /// the earlier ones.
    fn process(&mut self) -> String {
        let (state, fds) = self.recv();
        if state == "READY=1" {
            assert!(fds.is_empty(), "busd sent file descriptors with {state:?}");
        } else if state == "BARRIER=1" {
            assert_eq!(fds.len(), 1, "the barrier comes with a pipe to wait for");
        } else if let Some(name) = state.strip_prefix("FDSTORE=1\nFDPOLL=0\nFDNAME=") {
            // The store doesn't watch the file descriptors for hang-ups, which would drop what a
            // client sent before it hung up.
            let Ok([fd]) = <[OwnedFd; 1]>::try_from(fds) else {
                panic!("busd sent a number of file descriptors other than 1 to store as `{name}`");
            };
            // The store keeps one file descriptor of each open file description, under the name
            // that the first came with, and closes the others without a word.
            if !self.store.iter().any(|(_, stored)| same_file(stored, &fd)) {
                self.store.push((name.to_owned(), fd));
            }
        } else if let Some(name) = state.strip_prefix("FDSTOREREMOVE=1\nFDNAME=") {
            assert!(fds.is_empty(), "busd sent file descriptors with {state:?}");
            let stored = self.store.len();
            self.store.retain(|(stored, _)| stored != name);
            assert!(
                self.store.len() < stored,
                "busd removed `{name}`, which isn't stored"
            );
            self.removed.push(name.to_owned());
        } else {
            panic!("busd sent {state:?}, which systemd doesn't take");
        }

        state
    }

    /// The names of what is stored, sorted.
    fn names(&self) -> Vec<String> {
        let mut names = self.names_in_order();
        names.sort_unstable();

        names
    }

    /// The names of what is stored, in the order that it was stored.
    fn names_in_order(&self) -> Vec<String> {
        self.store.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Replace the stored state of the bus with one that no bus can make sense of.
    fn spoil_state(&mut self) {
        let garbage = busd::systemd::memfd("busd-state", b"not the state of a bus").unwrap();
        let (_, state) = self
            .store
            .iter_mut()
            .find(|(name, _)| name == "busd-state")
            .unwrap();
        *state = garbage;
    }

    /// The names that busd removed from the store since this was last called, sorted.
    fn take_removed(&mut self) -> Vec<String> {
        let mut removed = std::mem::take(&mut self.removed);
        removed.sort_unstable();

        removed
    }

    /// Check that the store holds what a bus hands over, and give how many connection FDs that is.
    ///
    /// That is its state memfd first, then its listening socket if it bound that itself, so that a
    /// store too small for all of the stored FDs takes those that matter most, and then the queue
    /// sockets, which hold the connection FDs: the client socket of each client, and its
    /// client-sent FDs. This takes those out of the queue sockets, and puts them back in queue
    /// sockets of its own, in place of those of busd, for the next instance to take them out of.
    fn check_stored(&mut self, listener: bool) -> usize {
        let names: Vec<_> = self.store.iter().map(|(name, _)| name.as_str()).collect();
        let head: &[&str] = if listener {
            &["busd-state", "busd-listener"]
        } else {
            &["busd-state"]
        };
        let Some((stored_head, queues)) = names.split_at_checked(head.len()) else {
            panic!("busd stored {names:?}, which isn't {head:?} and its clients");
        };
        assert_eq!(stored_head, head);
        for name in queues {
            assert_eq!(
                *name, "busd-clients",
                "busd stored `{name}` among its clients"
            );
        }

        let mut count = 0;
        for (_, stored) in self.store.iter_mut().skip(head.len()) {
            let mut queued = Vec::new();
            busd::systemd::take_queued(stored.as_fd(), |index, fds, complete| {
                assert!(complete, "the test took some of those at {index} only");
                queued.push((index, fds));
            })
            .unwrap();
            let queue = busd::systemd::FdQueue::new().unwrap();
            for (index, fds) in &queued {
                let fds: Vec<_> = fds.iter().map(AsFd::as_fd).collect();
                assert!(
                    queue.push(*index, &fds).unwrap(),
                    "a queue of the test is full"
                );
                count += fds.len();
            }
            *stored = queue.into_socket();
        }

        count
    }

    /// Check that busd has sent nothing more than the test took.
    ///
    /// For a busd that has exited, as what it sent is in the socket by then.
    fn assert_silent(&self) {
        let result = self.try_recv(RecvFlags::DONTWAIT);
        assert!(
            matches!(result, Err(Errno::AGAIN)),
            "busd sent something more: {result:?}"
        );
    }

    /// The next notification, with the file descriptors that come with it.
    fn recv(&self) -> (String, Vec<OwnedFd>) {
        self.try_recv(RecvFlags::empty())
            .unwrap_or_else(|e| panic!("busd sent no notification in time: {e}"))
    }

    fn try_recv(&self, flags: RecvFlags) -> rustix::io::Result<(String, Vec<OwnedFd>)> {
        let mut state = [0; MAX_NOTIFICATION];
        let mut space = [MaybeUninit::uninit(); cmsg_space!(ScmRights(MAX_FDS))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let received = recvmsg(
            &self.socket,
            &mut [IoSliceMut::new(&mut state)],
            &mut control,
            // The file descriptors are the test's to pass on, not to leak into what it starts.
            flags | RecvFlags::CMSG_CLOEXEC,
        )?;
        let truncated = ReturnFlags::TRUNC | ReturnFlags::CTRUNC;
        assert!(
            !received.flags.intersects(truncated),
            "a notification of busd is too large for the test"
        );
        // Every file descriptor is taken, so that none stays open in the kernel, with busd waiting
        // for it to close.
        let fds = control
            .drain()
            .filter_map(|message| match message {
                RecvAncillaryMessage::ScmRights(fds) => Some(fds),
                _ => None,
            })
            .flatten()
            .collect();
        let state = str::from_utf8(&state[..received.bytes]).unwrap();

        Ok((state.to_owned(), fds))
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

/// Whether `a` and `b` are of the same open file description, as systemd tells through
/// `kcmp(2)`, if the kernel lets the test call that.
fn same_file(a: &OwnedFd, b: &OwnedFd) -> bool {
    let pid = libc::c_long::from(Pid::as_raw(Some(getpid())));
    // SAFETY: The call reads and writes no memory, and takes each of its arguments as a long.
    let order = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid,
            pid,
            KCMP_FILE,
            libc::c_long::from(a.as_raw_fd()),
            libc::c_long::from(b.as_raw_fd()),
        )
    };

    order == 0
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
/// first of the passed FDs, the state memfd, the listening socket and the queue socket, and the
/// spare FDs that busd keeps free, besides the clients, each of which takes its client socket and
/// its process FD.
const LOW_NOFILE: libc::rlim_t =
    (FIRST_FD as libc::rlim_t) + 3 + 32 + 2 * FITTING_CLIENTS as libc::rlim_t;
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
/// How large a notification the test takes, which is that of systemd.
const MAX_NOTIFICATION: usize = 4096;
/// How many file descriptors the test takes with a notification: busd sends one.
const MAX_FDS: usize = 4;
/// How many file descriptors fit in a message: `SCM_MAX_FD` of Linux, as unix(7) tells.
const SCM_MAX_FD: usize = 253;
/// How many client-sent FDs a client sends ahead, which more than one datagram takes.
const AHEAD: usize = SCM_MAX_FD + 50;
/// What `kcmp(2)` compares of two file descriptors: their open file descriptions.
const KCMP_FILE: libc::c_long = 0;
