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

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

use crate::calls::Result;

mod self_test;
mod stats;

pub use self_test::self_test;
use stats::{mean, median, quantile, Bootstrap, RESAMPLES, SEED};

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
