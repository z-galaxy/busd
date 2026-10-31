//! The runs of the comparison: round after round of each workload, a run of every broker in each
//! round, in an order shuffled for each round, each run with a broker of its own, and what each run
//! measured.

use std::{
    collections::HashSet,
    env,
    fs::{self, DirBuilder, File},
    io::{self, BufRead, BufReader},
    os::unix::{fs::DirBuilderExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{ChildStdout, Command, ExitCode, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use fastrand::Rng;
use rustix::process::{
    getpid, kill_process, set_child_subreaper, waitid, Pid, Signal, WaitId, WaitIdOptions,
};
use serde::{ser::SerializeMap, Serialize, Serializer};
use serde_json::Value;

use crate::{
    brokers::Broker,
    calls::Result,
    environment::Timestamp,
    interrupt,
    output::{self, note},
    process::{exit_code, read_tail, with_log, BrokerProcess},
    procfs,
    protocol::PhaseLine,
    report,
    setup::Setup,
    strace::Tracer,
    summary,
    usage::Usage,
    workload::Workload,
    RunArgs,
};

/// Runs the comparison that `args` asks for, writes its results, and prints its summary.
///
/// The exit status is 130 if the comparison was interrupted, 1 if a run failed, and 0 otherwise.
pub fn compare(args: &RunArgs) -> Result<ExitCode> {
    // The processes that a broker leaves behind as it stops come to the runner, rather than to
    // init, to be reaped and counted with the broker.
    set_child_subreaper(Some(getpid()))?;
    let setup = Setup::prepare(args)?;
    interrupt::listen()?;

    let mut reports = Vec::new();
    let mut failed = false;
    for &workload in &setup.workloads {
        let name = workload.name();
        let results = run_workload(&setup, workload);
        // A report that cannot be written fails its workload alone: the other workloads still
        // run, and the summary has the reports that were written.
        match report::write(&setup, &results) {
            Ok(report) => reports.push(report),
            Err(e) => {
                note!("cannot write the results of {name}: {e}");
                failed = true;
            }
        }

        // Told once the report is written, so that its files are there first.
        let failures = results.failures.len();
        if failures > 0 {
            note!("{failures} run(s) of {name} failed (see above): the tables leave them out");
            failed = true;
        }
        if interrupt::interrupted() {
            note!("interrupted: the results of {name} have the runs that finished");
            break;
        }
    }

    let summary = summary::summary(&setup, &reports);
    let path = setup.out.join("summary.md");
    if let Err(e) = fs::write(&path, &summary) {
        note!("cannot write {}: {e}", path.display());
        failed = true;
    }
    output::print(&summary);
    note!("The results are in {}", setup.out.display());

    let status = match (interrupt::interrupted(), failed) {
        (true, _) => 130,
        (false, true) => 1,
        (false, false) => 0,
    };

    Ok(ExitCode::from(status))
}

/// The runs of a workload.
pub struct Results {
    /// The workload.
    pub workload: Workload,
    /// When its first run started.
    pub started: Timestamp,
    /// The runs that finished, the warm-up runs among them.
    pub runs: Vec<Run>,
    /// The runs that failed.
    pub failures: Vec<Failure>,
}

/// What a run measured, as the results file has it: a flat record of fields.
pub struct Run {
    /// The label of the broker.
    pub label: String,
    /// The round, counted from 0, the warm-up rounds included.
    pub round: u32,
    /// Whether the round is one of warm-up.
    pub warmup: bool,
    /// What the client measured, by name, in the order of its phases.
    metrics: Vec<(String, f64)>,
    /// The CPU time that the client and the broker used in each phase, in seconds, as
    /// `<phase>_client_cpu_s` and `<phase>_broker_cpu_s`, if /proc told.
    phase_cpu: Vec<(String, Option<f64>)>,
    /// The wall time of the whole client process, its start-up included, in seconds.
    client_wall_s: f64,
    /// What the client used.
    client: Usage,
    /// What the broker used, all of its processes together.
    broker: Usage,
    /// The peak resident memory of the broker, summed over its processes, in KiB, if /proc told.
    broker_maxrss_kib: Option<u64>,
    /// The exit status of the broker, as `process::exit_code` gives it.
    broker_exit: i32,
}

impl Run {
    /// The fields of the run after its label, its round and whether it warmed up, by name, in the
    /// order of the results file.
    pub fn fields(&self) -> Vec<(&str, Value)> {
        let metrics = self
            .metrics
            .iter()
            .map(|(name, value)| (name, Value::from(*value)));
        let phase_cpu = self
            .phase_cpu
            .iter()
            .map(|(name, value)| (name, Value::from(*value)));
        let mut fields: Vec<(&str, Value)> = metrics
            .chain(phase_cpu)
            .map(|(name, value)| (name.as_str(), value))
            .collect();
        fields.extend([
            ("client_wall_s", self.client_wall_s.into()),
            ("client_cpu_s", self.client.cpu_s().into()),
            ("client_vcsw", self.client.vcsw.into()),
            ("broker_user_s", self.broker.user_s.into()),
            ("broker_sys_s", self.broker.sys_s.into()),
            ("broker_cpu_s", self.broker.cpu_s().into()),
            ("broker_vcsw", self.broker.vcsw.into()),
            ("broker_maxrss_kib", self.broker_maxrss_kib.into()),
            ("broker_exit", self.broker_exit.into()),
        ]);

        fields
    }

    /// The run of `broker` in round `round`, from what it measured of the client and of the
    /// broker, unless two of its fields would have the same name.
    fn new(
        broker: &Broker,
        round: u32,
        warmup: bool,
        client: ClientRun,
        broker_usage: BrokerUsage,
    ) -> Result<Self> {
        let run = Self {
            label: broker.label.clone(),
            round,
            warmup,
            metrics: client.metrics,
            phase_cpu: client.phase_cpu,
            client_wall_s: client.wall_s,
            client: client.usage,
            broker: broker_usage.usage,
            broker_maxrss_kib: broker_usage.maxrss_kib,
            broker_exit: broker_usage.exit,
        };

        let twice = {
            let mut names = HashSet::from(["label", "round", "warmup"]);
            let fields = run.fields().into_iter();
            fields
                .map(|(name, _)| name)
                .find(|name| !names.insert(name))
                .map(str::to_owned)
        };
        if let Some(name) = twice {
            return Err(format!("two values of the run are named {name}").into());
        }

        Ok(run)
    }

    /// The line that tells how the run went, after its position: the medians and the 99th
    /// percentiles of the latencies that the client measured, if it measured any, or else its
    /// durations, and what the broker cost.
    fn progress(&self) -> String {
        let latencies = self.metrics.iter().filter_map(|(name, p50)| {
            let scenario = name.strip_suffix("_p50_us")?;
            let p99 = self.metric(&format!("{scenario}_p99_us"))?;

            Some(format!("{scenario} {p50:.1}/{p99:.1}us"))
        });
        let mut parts: Vec<String> = latencies.collect();
        if parts.is_empty() {
            let durations = self.metrics.iter().filter_map(|(name, seconds)| {
                let name = name.strip_suffix("_s")?;

                Some(format!("{name} {seconds:.3}s"))
            });
            parts.extend(durations);
        }

        let rss = self
            .broker_maxrss_kib
            .map_or_else(|| "-".to_owned(), |kib| kib.to_string());
        parts.push(format!(
            "broker cpu {:.2}s rss {rss} KiB exit {}",
            self.broker.cpu_s(),
            self.broker_exit
        ));
        if self.warmup {
            parts.push("(warmup)".to_owned());
        }

        parts.join(" ")
    }

    /// The value of the metric `name` of the client, if it measured one of that name.
    fn metric(&self, name: &str) -> Option<f64> {
        let (_, value) = self.metrics.iter().find(|(metric, _)| metric == name)?;

        Some(*value)
    }
}

impl Serialize for Run {
    /// The run as a flat map: its label, its round, whether it warmed up, and its fields.
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("label", &self.label)?;
        map.serialize_entry("round", &self.round)?;
        map.serialize_entry("warmup", &self.warmup)?;
        for (name, value) in self.fields() {
            map.serialize_entry(name, &value)?;
        }

        map.end()
    }
}

/// A run that failed, as the results file notes it.
#[derive(Serialize)]
pub struct Failure {
    /// The label of the broker.
    pub label: String,
    /// The round, counted as `Run::round` is.
    pub round: u32,
    /// Whether the round is one of warm-up.
    pub warmup: bool,
    /// What went wrong.
    pub error: String,
}

/// The runs of `workload`: its rounds of warm-up, then its measured ones, until they are all done,
/// or the runner is interrupted.
///
/// Each round runs every broker once, in an order of its own, shuffled from the seed of the
/// workload and the number of the round.
fn run_workload(setup: &Setup, workload: Workload) -> Results {
    let name = workload.name();
    let measured = setup.measured_rounds(workload);
    let warmup = setup.warmup;
    note!(
        "== {name}: {} after {warmup} of warm-up",
        summary::rounds(measured)
    );
    let rounds = warmup + measured;
    let total = rounds as usize * setup.brokers.len();

    let mut results = Results {
        workload,
        started: Timestamp::now(),
        runs: Vec::new(),
        failures: Vec::new(),
    };
    'rounds: for round in 0..rounds {
        let mut order: Vec<&Broker> = setup.brokers.iter().collect();
        let seed = setup.seed(workload).wrapping_add(round.into());
        Rng::with_seed(seed).shuffle(&mut order);

        for broker in order {
            if interrupt::interrupted() {
                break 'rounds;
            }
            let warming_up = round < warmup;
            let number = results.runs.len() + results.failures.len() + 1;
            let position = format!("[{number}/{total}] {name} round {round} {}", broker.label);
            match run_once(setup, workload, broker, round, warming_up) {
                Ok(run) => {
                    note!("{position}: {}", run.progress());
                    results.runs.push(run);
                }
                // A run that the interrupt cut short is no failure: it did not get to finish.
                Err(_) if interrupt::interrupted() => break 'rounds,
                Err(e) => {
                    note!("{position}: FAILED: {e}");
                    results.failures.push(Failure {
                        label: broker.label.clone(),
                        round,
                        warmup: warming_up,
                        error: e.to_string(),
                    });
                }
            }
        }
    }

    results
}

/// Starts `broker`, runs the client of `workload` against it once, and stops it, in round `round`.
///
/// Whatever goes wrong, the broker is killed, it and every process that it left behind are reaped,
/// and the temporary directory of the run is removed, as they are dropped.
fn run_once(
    setup: &Setup,
    workload: Workload,
    broker: &Broker,
    round: u32,
    warmup: bool,
) -> Result<Run> {
    let dir = TempDir::new()?;
    let socket = dir.path().join("bus");
    let log = dir.path().join("broker.log");
    let (env, argv) = broker.line.resolve(&socket);
    let mut process = BrokerProcess::start(&argv, &env, &log)?;
    process.wait_ready(&socket)?;

    let tracer = match &setup.strace_dir {
        Some(strace_dir) if !warmup => {
            let summary = format!("{}-{}-round{round}.txt", workload.name(), broker.label);
            let summary = strace_dir.join(summary);
            let tracer_log = dir.path().join("strace.log");
            Some(Tracer::attach(process.processes(), &summary, &tracer_log)?)
        }
        _ => None,
    };
    let client = run_client(workload, &socket, &mut process, dir.path()).map_err(|e| {
        match read_tail(&log) {
            log if log.is_empty() => e.to_string(),
            log => format!("{e}\nbroker log:\n{log}"),
        }
    })?;
    let maxrss_kib = process.peak_rss_kib();
    if let Some(tracer) = tracer {
        // Detached, and its summary written, before the broker goes.
        tracer.stop()?;
    }

    // What is reaped from here on is the broker and the processes that it leaves behind, the
    // client and strace being reaped already.
    let before = Usage::of_children()?;
    let exit = process.stop()?;
    let usage = Usage::of_children()?.since(before);
    let broker_usage = BrokerUsage {
        usage,
        maxrss_kib,
        exit,
    };

    Run::new(broker, round, warmup, client, broker_usage)
}

/// What a run measured of its client: what the client printed, and what it used.
struct ClientRun {
    /// What the client measured, by name.
    metrics: Vec<(String, f64)>,
    /// The CPU time that the client and the broker used in each phase, as `Run::phase_cpu` has it.
    phase_cpu: Vec<(String, Option<f64>)>,
    /// The wall time of the whole client process, in seconds.
    wall_s: f64,
    /// What the client used.
    usage: Usage,
}

/// What a run measured of its broker.
struct BrokerUsage {
    /// What it used over its whole life.
    usage: Usage,
    /// Its peak resident memory, in KiB, as `Run::broker_maxrss_kib` has it.
    maxrss_kib: Option<u64>,
    /// Its exit status, as `Run::broker_exit` has it.
    exit: i32,
}

/// A directory of a run's own, under the temporary directory of the system, which is removed with
/// everything in it once dropped.
struct TempDir {
    /// Its path.
    path: PathBuf,
}

impl TempDir {
    /// A new directory, which only the runner's user can enter.
    fn new() -> io::Result<Self> {
        loop {
            let name = format!("busd-compare-{:016x}", fastrand::u64(..));
            let path = env::temp_dir().join(name);
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Its path.
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    /// Removes the directory, and everything in it.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Runs the client of `workload` once, the runner's own executable, against `broker` on `socket`,
/// with its standard error in `dir`, and measures the client and the broker as it runs.
///
/// The client is started by /proc/self/exe, the very file that the runner runs, rather than by its
/// path, which cargo gives to a new file as it builds the bench anew, as it does for `cargo bench
/// --bench compare -- pairs` while a comparison runs.
fn run_client(
    workload: Workload,
    socket: &Path,
    broker: &mut BrokerProcess,
    dir: &Path,
) -> Result<ClientRun> {
    let address = format!("unix:path={}", socket.display());
    let log = dir.join("client.log");
    let stderr = File::create(&log)?;
    // The first word of the client's command line, which ps shows: the path of the runner.
    let name = env::current_exe()?;
    let (watch, watched) = mpsc::channel();
    // It ends as soon as `watch` is dropped, as on any early return.
    let watchdog = thread::Builder::new()
        .name("watchdog".to_owned())
        .spawn(move || watchdog(&watched))?;
    // What the broker used before the client starts, which its first phase is counted from.
    let broker_start = broker.cpu_ticks();

    // The client is the only process that the runner reaps from here until it reaps the client,
    // so the difference is the client's own usage.
    let before = Usage::of_children()?;
    let start = Instant::now();
    let mut client = Command::new("/proc/self/exe")
        .arg0(name)
        .args(["client", workload.name(), "--address", &address])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .map_err(|e| format!("cannot start the client: {e}"))?;
    let pid = Pid::from_child(&client);
    // The watchdog waits for the pid, and so never ends before it gets it.
    let _ = watch.send(pid);

    let phases = match client.stdout.take() {
        Some(stdout) => read_phases(stdout, pid, broker, broker_start),
        None => Err("the standard output of the client is not piped".into()),
    };
    if phases.is_err() {
        let _ = kill_process(pid, Signal::KILL);
    }
    // The client is waited for without being reaped, so that its pid stays its own, and the
    // watchdog cannot kill another process that got it, until the watchdog has ended.
    let _ = waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
    );
    drop(watch);
    let killed = watchdog.join().unwrap_or(None);
    let status = client.wait()?;
    let wall_s = start.elapsed().as_secs_f64();
    let usage = Usage::of_children()?.since(before);

    match killed {
        Some(Kill::Timeout) => {
            let secs = CLIENT_TIMEOUT.as_secs();
            return Err(format!("the client was still running after {secs}s").into());
        }
        Some(Kill::Interrupted) => return Err("interrupted".into()),
        None => {}
    }
    let phases = phases?;
    if !status.success() {
        let message = format!("the client exited with status {}", exit_code(status));
        return Err(with_log(&message, &log).into());
    }
    if phases.phase_cpu.is_empty() {
        return Err(with_log("the client printed no phase line", &log).into());
    }

    Ok(ClientRun {
        metrics: phases.metrics,
        phase_cpu: phases.phase_cpu,
        wall_s,
        usage,
    })
}

/// Why the watchdog of a client killed it.
enum Kill {
    /// It ran for longer than `CLIENT_TIMEOUT`.
    Timeout,
    /// The runner was interrupted.
    Interrupted,
}

/// Kills the client whose pid comes on `watched` once it has run for `CLIENT_TIMEOUT`, or once the
/// runner is interrupted, unless the channel closes before, and says why it killed it, if it did.
///
/// The client of a run that the runner alone is asked to stop, as `kill -INT` asks, gets no signal
/// from a terminal, and would otherwise finish its run first.
fn watchdog(watched: &Receiver<Pid>) -> Option<Kill> {
    let pid = watched.recv().ok()?;
    let deadline = Instant::now() + CLIENT_TIMEOUT;
    loop {
        match watched.recv_timeout(WATCH_TICK) {
            Err(RecvTimeoutError::Timeout) => {}
            // Closed, as the client was waited for.
            Ok(_) | Err(RecvTimeoutError::Disconnected) => return None,
        }
        let kill = if interrupt::interrupted() {
            Kill::Interrupted
        } else if Instant::now() > deadline {
            Kill::Timeout
        } else {
            continue;
        };
        let _ = kill_process(pid, Signal::KILL);

        return Some(kill);
    }
}

/// How long a client has to finish its run.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(600);

/// How long the watchdog of a client waits between two looks at the time and at the interrupt.
const WATCH_TICK: Duration = Duration::from_millis(100);

/// What the client of a run printed, and what its phases cost.
#[derive(Default)]
struct Phases {
    /// What the client measured, by name, as `Run::metrics` has it.
    metrics: Vec<(String, f64)>,
    /// The CPU time that the client and the broker used in each phase, as `Run::phase_cpu` has it.
    phase_cpu: Vec<(String, Option<f64>)>,
}

/// Reads the phase lines that the client `client` prints on `stdout`, until it closes it, and
/// notes the CPU time of the client and of `broker` as each line comes, as it ends a phase.
///
/// The CPU time of the first phase is counted from the start of the client for the client, and
/// from `broker_start`, what the broker had used by then, for the broker. The CPU time of each
/// other phase is counted from the end of the phase before. That of the client is its own, as it
/// starts no process, and that of the broker is that of the processes that it had once it was
/// ready, as `BrokerProcess::cpu_ticks` reads it.
fn read_phases(
    stdout: ChildStdout,
    client: Pid,
    broker: &mut BrokerProcess,
    broker_start: Option<u64>,
) -> Result<Phases> {
    let mut phases = Phases::default();
    let mut ended = (Some(0), broker_start);
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        // Before anything else, and back to back, the client first, so that the end of the phase
        // is where the client printed the line, for both.
        let now = (procfs::cpu_ticks(&[client]), broker.cpu_ticks());
        let phase: PhaseLine = serde_json::from_str(&line)
            .map_err(|e| format!("the client printed a line that is not a phase ({e}): {line}"))?;

        let name = &phase.phase;
        phases.phase_cpu.extend([
            (format!("{name}_client_cpu_s"), spent(ended.0, now.0)),
            (format!("{name}_broker_cpu_s"), spent(ended.1, now.1)),
        ]);
        phases.metrics.extend(phase.metrics);
        ended = now;
    }

    Ok(phases)
}

/// The seconds of CPU time from `ended` to `now`, in clock ticks, if /proc told both, and the time
/// did not shrink in between, as it can if a process went without its time going to its parent.
fn spent(ended: Option<u64>, now: Option<u64>) -> Option<f64> {
    Some(procfs::seconds(now?.checked_sub(ended?)?))
}
