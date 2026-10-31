//! The results of each workload, as files of the directory of the results: the runs and their
//! statistics as JSON, the tables of the statistics as Markdown, and the comparisons of busd with
//! each other broker in pairs, round by round.

use std::{fs, path::Path};

use serde::{Serialize, Serializer};
use serde_json::{json, Value};

use crate::{
    brokers::BUSD,
    calls::Result,
    environment::Machine,
    output::note,
    pairs::{self, Comparison},
    run::{Failure, Results, Run},
    setup::Setup,
    summary,
    workload::Workload,
};

/// What the summary needs of the results of a workload.
pub struct Report {
    /// The workload.
    pub workload: Workload,
    /// The statistics of each field over the measured runs, by label.
    pub summary: Ordered<Ordered<Stats>>,
    /// The comparisons of busd with each other broker in pairs, on the fields of the table of the
    /// summary, each over the pairs whose runs both have it, by the label of the other: those
    /// that could be made on any of the fields.
    pub pairs: Vec<(String, Comparison)>,
}

impl Report {
    /// The median of `field` over the measured runs of `label`, if any has it.
    pub fn median(&self, field: &str, label: &str) -> Option<f64> {
        self.summary.get(field)?.get(label)?.median
    }

    /// How many of the measured runs of `label` have `field` as a number.
    pub fn count(&self, field: &str, label: &str) -> usize {
        let stats = self
            .summary
            .get(field)
            .and_then(|by_label| by_label.get(label));

        stats.map_or(0, |stats| stats.n)
    }
}

/// Writes the results of a workload, the runs of `results`: `<workload>.json`, `<workload>.md`,
/// and the comparisons in pairs, `<workload>-busd-vs-<label>.stats.md` and `.stats.json`.
pub fn write(setup: &Setup, results: &Results) -> Result<Report> {
    let workload = results.workload;
    let name = workload.name();
    let labels: Vec<&str> = setup
        .brokers
        .iter()
        .map(|broker| broker.label.as_str())
        .collect();
    let summary = summarize(&results.runs, &labels);

    let brokers = setup.brokers.iter().map(|broker| BrokerConfig {
        label: &broker.label,
        command: &broker.command,
    });
    let file = ResultsFile {
        machine: &setup.environment.machine,
        config: Config {
            workload: name,
            brokers: brokers.collect(),
            rounds: setup.measured_rounds(workload),
            warmup: setup.warmup,
            seed: setup.seed(workload),
            strace_dir: setup.strace_dir.as_deref(),
            meta: &setup.meta,
        },
        started: results.started.iso(),
        runs: &results.runs,
        failures: &results.failures,
        summary: &summary,
    };
    // Before the tables, so that whatever goes wrong after, the runs are kept.
    let path = setup.out.join(format!("{name}.json"));
    write_json(&path, &file)?;
    let measured = measured_runs(&results.runs, &labels);
    let tables = tables(setup, workload, &summary, &measured);
    fs::write(setup.out.join(format!("{name}.md")), tables)?;
    let runs = results.runs.iter().map(serde_json::to_value);
    let runs = runs.collect::<serde_json::Result<Vec<Value>>>()?;
    let source = source(setup, results, &path)?;
    let pairs = compare_in_pairs(setup, workload, &runs, &source)?;

    Ok(Report {
        workload,
        summary,
        pairs,
    })
}

/// Writes `value` to the file at `path` as JSON, indented, making the directory of the file first.
pub fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let mut json = serde_json::to_string_pretty(value)?;
    json.push('\n');
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, json).map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    Ok(())
}

/// Pairs of a name and a value, which serialize as a map, in their order.
pub struct Ordered<V>(pub Vec<(String, V)>);

impl<V> Ordered<V> {
    /// The value of `name`, if there is one.
    pub fn get(&self, name: &str) -> Option<&V> {
        let (_, value) = self.0.iter().find(|(key, _)| key == name)?;

        Some(value)
    }
}

impl<V> Serialize for Ordered<V>
where
    V: Serialize,
{
    /// The pairs as a map, in their order.
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_map(self.0.iter().map(|(name, value)| (name, value)))
    }
}

/// The statistics of a field over the measured runs of a label.
#[derive(Serialize)]
pub struct Stats {
    /// How many of the runs have the field as a number.
    pub n: usize,
    /// The mean of the field over those runs.
    pub mean: Option<f64>,
    /// The sample standard deviation of the field over those runs, if there are 2 or more.
    pub stddev: Option<f64>,
    /// The median of the field over those runs.
    pub median: Option<f64>,
    /// The smallest value of the field.
    pub min: Option<f64>,
    /// The largest value of the field.
    pub max: Option<f64>,
}

impl Stats {
    /// The statistics of `values`.
    fn of(mut values: Vec<f64>) -> Self {
        values.sort_by(f64::total_cmp);
        let n = values.len();
        let (Some(&min), Some(&max)) = (values.first(), values.last()) else {
            return Self {
                n,
                mean: None,
                stddev: None,
                median: None,
                min: None,
                max: None,
            };
        };
        let mean = values.iter().sum::<f64>() / n as f64;
        let squares = values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>();
        let stddev = (n > 1).then(|| (squares / (n - 1) as f64).sqrt());
        let median = match n % 2 {
            1 => values[n / 2],
            _ => (values[n / 2 - 1] + values[n / 2]) / 2.0,
        };

        Self {
            n,
            mean: Some(mean),
            stddev,
            median: Some(median),
            min: Some(min),
            max: Some(max),
        }
    }
}

/// A results file: what the comparison ran of a workload, and what came of it, in the shape of
/// the results files of the scripts of dbus-bench.
#[derive(Serialize)]
struct ResultsFile<'a> {
    /// The machine.
    machine: &'a Machine,
    /// What was run.
    config: Config<'a>,
    /// When the first run started.
    started: String,
    /// The runs that finished, the warm-up runs among them.
    runs: &'a [Run],
    /// The runs that failed.
    failures: &'a [Failure],
    /// The statistics of each field over the measured runs, by label.
    summary: &'a Ordered<Ordered<Stats>>,
}

/// What a results file notes of what was run.
#[derive(Serialize)]
struct Config<'a> {
    /// The name of the workload.
    workload: &'a str,
    /// The brokers.
    brokers: Vec<BrokerConfig<'a>>,
    /// The rounds that the comparison was set to measure.
    rounds: u32,
    /// The rounds of warm-up.
    warmup: u32,
    /// The seed of the shuffle of the first round, to which each round adds its number.
    seed: u64,
    /// The directory of the summaries of strace, if it traced the brokers.
    strace_dir: Option<&'a Path>,
    /// The versions that the tables do not name.
    meta: &'a Ordered<String>,
}

/// What a results file notes of a broker.
#[derive(Serialize)]
struct BrokerConfig<'a> {
    /// The label of its runs.
    label: &'a str,
    /// The command that started it.
    command: &'a str,
}

/// The statistics of every field of the measured runs of `runs` that is a number in some of them,
/// but `broker_exit`, in the order that the fields first come in, for each of `labels`.
fn summarize(runs: &[Run], labels: &[&str]) -> Ordered<Ordered<Stats>> {
    let measured: Vec<(&str, Vec<(&str, f64)>)> = runs
        .iter()
        .filter(|run| !run.warmup)
        .map(|run| {
            let fields = run.fields().into_iter();
            let numbers = fields.filter_map(|(name, value)| Some((name, value.as_f64()?)));

            (run.label.as_str(), numbers.collect())
        })
        .collect();

    let mut fields: Vec<&str> = Vec::new();
    for (_, numbers) in &measured {
        for &(field, _) in numbers {
            // An exit status, not a measurement.
            if field != "broker_exit" && !fields.contains(&field) {
                fields.push(field);
            }
        }
    }

    let stats = fields.iter().map(|&field| {
        let by_label = labels.iter().map(|&label| {
            let runs = measured.iter().filter(|(run_label, _)| *run_label == label);
            let values = runs.filter_map(|(_, numbers)| {
                let (_, value) = numbers.iter().find(|(name, _)| *name == field)?;

                Some(*value)
            });

            (label.to_owned(), Stats::of(values.collect()))
        });

        (field.to_owned(), Ordered(by_label.collect()))
    });

    Ordered(stats.collect())
}

/// How many measured runs each of `labels` has in `runs`, by label.
fn measured_runs<'l>(runs: &[Run], labels: &[&'l str]) -> Vec<(&'l str, usize)> {
    let count = |label| {
        let measured = runs.iter().filter(|run| !run.warmup && run.label == label);

        measured.count()
    };

    labels.iter().map(|&label| (label, count(label))).collect()
}

/// The tables of `summary`, the statistics of the fields of the runs of `workload`, as Markdown,
/// after a header that notes the rounds, the machine and the versions.
///
/// The header says how many of the rounds that the comparison was set to measure each broker has a
/// measured run in, from `measured`, those counts by label: all of them, or else the fewest and
/// the most, with the count of each broker where those differ.
fn tables(
    setup: &Setup,
    workload: Workload,
    summary: &Ordered<Ordered<Stats>>,
    measured: &[(&str, usize)],
) -> String {
    let machine = &setup.environment.machine;
    let rounds = setup.measured_rounds(workload) as usize;
    let warmup = setup.warmup;
    let counts = measured.iter().map(|&(_, count)| count);
    let fewest = counts.clone().min().unwrap_or(0);
    let most = counts.max().unwrap_or(0);
    let measured = if (fewest, most) == (rounds, rounds) {
        format!("{rounds} measured")
    } else if fewest == most {
        format!("{fewest} of {rounds} measured")
    } else {
        let each: Vec<String> = measured
            .iter()
            .map(|(label, count)| format!("{label} {count}"))
            .collect();

        format!(
            "{fewest} to {most} of {rounds} measured ({})",
            each.join(", ")
        )
    };
    let mut lines = vec![format!(
        "rounds: {measured} after {warmup} warmup; {} CPUs, {}",
        machine.nproc, machine.cpu_model
    )];
    lines.extend(
        setup
            .meta
            .0
            .iter()
            .map(|(key, value)| format!("{key}: {value}")),
    );

    for (field, by_label) in &summary.0 {
        let (unit, decimals) = unit(field);
        lines.extend([String::new(), format!("### {field}{unit}"), String::new()]);
        lines.push("| broker | n | mean | stddev | median | min | max |".to_owned());
        lines.push("|---|---:|---:|---:|---:|---:|---:|".to_owned());
        for (label, stats) in &by_label.0 {
            let values = [stats.mean, stats.stddev, stats.median, stats.min, stats.max];
            let cells: Vec<String> = values
                .iter()
                .map(|value| value.map_or_else(|| "-".to_owned(), |v| format!("{v:.decimals$}")))
                .collect();
            lines.push(format!("| {label} | {} | {} |", stats.n, cells.join(" | ")));
        }
    }
    let mut tables = lines.join("\n");
    tables.push('\n');

    tables
}

/// The unit of `field`, as the end of its name says, for the heading of its table, and how many
/// decimals its values get there.
fn unit(field: &str) -> (&'static str, usize) {
    if field.ends_with("_s") {
        (" (seconds)", 3)
    } else if field.ends_with("_kib") {
        (" (KiB)", 0)
    } else if field.ends_with("_us") {
        (" (microseconds)", 1)
    } else {
        ("", 3)
    }
}

/// What the comparisons in pairs of the runs of `results` are made from, as their JSON notes it:
/// the results file at `path`, which has the runs, when the first of them started, the machine,
/// and the notes of `setup`, the versions that the tables do not name.
fn source(setup: &Setup, results: &Results, path: &Path) -> Result<Value> {
    let machine = serde_json::to_value(&setup.environment.machine)?;
    let meta = serde_json::to_value(&setup.meta)?;

    Ok(json!({
        "file": path.display().to_string(),
        "started": results.started.iso(),
        "machine": machine,
        "meta": meta,
    }))
}

/// Compares busd with each other broker in pairs, round by round, over `runs`, the runs of
/// `workload` as its results file has them, and writes each comparison, which notes `source`, what
/// it is made from: the comparisons of the summary, by the label of the other broker.
///
/// The comparison that is written is of every field that is a number in every measured run of
/// both brokers, as `pairs` makes it by default, and that of the summary is of the fields of its
/// table, each over the pairs whose runs both have it (see `compare_columns`). A comparison that
/// cannot be made, as without a round that both ran in, is reported, and the others go on.
fn compare_in_pairs(
    setup: &Setup,
    workload: Workload,
    runs: &[Value],
    source: &Value,
) -> Result<Vec<(String, Comparison)>> {
    let name = workload.name();

    let mut comparisons = Vec::new();
    for broker in setup.brokers.iter().filter(|broker| broker.label != BUSD) {
        let label = &broker.label;
        match pairs::compare(runs, label, BUSD, None, setup.margin) {
            Ok(mut comparison) => {
                comparison.source = Some(source.clone());
                let stem = format!("{name}-busd-vs-{label}");
                write_json(
                    &setup.out.join(format!("{stem}.stats.json")),
                    &comparison.json(),
                )?;
                fs::write(
                    setup.out.join(format!("{stem}.stats.md")),
                    comparison.markdown(),
                )?;
            }
            Err(e) => note!("busd cannot be compared with {label} on {name} in pairs: {e}"),
        }
        if let Some(comparison) = compare_columns(runs, label, workload, setup.margin) {
            comparisons.push((label.clone(), comparison));
        }
    }

    Ok(comparisons)
}

/// Compares busd with the broker `label` in pairs, over `runs`, the runs of `workload` as its
/// results file has them, on the fields of the table of its summary, each over the pairs whose runs
/// both have it, by `margin`: unless no field is a number in both runs of a round.
///
/// That a field is missing in some runs, as when /proc did not tell, leaves those rounds out of
/// its comparison alone, whereas `pairs` would leave the field out by default.
fn compare_columns(
    runs: &[Value],
    label: &str,
    workload: Workload,
    margin: f64,
) -> Option<Comparison> {
    // One by one, as a field that is a number in no pair fails the comparison of every field named
    // with it. Each comparison has the same pairs, and its bootstrap the same draws.
    let mut compared = summary::fields(workload).filter_map(|field| {
        pairs::compare(runs, label, BUSD, Some(&[field.to_owned()]), margin).ok()
    });
    let mut comparison = compared.next()?;
    comparison
        .metrics
        .extend(compared.flat_map(|other| other.metrics));

    Some(comparison)
}
