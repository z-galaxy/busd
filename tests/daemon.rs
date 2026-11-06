//! The busd binary, run the way that a service manager or a script runs it.

use std::{
    env::temp_dir,
    io::{BufRead, BufReader, Read},
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
use zbus::Address;

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

/// A busd process, killed if it's still running when dropped.
struct Busd {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Busd {
    /// Starts busd, listening on `address`, and logging what `log` says, as `RUST_LOG` would.
    ///
    /// It reads the session configuration of the tests, rather than the one of the system. Without
    /// `log`, it logs its errors alone, as it does without `RUST_LOG`, whatever the `RUST_LOG` of
    /// the tests.
    ///
    /// busd runs as it would without systemd, whatever the environment of the tests tells of it.
    fn spawn(address: &str, log: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_busd"));
        command
            .args(["--config", CONFIG, "--address", address, "--print-address"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        match log {
            Some(log) => command.env("RUST_LOG", log),
            None => command.env_remove("RUST_LOG"),
        };
        for var in SYSTEMD_VARS {
            command.env_remove(var);
        }
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

/// What systemd tells a service of in its environment: the sockets that it passes.
const SYSTEMD_VARS: [&str; 4] = [
    "LISTEN_PID",
    "LISTEN_PIDFDID",
    "LISTEN_FDS",
    "LISTEN_FDNAMES",
];
