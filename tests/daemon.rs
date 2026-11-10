//! The busd binary, run the way that a service manager or a script runs it.

#[cfg(target_os = "linux")]
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::{
    env::temp_dir,
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

use ntest::timeout;
use rand::{
    distr::{Alphanumeric, SampleString},
    rng,
};
use rustix::process::{kill_process, Pid, Signal};
use zbus::{connection, fdo, Address, Connection};
use zruntime::SharedRuntime;

/// On SIGTERM, which service managers and `kill` send to stop a service, busd stops as it does on
/// SIGINT: it removes the file of its socket, and exits successfully.
#[test]
#[timeout(30000)]
fn sigterm() {
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let mut busd = Busd::spawn(&format!("unix:path={}", path.display()), None);
    // Once busd prints its address, it listens.
    busd.address();
    assert!(path.exists(), "busd doesn't listen on {}", path.display());

    let status = busd.stop(Signal::TERM);
    assert!(status.success(), "busd exited with {status}");
    assert!(!path.exists(), "busd left {} behind", path.display());
}

/// What `--print-address` prints to stdout is the address alone, however much busd logs, so that
/// a script can read it from there.
#[test]
#[timeout(30000)]
fn print_address() {
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let mut busd = Busd::spawn(&format!("unix:path={}", path.display()), Some("trace"));
    busd.address();

    busd.stop(Signal::TERM);
    let mut rest = String::new();
    busd.stdout.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "", "busd printed more than its address");
}

/// busd logs where it listens without being asked to, and without the escape codes of colors, as
/// what it logs to is a pipe rather than a terminal, as a journal or a file would be.
#[cfg(feature = "tracing-subscriber")]
#[test]
#[timeout(30000)]
fn logs_plainly_by_default() {
    let path = temp_dir().join(Alphanumeric.sample_string(&mut rng(), 10));
    let mut command = Busd::command(&format!("unix:path={}", path.display()), None);
    command.stderr(Stdio::piped());
    let mut busd = Busd::start(command);
    busd.address();

    let status = busd.stop(Signal::TERM);
    assert!(status.success(), "busd exited with {status}");
    let mut logs = String::new();
    let mut stderr = busd.child.stderr.take().unwrap();
    stderr.read_to_string(&mut logs).unwrap();
    assert!(logs.contains("Listening on UNIX socket file"), "{logs}");
    assert!(!logs.contains('\x1b'), "{logs:?}");
}

/// Under systemd, busd tells the service manager once it listens, for a service of `Type=notify` to
/// be ready then.
#[cfg(target_os = "linux")]
#[test]
#[timeout(30000)]
fn notifies_readiness() {
    let path = temp_dir().join(Alphanumeric.sample_string(&mut rng(), 10));
    let notify_socket = temp_dir().join(Alphanumeric.sample_string(&mut rng(), 10));
    let manager = UnixDatagram::bind(&notify_socket).unwrap();
    manager
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut command = Busd::command(&format!("unix:path={}", path.display()), None);
    command.env("NOTIFY_SOCKET", &notify_socket);
    let mut busd = Busd::start(command);

    let mut state = [0; 4096];
    let len = manager.recv(&mut state).unwrap();
    assert_eq!(&state[..len], b"READY=1");
    UnixStream::connect(&path).expect("busd doesn't listen once it's ready");

    let status = busd.stop(Signal::TERM);
    assert!(status.success(), "busd exited with {status}");
    fs::remove_file(&notify_socket).unwrap();
}

/// A `$NOTIFY_SOCKET` that no one listens on doesn't keep busd from starting and serving: it only
/// can't tell systemd that it's ready.
#[test]
#[timeout(30000)]
fn notify_socket_nobody_listens_on() {
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let path = temp_dir().join(s);
    let address = format!("unix:path={}", path.display());
    let mut command = Busd::command(&address, None);
    command.env("NOTIFY_SOCKET", temp_dir().join("busd-nobody-listens-here"));
    let mut busd = Busd::start(command);
    let address = busd.address();

    let connection = zruntime::block_on(async {
        let connecting = connection::Builder::address(address).build();
        SharedRuntime::current()
            .unwrap()
            .timeout(Duration::from_secs(10), connecting)
            .await
            .expect("Timed out")
    })
    .unwrap();
    assert!(connection.unique_name().is_some());

    let status = busd.stop(Signal::TERM);
    assert!(status.success(), "busd exited with {status}");
}

/// On `ReloadConfig`, busd reads its configuration again, and goes by the policy of it from then
/// on: it refuses a name to a client that it let own one, once the configuration lets no one own
/// names. A configuration that it can't read leaves it as it is.
#[test]
#[timeout(30000)]
fn reloads_its_configuration() {
    let s = Alphanumeric.sample_string(&mut rng(), 10);
    let address = format!("unix:path={}", temp_dir().join(&s).display());
    let config = temp_dir().join(format!("{s}.conf"));
    write_config(&config, r#"<allow own="*"/>"#);
    let mut busd = Busd::start(Busd::command_with(config.to_str().unwrap(), &address, None));
    let address = busd.address();

    zruntime::block_on(async {
        let client = connection::Builder::address(address).build().await.unwrap();
        request_name(&client, "org.busd.Daemon.Before")
            .await
            .unwrap();

        write_config(&config, r#"<deny own="*"/>"#);
        call_bus(&client, "ReloadConfig", &()).await.unwrap();
        let refused = request_name(&client, "org.busd.Daemon.After").await;
        assert!(
            matches!(refused, Err(fdo::Error::AccessDenied(_))),
            "busd goes by the configuration that it started with: {refused:?}"
        );

        fs::write(&config, "<busconfig>").unwrap();
        let failed = call_bus(&client, "ReloadConfig", &()).await;
        let ignored = "Config invalid. Reload ignored.";
        assert!(
            matches!(&failed, Err(fdo::Error::Failed(why)) if why == ignored),
            "busd took a configuration that it can't read: {failed:?}"
        );
        let refused = request_name(&client, "org.busd.Daemon.Still").await;
        assert!(
            matches!(refused, Err(fdo::Error::AccessDenied(_))),
            "busd let go of its policy: {refused:?}"
        );
    });

    fs::remove_file(&config).unwrap();
    let status = busd.stop(Signal::TERM);
    assert!(status.success(), "busd exited with {status}");
}

/// Write a configuration to `path` whose policy lets everyone send and receive everything, and
/// has the rule `own` for the names that they ask for.
fn write_config(path: &Path, own: &str) {
    let config = format!(
        r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
        "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
        <busconfig>
            <policy context="default">
                <allow send_destination="*"/>
                <allow eavesdrop="true"/>
                {own}
            </policy>
        </busconfig>"#
    );

    fs::write(path, config).unwrap();
}

/// Have `conn` ask the bus for the well-known `name`.
async fn request_name(conn: &Connection, name: &str) -> fdo::Result<()> {
    call_bus(conn, "RequestName", &(name, 0u32)).await
}

/// Have `conn` call `method` of the bus, with `body`, and wait for the reply.
async fn call_bus<B>(conn: &Connection, method: &str, body: &B) -> fdo::Result<()>
where
    B: serde::Serialize + zbus::wire::DynamicType,
{
    let bus = "org.freedesktop.DBus";
    conn.call_method(Some(bus), "/org/freedesktop/DBus", Some(bus), method, body)
        .await?;

    Ok(())
}

/// A busd process, killed if it's still running when dropped.
struct Busd {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Busd {
    /// Starts busd, listening on `address`, and logging what `log` says, as `RUST_LOG` would.
    ///
    /// It reads the session configuration of the tests, rather than the one of the system. Without
    /// `log`, it logs as it does without `RUST_LOG`, whatever the `RUST_LOG` of the tests.
    fn spawn(address: &str, log: Option<&str>) -> Self {
        Self::start(Self::command(address, log))
    }

    /// The command that [`Busd::spawn`] runs.
    ///
    /// busd runs as it would without systemd, whatever the environment of the tests tells of it.
    fn command(address: &str, log: Option<&str>) -> Command {
        Self::command_with(CONFIG, address, log)
    }

    /// The command that [`Busd::spawn`] runs, with the configuration file `config` rather than the
    /// session configuration of the tests.
    fn command_with(config: &str, address: &str, log: Option<&str>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_busd"));
        command
            .args(["--config", config, "--address", address, "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match log {
            Some(log) => command.env("RUST_LOG", log),
            None => command.env_remove("RUST_LOG"),
        };
        for var in SYSTEMD_VARS {
            command.env_remove(var);
        }

        command
    }

    /// Starts busd with `command`, as [`Busd::command`] makes it.
    fn start(mut command: Command) -> Self {
        let mut child = command.spawn().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Self { child, stdout }
    }

    /// Waits for busd to listen, and returns the address that it prints for its clients.
    fn address(&mut self) -> Address {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let line = line.trim_end();

        Address::from_str(line)
            .unwrap_or_else(|e| panic!("busd printed `{line}` rather than an address: {e}"))
    }

    /// Sends `signal` to busd, and waits a few seconds at most for it to exit.
    ///
    /// A busd that doesn't exit so fails the test, which then kills it, rather than hang the test
    /// until it times out, which would leave busd running.
    fn stop(&mut self, signal: Signal) -> ExitStatus {
        kill_process(Pid::from_child(&self.child), signal).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "busd didn't exit on {signal:?}");
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

const CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/session.conf");

/// What systemd tells a service of in its environment: the sockets that it passes, the socket to
/// send notifications to, and how many file descriptors it may store.
const SYSTEMD_VARS: [&str; 6] = [
    "LISTEN_PID",
    "LISTEN_PIDFDID",
    "LISTEN_FDS",
    "LISTEN_FDNAMES",
    "NOTIFY_SOCKET",
    "FDSTORE",
];
