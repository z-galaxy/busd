//! busd against the D-Bus brokers that systems run, [dbus-broker] and [dbus-daemon], on the
//! workloads of the benchmark that came before this one, [dbus-bench], so that what busd's README
//! says of how it compares with them can be measured again, on any machine.
//!
//! # Running it
//!
//! ```sh
//! cargo bench --bench compare
//! ```
//!
//! runs every workload against busd and against each of the other brokers that is installed: 60
//! rounds of each workload, and 40 of the shared connection, which takes longer, after 3 rounds of
//! warm-up. That takes half an hour or more. A quick check that everything works:
//!
//! ```sh
//! cargo bench --bench compare -- --rounds 2 --shared-rounds 2 --warmup 1
//! ```
//!
//! To measure a branch against another revision of busd, such as `main`, as `baseline`:
//!
//! ```sh
//! cargo bench --bench compare -- --baseline main
//! ```
//!
//! `--workload` picks the workloads to run, and `--broker` the brokers: by name, among those that
//! the comparison knows (busd, dbus-broker and dbus-daemon; busd alone leaves the others out), or
//! as `LABEL=COMMAND`, with a command that starts another broker on `{address}`, the address of the
//! socket of a run, or on `{path}`, its path. The command is split into words as a shell would
//! split it, but not run by one, and the words before the program that look like `NAME=value` set
//! environment variables for the broker alone. `--help` lists the rest.
//!
//! It needs Linux, as it reads /proc, and the brokers that it compares busd with: dbus-daemon,
//! and dbus-broker, which needs dbus-broker-launch, systemd-socket-activate and journald, as a
//! desktop's systemd provides them. Those that are missing are left out. For numbers worth
//! comparing, keep the machine otherwise idle.
//!
//! # The method
//!
//! Each workload is a client that runs against a broker once per run: method calls, signals, the
//! latencies of both, callers that share a connection, and method calls with a body of variants
//! (see `workload.rs`). The client is this executable, run with the hidden `client` subcommand,
//! which reports the end of each phase of its workload with a line on its standard output (see
//! `protocol.rs`).
//!
//! Each run gets a broker of its own, on a socket in a temporary directory of its own: the runner
//! starts the broker, waits until it answers a D-Bus authentication on its socket, runs the client
//! against it once, reads the broker's peak memory, stops the broker with SIGINT, or SIGKILL after
//! 5 seconds, and reaps it with every process that it started. The runs go round after round, every
//! broker once in each round, in an order shuffled anew for each round, reproducibly from `--seed`,
//! so that whatever drift the machine has hits every broker alike. The rounds of warm-up run first,
//! and the tables leave them out.
//!
//! busd is built as it ships, with `cargo build --release`, in `target/compare/build`, rather than
//! as the bench build has it, which unifies the features of the dev-dependencies into those of
//! busd's own. The clients are those of dbus-bench, but on busd's own zbus and zruntime, the
//! versions that its Cargo.lock has. They are built as the bench build has them: with busd's
//! release profile, opt-level "s", fat LTO and 1 codegen unit, and with the features of zbus
//! unified with those of busd and of its dev-dependencies, which add tracing, bus-impl, service and
//! proxy to those of dbus-bench's clients. They took 3.5% longer than clients built with opt-level
//! 3 on the calls of 20 clients through dbus-broker, on a VM with 4 vCPUs. So the numbers compare
//! within one run, whose brokers all share one client, and only roughly with the results of
//! dbus-bench, or with those of a checkout whose Cargo.lock has another zbus. `environment.txt`
//! names the versions of the clients.
//!
//! A broker may run as more than one process: dbus-broker's launcher starts the broker proper as a
//! process of its own. What is recorded of a broker covers all of its processes. The runner makes
//! itself their subreaper, so that those that a broker leaves behind as it stops come to it, to be
//! reaped and counted, rather than to init.
//!
//! dbus-broker runs under its launcher, which takes the socket to listen on from systemd's socket
//! activation, as systemd-socket-activate hands it over, and logs to the journal. Where there is no
//! journald, as in a container, the launcher fails, unless something else listens on
//! /run/systemd/journal/socket: where that does not exist, the comparison leaves dbus-broker out,
//! unless `--broker dbus-broker` names it.
//!
//! # What a run records
//!
//! Each run is a flat record of the results file of its workload, of these fields:
//!
//! * `label`, `round` and `warmup`: the broker, the round, counted from 0, the rounds of warm-up
//!   included, and whether the round is one of those.
//! * What the client measured, by the names that it gives them, such as `single_s` and `multi_s`,
//!   the durations of the two phases of the calls, in seconds. The end of each name says its unit:
//!   `_s` for seconds, `_us` for microseconds, and none for a count.
//! * `<phase>_client_cpu_s` and `<phase>_broker_cpu_s`, such as `multi_broker_cpu_s`: the CPU time,
//!   user and system, that the client and the broker used during each phase of the client, from
//!   /proc/PID/stat as the client reports the end of the phase. The first phase starts as the
//!   client does. They are counted in clock ticks, usually of 10 ms each.
//! * `client_wall_s`: the wall time of the whole client process, its start-up included.
//! * `client_cpu_s`: the client's user and system CPU time, from getrusage(RUSAGE_CHILDREN) before
//!   and after it runs. Next to `client_wall_s`, it tells whether the client was busy all along, or
//!   waited for the broker.
//! * `client_vcsw` and `broker_vcsw`: how many times the client and the broker gave up the CPU to
//!   wait, their voluntary context switches. Each is a wait that found nothing to do, so a process
//!   that is handed its messages in bigger batches waits less often.
//! * `broker_cpu_s`: the broker's user and system CPU time over its whole life, also given apart,
//!   as `broker_user_s` and `broker_sys_s`, from getrusage(RUSAGE_CHILDREN) before and after it and
//!   its processes are reaped. It includes the start-up and the shutdown of the broker, which are
//!   small next to a run. A broker with several threads can use more CPU time than the run takes.
//! * `broker_maxrss_kib`: the broker's peak resident memory, VmHWM in /proc/PID/status, read once
//!   the client has finished, and summed over its processes.
//! * `broker_exit`: the broker's exit status, negative for the signal that ended it: busd exits
//!   with 0 on SIGINT, which kills dbus-daemon: -2.
//!
//! # The results
//!
//! They go to a directory of their own, `target/compare/results-<rev>-<time>`, `<rev>` being what
//! `git describe` names the revision of the checkout, and `<time>` when the comparison started, in
//! UTC, such as `20261031T140509Z`, or to `results-<rev>-vs-<baseline>-<time>` with `--baseline`.
//! `--out` names another directory, which is emptied first if it is empty or holds the results of
//! an earlier comparison, and refused otherwise: cargo bench runs the comparison in the directory
//! of the checkout, which a relative path is taken from. Each comparison writes:
//!
//! * `environment.txt`: the versions of what the tables do not name, such as the kernel, the
//!   compiler and the other brokers.
//! * `<workload>.json`: the runs of the workload, those that failed, and the statistics of each
//!   field, in the shape of the results of dbus-bench's scripts.
//! * `<workload>.md`: the statistics of each field, per broker, over the measured runs.
//! * `<workload>-busd-vs-<label>.stats.md` and `.stats.json`: busd against each other broker, in
//!   pairs of runs of the same round, which cancels most of the drift of the machine, and says
//!   whether busd differs from the other broker by more than `--margin`.
//! * `summary.md`, which is also printed at the end: the medians of each workload, and the gaps of
//!   busd to each other broker, in the form of the Performance section of busd's README.
//!
//! A progress line for each run goes to the standard error. A run that fails is reported there,
//! noted in the results file, and left out of the tables, and the other runs go on, but the exit
//! status is 1. So it is if the results of a workload cannot be written: the other workloads go
//! on, and the summary has those whose results were written. Ctrl-C stops the run under way, and
//! the comparison writes the results of the runs that finished, with exit status 130.
//!
//! Under `cargo bench`, though, Ctrl-C has cargo itself exit at once, and the shell prompts again
//! while the comparison still stops its broker and writes its results, until it prints where they
//! are. The executable that `cargo bench --bench compare --no-run` names, run with the arguments
//! that would follow `--`, is interrupted cleanly.
//!
//! `pairs` compares two labels of a results file in pairs again, with other settings, such as
//! another `--margin`:
//!
//! ```sh
//! cargo bench --bench compare -- pairs target/compare/results-REV-TIME/calls.json \
//!     --baseline dbus-broker --candidate busd
//! ```
//!
//! # Diagnosis
//!
//! `--strace-dir DIR` attaches `strace -c -f` to the broker during each measured run, and leaves
//! its summary of the system calls in `DIR/<workload>-<label>-round<k>.txt`, `k` counting the
//! rounds from 0. Tracing slows the broker down a lot, so the timings and the CPU times of such
//! runs are distorted: it tells which system calls dominate, not how the brokers compare.
//!
//! [dbus-broker]: https://github.com/bus1/dbus-broker
//! [dbus-daemon]: https://gitlab.freedesktop.org/dbus/dbus
//! [dbus-bench]: https://github.com/zeenix/dbus-bench

#[cfg(target_os = "linux")]
mod brokers;
#[cfg(target_os = "linux")]
mod builds;
#[path = "../common/calls.rs"]
mod calls;
mod client;
#[cfg(target_os = "linux")]
mod command;
#[path = "../common/dicts.rs"]
mod dicts;
#[cfg(target_os = "linux")]
mod environment;
#[cfg(target_os = "linux")]
mod interrupt;
#[cfg(target_os = "linux")]
mod output;
#[cfg(target_os = "linux")]
mod pairs;
#[cfg(target_os = "linux")]
mod process;
#[cfg(target_os = "linux")]
mod procfs;
mod protocol;
#[cfg(target_os = "linux")]
mod report;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod setup;
#[path = "../common/shared.rs"]
mod shared;
#[path = "../common/signals.rs"]
mod signals;
#[cfg(target_os = "linux")]
mod strace;
#[cfg(target_os = "linux")]
mod summary;
#[cfg(target_os = "linux")]
mod usage;
mod workload;

use std::{path::PathBuf, process::ExitCode};

use clap::{Args, Parser, Subcommand};

use workload::Workload;

/// Does what the command line asks: the comparison, unless it names something else.
fn main() -> ExitCode {
    let cli = Cli::parse();
    let done = match cli.command {
        Some(Command::Client { workload, address }) => client(workload, &address),
        #[cfg(target_os = "linux")]
        Some(Command::Pairs(args)) => pairs(&args),
        #[cfg(target_os = "linux")]
        None => run::compare(&cli.run),
        #[cfg(not(target_os = "linux"))]
        Some(Command::Pairs(_)) | None => needs_linux(),
    };

    done.unwrap_or_else(|e| {
        #[cfg(target_os = "linux")]
        output::note!("error: {e}");
        // Elsewhere, where only the client runs, there is no `output`.
        #[cfg(not(target_os = "linux"))]
        eprintln!("error: {e}");

        ExitCode::FAILURE
    })
}

/// busd against the D-Bus brokers that systems run, on the workloads of dbus-bench.
///
/// Runs every workload against each broker, round after round, and writes the results, with a
/// summary in the form of the Performance section of busd's README. The documentation of the
/// bench target, `benches/compare/main.rs`, has the details.
#[derive(Parser)]
#[command(
    name = "compare",
    bin_name = "cargo bench --bench compare --",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Something else than the comparison.
    #[command(subcommand)]
    command: Option<Command>,
    /// What the comparison runs.
    #[command(flatten)]
    run: RunArgs,
    /// What `cargo bench` appends to the arguments of every benchmark; ignored.
    #[arg(long, global = true, hide = true)]
    bench: bool,
}

/// Something else than the comparison.
#[derive(Subcommand)]
enum Command {
    /// Runs the client of a workload against the bus at an address once: what each run of the
    /// comparison spawns.
    #[command(hide = true)]
    Client {
        /// The workload to run.
        workload: Workload,
        /// The address of the bus.
        #[arg(long)]
        address: String,
    },
    /// Compares two labels of a results file in pairs, round by round, as the comparison
    /// compares busd with each other broker.
    Pairs(PairsArgs),
}

/// What the comparison runs, and where its results go.
#[derive(Args)]
pub struct RunArgs {
    /// A workload to run, which can be given more than once, to run several, in the order given.
    #[arg(
        long = "workload",
        value_name = "WORKLOAD",
        value_enum,
        default_values_t = Workload::ALL
    )]
    pub workloads: Vec<Workload>,
    /// A broker to compare busd with, which can be given more than once: one that the comparison
    /// knows, by name (dbus-broker, dbus-daemon, or busd, for busd alone), or another, as
    /// LABEL=COMMAND, a command that starts it on {address}, or on {path}, the path of the socket.
    /// By default, busd and every broker that the comparison knows and that is installed.
    #[arg(long = "broker", value_name = "BROKER")]
    pub brokers: Vec<String>,
    /// The rounds to measure, each of which runs each broker once.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..))]
    pub rounds: u32,
    /// The rounds to measure of the shared connection, which take longer.
    #[arg(long, default_value_t = 40, value_parser = clap::value_parser!(u32).range(1..))]
    pub shared_rounds: u32,
    /// The rounds to run first, which the tables leave out.
    #[arg(long, default_value_t = 3)]
    pub warmup: u32,
    /// What the shuffles of the order of the brokers in each round start from.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// The directory of the results, which is emptied first. By default, a new one,
    /// target/compare/results-REV-TIME, REV being the revision of this checkout, and TIME when the
    /// comparison started, in UTC.
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// A revision of busd to measure too, as baseline, such as main: a branch, a tag or a commit
    /// of this checkout, or else of https://github.com/z-galaxy/busd, which it is fetched from.
    #[arg(long, value_name = "REV")]
    pub baseline: Option<String>,
    /// Diagnosis: where to leave a summary of the system calls of the broker of each measured
    /// run, by strace, which distorts the timings.
    #[arg(long, value_name = "DIR")]
    pub strace_dir: Option<PathBuf>,
    /// The change, in percent, that the comparisons in pairs take for none.
    #[arg(
        long,
        value_name = "PERCENT",
        default_value_t = 5.0,
        value_parser = margin,
        allow_negative_numbers = true
    )]
    pub margin: f64,
}

/// What `pairs` compares, and how.
#[derive(Args)]
struct PairsArgs {
    /// The results file of a workload, such as calls.json.
    #[arg(required_unless_present = "self_test")]
    results: Option<PathBuf>,
    /// The label that the candidate is compared against.
    #[arg(long, value_name = "LABEL", required_unless_present = "self_test")]
    baseline: Option<String>,
    /// The label that is compared.
    #[arg(long, value_name = "LABEL", required_unless_present = "self_test")]
    candidate: Option<String>,
    /// A field of the runs to compare, which can be given more than once. By default, every field
    /// that is a number in every measured run of both labels, but round and broker_exit.
    #[arg(long = "metric", value_name = "NAME")]
    metrics: Vec<String>,
    /// The change, in percent, that is taken for none.
    #[arg(
        long,
        value_name = "PERCENT",
        default_value_t = 5.0,
        value_parser = margin,
        allow_negative_numbers = true
    )]
    margin: f64,
    /// Where to write every number of the comparison, as JSON.
    #[arg(long, value_name = "OUT")]
    json: Option<PathBuf>,
    /// Checks the statistics against values that are known, and exits.
    #[arg(long, conflicts_with_all = ["results", "baseline", "candidate", "metrics", "json"])]
    self_test: bool,
}

/// Runs the client of `workload` against the bus at `address`, as a run of the comparison does.
fn client(workload: Workload, address: &str) -> calls::Result<ExitCode> {
    client::run(workload, address).map_err(|e| format!("the {} client: {e}", workload.name()))?;

    Ok(ExitCode::SUCCESS)
}

/// Compares two labels of a results file in pairs, as `args` asks, prints the comparison, and
/// writes it as JSON where asked to.
#[cfg(target_os = "linux")]
fn pairs(args: &PairsArgs) -> calls::Result<ExitCode> {
    if args.self_test {
        pairs::self_test()?;
        output::print("The statistics passed their self-test.\n");

        return Ok(ExitCode::SUCCESS);
    }
    let (Some(results), Some(baseline), Some(candidate)) =
        (&args.results, &args.baseline, &args.candidate)
    else {
        unreachable!("clap requires them all without --self-test");
    };
    if baseline == candidate {
        return Err("--baseline and --candidate name the same label".into());
    }

    let path = results.display();
    let text = std::fs::read_to_string(results).map_err(|e| format!("cannot read {path}: {e}"))?;
    let data: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("cannot read {path}: {e}"))?;
    let Some(runs) = data.get("runs").and_then(serde_json::Value::as_array) else {
        return Err(format!("{path} has no list of runs: it is no results file").into());
    };
    let metrics = (!args.metrics.is_empty()).then_some(args.metrics.as_slice());
    let mut comparison = pairs::compare(runs, baseline, candidate, metrics, args.margin)?;
    comparison.source = Some(serde_json::json!({
        "file": path.to_string(),
        "started": data["started"],
        "machine": data["machine"],
        "meta": data["config"]["meta"],
    }));

    // Before it is printed, so that a closed standard output cannot lose it.
    if let Some(json) = &args.json {
        report::write_json(json, &comparison.json())?;
    }
    output::print(&comparison.markdown());

    Ok(ExitCode::SUCCESS)
}

/// Tells that the comparison needs Linux, where it is not.
#[cfg(not(target_os = "linux"))]
fn needs_linux() -> calls::Result<ExitCode> {
    eprintln!("The comparison with other brokers needs Linux: it reads /proc.");

    Ok(ExitCode::FAILURE)
}

/// The change, in percent, that `value` gives as a margin, if it is a number, 0 or more.
fn margin(value: &str) -> Result<f64, String> {
    let margin: f64 = value.parse().map_err(|e| format!("{e}"))?;
    if !margin.is_finite() || margin < 0.0 {
        return Err("it must be a number, 0 or more".to_owned());
    }

    Ok(margin)
}
