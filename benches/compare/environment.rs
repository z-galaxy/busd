//! What the results note of the machine and of the software that they were measured with: the
//! machine in each results file, and the rest in `environment.txt`, as the tables name none of it.
//!
//! Every probe runs before the first run, as the processes that it starts would otherwise be
//! counted with a client or a broker (see `usage.rs`).

use std::{
    ffi::OsStr,
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::{
    builds::{self, Revision, CHECKOUT},
    procfs,
};

/// The machine and the software that the comparison is measured with.
pub struct Environment {
    /// When the comparison started, once busd was built, just before its first run.
    pub started: Timestamp,
    /// The machine.
    pub machine: Machine,
    /// What the compiler calls itself: the first line of `rustc --version`.
    pub rustc: String,
    /// The revision of this checkout, as `git describe --always --dirty --abbrev=7` names it.
    pub rev: String,
    /// The revision of this checkout, with the full hash of its commit.
    pub full_rev: String,
    /// The text of `environment.txt`.
    pub text: String,
}

impl Environment {
    /// Probes the machine and the software, with `baseline`, the build of busd at the revision that
    /// `--baseline` names, if there is one.
    pub fn probe(baseline: Option<&Revision>) -> Self {
        let started = Timestamp::now();
        let (cpu_model, nproc) = procfs::cpus();
        let machine = Machine {
            nproc,
            cpu_model,
            kernel: procfs::kernel(),
        };
        let rustc = first_line("rustc", &["--version"]).unwrap_or_default();
        let rev = builds::describe(7).unwrap_or_else(|| "unknown".to_owned());
        let full_rev = builds::describe(40).unwrap_or_else(|| "unknown".to_owned());

        let probe = |program: &str| first_line(program, &["--version"]).unwrap_or_default();
        let mut lines = vec![
            format!("date: {}", started.iso()),
            format!("kernel: {}", machine.kernel),
            format!("cpu: {} x {}", machine.cpu_model, machine.nproc),
            format!("glibc: {}", probe("ldd")),
            format!("rustc: {rustc}"),
            format!(
                "cargo: {}",
                first_line(builds::cargo(), &["--version"]).unwrap_or_default()
            ),
            format!("busd: {full_rev}, {}", versions(Path::new(CHECKOUT))),
        ];
        if let Some(baseline) = baseline {
            lines.push(format!(
                "baseline: {}, {}",
                baseline.hash,
                versions(&baseline.dir)
            ));
        }
        // The clients are this executable, as the bench build of this checkout has it.
        lines.push(format!(
            "clients: {}, built with the bench profile (opt-level \"s\", fat LTO, 1 codegen unit), \
             zbus features unified with busd's and the dev-dependencies'",
            versions(Path::new(CHECKOUT))
        ));
        lines.push(format!("dbus-broker: {}", probe("dbus-broker")));
        lines.push(format!("dbus-daemon: {}", probe("dbus-daemon")));
        let mut text = lines.join("\n");
        text.push('\n');

        Self {
            started,
            machine,
            rustc,
            rev,
            full_rev,
            text,
        }
    }
}

/// The machine, as each results file notes it.
#[derive(Serialize)]
pub struct Machine {
    /// How many CPUs it has.
    pub nproc: usize,
    /// Their model.
    pub cpu_model: String,
    /// The release of its kernel.
    pub kernel: String,
}

/// The first line that `program` prints with `args`, whatever its exit status, if it can be run.
pub fn first_line<P>(program: P, args: &[&str]) -> Option<String>
where
    P: AsRef<OsStr>,
{
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    Some(stdout.lines().next()?.trim_end().to_owned())
}

/// A moment, to the second, in UTC.
#[derive(Clone, Copy)]
pub struct Timestamp {
    /// The seconds since the Unix epoch.
    secs: u64,
}

impl Timestamp {
    /// The moment of the call.
    pub fn now() -> Self {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        Self {
            secs: since_epoch.as_secs(),
        }
    }

    /// The date, as `2026-10-31`.
    pub fn date(self) -> String {
        let (year, month, day) = civil(self.secs / SECONDS_PER_DAY);

        format!("{year:04}-{month:02}-{day:02}")
    }

    /// The date and the time, in ISO 8601, as `2026-10-31T14:05:09Z`.
    pub fn iso(self) -> String {
        let time = self.secs % SECONDS_PER_DAY;
        let (hours, minutes, seconds) = (time / 3600, time / 60 % 60, time % 60);

        format!("{}T{hours:02}:{minutes:02}:{seconds:02}Z", self.date())
    }

    /// The date and the time in the basic format of ISO 8601, which has no separators, so that it
    /// can name a file, as `20261031T140509Z`.
    pub fn basic(self) -> String {
        self.iso().replace(['-', ':'], "")
    }
}

/// How many seconds a day of UTC has.
const SECONDS_PER_DAY: u64 = 86_400;

/// The versions of zbus and zruntime that the Cargo.lock in `dir` has.
fn versions(dir: &Path) -> String {
    format!(
        "zbus {}, zruntime {}",
        locked(dir, "zbus"),
        locked(dir, "zruntime")
    )
}

/// The version of the package `name` that the Cargo.lock in `dir` has, with the repository and the
/// commit that it is at where it comes from git, or its versions, joined with "and", if it has
/// several.
fn locked(dir: &Path, name: &str) -> String {
    let lock = fs::read_to_string(dir.join("Cargo.lock")).unwrap_or_default();
    // What comes before the first package is the version of the lock file.
    let packages = lock.split("[[package]]").skip(1);
    let versions: Vec<String> = packages
        .filter_map(|package| {
            let field = |key: &str| {
                package.lines().find_map(|line| {
                    let (k, value) = line.split_once(" = ")?;

                    (k == key).then(|| value.trim_matches('"'))
                })
            };
            if field("name")? != name {
                return None;
            }
            let version = field("version")?;
            // Such as git+https://github.com/z-galaxy/zbus?rev=90a0bd1#90a0bd1bc7ed..., the commit
            // after the #.
            let Some(git) = field("source").and_then(|source| source.strip_prefix("git+")) else {
                return Some(version.to_owned());
            };
            let (repository, commit) = git.split_once('#').unwrap_or((git, ""));
            let repository = repository.split('?').next().unwrap_or(repository);

            Some(format!("{version} {repository} at {commit}"))
        })
        .collect();

    versions.join(" and ")
}

/// The year, the month and the day of the day `days` after 1970-01-01, by the Gregorian calendar,
/// as Howard Hinnant's `civil_from_days` works them out.
fn civil(days: u64) -> (u64, u64, u64) {
    // Counted from 0000-03-01, so that a leap day ends its year, in eras of 400 years.
    let days = days + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    // The months from March.
    let month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month + 2) / 5 + 1;
    let month = if month < 10 { month + 3 } else { month - 9 };
    let year = era * 400 + year_of_era + u64::from(month <= 2);

    (year, month, day)
}
