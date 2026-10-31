//! The comparison of two labels of a workload's results, such as busd and dbus-broker, round by
//! round.
//!
//! The results of a workload hold a run of each label in each round, and the labels of a round run
//! close together in time, in a random order, so that the two runs of a round share most of the
//! drift of the machine. Comparing them as pairs, rather than as two independent samples, cancels
//! that drift. So `compare` pairs the measured runs of a baseline and of a candidate by round,
//! leaving out the warmup runs and the rounds in which either label has no measured run, such as
//! one whose run failed, and compares each metric, a field of the runs, over the pairs:
//!
//! * the mean and the median of each label;
//! * the percent difference of the means, `100 * (candidate mean / baseline mean - 1)`, with its
//!   95% and 90% intervals from a percentile bootstrap: the pairs are resampled with replacement
//!   `RESAMPLES` times, the same rounds for both labels, and the percent difference is taken in
//!   each resample;
//! * the median over the pairs of candidate / baseline, as means are sensitive to the odd slow run:
//!   where the two disagree, look at the runs;
//! * a `Verdict` on the change, by the margin of the change that does not matter.
//!
//! With fewer than about 10 pairs, the intervals are too narrow: take 15 rounds or more.
//!
//! The numbers, and the JSON of a comparison, are those of `scripts/paired-stats.py` of
//! [dbus-bench], so that comparisons from the two read alike, but for the ends of the intervals,
//! and for the script's paired t interval of the mean difference and its Wilcoxon signed-rank
//! test, which never decided a verdict, and which this does not compute. The bootstrap draws its
//! resamples from fastrand's generator rather than from Python's, so that the ends of the
//! intervals differ from the script's by a few hundredths of a point on most metrics, and by up to
//! about 3% of their width on the widest, such as those of the tail latencies: a verdict close to
//! the edge of the margin can differ with them. The values and the order of the metrics are the
//! script's, as the bench builds serde_json with two features: `float_roundtrip`, which reads each
//! number of a results file exactly, as Python does, and `preserve_order`, which keeps the fields
//! of each run in the order of the file, as Python's dicts do.
//!
//! [dbus-bench]: https://github.com/zeenix/dbus-bench

use std::{
    collections::{BTreeMap, BTreeSet},
    f64::consts::TAU,
    fmt::Debug,
};

use serde_json::{json, Map, Value};

use crate::calls::Result;

/// The comparison of a candidate against a baseline, metric by metric, over the measured rounds
/// that both ran in.
pub struct Comparison {
    /// What the comparison was made from, if that is known: the results file, when its first run
    /// started, the machine, and the notes of its config, such as the versions of the brokers, as
    /// an object of `file`, `started`, `machine` and `meta`, which `compare` leaves to its caller.
    pub source: Option<Value>,
    /// The label of the runs that the candidate is compared against.
    pub baseline: String,
    /// The label of the runs that are compared.
    pub candidate: String,
    /// The change, in percent, that does not matter.
    pub margin: f64,
    /// The rounds in which both labels have a measured run, in increasing order.
    pub paired_rounds: Vec<u64>,
    /// The rounds in which only the baseline has a measured run, in increasing order.
    pub only_baseline: Vec<u64>,
    /// The rounds in which only the candidate has a measured run, in increasing order.
    pub only_candidate: Vec<u64>,
    /// The fields that are a number in some measured runs of the two labels but not in all, which
    /// are not compared unless they are named, by how many of the runs lack a number there. It is
    /// empty when the metrics to compare are named.
    pub not_compared: BTreeMap<String, usize>,
    /// The comparison of each metric, in the order of the fields of the runs, or in that of the
    /// metrics that were named.
    pub metrics: Vec<MetricComparison>,
}

/// The comparison of one metric.
pub struct MetricComparison {
    /// The name of the metric: the field of the runs, such as `multi_s`.
    pub metric: String,
    /// How many pairs of runs have it.
    pub n: usize,
    /// The mean of the baseline over the pairs.
    pub baseline_mean: f64,
    /// The mean of the candidate over the pairs.
    pub candidate_mean: f64,
    /// The median of the baseline over the pairs.
    pub baseline_median: f64,
    /// The median of the candidate over the pairs.
    pub candidate_median: f64,
    /// `100 * (candidate mean / baseline mean - 1)`, if the baseline mean is positive.
    ///
    /// It is 0 when the two labels are the same in every pair, even if the baseline mean is not
    /// positive.
    pub pct: Option<f64>,
    /// The 95% percentile bootstrap interval of `pct`.
    pub ci95: Option<[f64; 2]>,
    /// The 90% percentile bootstrap interval of `pct`.
    pub ci90: Option<[f64; 2]>,
    /// How many bootstrap resamples the intervals were taken from: `RESAMPLES`, less those that
    /// were left out (`skipped`).
    ///
    /// It is 0 when there was no bootstrap: with a single pair, without `pct`, or when the two
    /// labels are the same in every pair, whose intervals are `[0, 0]`.
    pub resamples: usize,
    /// How many bootstrap resamples were left out, as their baseline mean was not positive.
    pub skipped: usize,
    /// The median over the pairs of candidate / baseline.
    ///
    /// The pairs whose baseline is 0 have no ratio, and it is missing if all of them do.
    pub median_ratio: Option<f64>,
    /// What the intervals say about the change, given the margin.
    pub verdict: Verdict,
    /// Whether the 95% interval excludes 0.
    ///
    /// Then a change of the sign of `pct` is likely, whatever its size, even if it is within the
    /// margin: a change larger than the margin is repeatable by definition.
    pub repeatable: bool,
    /// What the numbers should be read with, such as a baseline mean that is not positive.
    pub notes: Vec<String>,
}

/// What the intervals of a metric say about its change.
///
/// The verdict rests on the bootstrap intervals of the percent difference and the margin, the
/// change that does not matter, alone. Its words assume that less is better, as it is of the times,
/// the CPU times and the memory that the runs measure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The 90% interval lies within the margin.
    ///
    /// The change is shown to be smaller than the margin, by two one-sided tests at the 5% level:
    /// an equivalence test.
    NoChange,
    /// The 95% interval lies above the margin: the candidate is slower, or uses more.
    Larger,
    /// The 95% interval lies below the margin, taken negative: the candidate is faster, or uses
    /// less.
    Smaller,
    /// Anything else.
    ///
    /// The data cannot tell whether the change is within the margin, as when there are no
    /// intervals, with a single pair or a baseline mean that is not positive.
    Inconclusive,
}

impl Comparison {
    /// The comparison of `metric`, if it was compared.
    pub fn metric(&self, metric: &str) -> Option<&MetricComparison> {
        self.metrics.iter().find(|m| m.metric == metric)
    }

    /// The comparison as Markdown.
    ///
    /// A heading names the labels and the rounds, a first table has the means, the percent
    /// difference, its 95% and 90% intervals and the verdict of each metric, and a second one the
    /// medians and the median ratio. A legend and the notes follow.
    pub fn markdown(&self) -> String {
        let (baseline, candidate, margin) = (&self.baseline, &self.candidate, self.margin);
        let rounds = describe_rounds(
            &self.paired_rounds,
            &self.only_baseline,
            &self.only_candidate,
        );
        let mut lines = vec![
            format!("### {candidate} against {baseline}: {rounds}"),
            String::new(),
            format!(
                "| Metric | n | {baseline} mean | {candidate} mean | Diff | 95% CI | 90% CI | \
                 Verdict |"
            ),
            "|---|---:|---:|---:|---:|---|---|---|".to_owned(),
        ];
        lines.extend(self.metrics.iter().map(|metric| metric.row(margin)));
        lines.extend([
            String::new(),
            format!("| Metric | {baseline} median | {candidate} median | Median ratio |"),
            "|---|---:|---:|---:|".to_owned(),
        ]);
        lines.extend(self.metrics.iter().map(MetricComparison::medians_row));
        lines.extend([
            String::new(),
            format!(
                "Diff is 100 * ({candidate} mean / {baseline} mean - 1), less being better, and \
                 its CIs are from a percentile bootstrap of the pairs, of {RESAMPLES} resamples \
                 drawn by fastrand's generator from the seed {SEED}."
            ),
            format!(
                "The verdicts are by the margin of ±{margin}%: within ±{margin}% if the 90% CI \
                 lies within the margin, **worse** or **better** if the 95% CI lies beyond it, \
                 and inconclusive otherwise. A star marks a repeatable change, one whose 95% CI \
                 excludes 0, where the verdict is within the margin or inconclusive."
            ),
            format!("The median ratio is the median over the pairs of {candidate} / {baseline}."),
        ]);
        let notes = self.notes();
        if !notes.is_empty() {
            lines.push(String::new());
            lines.extend(notes);
        }

        lines.join("\n") + "\n"
    }

    /// The comparison as JSON, with every number.
    ///
    /// It has the shape and the names of the reports of `scripts/paired-stats.py` of dbus-bench,
    /// its `source` first, which is left out if the comparison has none, less the paired t
    /// interval and the Wilcoxon test of the script. Its bootstrap names the generator of its
    /// draws, which the script's did not, as it drew them from Python's.
    pub fn json(&self) -> Value {
        let source = self.source.iter().map(|source| ("source", source.clone()));
        let fields = [
            ("baseline", json!(self.baseline)),
            ("candidate", json!(self.candidate)),
            ("margin_pct", json!(self.margin)),
            (
                "bootstrap",
                json!({
                    "resamples": RESAMPLES,
                    "seed": SEED,
                    "generator": "fastrand 2 (wyrand): Rng::with_seed(seed), usize(..n)",
                    "intervals": "percentile, linear interpolation",
                }),
            ),
            (
                "rounds",
                json!({
                    "paired": self.paired_rounds,
                    "only_baseline": self.only_baseline,
                    "only_candidate": self.only_candidate,
                }),
            ),
            ("not_compared", json!(self.not_compared)),
            (
                "metrics",
                self.metrics.iter().map(MetricComparison::json).collect(),
            ),
        ];
        let report = source
            .chain(fields)
            .map(|(name, value)| (name.to_owned(), value));

        Value::Object(report.collect())
    }
}

impl Verdict {
    /// The verdict in words, by `margin`, the change in percent that does not matter:
    /// `within ±M%`, M being `margin`, `worse`, `better` or `inconclusive`.
    pub fn words(self, margin: f64) -> String {
        match self {
            Self::NoChange => format!("within ±{margin}%"),
            Self::Larger => "worse".to_owned(),
            Self::Smaller => "better".to_owned(),
            Self::Inconclusive => "inconclusive".to_owned(),
        }
    }
}

/// Compares the measured runs of `candidate` with those of `baseline` in `runs`, the runs of a
/// results file, round by round.
///
/// A run is an object with a `label`, a `round`, a whole number that is not negative, and, for a
/// warmup run, which is left out, a `warmup` that is true. Its other fields that are a number (a
/// bool is not) are its metrics.
///
/// `metrics` names the fields to compare, over the pairs whose runs both have a number there;
/// without it, or with no name in it, every field that is a number in every measured run of both
/// labels is, but `round` and `broker_exit`, in the order that they are first seen in the runs,
/// which is that of the results file, as serde_json's `preserve_order` feature keeps it, and as
/// paired-stats.py has it. `margin` is the change, in percent, that does not matter.
///
/// The comparison has no `source`, which the caller sets if it knows the results file.
///
/// Fails if the labels are the same, if the margin is negative or no number, if either label has no
/// measured run, two in a round or one without a round, if they have no measured round in common,
/// if there is no metric to compare, or if a metric is a number in no pair.
pub fn compare(
    runs: &[Value],
    baseline: &str,
    candidate: &str,
    metrics: Option<&[String]>,
    margin: f64,
) -> Result<Comparison> {
    if baseline == candidate {
        return Err(format!("the baseline and the candidate are both {baseline:?}").into());
    }
    if !margin.is_finite() || margin < 0.0 {
        return Err(format!("the margin is {margin}%, not 0% or more").into());
    }
    let base_runs = measured_runs(runs, baseline)?;
    let cand_runs = measured_runs(runs, candidate)?;
    let paired_rounds: Vec<u64> = base_runs
        .keys()
        .copied()
        .filter(|round| cand_runs.contains_key(round))
        .collect();
    let only_baseline = base_runs
        .keys()
        .copied()
        .filter(|round| !cand_runs.contains_key(round))
        .collect();
    let only_candidate = cand_runs
        .keys()
        .copied()
        .filter(|round| !base_runs.contains_key(round))
        .collect();
    if paired_rounds.is_empty() {
        return Err(
            format!("{baseline:?} and {candidate:?} have no measured round in common").into(),
        );
    }

    let all_runs: Vec<_> = base_runs
        .values()
        .chain(cand_runs.values())
        .copied()
        .collect();
    let (names, not_compared) = match metrics {
        Some(names) if !names.is_empty() => {
            let mut unique: Vec<String> = Vec::new();
            for name in names {
                if !unique.contains(name) {
                    unique.push(name.clone());
                }
            }
            (unique, BTreeMap::new())
        }
        _ => default_metrics(&all_runs),
    };
    if names.is_empty() {
        let message = "no field is a number in every measured run of both labels: name the \
                       metrics to compare";
        return Err(message.into());
    }
    let numeric = |name: &str| {
        all_runs
            .iter()
            .any(|run| run.get(name).and_then(number).is_some())
    };
    if let Some(name) = names.iter().find(|name| !numeric(name)) {
        let known = known_metrics(&all_runs).join(", ");
        return Err(format!(
            "{name:?} is a number in no measured run of {baseline:?} or {candidate:?}, whose \
             numeric fields are: {known}"
        )
        .into());
    }

    let mut bootstrap = Bootstrap::default();
    let mut compared = Vec::with_capacity(names.len());
    for metric in &names {
        let (base, cand): (Vec<f64>, Vec<f64>) = paired_rounds
            .iter()
            .filter_map(|round| {
                let base = base_runs[round].get(metric).and_then(number)?;
                let cand = cand_runs[round].get(metric).and_then(number)?;
                Some((base, cand))
            })
            .unzip();
        if base.is_empty() {
            return Err(format!("{metric:?} is a number in no round of both labels").into());
        }
        let mut comparison = analyze(metric, &base, &cand, margin, &mut bootstrap);
        let lacking = paired_rounds.len() - base.len();
        if lacking > 0 {
            let paired = paired_rounds.len();
            comparison.notes.push(format!(
                "{lacking} of the {paired} paired rounds lack this field in a run"
            ));
        }
        compared.push(comparison);
    }

    Ok(Comparison {
        source: None,
        baseline: baseline.to_owned(),
        candidate: candidate.to_owned(),
        margin,
        paired_rounds,
        only_baseline,
        only_candidate,
        not_compared,
        metrics: compared,
    })
}

/// Checks the statistics against values worked out by hand or by other tools, and fails on the
/// first that differs.
///
/// The checks are those of `scripts/paired-stats.py` of dbus-bench that still apply here, and
/// others: of the sums, the means, the medians and the quantiles, of the verdicts, of the analysis
/// of a metric, of `compare` on runs made up for it, with the errors that it must report, of the
/// Markdown and the JSON of a comparison, and of the bootstrap.
pub fn self_test() -> Result<()> {
    check_statistics()?;
    check_verdicts()?;
    check_analysis()?;
    check_comparison()?;
    check_reports()?;
    check_bootstrap()
}

/// The rounds of a comparison in words: how many pairs it has, from which rounds, and the rounds
/// that are left out, as only one of the labels has a run in them.
///
/// The rounds are in increasing order.
fn describe_rounds(paired: &[u64], only_baseline: &[u64], only_candidate: &[u64]) -> String {
    let mut parts = vec![match paired {
        [] => "no pair".to_owned(),
        [round] => format!("1 pair (round {round})"),
        [first, .., last] => format!("{} pairs (rounds {first} to {last})", paired.len()),
    }];
    for (label, left_out) in [("baseline", only_baseline), ("candidate", only_candidate)] {
        let listed: Vec<String> = left_out.iter().map(u64::to_string).collect();
        let listed = listed.join(", ");
        match left_out {
            [] => {}
            [_] => parts.push(format!(
                "left out, as only the {label} has a run in it: round {listed}"
            )),
            _ => parts.push(format!(
                "left out, as only the {label} has a run in them: rounds {listed}"
            )),
        }
    }

    parts.join("; ")
}

impl Comparison {
    /// The notes of the comparison, each an item of a Markdown list: whether there are too few
    /// pairs, the fields that are not compared, and the notes of the metrics.
    fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.paired_rounds.len() < FEW {
            notes.push(format!(
                "- the intervals are too narrow with fewer than {FEW} pairs: take {ENOUGH} rounds \
                 or more"
            ));
        }
        if !self.not_compared.is_empty() {
            // A label has a single measured run in a round, or the comparison would have failed.
            let runs =
                2 * self.paired_rounds.len() + self.only_baseline.len() + self.only_candidate.len();
            let fields: Vec<String> = self
                .not_compared
                .iter()
                .map(|(name, lacking)| match lacking {
                    1 => format!("{name} (1 run)"),
                    _ => format!("{name} ({lacking} runs)"),
                })
                .collect();
            let fields = fields.join(", ");
            notes.push(format!(
                "- not compared, as some of the {runs} runs have no number there: {fields}"
            ));
        }
        for metric in &self.metrics {
            for note in &metric.notes {
                notes.push(format!("- {}: {note}", metric.metric));
            }
        }

        notes
    }
}

/// The number of pairs below which the notes of a comparison, and the summary, say that its
/// intervals are too narrow.
pub const FEW: usize = 10;

/// The number of rounds that the notes of a comparison advise when it has too few pairs.
const ENOUGH: usize = 15;

impl MetricComparison {
    /// The row of the metric in the first table of `Comparison::markdown`, with its means, the
    /// percent difference, its 95% and 90% intervals and the verdict, by `margin`.
    fn row(&self, margin: f64) -> String {
        let unit = Unit::of(&self.metric, self.baseline_mean);

        format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            self.metric,
            self.n,
            unit.show(self.baseline_mean),
            unit.show(self.candidate_mean),
            show_pct(self.pct),
            show_interval(self.ci95),
            show_interval(self.ci90),
            self.verdict_text(margin),
        )
    }

    /// The verdict as the tables show it: `within ±M%`, M being `margin`, `**worse**`,
    /// `**better**` or `inconclusive`, with a star for a repeatable change that the verdict does
    /// not tell of, one within the margin or inconclusive.
    fn verdict_text(&self, margin: f64) -> String {
        let words = self.verdict.words(margin);
        match self.verdict {
            Verdict::Larger | Verdict::Smaller => format!("**{words}**"),
            // A change beyond the margin is repeatable by definition, which its verdict says.
            Verdict::NoChange | Verdict::Inconclusive if self.repeatable => words + "*",
            Verdict::NoChange | Verdict::Inconclusive => words,
        }
    }

    /// The row of the metric in the second table of `Comparison::markdown`, with its medians and
    /// the median ratio.
    fn medians_row(&self) -> String {
        let unit = Unit::of(&self.metric, self.baseline_mean);
        let ratio = self
            .median_ratio
            .map_or_else(|| "-".to_owned(), |ratio| format!("{ratio:.3}"));

        format!(
            "| {} | {} | {} | {ratio} |",
            self.metric,
            unit.show(self.baseline_median),
            unit.show(self.candidate_median),
        )
    }

    /// The comparison of the metric as JSON, as `Comparison::json` has it.
    fn json(&self) -> Value {
        json!({
            "metric": self.metric,
            "n": self.n,
            "baseline": {"mean": self.baseline_mean, "median": self.baseline_median},
            "candidate": {"mean": self.candidate_mean, "median": self.candidate_median},
            "pct": {
                "value": self.pct,
                "ci95": self.ci95,
                "ci90": self.ci90,
                "resamples": self.resamples,
                "skipped": self.skipped,
            },
            "median_ratio": self.median_ratio,
            "verdict": self.verdict.name(),
            "repeatable_change": self.repeatable,
            "notes": self.notes,
        })
    }
}

/// How the tables of `Comparison::markdown` show the values of a metric, in the unit that the end
/// of its name says.
struct Unit {
    /// What a value is multiplied by to be in the unit.
    scale: f64,
    /// How many decimals a value is shown with.
    decimals: usize,
    /// What follows a value: a space and the unit, if the metric has one.
    suffix: &'static str,
}

impl Unit {
    /// The unit of the values of `metric`, whose baseline mean is `mean`.
    ///
    /// A name that ends in `_s` is of seconds, shown with 3 decimals, or 2 from 10 s up, one that
    /// ends in `_us` of microseconds, shown whole, and one that ends in `_kib` of KiB, shown as MiB
    /// with 1 decimal. Any other, such as a count, is shown as it is, with 1 decimal from 100 up,
    /// or 3 below.
    fn of(metric: &str, mean: f64) -> Self {
        let (scale, decimals, suffix) = if metric.ends_with("_s") {
            (1.0, if mean.abs() >= 10.0 { 2 } else { 3 }, " s")
        } else if metric.ends_with("_us") {
            (1.0, 0, " µs")
        } else if metric.ends_with("_kib") {
            (1.0 / 1024.0, 1, " MiB")
        } else {
            (1.0, if mean.abs() >= 100.0 { 1 } else { 3 }, "")
        };

        Self {
            scale,
            decimals,
            suffix,
        }
    }

    /// `value`, a value of the metric, in the unit.
    fn show(&self, value: f64) -> String {
        format!("{:.*}{}", self.decimals, value * self.scale, self.suffix)
    }
}

/// A percent difference as the tables show it, such as `+3.6%`, or `-` if there is none.
fn show_pct(pct: Option<f64>) -> String {
    pct.map_or_else(|| "-".to_owned(), |pct| format!("{pct:+.1}%"))
}

/// An interval of a percent difference as the tables show it, such as `+2.06, +5.12`, or `-` if
/// there is none.
///
/// It has two decimals, as the verdict is by where its ends lie against the margin, which one
/// decimal can round them onto.
fn show_interval(interval: Option<[f64; 2]>) -> String {
    interval.map_or_else(
        || "-".to_owned(),
        |[low, high]| format!("{low:+.2}, {high:+.2}"),
    )
}

impl Verdict {
    /// The verdict as the JSON of a comparison names it: in the words of the reports of
    /// dbus-bench, so that the reports of the two can be read alike.
    fn name(self) -> &'static str {
        match self {
            Self::NoChange => "no material change",
            Self::Larger => "slower/larger",
            Self::Smaller => "faster/smaller",
            Self::Inconclusive => "inconclusive",
        }
    }
}

/// The measured runs of `label` in `runs`, by round: its runs that are not warmup runs.
///
/// Fails if the label has no measured run, two in a round, or one without a round number.
fn measured_runs<'r>(
    runs: &'r [Value],
    label: &str,
) -> Result<BTreeMap<u64, &'r Map<String, Value>>> {
    let mut measured = BTreeMap::new();
    for run in runs.iter().filter_map(Value::as_object) {
        if run.get("label").and_then(Value::as_str) != Some(label) || is_warmup(run) {
            continue;
        }
        let Some(round) = run.get("round").and_then(Value::as_u64) else {
            return Err(format!("a run of {label:?} has no round number").into());
        };
        if measured.insert(round, run).is_some() {
            return Err(format!(
                "{label:?} has two measured runs in round {round}: they cannot be paired"
            )
            .into());
        }
    }
    if measured.is_empty() {
        let labels: BTreeSet<&str> = runs
            .iter()
            .filter_map(Value::as_object)
            .filter(|run| !is_warmup(run))
            .filter_map(|run| run.get("label")?.as_str())
            .collect();
        let labels = if labels.is_empty() {
            "none".to_owned()
        } else {
            labels.into_iter().collect::<Vec<_>>().join(", ")
        };
        let message = format!("no measured run of {label:?}; the measured runs are of: {labels}");
        return Err(message.into());
    }

    Ok(measured)
}

/// Whether `run` is a warmup run, which comparisons leave out: one whose `warmup` is true.
fn is_warmup(run: &Map<String, Value>) -> bool {
    matches!(run.get("warmup"), Some(Value::Bool(true)))
}

/// The fields that are a number in every one of `runs`, in the order that they are first seen,
/// but those of `SKIPPED`, and how many of the runs lack a number in each of the others that are a
/// number in some of them.
fn default_metrics(runs: &[&Map<String, Value>]) -> (Vec<String>, BTreeMap<String, usize>) {
    let mut metrics = Vec::new();
    let mut lacking = BTreeMap::new();
    for name in known_metrics(runs) {
        if SKIPPED.contains(&name) {
            continue;
        }
        let missing = runs
            .iter()
            .filter(|run| run.get(name).and_then(number).is_none())
            .count();
        if missing == 0 {
            metrics.push(name.to_owned());
        } else {
            lacking.insert(name.to_owned(), missing);
        }
    }

    (metrics, lacking)
}

/// The fields of the runs that are a number but are no metrics: `round`, and `broker_exit`, which
/// is an exit status.
const SKIPPED: [&str; 2] = ["round", "broker_exit"];

/// The fields that are a number in some of `runs`, in the order that they are first seen, but
/// `round`.
fn known_metrics<'r>(runs: &[&'r Map<String, Value>]) -> Vec<&'r str> {
    let mut names: Vec<&str> = Vec::new();
    for run in runs {
        for (name, value) in *run {
            if name != "round" && number(value).is_some() && !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
    }

    names
}

/// `value` as a number, if it is a finite one, which a bool is not.
fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|value| value.is_finite())
}

/// The comparison of `metric`, from its values in the paired rounds: `base`, those of the baseline,
/// and `cand`, those of the candidate, in the same rounds, and at least one of each.
///
/// The intervals of the percent difference come from a bootstrap of the pairs, whose resamples
/// `bootstrap` draws, and the verdict from them and `margin`.
fn analyze(
    metric: &str,
    base: &[f64],
    cand: &[f64],
    margin: f64,
    bootstrap: &mut Bootstrap,
) -> MetricComparison {
    let n = base.len();
    let baseline_mean = mean(base);
    let candidate_mean = mean(cand);
    let ratios: Vec<f64> = base
        .iter()
        .zip(cand)
        .filter(|(base, _)| **base != 0.0)
        .map(|(base, cand)| cand / base)
        .collect();
    let mut comparison = MetricComparison {
        metric: metric.to_owned(),
        n,
        baseline_mean,
        candidate_mean,
        baseline_median: median(base),
        candidate_median: median(cand),
        pct: None,
        ci95: None,
        ci90: None,
        resamples: 0,
        skipped: 0,
        median_ratio: (!ratios.is_empty()).then(|| median(&ratios)),
        verdict: Verdict::Inconclusive,
        repeatable: false,
        notes: Vec::new(),
    };

    // Differences up to this size are those of rounding, such as of values counted in clock ticks.
    let largest = base
        .iter()
        .chain(cand)
        .fold(0.0, |largest: f64, x| largest.max(x.abs()));
    let tolerance = 1e-9 * largest;
    let same = base
        .iter()
        .zip(cand)
        .all(|(b, c)| (c - b).abs() <= tolerance);
    if same {
        // The same in every pair, even if that is 0, which has no ratio.
        comparison.pct = Some(0.0);
        if n >= 2 {
            comparison.ci95 = Some([0.0, 0.0]);
            comparison.ci90 = Some([0.0, 0.0]);
        }
    } else if baseline_mean <= 0.0 {
        comparison.notes.push(
            "the baseline mean is not positive, so there is no percent difference".to_owned(),
        );
    } else {
        comparison.pct = Some(100.0 * (candidate_mean / baseline_mean - 1.0));
        if n >= 2 {
            let (values, skipped) = bootstrap.percent(base, cand);
            comparison.resamples = values.len();
            comparison.skipped = skipped;
            if skipped > 0 {
                comparison.notes.push(format!(
                    "{skipped} bootstrap resamples had a baseline mean that is not positive and \
                     are left out"
                ));
            }
            if !values.is_empty() {
                comparison.ci95 = Some([quantile(&values, 0.025), quantile(&values, 0.975)]);
                comparison.ci90 = Some([quantile(&values, 0.05), quantile(&values, 0.95)]);
            }
        }
    }
    if n < 2 {
        comparison
            .notes
            .push("one pair only: no intervals".to_owned());
    }
    (comparison.verdict, comparison.repeatable) =
        classify(comparison.ci95, comparison.ci90, margin);

    comparison
}

/// The mean of `values`, at least one, from their sum correctly rounded, as Python's
/// `statistics.fmean` takes it.
fn mean(values: &[f64]) -> f64 {
    fsum(values) / values.len() as f64
}

/// The sum of `values`, correctly rounded, by Shewchuk's algorithm, as Python's `math.fsum` has it,
/// for values whose sums do not overflow.
///
/// Partial sums that do not overlap, in increasing size, hold the exact sum of the values so far:
/// each value is added to them in turn, each addition rounded, but with its rounding error kept as
/// a partial of its own. Their sum is then taken from the largest down, for as long as it is exact,
/// and rounded half to even.
fn fsum(values: &[f64]) -> f64 {
    let mut partials: Vec<f64> = Vec::new();
    let mut next = Vec::new();
    for &value in values {
        let mut sum = value;
        for &partial in &partials {
            let (large, small) = if sum.abs() < partial.abs() {
                (partial, sum)
            } else {
                (sum, partial)
            };
            sum = large + small;
            let error = small - (sum - large);
            if error != 0.0 {
                next.push(error);
            }
        }
        if sum != 0.0 {
            next.push(sum);
        }
        std::mem::swap(&mut partials, &mut next);
        next.clear();
    }

    let Some((&largest, smaller)) = partials.split_last() else {
        return 0.0;
    };
    let mut sum = largest;
    let mut error = 0.0;
    // The partials that are not in the sum yet are those below `left`.
    let mut left = smaller.len();
    while left > 0 {
        left -= 1;
        let large = sum;
        sum = large + smaller[left];
        error = smaller[left] - (sum - large);
        if error != 0.0 {
            break;
        }
    }
    // The last addition rounded its error away. If that error was half a unit in the last place of
    // the sum, a tie, which was rounded to even, but the partials below it have its sign, the exact
    // sum lies beyond the tie, and rounds the other way.
    if left > 0
        && (error < 0.0 && smaller[left - 1] < 0.0 || error > 0.0 && smaller[left - 1] > 0.0)
    {
        let twice = error * 2.0;
        let rounded = sum + twice;
        if twice == rounded - sum {
            sum = rounded;
        }
    }

    sum
}

/// The median of `values`, at least one: the middle one once they are sorted, or the mean of the
/// two in the middle, as Python's `statistics.median` takes it.
fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// The percentile bootstrap of the metrics of a comparison.
///
/// The indices of the pairs in its resamples depend only on the number of pairs, so they are drawn
/// once for each number of pairs and shared by the metrics that have that many, which come out the
/// same as if each drew its own. The draws are those of fastrand's generator from the seed `SEED`,
/// on a 64-bit target (fastrand draws an index from another stream on a 32-bit one).
#[derive(Default)]
struct Bootstrap {
    /// The indices of the pairs in each resample, `RESAMPLES` of them one after another, by the
    /// number of pairs.
    drawn: BTreeMap<usize, Vec<usize>>,
}

impl Bootstrap {
    /// `100 * (candidate mean / baseline mean - 1)` in each resample of the pairs, whose baseline
    /// values are `base` and candidate values `cand`, in increasing order, and how many resamples
    /// have no such value, as their baseline mean is not positive.
    fn percent(&mut self, base: &[f64], cand: &[f64]) -> (Vec<f64>, usize) {
        let n = base.len();
        let mut values = Vec::with_capacity(RESAMPLES);
        let mut skipped = 0;
        for resample in self.indices(n).chunks_exact(n) {
            // The ratio of the sums is that of the means.
            let base_sum: f64 = resample.iter().map(|&i| base[i]).sum();
            if base_sum <= 0.0 {
                skipped += 1;
                continue;
            }
            let cand_sum: f64 = resample.iter().map(|&i| cand[i]).sum();
            values.push(100.0 * (cand_sum / base_sum - 1.0));
        }
        values.sort_by(f64::total_cmp);

        (values, skipped)
    }

    /// The indices of the pairs in each resample of `n` pairs, one resample after another: `n`
    /// draws with replacement from `0..n` each.
    fn indices(&mut self, n: usize) -> &[usize] {
        self.drawn.entry(n).or_insert_with(|| {
            let mut rng = fastrand::Rng::with_seed(SEED);
            (0..RESAMPLES * n).map(|_| rng.usize(..n)).collect()
        })
    }
}

/// How many resamples the bootstrap draws.
const RESAMPLES: usize = 10_000;

/// The seed of the bootstrap's draws, which is always the same, so that a comparison can be made
/// again exactly.
const SEED: u64 = 1;

/// The quantile `q` of `sorted`, values in increasing order, at least one, by linear interpolation
/// between the order statistics, as numpy's default has it.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    let last = sorted.len() - 1;
    let position = q * last as f64;
    let below = position.floor() as usize;
    let above = (below + 1).min(last);

    sorted[below] + (sorted[above] - sorted[below]) * (position - below as f64)
}

/// The verdict on a change in percent, by its 95% and 90% intervals and `margin`, and whether it
/// is a repeatable change.
fn classify(ci95: Option<[f64; 2]>, ci90: Option<[f64; 2]>, margin: f64) -> (Verdict, bool) {
    let (Some(ci95), Some(ci90)) = (ci95, ci90) else {
        return (Verdict::Inconclusive, false);
    };
    let repeatable = ci95[0] > 0.0 || ci95[1] < 0.0;
    // The 90% interval lies within the 95% one, so at most one of the first three holds.
    let verdict = if ci90[0] > -margin && ci90[1] < margin {
        Verdict::NoChange
    } else if ci95[0] > margin {
        Verdict::Larger
    } else if ci95[1] < -margin {
        Verdict::Smaller
    } else {
        Verdict::Inconclusive
    };

    (verdict, repeatable)
}

/// Checks the sums, the means, the medians and the quantiles against Python's: those of
/// `math.fsum`, `statistics.fmean`, `statistics.median` and of the quantiles of
/// `scripts/paired-stats.py`, which are numpy's but for rounding. Then checks the order of the
/// fields that are a number.
fn check_statistics() -> Result<()> {
    // From left to right, the sum is 0.9999999999999999, not 1.
    let tenths = [0.1; 10];
    // The sum is rounded half to even across the partials: to 1e16 + 2, not 1e16.
    let tiny = [1e-16, 1.0, 1e16];
    let cancelling = [1e100, 1.0, -1e100, 1e-100, 1e50, -1.0, -1e50];
    let four = [1.0, 2.0, 3.0, 4.0];
    let eleven: Vec<f64> = (0..=10).map(f64::from).collect();
    let spread = [1.0, 3.0, 7.0, 15.0];
    let cases = [
        ("mean(tenths)", mean(&tenths), 0.1),
        ("fsum(tiny)", fsum(&tiny), 1.0000000000000002e16),
        ("fsum(cancelling)", fsum(&cancelling), 1e-100),
        ("fsum([])", fsum(&[]), 0.0),
        ("median([3, 1, 2])", median(&[3.0, 1.0, 2.0]), 2.0),
        ("median([4, 1, 3, 2])", median(&[4.0, 1.0, 3.0, 2.0]), 2.5),
        ("quantile(four, 0.25)", quantile(&four, 0.25), 1.75),
        ("quantile(eleven, 0.9)", quantile(&eleven, 0.9), 9.0),
        ("quantile(spread, 0.025)", quantile(&spread, 0.025), 1.15),
        (
            "quantile(spread, 0.975)",
            quantile(&spread, 0.975),
            14.399999999999999,
        ),
        ("quantile([5], 0.3)", quantile(&[5.0], 0.3), 5.0),
    ];
    for (name, got, want) in cases {
        check_eq(name, got, want)?;
    }

    // The fields of a run keep their order, which is not that of their names, and a field that a
    // later run has comes after those of the first, whatever its name.
    let first = json!({"c_s": 1, "round": 1, "label": "x", "b_s": 0.5});
    let second = json!({"a_s": 2.5, "c_s": 3, "flag": true});
    let runs: Vec<_> = [&first, &second]
        .into_iter()
        .filter_map(Value::as_object)
        .collect();
    check_eq(
        "the numeric fields",
        known_metrics(&runs),
        ["c_s", "b_s", "a_s"],
    )
}

/// Checks the verdicts, by the intervals and the margin, in words, and as the tables show them.
fn check_verdicts() -> Result<()> {
    use Verdict::{Inconclusive, Larger, NoChange, Smaller};

    // The 95% interval, the 90% one, and what they make of a change by a margin of 5%.
    let cases = [
        ([-1.4, 1.9], [-1.0, 1.5], NoChange, false),
        ([0.5, 3.0], [0.8, 2.7], NoChange, true),
        ([-3.0, -0.5], [-2.7, -0.8], NoChange, true),
        ([5.2, 9.0], [5.5, 8.5], Larger, true),
        ([-9.0, -5.2], [-8.5, -5.5], Smaller, true),
        ([-2.0, 7.0], [-1.0, 6.0], Inconclusive, false),
        ([3.0, 9.0], [3.5, 8.0], Inconclusive, true),
        // The 90% interval starts at the margin.
        ([4.0, 8.0], [5.0, 7.0], Inconclusive, true),
        // The 95% interval starts at the margin.
        ([5.0, 8.0], [5.5, 7.0], Inconclusive, true),
        ([-4.9, 4.9], [-4.5, 4.5], NoChange, false),
        ([-4.9, 4.9], [-4.5, 5.0], Inconclusive, false),
    ];
    for (ci95, ci90, verdict, repeatable) in cases {
        let name = format!("the verdict on a 95% CI of {ci95:?} and a 90% CI of {ci90:?}");
        let got = classify(Some(ci95), Some(ci90), 5.0);
        check_eq(&name, got, (verdict, repeatable))?;
    }
    let none = classify(None, None, 5.0);
    check_eq("the verdict without CIs", none, (Inconclusive, false))?;
    let words = [
        (NoChange.words(2.5), "within ±2.5%"),
        (Larger.words(5.0), "worse"),
        (Smaller.words(5.0), "better"),
        (Inconclusive.words(5.0), "inconclusive"),
    ];
    for (got, want) in words {
        check_eq("a verdict in words", got, want)?;
    }

    let four = [1.0, 2.0, 3.0, 4.0];
    let more = [1.1, 2.2, 3.3, 4.4];
    let less = [0.9, 1.8, 2.7, 3.6];
    let little = [1.02, 2.04, 3.06, 4.08];
    // Each pair has 2% or 8% more: the 95% interval spans the margin, but not 0.
    let mixed = [1.02, 2.16, 3.06, 4.32];
    let mut bootstrap = Bootstrap::default();
    let mut verdict = |cand: &[f64], margin| {
        let compared = analyze("x", &four[..cand.len()], cand, margin, &mut bootstrap);
        compared.verdict_text(margin)
    };
    let cases = [
        (verdict(&more, 5.0), "**worse**"),
        (verdict(&less, 5.0), "**better**"),
        (verdict(&four, 5.0), "within ±5%"),
        (verdict(&four, 2.5), "within ±2.5%"),
        (verdict(&little, 5.0), "within ±5%*"),
        (verdict(&mixed, 5.0), "inconclusive*"),
        (verdict(&[2.0], 5.0), "inconclusive"),
    ];
    for (got, want) in cases {
        check_eq("a verdict as the tables show it", got, want)?;
    }

    Ok(())
}

/// Checks the analysis of a metric, from values whose numbers are exact or known.
fn check_analysis() -> Result<()> {
    use Verdict::{Inconclusive, Larger, NoChange};

    let mut bootstrap = Bootstrap::default();
    let mut analyzed = |base: &[f64], cand: &[f64]| analyze("x", base, cand, 5.0, &mut bootstrap);
    let four = [1.0, 2.0, 3.0, 4.0];

    let exact = analyzed(&four, &[2.0, 2.0, 6.0, 8.0]);
    check_eq("exact: n", exact.n, 4)?;
    check_eq("exact: baseline mean", exact.baseline_mean, 2.5)?;
    check_eq("exact: candidate mean", exact.candidate_mean, 4.5)?;
    check_eq("exact: baseline median", exact.baseline_median, 2.5)?;
    check_eq("exact: candidate median", exact.candidate_median, 4.0)?;
    check_eq("exact: diff %", exact.pct, Some(80.0))?;
    check_eq("exact: median ratio", exact.median_ratio, Some(2.0))?;

    let same = analyzed(&four, &four);
    check_eq("identical: diff %", same.pct, Some(0.0))?;
    check_eq("identical: 95% CI", same.ci95, Some([0.0, 0.0]))?;
    check_eq("identical: 90% CI", same.ci90, Some([0.0, 0.0]))?;
    check_eq("identical: resamples", same.resamples, 0)?;
    check_eq("identical: verdict", same.verdict, NoChange)?;
    check("identical: no repeatable change", !same.repeatable)?;
    // The candidate differs by rounding alone.
    let rounding = analyzed(&[1.0, 2.0], &[1.0 + 1e-12, 2.0]);
    check_eq("rounding: diff %", rounding.pct, Some(0.0))?;
    check_eq("rounding: 95% CI", rounding.ci95, Some([0.0, 0.0]))?;
    let zeros = analyzed(&[0.0, 0.0], &[0.0, 0.0]);
    check_eq("zeros: diff %", zeros.pct, Some(0.0))?;
    check_eq("zeros: median ratio", zeros.median_ratio, None)?;

    let tenth = analyzed(&four, &[1.1, 2.2, 3.3, 4.4]);
    check_close("+10%: diff %", tenth.pct, 10.0, 1e-9)?;
    check_interval("+10%: 95% CI", tenth.ci95, [10.0, 10.0], 1e-9)?;
    check_interval("+10%: 90% CI", tenth.ci90, [10.0, 10.0], 1e-9)?;
    check_eq("+10%: resamples", tenth.resamples, RESAMPLES)?;
    check_eq("+10%: verdict", tenth.verdict, Larger)?;
    check("+10%: a repeatable change", tenth.repeatable)?;

    let one = analyzed(&[1.0], &[2.0]);
    let no_intervals = "one pair only: no intervals";
    check_eq("one pair: diff %", one.pct, Some(100.0))?;
    check_eq("one pair: 95% CI", one.ci95, None)?;
    check_eq("one pair: notes", one.notes, [no_intervals])?;
    check_eq("one pair: verdict", one.verdict, Inconclusive)?;

    let negative = analyzed(&[-1.0, -2.0], &[1.0, 2.0]);
    let no_pct = "the baseline mean is not positive, so there is no percent difference";
    check_eq("negative: diff %", negative.pct, None)?;
    check_eq("negative: 95% CI", negative.ci95, None)?;
    check_eq("negative: notes", negative.notes, [no_pct])?;
    check_eq("negative: verdict", negative.verdict, Inconclusive)?;
    let zero = analyzed(&[0.0, 0.0], &[1.0, 1.0]);
    check_eq("zero baseline: diff %", zero.pct, None)?;
    check_eq("zero baseline: median ratio", zero.median_ratio, None)?;

    // A resample of the first three pairs alone, (3/4)^4 of them, has a baseline mean of 0.
    let mostly = analyzed(&[0.0, 0.0, 0.0, 1.0], &[1.0, 1.0, 1.0, 2.0]);
    let skipped = mostly.skipped;
    check_eq("mostly zeros: diff %", mostly.pct, Some(400.0))?;
    check_eq("mostly zeros: median ratio", mostly.median_ratio, Some(2.0))?;
    let drawn = mostly.resamples + skipped;
    check_eq(
        "mostly zeros: resamples, with those left out",
        drawn,
        RESAMPLES,
    )?;
    let about = skipped.abs_diff(3164) < 200;
    check(
        &format!("mostly zeros: {skipped} resamples left out, about 3164"),
        about,
    )?;
    let note = format!(
        "{skipped} bootstrap resamples had a baseline mean that is not positive and are left out"
    );
    check_eq("mostly zeros: notes", mostly.notes, [note])
}

/// Checks `compare` on the runs of `sample_runs`, and the errors that it must report.
fn check_comparison() -> Result<()> {
    let runs = sample_runs();
    let comparison = compare(&runs, "old", "new", None, 5.0)?;
    check_eq("paired rounds", &comparison.paired_rounds, &[1, 2, 3])?;
    check_eq(
        "rounds of the baseline only",
        &comparison.only_baseline,
        &[4],
    )?;
    check_eq(
        "rounds of the candidate only",
        &comparison.only_candidate,
        &[5],
    )?;
    let names: Vec<&str> = comparison
        .metrics
        .iter()
        .map(|m| m.metric.as_str())
        .collect();
    let want = ["count", "lat_us", "rss_kib", "wall_s", "x_s"];
    check_eq("metrics", names, want)?;
    let lacking = json!(comparison.not_compared);
    check_eq("not compared", lacking, json!({"lonely": 7, "sparse": 4}))?;
    check(
        "no comparison of sparse",
        comparison.metric("sparse").is_none(),
    )?;
    let x = required("x_s", comparison.metric("x_s"))?;
    check_eq("x_s: n", x.n, 3)?;
    check_eq("x_s: baseline mean", x.baseline_mean, 2.0)?;
    check_close("x_s: diff %", x.pct, 10.0, 1e-9)?;

    let named = ["x_s", "sparse", "x_s"].map(str::to_owned);
    let comparison = compare(&runs, "old", "new", Some(&named), 5.0)?;
    let names: Vec<&str> = comparison
        .metrics
        .iter()
        .map(|m| m.metric.as_str())
        .collect();
    check_eq("metrics named", names, ["x_s", "sparse"])?;
    check(
        "not compared, of metrics named",
        comparison.not_compared.is_empty(),
    )?;
    let sparse = required("sparse", comparison.metric("sparse"))?;
    check_eq("sparse: n", sparse.n, 1)?;
    let notes = [
        "one pair only: no intervals",
        "2 of the 3 paired rounds lack this field in a run",
    ];
    check_eq("sparse: notes", &sparse.notes, &notes)?;
    let none: &[String] = &[];
    let default = compare(&runs, "old", "new", Some(none), 5.0)?;
    check_eq("metrics of an empty list", default.metrics.len(), 5)?;

    let named = |metric: &str| compare(&runs, "old", "new", Some(&[metric.to_owned()]), 5.0);
    let mut late = runs.clone();
    late.push(json!({"label": "late", "round": 9, "x_s": 1.0}));
    let mut roundless = late.clone();
    roundless.push(json!({"label": "late", "warmup": false, "x_s": 1.0}));
    let mut twice = runs.clone();
    twice.push(json!({"label": "old", "round": 2, "x_s": 1.0}));
    let apart = [
        json!({"label": "a", "round": 1, "x_s": 1.0}),
        json!({"label": "b", "round": 1, "y_s": 1.0}),
    ];
    let errors = [
        (
            named("flag"),
            "\"flag\" is a number in no measured run of \"old\" or \"new\"",
        ),
        // The fields are in the order of the runs, those of the baseline first: `sparse` was set
        // on the first run after its other fields, and the run of round 4 alone has `lonely`.
        (
            named("none"),
            "fields are: broker_exit, count, lat_us, rss_kib, wall_s, x_s, sparse, lonely",
        ),
        (
            named("lonely"),
            "\"lonely\" is a number in no round of both labels",
        ),
        (
            compare(&runs, "old", "old", None, 5.0),
            "the candidate are both \"old\"",
        ),
        (
            compare(&runs, "old", "new", None, -1.0),
            "the margin is -1%, not 0% or more",
        ),
        (
            compare(&runs, "old", "new", None, f64::NAN),
            "the margin is NaN%",
        ),
        (
            compare(&runs, "old", "gone", None, 5.0),
            "\"gone\"; the measured runs are of: new, old, other",
        ),
        (
            compare(&late, "old", "late", None, 5.0),
            "\"old\" and \"late\" have no measured round in",
        ),
        (
            compare(&roundless, "old", "late", None, 5.0),
            "a run of \"late\" has no round number",
        ),
        (
            compare(&twice, "old", "new", None, 5.0),
            "\"old\" has two measured runs in round 2",
        ),
        (
            compare(&apart, "a", "b", None, 5.0),
            "no field is a number in every measured run",
        ),
    ];
    for (compared, message) in errors {
        check_error(compared, message)?;
    }

    Ok(())
}

/// Checks the Markdown and the JSON of the comparison of the runs of `sample_runs`.
fn check_reports() -> Result<()> {
    let runs = sample_runs();
    let mut comparison = compare(&runs, "old", "new", None, 5.0)?;
    let markdown = comparison.markdown();
    let lines = [
        "### new against old: 3 pairs (rounds 1 to 3); left out, as only the baseline has a run in \
         it: round 4; left out, as only the candidate has a run in it: round 5",
        "| Metric | n | old mean | new mean | Diff | 95% CI | 90% CI | Verdict |",
        "| count | 3 | 200.0 | 200.0 | +0.0% | +0.00, +0.00 | +0.00, +0.00 | within ±5% |",
        "| lat_us | 3 | 10 µs | 12 µs | +20.0% | +20.00, +20.00 | +20.00, +20.00 | **worse** |",
        "| rss_kib | 3 | 1.0 MiB | 2.0 MiB | +100.0% | +100.00, +100.00 | +100.00, +100.00 | \
         **worse** |",
        "| wall_s | 3 | 20.00 s | 10.00 s | -50.0% | -50.00, -50.00 | -50.00, -50.00 | \
         **better** |",
        "| x_s | 3 | 2.000 s | 2.200 s | +10.0% | +10.00, +10.00 | +10.00, +10.00 | **worse** |",
        "| Metric | old median | new median | Median ratio |",
        "| x_s | 2.000 s | 2.200 s | 1.100 |",
        "Diff is 100 * (new mean / old mean - 1), less being better, and its CIs are from a \
         percentile bootstrap of the pairs, of 10000 resamples drawn by fastrand's generator from \
         the seed 1.",
        "- the intervals are too narrow with fewer than 10 pairs: take 15 rounds or more",
        "- not compared, as some of the 8 runs have no number there: lonely (7 runs), sparse (4 \
         runs)",
    ];
    for line in lines {
        let found = markdown.lines().any(|l| l == line);
        check(&format!("the Markdown has the line {line:?}"), found)?;
    }
    let lacking_once = [
        json!({"label": "a", "round": 1, "x_s": 1.0, "y_s": 1.0}),
        json!({"label": "b", "round": 1, "x_s": 1.0}),
    ];
    let markdown = compare(&lacking_once, "a", "b", None, 5.0)?.markdown();
    let line = "- not compared, as some of the 2 runs have no number there: y_s (1 run)";
    let found = markdown.lines().any(|l| l == line);
    check(&format!("the Markdown has the line {line:?}"), found)?;
    let texts = [
        (describe_rounds(&[7], &[], &[]), "1 pair (round 7)"),
        (
            describe_rounds(&[1, 2, 3], &[4, 6], &[]),
            "3 pairs (rounds 1 to 3); left out, as only the baseline has a run in them: \
             rounds 4, 6",
        ),
        (Unit::of("count", 99.0).show(1.23456), "1.235"),
        (Unit::of("wall_s", 9.9).show(10.0), "10.000 s"),
    ];
    for (got, want) in texts {
        check_eq("a text of the Markdown", got, want)?;
    }

    let mut report = comparison.json();
    let metrics = report["metrics"].take();
    let want = json!({
        "baseline": "old", "candidate": "new", "margin_pct": 5.0,
        "bootstrap": {
            "resamples": 10000, "seed": 1,
            "generator": "fastrand 2 (wyrand): Rng::with_seed(seed), usize(..n)",
            "intervals": "percentile, linear interpolation",
        },
        "rounds": {"paired": [1, 2, 3], "only_baseline": [4], "only_candidate": [5]},
        "not_compared": {"lonely": 7, "sparse": 4}, "metrics": null,
    });
    // Without a source, the JSON has none.
    check_eq("the JSON, but its metrics", report, want)?;
    let source = json!({"file": "x.json", "started": "2026-10-31T14:05:09Z"});
    comparison.source = Some(source.clone());
    let report = comparison.json();
    check_eq("the source of the JSON", &report["source"], &source)?;
    // The objects are equal whatever the order of their fields, which is checked apart.
    let fields: Vec<&String> = report.as_object().into_iter().flat_map(Map::keys).collect();
    let want = [
        "source",
        "baseline",
        "candidate",
        "margin_pct",
        "bootstrap",
        "rounds",
        "not_compared",
        "metrics",
    ];
    check_eq("the fields of the JSON, in order", fields, want)?;
    let metrics = required("the metrics of the JSON", metrics.as_array())?;
    let count = json!({
        "metric": "count", "n": 3, "baseline": {"mean": 200.0, "median": 200.0},
        "candidate": {"mean": 200.0, "median": 200.0}, "median_ratio": 1.0,
        "pct": {"value": 0.0, "ci95": [0.0, 0.0], "ci90": [0.0, 0.0], "resamples": 0, "skipped": 0},
        "verdict": "no material change", "repeatable_change": false, "notes": [],
    });
    check_eq("the JSON of count", &metrics[0], &count)?;
    let x = &metrics[4];
    let got = json!([
        x["metric"],
        x["n"],
        x["baseline"],
        x["pct"]["resamples"],
        x["pct"]["skipped"],
        x["repeatable_change"],
        x["notes"],
    ]);
    let want = json!(["x_s", 3, {"mean": 2.0, "median": 2.0}, 10000, 0, true, []]);
    check_eq("the JSON of x_s", got, want)?;
    let verdicts: Vec<&Value> = metrics.iter().map(|metric| &metric["verdict"]).collect();
    let want = [
        "no material change",
        "slower/larger",
        "slower/larger",
        "faster/smaller",
        "slower/larger",
    ];
    check_eq("the verdicts of the JSON", verdicts, want)?;

    let report = compare(&runs, "old", "new", Some(&["sparse".to_owned()]), 5.0)?.json();
    let sparse = &report["metrics"][0];
    let got = json!([
        sparse["verdict"],
        sparse["pct"]["ci95"],
        sparse["median_ratio"]
    ]);
    check_eq(
        "the JSON of sparse",
        got,
        json!(["inconclusive", null, 7.0 / 6.0]),
    )
}

/// Runs of an old build and a new one, in rounds 1 to 3, and of either alone in a round of its
/// own, among runs of another label, a warmup run and an entry that is no run.
///
/// The new build takes 10% longer on `x_s`, 20% on `lat_us` and half as long on `wall_s`, uses
/// twice the memory of `rss_kib`, and has the same `count`. Some runs lack `sparse`, and a run of
/// the old build alone has `lonely`.
fn sample_runs() -> Vec<Value> {
    let measured = [
        ("old", 1, 1.0),
        ("new", 1, 1.1),
        ("old", 2, 2.0),
        ("new", 2, 2.2),
        ("old", 3, 3.0),
        ("new", 3, 3.3),
        ("old", 4, 4.0),
        ("new", 5, 5.5),
    ];
    let mut runs: Vec<Value> = measured
        .iter()
        .map(|&(label, round, x)| {
            let old = label == "old";
            let (lat, rss, wall) = if old {
                (10, 1024, 20.0)
            } else {
                (12, 2048, 10.0)
            };
            json!({
                "label": label, "round": round, "warmup": false, "broker_exit": 0, "count": 200,
                "flag": old, "lat_us": lat, "rss_kib": rss, "wall_s": wall, "x_s": x,
            })
        })
        .collect();
    for (run, sparse) in [(0, 5), (2, 6), (3, 7), (5, 8)] {
        runs[run]["sparse"] = json!(sparse);
    }
    runs[6]["lonely"] = json!(1);
    runs.extend([
        json!({"label": "old", "round": 1, "warmup": true, "x_s": 100.0}),
        json!({"label": "other", "round": 1, "warmup": false, "x_s": 9.0}),
        json!("no run"),
    ]);

    runs
}

/// Checks that the bootstrap comes out the same every time, whichever metrics share its draws,
/// and that it finds a change made up for it.
fn check_bootstrap() -> Result<()> {
    let runs = synthetic_runs();
    let both = compare(&runs, "base", "cand", None, 5.0)?;
    let again = compare(&runs, "base", "cand", None, 5.0)?;
    check(
        "the same comparison, made again",
        both.json() == again.json(),
    )?;
    let alone = compare(&runs, "base", "cand", Some(&["b_s".to_owned()]), 5.0)?;
    let alone = required("b_s alone", alone.metric("b_s"))?;
    let among = required("b_s", both.metric("b_s"))?;
    check_eq("b_s alone: 95% CI", alone.ci95, among.ci95)?;
    check_eq("b_s alone: 90% CI", alone.ci90, among.ci90)?;
    let [low, high] = required("b_s: 95% CI", among.ci95)?;
    check(
        &format!("b_s: the 95% CI [{low}, {high}] is no point"),
        low < high,
    )?;

    // A 95% interval misses the change in one draw of the runs in 20, so the check is that it comes
    // within 0.5 of it: for it not to, the percent difference, whose standard error is about 0.16
    // here, would have to be off by about 0.8.
    let a = required("a_s", both.metric("a_s"))?;
    check_close("a_s: diff %", a.pct, 2.0, 1.0)?;
    let [low, high] = required("a_s: 95% CI", a.ci95)?;
    let near_two = low < 2.5 && 1.5 < high;
    check(
        &format!("a_s: the 95% CI [{low:.2}, {high:.2}] near 2"),
        near_two,
    )?;
    check_eq("a_s: verdict", a.verdict, Verdict::NoChange)?;
    check("a_s: a repeatable change", a.repeatable)
}

/// The runs of 40 rounds of a baseline, `base`, and a candidate, `cand`, made up of random draws.
///
/// The candidate takes 2% longer than the baseline on `a_s`, and 3% less on `b_s`, give or take
/// as much noise as each takes.
fn synthetic_runs() -> Vec<Value> {
    let mut rng = fastrand::Rng::with_seed(20260101);
    let mut runs = Vec::new();
    for round in 0..40 {
        let a = gauss(&mut rng, 0.09, 0.0009);
        let b = gauss(&mut rng, 0.05, 0.001);
        let cand_a = 1.02 * a + gauss(&mut rng, 0.0, 0.0009);
        let cand_b = 0.97 * b + gauss(&mut rng, 0.0, 0.001);
        runs.push(json!({"label": "base", "round": round, "a_s": a, "b_s": b}));
        runs.push(json!({"label": "cand", "round": round, "a_s": cand_a, "b_s": cand_b}));
    }

    runs
}

/// A draw of `rng` from the normal distribution of mean `mean` and standard deviation `sd`, by
/// the Box-Muller transform of two uniform draws.
fn gauss(rng: &mut fastrand::Rng, mean: f64, sd: f64) -> f64 {
    // 1 - f64() is in (0, 1], so its logarithm is finite.
    let radius = (-2.0 * (1.0 - rng.f64()).ln()).sqrt();

    mean + sd * radius * (TAU * rng.f64()).cos()
}

/// Checks that `got`, what the check `name` found, is `want`.
fn check_eq<T, U>(name: &str, got: T, want: U) -> Result<()>
where
    T: PartialEq<U> + Debug,
    U: Debug,
{
    if got == want {
        return Ok(());
    }

    Err(format!("the self-test failed: {name} is {got:?}, not {want:?}").into())
}

/// Checks that `got`, the interval that the check `name` found, is there and has both ends within
/// `tolerance` of those of `want`.
fn check_interval(name: &str, got: Option<[f64; 2]>, want: [f64; 2], tolerance: f64) -> Result<()> {
    let [low, high] = required(name, got)?;
    check_close(&format!("{name}, low"), Some(low), want[0], tolerance)?;
    check_close(&format!("{name}, high"), Some(high), want[1], tolerance)
}

/// Checks that `got`, what the check `name` found, is there and within `tolerance` of `want`.
fn check_close(name: &str, got: Option<f64>, want: f64, tolerance: f64) -> Result<()> {
    let got = required(name, got)?;
    if (got - want).abs() <= tolerance {
        return Ok(());
    }

    Err(format!("the self-test failed: {name} is {got}, not {want} ± {tolerance}").into())
}

/// Checks that `compared` is an error with `message` in its own.
fn check_error(compared: Result<Comparison>, message: &str) -> Result<()> {
    let Err(error) = compared else {
        return Err(format!("the self-test failed: no error, where {message:?} is due").into());
    };
    let error = error.to_string();

    check(
        &format!("the error {error:?} has {message:?}"),
        error.contains(message),
    )
}

/// Fails the self-test, naming the check `name`, unless it `holds`.
fn check(name: &str, holds: bool) -> Result<()> {
    if holds {
        return Ok(());
    }

    Err(format!("the self-test failed: {name}").into())
}

/// `found`, what the check `name` found, which must be there.
fn required<T>(name: &str, found: Option<T>) -> Result<T> {
    found.ok_or_else(|| format!("the self-test failed: {name} is missing").into())
}
