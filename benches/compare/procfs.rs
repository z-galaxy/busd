//! What /proc tells the runner: of the processes of a run, their descendants, process groups, CPU
//! times, peak memory and tracers, and of the machine, its CPUs and its kernel.

use std::{collections::HashMap, fs};

use rustix::process::Pid;

/// The CPU time, user and system, in clock ticks, that the processes `pids` have used so far, if
/// they are all still there, as a process that has exited is until it is reaped.
///
/// That is the time of each of them, its threads included, and of those that it reaped, which
/// /proc gives as the time of its children. Only their own stats are read, one after the other.
pub fn cpu_ticks(pids: &[Pid]) -> Option<u64> {
    pids.iter()
        .map(|&pid| {
            let fields = stat(pid)?;
            // utime, stime, cutime and cstime are the 14th to 17th fields, the 12th to 15th after
            // the command name.
            let times = fields.get(11..15)?;

            times
                .iter()
                .map(|time| time.parse::<u64>().ok())
                .sum::<Option<u64>>()
        })
        .sum()
}

/// The seconds of `ticks` clock ticks, the unit of the CPU times of /proc/PID/stat, which is
/// usually 10 ms.
pub fn seconds(ticks: u64) -> f64 {
    ticks as f64 / clock_ticks()
}

/// The live processes that the process `pid` started, and those that they started, and so on.
pub fn descendants(pid: Pid) -> Vec<Pid> {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (process, fields) in processes() {
        // The parent's pid is the 4th field, the 2nd after the command name.
        if let Some(parent) = fields.get(1).and_then(|field| parse_pid(field)) {
            children.entry(parent).or_default().push(process);
        }
    }

    let mut found = Vec::new();
    let mut todo = vec![pid];
    while let Some(parent) = todo.pop() {
        for &child in children.get(&parent).into_iter().flatten() {
            found.push(child);
            todo.push(child);
        }
    }

    found
}

/// The processes, zombies included, whose process group is `pgid`.
pub fn group_members(pgid: Pid) -> Vec<Pid> {
    processes()
        // The process group is the 5th field, the 3rd after the command name.
        .filter(|(_, fields)| fields.get(2).and_then(|field| parse_pid(field)) == Some(pgid))
        .map(|(pid, _)| pid)
        .collect()
}

/// The peak resident memory of the process `pid` so far, VmHWM, in KiB, if /proc says.
pub fn peak_rss_kib(pid: Pid) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;

    status_field(&status, "VmHWM:")?.parse().ok()
}

/// Whether every thread of the processes `pids` has a tracer.
pub fn traced(pids: &[Pid]) -> bool {
    pids.iter().all(|pid| {
        let Ok(threads) = fs::read_dir(format!("/proc/{pid}/task")) else {
            return false;
        };

        threads.into_iter().all(|thread| {
            let Ok(thread) = thread else {
                return false;
            };
            let Ok(status) = fs::read_to_string(thread.path().join("status")) else {
                return false;
            };

            status_field(&status, "TracerPid:").is_some_and(|tracer| tracer != "0")
        })
    })
}

/// Whether the runner ignores the signal of number `signal`, as `nohup` has it ignore SIGHUP.
pub fn ignores(signal: i32) -> bool {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    // A mask in hex, with the bit of signal n at n - 1.
    let Some(ignored) = status_field(&status, "SigIgn:") else {
        return false;
    };

    u64::from_str_radix(ignored, 16).is_ok_and(|ignored| ignored >> (signal - 1) & 1 == 1)
}

/// The fields of /proc/PID/stat after the command name, if there is such a process.
pub fn stat(pid: Pid) -> Option<Vec<String>> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name, in parentheses, can hold anything, spaces and parentheses included: the
    // fields are counted from the last parenthesis.
    let (_, fields) = stat.rsplit_once(')')?;

    Some(fields.split_whitespace().map(str::to_owned).collect())
}

/// The model of the CPUs, as /proc/cpuinfo names it, and how many of them there are.
pub fn cpus() -> (String, usize) {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut model = None;
    let mut count = 0;
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "processor" => count += 1,
            "model name" if model.is_none() => model = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    // Not every architecture names its CPUs there.
    let model = model.unwrap_or_else(|| std::env::consts::ARCH.to_owned());

    (model, count.max(1))
}

/// The release of the kernel, such as `6.18.44`.
pub fn kernel() -> String {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();

    release.trim().to_owned()
}

/// Every process, with the fields of its stat after the command name.
fn processes() -> impl Iterator<Item = (Pid, Vec<String>)> {
    let entries = fs::read_dir("/proc").into_iter().flatten().flatten();

    entries.filter_map(|entry| {
        let pid = parse_pid(entry.file_name().to_str()?)?;

        Some((pid, stat(pid)?))
    })
}

/// The pid that `text` is, if it is one.
fn parse_pid(text: &str) -> Option<Pid> {
    Pid::from_raw(text.parse().ok()?)
}

/// The value of the field `name`, such as `VmHWM:`, of a /proc/PID/status: the first word after
/// the name, without its unit.
fn status_field<'s>(status: &'s str, name: &str) -> Option<&'s str> {
    let line = status.lines().find(|line| line.starts_with(name))?;

    line[name.len()..].split_whitespace().next()
}

/// How many clock ticks there are in a second.
fn clock_ticks() -> f64 {
    rustix::param::clock_ticks_per_second() as f64
}
