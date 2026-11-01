//! The busd binary, run the way that a service manager or a script runs it.

use std::{
    env::temp_dir,
    io::{BufRead, BufReader},
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
    let mut busd = Busd::spawn(&format!("unix:path={}", path.display()));
    // Once busd prints its address, it listens.
    busd.address();
    assert!(path.exists(), "busd doesn't listen on {}", path.display());

    let status = busd.stop(Signal::TERM);
    assert!(status.success(), "busd exited with {status}");
    assert!(!path.exists(), "busd left {} behind", path.display());
}

/// A busd process, killed if it's still running when dropped.
struct Busd {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl Busd {
    /// Starts busd, listening on `address`.
    ///
    /// It reads the session configuration of the tests, rather than the one of the system, and
    /// logs its errors alone, as it does without `RUST_LOG`, whatever the `RUST_LOG` of the tests.
    fn spawn(address: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_busd"))
            .args(["--config", CONFIG, "--address", address, "--print-address"])
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        Self { child, stdout }
    }

    /// Waits for busd to listen, and returns the address that it prints for its clients.
    fn address(&mut self) -> Address {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();

        Address::from_str(line.trim_end()).unwrap()
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
