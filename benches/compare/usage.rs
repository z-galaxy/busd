//! The resource usage of the processes that the runner has reaped, which is how it measures the CPU
//! time and the context switches of each client and of each broker over its whole life.
//!
//! getrusage(RUSAGE_CHILDREN) adds up the usage of every child that the runner has reaped, and of
//! the descendants that those reaped in turn. The difference between two readings is therefore the
//! usage of whatever the runner reaped in between: a client, or a broker and the processes that it
//! left behind. That only holds if nothing else is reaped in that window: the version probes, git
//! and cargo all run before the first run, strace is reaped before the window of the broker opens,
//! and the thread that listens for signals starts no process.

use std::{io, mem::MaybeUninit};

/// What the processes that the runner reaped have used, as getrusage(RUSAGE_CHILDREN) counts it.
#[derive(Clone, Copy)]
pub struct Usage {
    /// The user CPU time, in seconds.
    pub user_s: f64,
    /// The system CPU time, in seconds.
    pub sys_s: f64,
    /// The voluntary context switches: how many times they gave up the CPU to wait.
    pub vcsw: libc::c_long,
}

impl Usage {
    /// What the processes reaped so far have used.
    pub fn of_children() -> io::Result<Self> {
        let mut usage = MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: `usage` has room for a whole `rusage`, which is all that getrusage writes to.
        if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, usage.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: getrusage succeeded, and so filled in the whole `rusage`.
        let usage = unsafe { usage.assume_init() };

        Ok(Self {
            user_s: seconds(usage.ru_utime),
            sys_s: seconds(usage.ru_stime),
            vcsw: usage.ru_nvcsw,
        })
    }

    /// What was used from the reading `earlier` until this one.
    pub fn since(self, earlier: Self) -> Self {
        Self {
            user_s: self.user_s - earlier.user_s,
            sys_s: self.sys_s - earlier.sys_s,
            vcsw: self.vcsw - earlier.vcsw,
        }
    }

    /// The CPU time, user and system, in seconds.
    pub fn cpu_s(self) -> f64 {
        self.user_s + self.sys_s
    }
}

/// The seconds of a `timeval`.
fn seconds(time: libc::timeval) -> f64 {
    time.tv_sec as f64 + time.tv_usec as f64 / 1e6
}
