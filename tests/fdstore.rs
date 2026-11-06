//! The busd binary, restarted the way that systemd restarts a service with a file descriptor store.
//!
//! The test is the service manager: it owns the notification socket that `$NOTIFY_SOCKET` names,
//! keeps the file descriptors that busd stores in the manager, and passes them to the next instance
//! of busd as `$LISTEN_FDS` does.
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
/// The first instance stores its state, the socket it listens on and the sockets of its clients in
/// the file descriptor store, and exits without removing the file of its socket. The second one,
/// which is passed them, removes them from the store and tells that it's ready. A client that never
/// reconnected still owns its name, and a client that connects now gets a number after those of the
/// others, and sees who owns the name. The second instance hands over in turn, with both clients,
/// as an instance that took over hands over as well as one that didn't.
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
    systemd.assert_stored(&[client.id], true);

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
    systemd.assert_stored(&[client.id, late.id], true);
}

/// File descriptors that a client sent ahead of the message that they are for, as the specification
/// allows, which busd read but no message took by the time that it hands over, are stored after
/// the socket of the client, and the next instance passes them on with that message.
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
    // busd numbers its clients in the order that they connect.
    let sender_id = receiver.id + 1;
    hand_over(&mut first, &mut systemd, &socket);
    systemd.assert_stored(&[receiver.id, sender_id], true);
    let names = systemd.names_in_order();
    let at = names
        .iter()
        .position(|name| *name == format!("busd-client-{sender_id}-0"))
        .unwrap();
    assert_eq!(
        names.get(at + 1),
        Some(&format!("busd-client-{sender_id}-1")),
        "busd stored {names:?}"
    );

    let stored = systemd.names();
    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), stored);
    // The stream takes what the connection reads from now on: the signal among it.
    let mut messages = MessageStream::from(&receiver.connection);
    sender.send_for_fd(&receiver.unique_name).unwrap();
    ping(&mut messages, &mut kept);
}

/// File descriptors of one open file description, which systemd stores once, as a file descriptor
/// that a client sent twice ahead of the messages that they are for, are stored once, and the next
/// instance passes each on all the same.
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
    // busd numbers its clients in the order that they connect.
    let sender_id = receiver.id + 1;
    hand_over(&mut first, &mut systemd, &socket);
    systemd.assert_stored(&[receiver.id, sender_id], true);
    let names = systemd.names();
    let duplicate = format!("busd-client-{sender_id}-2-{sender_id}-1");
    assert!(names.contains(&duplicate), "busd stored {names:?}");

    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
    systemd.until_ready();
    let mut messages = MessageStream::from(&receiver.connection);
    for _ in 0..2 {
        sender.send_for_fd(&receiver.unique_name).unwrap();
        ping(&mut messages, &mut kept);
    }
}

/// File descriptors of one file, with the same flags, but of open file descriptions of their own,
/// as two opens of the file make, are each stored as they are, where the kernel doesn't tell busd
/// whether they are of the same open file description, rather than one taken for a duplicate of
/// the other: the next instance passes each on as it was, with an offset of its own.
#[test]
#[timeout(60000)]
fn restart_with_fds_of_one_file_sent_ahead_without_fd_queries() {
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

    let mut first = Busd::spawn_without_fd_queries(&systemd, &address, FDSTORE_MAX);
    systemd.until_ready();
    let receiver = Client::connect(&address, None);
    let mut sender = {
        let sent = offsets.map(open_at);

        FdClient::connect(&socket.0, &sent.each_ref().map(AsFd::as_fd)).unwrap()
    };
    // busd numbers its clients in the order that they connect.
    let sender_id = receiver.id + 1;
    hand_over(&mut first, &mut systemd, &socket);
    systemd.assert_stored(&[receiver.id, sender_id], true);
    let names = systemd.names();
    for n in 1..=offsets.len() {
        let name = format!("busd-client-{sender_id}-{n}");
        assert!(names.contains(&name), "busd stored {names:?}");
    }

    let _second = Busd::spawn(&systemd, &address, &systemd.passed(None), FDSTORE_MAX);
    systemd.until_ready();
    let mut messages = MessageStream::from(&receiver.connection);
    for offset in offsets {
        sender.send_for_fd(&receiver.unique_name).unwrap();
        let mut received = File::from(received_fd(&mut messages));
        assert_eq!(received.stream_position().unwrap(), offset);
    }
}

/// A bus that socket activation started hands over its clients, but not the socket it listens on,
/// which the activation owns: the unit may have any name, even one that starts like those of busd's
/// own file descriptors, and the next instance is passed the socket along with what was stored.
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
    systemd.assert_stored(&[client.id], false);

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
    systemd.assert_stored(&[client.id], true);
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

/// A bus that hands over as many clients as the next instance may start with: systemd passes the
/// next instance its file descriptors under the soft limit of the service, so a bus with that limit
/// low hands over fewer clients than it has, and the others get disconnected, losing their names.
#[test]
#[timeout(60000)]
fn hand_over_fits_the_fd_limit() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::bind();

    let mut first = Busd::spawn_limited(
        &systemd,
        &address,
        &[],
        FDSTORE_MAX,
        Some(Nofile::Soft(LOW_NOFILE)),
    );
    systemd.until_ready();
    let names: Vec<_> = (0..CLIENTS).map(|i| format!("{NAME}{i}")).collect();
    let clients: Vec<_> = names
        .iter()
        .map(|name| Client::connect(&address, Some(name)))
        .collect();
    hand_over(&mut first, &mut systemd, &socket);
    let stored = systemd.client_ids();
    assert_eq!(
        stored.len(),
        FITTING_CLIENTS,
        "busd stored {:?}",
        systemd.names()
    );
    systemd.assert_stored(&stored, true);

    let removed = systemd.names();
    let passed = systemd.passed(None);
    let _second = Busd::spawn_limited(
        &systemd,
        &address,
        &passed,
        FDSTORE_MAX,
        Some(Nofile::Soft(LOW_NOFILE)),
    );
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), removed);
    let late = Client::connect(&address, None);
    for (client, name) in clients.iter().zip(&names) {
        if stored.contains(&client.id) {
            assert_eq!(owner(client, name), client.unique_name);
            assert_eq!(owner(&late, name), client.unique_name);
        } else {
            assert!(
                try_owner(client, name).is_err(),
                "{name} is still connected"
            );
            assert!(try_owner(&late, name).is_err(), "{name} is still owned");
        }
    }
}

/// A bus that has no file descriptor to spare, as it has as many open as its hard limit takes,
/// still hands over: it keeps some free from the start for the socket it listens on, the state and
/// the barrier, so it stores the state and the listener, and the clients that it can duplicate.
/// The next instance, which has no such limit, serves the clients that were stored.
#[test]
#[timeout(60000)]
fn hand_over_at_the_hard_fd_limit() {
    let _serial = serialize();
    let socket = Removed(temp_path());
    let address = socket.address();
    let mut systemd = Systemd::bind();

    let hard = Some(Nofile::Hard(HARD_NOFILE));
    let mut first = Busd::spawn_limited(&systemd, &address, &[], FDSTORE_MAX, hard);
    systemd.until_ready();
    // Clients connect until busd has hardly a file descriptor left.
    let mut clients = Vec::new();
    while HARD_NOFILE - first.open_fds() > FREE_FDS {
        let name = format!("{NAME}{}", clients.len());
        clients.push((Client::connect(&address, Some(&name)), name));
    }
    hand_over(&mut first, &mut systemd, &socket);
    let stored = systemd.client_ids();
    systemd.assert_stored(&stored, true);

    let removed = systemd.names();
    let passed = systemd.passed(None);
    let _second = Busd::spawn(&systemd, &address, &passed, FDSTORE_MAX);
    systemd.until_ready();
    assert_eq!(systemd.take_removed(), removed);
    let late = Client::connect(&address, None);
    for (client, name) in &clients {
        if stored.contains(&client.id) {
            assert_eq!(owner(client, name), client.unique_name);
            assert_eq!(owner(&late, name), client.unique_name);
        } else {
            assert!(
                try_owner(client, name).is_err(),
                "{name} is still connected"
            );
        }
    }
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
    systemd.assert_stored(&[client.id], false);
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

/// A file descriptor store that takes one file descriptor is too small to hand over with: the bus
/// stops as it does without systemd. It stores nothing, exits successfully and removes the file of
/// its socket.
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
        let bytes = hello.data().bytes();
        let mut space = vec![MaybeUninit::uninit(); cmsg_space!(ScmRights(fds.len()))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(fds)));
        let sent = sendmsg(
            &stream,
            &[IoSlice::new(bytes)],
            &mut control,
            SendFlags::empty(),
        )?;
        // A stream that no message was written to yet takes a short one whole.
        assert_eq!(sent, bytes.len());
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

    /// Starts busd as `spawn` does, with the soft limit of its open file descriptors at `nofile`,
    /// if given, and the hard limit as it is, for busd to raise the soft limit again.
    fn spawn_limited(
        systemd: &Systemd,
        address: &str,
        passed: &[(&str, BorrowedFd<'_>)],
        store_max: usize,
        nofile: Option<Nofile>,
    ) -> Self {
        Self::spawn_with(systemd, address, passed, store_max, nofile, None)
    }

    /// Starts busd as `spawn` does, with nothing passed, on a kernel that doesn't tell whether two
    /// file descriptors are of the same open file description, as one before Linux 6.10, which
    /// lacks `F_DUPFD_QUERY`, whose filter of system calls denies `kcmp(2)`.
    fn spawn_without_fd_queries(systemd: &Systemd, address: &str, store_max: usize) -> Self {
        let filter = Some(without_fd_queries());

        Self::spawn_with(systemd, address, &[], store_max, None, filter)
    }

    /// Starts busd as `spawn_limited` does, under the filter of system calls `filter`, if given.
    fn spawn_with(
        systemd: &Systemd,
        address: &str,
        passed: &[(&str, BorrowedFd<'_>)],
        store_max: usize,
        nofile: Option<Nofile>,
        filter: Option<[libc::sock_filter; FILTER_LEN]>,
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
        // async-signal-safe functions may be called: it calls `getrlimit`, `setrlimit`, `dup2`,
        // `prctl` and `seccomp`, and `last_os_error` if one fails, which read `errno` and neither
        // allocate nor take a lock. It owns the numbers of the file descriptors and the filter,
        // which the program that it hands `seccomp` points to, and borrows nothing. The sources are
        // open in the child, as `copies` holds them until `spawn` returns. They are all `FIRST_FD +
        // count` or above, so none is among the targets, `FIRST_FD` up to that, and no `dup2`
        // closes a source that another has yet to copy. A target that is open is closed, but only
        // in the child, whose copy nothing uses but for `std`'s pipe for reporting a failed `exec`:
        // if that were one, the test would fail for want of a busd that is ready, rather than for
        // the error. `dup2` clears the close-on-exec flag of the target, which keeps it open across
        // `exec`. The copies stay close-on-exec, so they close then.
        unsafe {
            command.pre_exec(move || {
                if let Some(nofile) = nofile {
                    let mut limit = libc::rlimit {
                        rlim_cur: 0,
                        rlim_max: 0,
                    };
                    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    match nofile {
                        Nofile::Soft(soft) => limit.rlim_cur = soft,
                        Nofile::Hard(hard) => {
                            limit.rlim_cur = hard;
                            limit.rlim_max = hard;
                        }
                    }
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                for (target, source) in (FIRST_FD..).zip(&sources) {
                    if libc::dup2(*source, target) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                let Some(filter) = &filter else {
                    return Ok(());
                };
                // Only a process that can't gain privileges may set a filter up without them.
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                let program = libc::sock_fprog {
                    len: FILTER_LEN as libc::c_ushort,
                    filter: filter.as_ptr().cast_mut(),
                };
                let filtered = libc::syscall(
                    libc::SYS_seccomp,
                    libc::SECCOMP_SET_MODE_FILTER,
                    0,
                    &program,
                );
                if filtered < 0 {
                    return Err(io::Error::last_os_error());
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
    /// The file descriptor store of the service: what busd stored, and didn't remove, with the
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

    /// The file descriptors to pass to busd as it starts: the socket of the socket activation, if
    /// any, and what is stored, with their names.
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
    /// It keeps the file descriptors that busd stores, and closes those that busd removes. It
    /// closes the one that comes with a barrier, which lets busd go on, as it gets to the message
    /// only once it has dealt with the earlier ones.
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

    /// The numbers of the clients that are stored, sorted.
    fn client_ids(&self) -> Vec<usize> {
        let mut ids: Vec<_> = self
            .store
            .iter()
            .filter_map(|(name, _)| client_of_socket(name))
            .collect();
        ids.sort_unstable();

        ids
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

    /// Check that the store holds what a bus hands over with the clients of the numbers `ids`.
    ///
    /// That is its state first, then the socket it listens on if it bound that itself, so that a
    /// store too small for all of the sockets takes those that matter most, and then the file
    /// descriptors of the connections of its clients: the socket of each, followed by those that
    /// came with the messages that busd didn't take, which this leaves out.
    fn assert_stored(&self, ids: &[usize], listener: bool) {
        let names: Vec<_> = self.store.iter().map(|(name, _)| name.as_str()).collect();
        let head: &[&str] = if listener {
            &["busd-state", "busd-listener"]
        } else {
            &["busd-state"]
        };
        let Some((stored_head, clients)) = names.split_at_checked(head.len()) else {
            panic!("busd stored {names:?}, which isn't {head:?} and its clients");
        };
        assert_eq!(stored_head, head);

        for name in clients {
            assert!(
                name.starts_with("busd-client-"),
                "busd stored `{name}` among its clients"
            );
        }
        let mut clients: Vec<_> = clients
            .iter()
            .filter_map(|name| client_of_socket(name))
            .collect();
        clients.sort_unstable();
        let mut expected = ids.to_vec();
        expected.sort_unstable();
        assert_eq!(clients, expected);
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

/// A limit of the open file descriptors of busd.
#[derive(Clone, Copy)]
enum Nofile {
    /// The soft limit, which busd raises to the hard one as it starts.
    Soft(libc::rlim_t),
    /// Both limits, so that busd can't raise it.
    Hard(libc::rlim_t),
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

/// The number of the client whose socket busd stores under `name`, if it stores one under it: the
/// first of the file descriptors of the connection of the client.
fn client_of_socket(name: &str) -> Option<usize> {
    name.strip_prefix("busd-client-")?
        .strip_suffix("-0")?
        .parse()
        .ok()
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

/// A filter of system calls, in the classic BPF of `seccomp(2)`, that has those that tell whether
/// two file descriptors are of the same open file description fail, as on a kernel that lacks
/// them: `fcntl(2)` with `F_DUPFD_QUERY` with `EINVAL`, and `kcmp(2)` with `ENOSYS`.
fn without_fd_queries() -> [libc::sock_filter; FILTER_LEN] {
    let load = |offset| libc::sock_filter {
        code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
        jt: 0,
        jf: 0,
        k: offset,
    };
    // Skips the `skip` instructions after it unless what was loaded is `value`.
    let unless = |value, skip| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt: 0,
        jf: skip,
        k: value,
    };
    let ret = |action| libc::sock_filter {
        code: (libc::BPF_RET | libc::BPF_K) as u16,
        jt: 0,
        jf: 0,
        k: action,
    };
    let fail = |errno: libc::c_int| ret(libc::SECCOMP_RET_ERRNO | errno as u32);

    [
        load(SECCOMP_NR),
        unless(libc::SYS_kcmp as u32, 1),
        fail(libc::ENOSYS),
        unless(libc::SYS_fcntl as u32, 3),
        load(SECCOMP_ARG1),
        unless(F_DUPFD_QUERY, 1),
        fail(libc::EINVAL),
        ret(libc::SECCOMP_RET_ALLOW),
    ]
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
/// A soft limit of open file descriptors that takes the fitting clients and no more: the first
/// file descriptor that systemd passes, the state and the listener, and the spare ones that busd
/// keeps free, besides the clients.
const LOW_NOFILE: libc::rlim_t =
    (FIRST_FD as libc::rlim_t) + 2 + 32 + FITTING_CLIENTS as libc::rlim_t;
const HARD_NOFILE: libc::rlim_t = 64;
const FREE_FDS: libc::rlim_t = 2;
/// How many clients the test hands over, and how many of them fit under `LOW_NOFILE`.
const CLIENTS: usize = 4;
const FITTING_CLIENTS: usize = 2;
/// The first file descriptor that systemd passes, `SD_LISTEN_FDS_START`.
const FIRST_FD: RawFd = 3;
/// How long the test waits for what should come soon, rather than hang if it never does.
const WAIT: Duration = Duration::from_secs(10);
/// The name of the unit of socket activation, which starts like the names of busd's own.
const ACTIVATION: &str = "busd-session.socket";
/// How large a notification the test takes, which is that of systemd.
const MAX_NOTIFICATION: usize = 4096;
/// How many file descriptors the test takes with a notification: busd sends one.
const MAX_FDS: usize = 4;
/// What `kcmp(2)` compares of two file descriptors: their open file descriptions.
const KCMP_FILE: libc::c_long = 0;
/// How many instructions the filter of [`without_fd_queries`] has.
const FILTER_LEN: usize = 8;
/// Where the number of the system call is in what a filter of `seccomp(2)` is given of it, `struct
/// seccomp_data`.
const SECCOMP_NR: u32 = 0;
/// Where the low half of the second argument of the system call is in there, which is the command
/// of `fcntl(2)`.
#[cfg(target_endian = "little")]
const SECCOMP_ARG1: u32 = 24;
#[cfg(target_endian = "big")]
const SECCOMP_ARG1: u32 = 28;
/// The command of `fcntl(2)` that tells whether two file descriptors are of the same open file
/// description, from Linux 6.10 on, `F_LINUX_SPECIFIC_BASE + 3`.
const F_DUPFD_QUERY: u32 = 1024 + 3;
