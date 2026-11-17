//! The comparison's self-test. It checks the comparison's statistics, verdicts and reports against
//! values worked out by hand or with other tools.

use std::{f64::consts::TAU, fmt::Debug};

use super::{stats::fsum, *};

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
