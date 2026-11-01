//! What the comparison runs, worked out before its first run: the workloads and their rounds, the
//! brokers, checked, found and built, and the directory of the results, with what they note of the
//! machine and the software.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use crate::{
    brokers::{self, Broker, BASELINE, BUSD},
    builds::{self, Revision, CHECKOUT},
    calls::Result,
    command::find_program,
    environment::Environment,
    output::note,
    report::Ordered,
    workload::Workload,
    RunArgs,
};

/// What the comparison runs, and where its results go.
pub struct Setup {
    /// The workloads, in the order that they run.
    pub workloads: Vec<Workload>,
    /// The brokers: busd first, then baseline, if there is one, then the others.
    pub brokers: Vec<Broker>,
    /// The measured rounds of every workload but the shared connection.
    pub rounds: u32,
    /// The measured rounds of the shared connection.
    pub shared_rounds: u32,
    /// The rounds that run first, which the tables leave out.
    pub warmup: u32,
    /// The change, in percent, that the comparisons in pairs take for none.
    pub margin: f64,
    /// The directory that strace leaves its summaries in, if it traces the brokers.
    pub strace_dir: Option<PathBuf>,
    /// The directory of the results.
    pub out: PathBuf,
    /// The machine and the software.
    pub environment: Environment,
    /// The notes of the results: the versions that the tables do not name.
    pub meta: Ordered<String>,
    /// What the seeds of the shuffles of the rounds start from.
    seed: u64,
}

impl Setup {
    /// Works out what `args` ask for: checks the brokers, builds busd, and baseline if asked to,
    /// probes the machine and the software, empties the directory of the results, where it writes
    /// `environment.txt`, and makes that of the summaries of strace, if asked to.
    pub fn prepare(args: &RunArgs) -> Result<Self> {
        let mut workloads = Vec::new();
        for &workload in &args.workloads {
            if !workloads.contains(&workload) {
                workloads.push(workload);
            }
        }
        // Checked before the builds, which take a while.
        let builds: &[&str] = match args.baseline {
            Some(_) => &[BUSD, BASELINE],
            None => &[BUSD],
        };
        let others = brokers::others(&args.brokers, builds)?;
        if let Some(out) = &args.out {
            check_out(out)?;
        }
        if args.strace_dir.is_some() {
            check_strace()?;
        }

        let busd = builds::this_checkout()?;
        let baseline = args.baseline.as_deref().map(Revision::build).transpose()?;
        let mut all = vec![brokers::busd(BUSD, BUSD.to_owned(), &busd)?];
        if let Some(baseline) = &baseline {
            let name = format!("{BASELINE} ({})", baseline.short());
            all.push(brokers::busd(BASELINE, name, &baseline.binary)?);
        }
        all.extend(others);

        let environment = Environment::probe(baseline.as_ref());
        let out = match &args.out {
            Some(out) => out.clone(),
            None => default_out(&environment, baseline.as_ref()),
        };
        empty_out(&out)?;
        fs::write(out.join("environment.txt"), &environment.text)?;
        // Once the directory of the results is emptied, as it may be in there.
        let strace_dir = args.strace_dir.as_deref().map(strace_dir).transpose()?;

        let mut meta = vec![
            ("rustc".to_owned(), environment.rustc.clone()),
            (BUSD.to_owned(), environment.full_rev.clone()),
        ];
        if let Some(baseline) = &baseline {
            meta.push((BASELINE.to_owned(), baseline.hash.clone()));
        }
        let versions = all
            .iter()
            .filter_map(|broker| Some((broker.label.clone(), broker.version.clone()?)));
        meta.extend(versions);

        Ok(Self {
            workloads,
            brokers: all,
            rounds: args.rounds,
            shared_rounds: args.shared_rounds,
            warmup: args.warmup,
            margin: args.margin,
            strace_dir,
            out,
            environment,
            meta: Ordered(meta),
            seed: args.seed,
        })
    }

    /// The measured rounds of `workload`.
    pub fn measured_rounds(&self, workload: Workload) -> u32 {
        match workload {
            Workload::Shared => self.shared_rounds,
            _ => self.rounds,
        }
    }

    /// The seed of the shuffle of the first round of `workload`, to which each round adds its
    /// number: `--seed`, plus an offset of the workload's own, so that each has a sequence of its
    /// own.
    pub fn seed(&self, workload: Workload) -> u64 {
        let offset = match workload {
            Workload::Calls => 31,
            Workload::Signals => 32,
            Workload::Latency => 33,
            Workload::Shared => 34,
            Workload::Dicts => 35,
        };

        self.seed.wrapping_add(offset)
    }
}

/// Checks that `dir`, the directory of the results, can be emptied: it must be missing, empty, or
/// hold results already, which have an `environment.txt`, so that a mistaken `--out` cannot wipe
/// anything else.
fn check_out(dir: &Path) -> Result<()> {
    let mut entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display()).into()),
    };
    if entries.next().is_some() && !dir.join("environment.txt").is_file() {
        let dir = dir.display();
        return Err(format!(
            "{dir} is not empty, and holds no results of an earlier comparison: name another \
             directory for the results"
        )
        .into());
    }

    Ok(())
}

/// Checks that strace is installed, for `--strace-dir`, and warns that it distorts the timings.
fn check_strace() -> Result<()> {
    if find_program("strace").is_none() {
        return Err("--strace-dir needs strace, which is not installed".into());
    }
    note!(
        "warning: --strace-dir is on: strace slows the brokers down, so the timings and the CPU \
         times of the measured runs are distorted. Use them to see system calls only."
    );

    Ok(())
}

/// The directory of the results, unless `--out` names one: `target/compare/results-<rev>-<time>`
/// of this checkout, or `results-<rev>-vs-<short hash of baseline>-<time>`, `<time>` being when
/// the comparison started, in UTC, as `20261031T140509Z`, so that no comparison empties the
/// directory of another.
fn default_out(environment: &Environment, baseline: Option<&Revision>) -> PathBuf {
    let mut name = format!("results-{}", environment.rev);
    if let Some(baseline) = baseline {
        name.push_str("-vs-");
        name.push_str(baseline.short());
    }
    name.push('-');
    name.push_str(&environment.started.basic());

    Path::new(CHECKOUT).join("target/compare").join(name)
}

/// Empties `dir`, or makes it, for the results, once `check_out` agrees: what an earlier
/// comparison left there would be mixed up with them otherwise.
fn empty_out(dir: &Path) -> Result<()> {
    check_out(dir)?;
    match fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(format!("cannot empty {}: {e}", dir.display()).into());
        }
        _ => {}
    }
    fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;

    Ok(())
}

/// Makes `dir`, for the summaries of strace, unless it exists: its absolute path.
fn strace_dir(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;

    Ok(fs::canonicalize(dir)?)
}
