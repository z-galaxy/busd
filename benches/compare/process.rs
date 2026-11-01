//! A broker as it runs: started on the socket of a run, waited for until it answers, measured, and
//! stopped along with every process that it started.
//!
//! A broker may run as more than one process: dbus-broker's launcher starts the broker proper as a
//! process of its own. What the runner measures of a broker covers all of its processes. The runner
//! is their subreaper, so that the processes that a broker leaves behind as it stops come to it, to
//! be reaped and counted, rather than to init. A runner that is killed before it can stop the
//! broker, as by SIGKILL, takes the broker with it.

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        net::UnixStream,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::{
    io::Errno,
    process::{
        getpid, getppid, getuid, kill_process, set_parent_process_death_signal, waitpid, Pid,
        Signal, WaitOptions,
    },
};

use crate::{calls::Result, interrupt, output::note, procfs};

/// A broker that runs, as a child of the runner, which reaps it and what it leaves behind once it
/// is stopped or dropped.
pub struct BrokerProcess {
    /// The process that the runner started.
    child: Child,
    /// Its pid, which is also the id of its process group.
    pid: Pid,
    /// Its processes, its own and those that it started, as found once it was ready, and again
    /// whenever one of them is gone.
    processes: Vec<Pid>,
    /// The file that it writes its output to.
    log: PathBuf,
    /// Its exit status, once it has been reaped.
    status: Option<ExitStatus>,
    /// Whether it and every process that it left behind have been reaped.
    gone: bool,
}

impl BrokerProcess {
    /// Starts the broker with the arguments `argv`, the program first, and the environment
    /// variables `env`, writing its output to `log`.
    pub fn start(argv: &[String], env: &[(String, String)], log: &Path) -> Result<Self> {
        let Some((program, args)) = argv.split_first() else {
            return Err("the command of the broker names no program".into());
        };
        let output = File::create(log)?;
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            // A process group of its own, so that the SIGINT of Ctrl-C at the terminal reaches the
            // runner alone, which then stops the broker, as it stops every broker.
            .process_group(0);
        die_with_runner(&mut command);
        let child = command
            .spawn()
            .map_err(|e| format!("cannot start {program}: {e}"))?;
        let pid = Pid::from_child(&child);

        Ok(Self {
            pid,
            processes: vec![pid],
            child,
            log: log.to_owned(),
            status: None,
            gone: false,
        })
    }

    /// Waits until the broker answers a D-Bus authentication on its socket at `socket`, or fails
    /// after `READY_TIMEOUT`, and then finds its processes, which `processes` and `cpu_ticks` go
    /// by.
    ///
    /// A broker that starts on its first connection, as dbus-broker does under socket activation,
    /// only answers once it runs.
    pub fn wait_ready(&mut self, socket: &Path) -> Result<()> {
        // The EXTERNAL mechanism, with the runner's uid in hex-encoded ASCII, as every D-Bus client
        // on a unix-domain socket starts. A broker that serves the connection answers it with OK.
        let uid = getuid().as_raw().to_string();
        let uid: String = uid.bytes().map(|byte| format!("{byte:02x}")).collect();
        let auth = format!("\0AUTH EXTERNAL {uid}\r\n");
        let deadline = Instant::now() + READY_TIMEOUT;

        loop {
            if let Some(status) = self.exited()? {
                let status = exit_code(status);
                let message = format!("the broker exited (status {status}) before it was ready");
                return Err(with_log(&message, &self.log).into());
            }
            if interrupt::interrupted() {
                return Err("interrupted".into());
            }
            if authenticates(socket, auth.as_bytes(), deadline) {
                self.processes = self.find_processes();

                return Ok(());
            }
            if Instant::now() > deadline {
                let secs = READY_TIMEOUT.as_secs();
                let message = format!("the broker was not ready after {secs}s");
                return Err(with_log(&message, &self.log).into());
            }
            thread::sleep(TICK);
        }
    }

    /// The broker's processes, its own and those that it started, as found once it was ready, or
    /// since, by `cpu_ticks`.
    pub fn processes(&self) -> &[Pid] {
        &self.processes
    }

    /// The CPU time, user and system, in clock ticks, that the broker's processes have used so far,
    /// if /proc tells.
    ///
    /// It reads the processes found once the broker was ready, and nothing else, so that it takes
    /// as little time as can be, unless one of them is gone, when they are found anew: the time of
    /// one that is gone went to the one of them that reaped it.
    pub fn cpu_ticks(&mut self) -> Option<u64> {
        if let Some(ticks) = procfs::cpu_ticks(&self.processes) {
            return Some(ticks);
        }
        self.processes = self.find_processes();

        procfs::cpu_ticks(&self.processes)
    }

    /// The broker's peak resident memory so far, VmHWM, in KiB, summed over its processes, if /proc
    /// tells it for the broker's own.
    pub fn peak_rss_kib(&self) -> Option<u64> {
        let own = procfs::peak_rss_kib(self.pid)?;
        let descendants = procfs::descendants(self.pid).into_iter();

        Some(own + descendants.filter_map(procfs::peak_rss_kib).sum::<u64>())
    }

    /// Stops the broker with SIGINT, or with SIGKILL if it has not exited within `STOP_TIMEOUT`,
    /// reaps it and the processes that it leaves behind, and returns its exit status as the results
    /// note it.
    pub fn stop(&mut self) -> Result<i32> {
        let status = self.stop_with(&[Signal::INT, Signal::KILL])?;

        Ok(exit_code(status))
    }

    /// Sends `signals` to the broker, one at a time, each `STOP_TIMEOUT` after the one before,
    /// until it exits, then reaps it and the processes that it leaves behind, and returns its exit
    /// status.
    fn stop_with(&mut self, signals: &[Signal]) -> Result<ExitStatus> {
        // Found while the broker still runs: once it has exited, those that are left are the
        // runner's, as their subreaper, and no longer its descendants.
        let orphans = match self.exited()? {
            Some(_) => Vec::new(),
            None => procfs::descendants(self.pid),
        };
        for &signal in signals {
            if self.exited()?.is_some() {
                break;
            }
            kill_process(self.pid, signal)?;
            let deadline = Instant::now() + STOP_TIMEOUT;
            while self.exited()?.is_none() && Instant::now() < deadline {
                thread::sleep(TICK);
            }
        }
        let Some(status) = self.exited()? else {
            return Err(format!("the broker (pid {}) would not exit", self.pid).into());
        };

        self.reap(orphans)?;
        self.gone = true;

        Ok(status)
    }

    /// The broker's exit status, if it has exited, in which case it is reaped.
    fn exited(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }

        Ok(self.status)
    }

    /// Reaps the processes that the broker, which has exited, left behind, killing those that do
    /// not go within `STOP_TIMEOUT`.
    ///
    /// Those are `orphans`, its descendants as found while it still ran, and whatever is left in
    /// its process group, the group of its own that it was started in. A broker that exited before
    /// it was stopped has no descendants left to find, as the runner adopted them, but they are
    /// still in its process group.
    fn reap(&self, mut orphans: Vec<Pid>) -> Result<()> {
        let deadline = Instant::now() + STOP_TIMEOUT;
        loop {
            // A process that is gone is dropped for good, so that a new one that gets its pid
            // later is left alone.
            orphans.retain(|&pid| procfs::stat(pid).is_some());
            let mut left = orphans.clone();
            let members = procfs::group_members(self.pid).into_iter();
            left.extend(members.filter(|pid| !orphans.contains(pid)));
            if left.is_empty() {
                return Ok(());
            }
            if Instant::now() > deadline + STOP_TIMEOUT {
                let left: Vec<String> = left.iter().map(ToString::to_string).collect();
                let left = left.join(", ");
                return Err(format!("processes {left} of the broker would not go").into());
            }

            for pid in left {
                // A process that has not exited is reaped by the next round. One that is not the
                // runner's child is reaped by its parent, or comes to the runner, as their
                // subreaper, once that parent is gone.
                if let Ok(Some(_)) = waitpid(Some(pid), WaitOptions::NOHANG) {
                    orphans.retain(|&orphan| orphan != pid);
                } else if Instant::now() > deadline {
                    let _ = kill_process(pid, Signal::KILL);
                }
            }
            thread::sleep(TICK);
        }
    }

    /// The broker's processes that are alive: its own, and those that it started.
    fn find_processes(&self) -> Vec<Pid> {
        let mut processes = vec![self.pid];
        processes.extend(procfs::descendants(self.pid));

        processes
    }
}

impl Drop for BrokerProcess {
    /// Kills the broker, and reaps it and what it leaves behind, if it was not stopped, so that a
    /// run that failed or was interrupted leaves nothing running.
    fn drop(&mut self) {
        if self.gone {
            return;
        }
        if let Err(e) = self.stop_with(&[Signal::KILL]) {
            note!("warning: {e}");
        }
    }
}

/// The exit status of a process as the results note it: its exit code, or minus the number of the
/// signal that ended it.
pub fn exit_code(status: ExitStatus) -> i32 {
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(signal)) => -signal,
        (None, None) => i32::MIN,
    }
}

/// `message`, followed by the end of the log at `path`, if there is anything in it, for the message
/// of a failure.
pub fn with_log(message: &str, path: &Path) -> String {
    let log = read_tail(path);
    if log.is_empty() {
        return message.to_owned();
    }

    format!("{message}:\n{log}")
}

/// The end of the file at `path`, such as a log, for the message of a failure: empty if it cannot
/// be read.
pub fn read_tail(path: &Path) -> String {
    let text = fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&text);
    let text = text.trim();
    let mut start = text.len().saturating_sub(TAIL);
    while !text.is_char_boundary(start) {
        start += 1;
    }

    text[start..].to_owned()
}

/// Has the process that `command` starts get SIGKILL once the runner is gone, even if the runner
/// had no chance to stop it, as when the runner gets SIGKILL itself, so that it does not outlive
/// the runner.
///
/// The kernel sends the signal as the thread that started the process ends, rather than the whole
/// runner: the brokers and strace are started from the main thread, which lives as long as the
/// runner does.
pub fn die_with_runner(command: &mut Command) {
    let runner = getpid();
    let watch = move || -> io::Result<()> {
        set_parent_process_death_signal(Some(Signal::KILL))?;
        // The runner may have gone before the signal was set, in which case none comes, and the
        // process has another parent by now: it ends there, before it runs anything.
        if getppid() != Some(runner) {
            return Err(Errno::SRCH.into());
        }

        Ok(())
    };
    // SAFETY: the closure runs in the child, between fork and exec, where only what is
    // async-signal-safe may run, as the other threads of the runner, which may hold locks, are not
    // there. It only makes two system calls, prctl and getppid, which take no lock, and compares
    // two pids: it allocates nothing, as an `io::Error` made of an errno needs no allocation.
    unsafe {
        command.pre_exec(watch);
    }
}

/// How long the runner waits between two looks at something that it waits for.
pub const TICK: Duration = Duration::from_millis(10);

/// How long a broker has to answer on its socket, once started.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a broker has to exit after each signal that stops it, and the processes that it leaves
/// behind to go before they are killed.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// How many bytes of the end of a log the message of a failure shows.
const TAIL: usize = 1000;

/// Whether the broker on `socket` answers the authentication `auth` with OK, trying no longer than
/// until `deadline`.
fn authenticates(socket: &Path, auth: &[u8], deadline: Instant) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    let timeout = deadline.saturating_duration_since(Instant::now()).max(TICK);
    if stream.set_read_timeout(Some(timeout)).is_err()
        || stream.set_write_timeout(Some(timeout)).is_err()
        || stream.write_all(auth).is_err()
    {
        return false;
    }

    let mut reply = [0; 256];
    stream
        .read(&mut reply)
        .is_ok_and(|read| reply[..read].starts_with(b"OK "))
}
