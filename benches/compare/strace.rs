//! Diagnosis: `strace -c -f` attached to a broker for a measured run, which leaves a summary of the
//! system calls of the broker once it is detached.
//!
//! Tracing slows the broker down a lot, so the timings and the CPU times of the runs that it traces
//! are distorted: it tells which system calls dominate, not how the brokers compare.

use std::{
    fs::{self, File},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use rustix::process::{kill_process, Pid, Signal};

use crate::{
    calls::Result,
    interrupt,
    process::{die_with_runner, exit_code, with_log, TICK},
    procfs,
};

/// strace, attached to the processes of a broker.
pub struct Tracer {
    /// The strace process.
    child: Child,
    /// Its pid.
    pid: Pid,
    /// Where it writes its summary.
    summary: PathBuf,
    /// Where it writes its own output.
    log: PathBuf,
}

impl Tracer {
    /// Attaches strace to `pids`, the processes of a broker, to write its summary to `summary` once
    /// it is detached, and its own output to `log`, and returns once it traces every thread of
    /// them.
    pub fn attach(pids: &[Pid], summary: &Path, log: &Path) -> Result<Self> {
        let output = File::create(log)?;
        let mut command = Command::new("strace");
        command.args(["-c", "-f"]);
        for pid in pids {
            command.arg("-p").arg(pid.to_string());
        }
        command
            .arg("-o")
            .arg(summary)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            // A process group of its own, as the broker has, so that Ctrl-C at the terminal does
            // not detach it before the runner does.
            .process_group(0);
        // Like the broker, it does not outlive the runner.
        die_with_runner(&mut command);
        let child = command
            .spawn()
            .map_err(|e| format!("cannot start strace: {e}"))?;
        let mut tracer = Self {
            pid: Pid::from_child(&child),
            child,
            summary: summary.to_owned(),
            log: log.to_owned(),
        };

        // The client must not start before strace traces the broker, or the summary would miss
        // the start of the run.
        let deadline = Instant::now() + ATTACH_TIMEOUT;
        while !procfs::traced(pids) {
            if let Some(status) = tracer.child.try_wait()? {
                let status = exit_code(status);
                let message = format!("strace exited (status {status}) instead of attaching");
                return Err(with_log(&message, log).into());
            }
            if interrupt::interrupted() {
                return Err("interrupted".into());
            }
            if Instant::now() > deadline {
                let secs = ATTACH_TIMEOUT.as_secs();
                let message = format!("strace did not attach within {secs}s");
                return Err(with_log(&message, log).into());
            }
            thread::sleep(TICK);
        }

        Ok(tracer)
    }

    /// Detaches strace, which writes its summary as it goes, and checks that it wrote one.
    pub fn stop(mut self) -> Result<()> {
        self.detach();
        let written = fs::metadata(&self.summary).is_ok_and(|summary| summary.len() > 0);
        if !written {
            return Err(with_log("strace left no summary", &self.log).into());
        }

        Ok(())
    }

    /// Has strace detach, with SIGINT, and waits until it has exited, for no longer than
    /// `EXIT_TIMEOUT`, after which it is killed.
    fn detach(&mut self) {
        // Reaped already, as `stop` reaps it before it is dropped.
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        let _ = kill_process(self.pid, Signal::INT);
        let deadline = Instant::now() + EXIT_TIMEOUT;
        while matches!(self.child.try_wait(), Ok(None)) {
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return;
            }
            thread::sleep(TICK);
        }
    }
}

impl Drop for Tracer {
    /// Detaches strace from the broker, if that was not done, as a run that fails leaves no tracer
    /// behind.
    fn drop(&mut self) {
        self.detach();
    }
}

/// How long strace has to attach to the threads of a broker.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// How long strace has to write its summary and exit once it is told to detach.
const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
