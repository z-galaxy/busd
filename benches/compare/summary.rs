//! The summary of the comparison, `summary.md`, ready to paste into the Performance section of
//! busd's README, in its form: a line that says what it was measured on, then for each workload, a
//! line that says what it is, the table of its medians, and how busd compares with each other
//! broker in pairs.
//!
//! The medians are of the measured runs that finished, which are fewer than the rounds that the
//! comparison was set to measure where runs failed, or where it was interrupted: then the first
//! line says how many runs the medians are of, and a line under the caption of each table that
//! lacks some says how many it has of each broker.

use crate::{
    brokers::BUSD,
    interrupt,
    pairs::{self, Comparison},
    report::Report,
    setup::Setup,
    workload::Workload,
};

/// The summary of the comparison that `setup` describes, from the reports of the workloads that
/// it ran.
pub fn summary(setup: &Setup, reports: &[Report]) -> String {
    let mut paragraphs = vec![measured_on(setup, reports)];
    for report in reports {
        paragraphs.push(caption(report.workload).to_owned());
        paragraphs.extend(medians_line(setup, report));
        paragraphs.push(table(setup, report));
        paragraphs.extend(gaps(setup, report));
    }
    let mut summary = paragraphs.join("\n\n");
    summary.push('\n');

    summary
}

/// `count` rounds, in words.
pub fn rounds(count: u32) -> String {
    match count {
        1 => "1 round".to_owned(),
        _ => format!("{count} rounds"),
    }
}

/// The fields of the runs of `workload` that the table of its summary shows, column by column.
pub fn fields(workload: Workload) -> impl Iterator<Item = &'static str> {
    columns(workload)
        .iter()
        .flat_map(|column| column.fields.iter().copied())
}

/// The line that says what the comparison was measured on, and how, from `reports`, those of the
/// workloads that ran.
fn measured_on(setup: &Setup, reports: &[Report]) -> String {
    let environment = &setup.environment;
    let machine = &environment.machine;
    // As /proc/cpuinfo has it, with runs of spaces in it at times.
    let cpu: Vec<&str> = machine.cpu_model.split_whitespace().collect();
    let cpu = cpu.join(" ");

    format!(
        "Measured on {} on {cpu} ({} CPUs), Linux {}, as the medians of {}, after {} of \
         warm-up; busd at {}, built with {}.",
        environment.started.date(),
        machine.nproc,
        machine.kernel,
        medians(setup, reports),
        rounds(setup.warmup),
        environment.rev,
        environment.rustc,
    )
}

/// What the medians of the tables of `reports` are of, in words: the rounds that the comparison
/// was set to measure, if every median is of a run of each of them, or else how many runs they are
/// of, the fewest and the most, over the cells that have one. It notes too whether the comparison
/// was interrupted, which leaves out the workloads after.
fn medians(setup: &Setup, reports: &[Report]) -> String {
    let in_full = !reports.is_empty()
        && reports.iter().all(|report| {
            let rounds = setup.measured_rounds(report.workload);

            complete(&counts(setup, report), rounds)
        });
    let mut notes = Vec::new();
    let medians = if in_full {
        let shared = reports
            .iter()
            .any(|report| report.workload == Workload::Shared);
        let others = reports
            .iter()
            .any(|report| report.workload != Workload::Shared);
        match (shared, others) {
            (true, false) => rounds(setup.shared_rounds),
            (true, true) if setup.shared_rounds != setup.rounds => {
                notes.push(format!("{} of the shared connection", setup.shared_rounds));
                rounds(setup.rounds)
            }
            _ => rounds(setup.rounds),
        }
    } else {
        let cells: Vec<usize> = reports
            .iter()
            .flat_map(|report| counts(setup, report))
            .flat_map(|(_, counts)| counts)
            .filter(|&n| n > 0)
            .collect();
        match (cells.iter().min(), cells.iter().max()) {
            (Some(&fewest), Some(&most)) => runs(fewest, most),
            _ => "no runs".to_owned(),
        }
    };
    if interrupt::interrupted() {
        notes.push("interrupted: partial results".to_owned());
    }

    if notes.is_empty() {
        medians
    } else {
        format!("{medians} ({})", notes.join("; "))
    }
}

/// How many runs each median of the table of `report` is of: for each broker of `setup`, its
/// label, and how many of its measured runs have a number in each field of the columns, in their
/// order.
fn counts<'s>(setup: &'s Setup, report: &Report) -> Vec<(&'s str, Vec<usize>)> {
    setup
        .brokers
        .iter()
        .map(|broker| {
            let label = broker.label.as_str();
            let counts = fields(report.workload).map(|field| report.count(field, label));

            (label, counts.collect())
        })
        .collect()
}

/// Whether every median of the table whose `counts` they are is of a run of each of `rounds`, the
/// rounds that the comparison was set to measure.
fn complete(counts: &[(&str, Vec<usize>)], rounds: u32) -> bool {
    let rounds = rounds as usize;

    counts
        .iter()
        .all(|(_, counts)| counts.iter().all(|&n| n == rounds))
}

/// A count of runs, or a range of counts from `fewest` to `most`, in words, such as `1 run`,
/// `5 runs` or `5 to 60 runs`.
fn runs(fewest: usize, most: usize) -> String {
    if fewest == most {
        count_of(fewest, "run")
    } else {
        format!("{fewest} to {most} runs")
    }
}

/// `count` of `noun`, in words, such as `1 pair` or `60 pairs`.
fn count_of(count: usize, noun: &str) -> String {
    match count {
        1 => format!("1 {noun}"),
        _ => format!("{count} {noun}s"),
    }
}

/// What the table of `workload` shows, as a line before it.
fn caption(workload: Workload) -> &'static str {
    match workload {
        Workload::Calls => {
            "Method calls with 16 KiB of data each, 1000 by one client, then 1000 by each of 20 \
             clients at once:"
        }
        Workload::Signals => {
            "Signals with 1 KiB of data each, a burst of 40,000 to one subscriber, then a \
             broadcast of 6000 to each of 20 subscribers:"
        }
        Workload::Latency => {
            "Latencies, as the medians over the rounds of their p50 and their p99: the round trips \
             of method calls, by one client and by 20 at once, and the delivery of the signals of \
             a broadcast to 20 subscribers:"
        }
        Workload::Shared => {
            "Method calls by 100 callers that share a connection, then by 100 clients, each with a \
             connection of its own:"
        }
        Workload::Dicts => {
            "Method calls with a dict of dicts of 100 variants, `a{sa{sv}}`, for their body, 1000 \
             by one client, then 1000 by each of 20 clients at once:"
        }
    }
}

/// The line under the caption of the table of `report` that says how many runs of each broker its
/// medians are of, the fewest and the most over its columns, unless every median is of a run of
/// each round that the comparison was set to measure.
fn medians_line(setup: &Setup, report: &Report) -> Option<String> {
    let counts = counts(setup, report);
    if complete(&counts, setup.measured_rounds(report.workload)) {
        return None;
    }
    let ranges: Vec<(&str, usize, usize)> = counts
        .iter()
        .map(|(label, counts)| {
            let fewest = counts.iter().copied().min().unwrap_or(0);
            let most = counts.iter().copied().max().unwrap_or(0);

            (*label, fewest, most)
        })
        .collect();
    let [(label, fewest, most), others @ ..] = ranges.as_slice() else {
        return None;
    };

    let text = if others.is_empty() {
        format!("{} of {label}", runs(*fewest, *most))
    } else if others.iter().all(|&(_, f, m)| (f, m) == (*fewest, *most)) {
        format!("{} of each broker", runs(*fewest, *most))
    } else {
        let mut parts = vec![format!("{} of {label}", runs(*fewest, *most))];
        let others = others
            .iter()
            .map(|(label, fewest, most)| format!("{} of {label}", range(*fewest, *most)));
        parts.extend(others);
        list(&parts)
    };

    Some(format!("The medians of {text}."))
}

/// A count, or a range of counts from `fewest` to `most`, such as `5` or `5 to 60`.
fn range(fewest: usize, most: usize) -> String {
    if fewest == most {
        fewest.to_string()
    } else {
        format!("{fewest} to {most}")
    }
}

/// `items` as a sentence lists them, such as `a`, `a and b` or `a, b and c`.
fn list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    }
}

/// The table of the medians of the workload of `report`, a row for each broker, busd first.
fn table(setup: &Setup, report: &Report) -> String {
    let columns = columns(report.workload);
    let mut headers = vec!["Broker"];
    headers.extend(columns.iter().map(|column| column.header));

    let rows: Vec<Vec<String>> = setup
        .brokers
        .iter()
        .map(|broker| {
            let mut row = vec![broker.name.clone()];
            row.extend(columns.iter().map(|column| {
                let medians = column.fields.iter();
                let medians = medians.map(|field| report.median(field, &broker.label));
                let medians: Option<Vec<f64>> = medians.collect();

                medians.map_or_else(|| "-".to_owned(), |medians| column.unit.cell(&medians))
            }));

            row
        })
        .collect();

    render(&headers, &rows)
}

/// A column of a table of the summary.
struct Column {
    /// Its header.
    header: &'static str,
    /// The fields whose medians it shows: one, or a median and a 99th percentile.
    fields: &'static [&'static str],
    /// How it shows them.
    unit: Unit,
    /// What the lines of the comparisons in pairs call each of `fields`.
    names: &'static [&'static str],
}

/// How a column of a table of the summary shows its medians.
enum Unit {
    /// A duration in seconds, to the millisecond.
    Seconds,
    /// A CPU time in seconds, to the hundredth.
    CpuTime,
    /// A memory size in KiB, shown in MiB, to the tenth.
    Memory,
    /// A median and a 99th percentile of latencies in microseconds, to the microsecond.
    Microseconds,
    /// A median and a 99th percentile of latencies in microseconds, shown in milliseconds, to the
    /// tenth.
    Milliseconds,
}

impl Unit {
    /// The cell of the medians `values` of the fields of a column.
    fn cell(&self, values: &[f64]) -> String {
        match (self, values) {
            (Self::Seconds, [seconds]) => format!("{seconds:.3} s"),
            (Self::CpuTime, [seconds]) => format!("{seconds:.2} s"),
            (Self::Memory, [kib]) => format!("{:.1} MiB", kib / 1024.0),
            (Self::Microseconds, [p50, p99]) => format!("{p50:.0} / {p99:.0} µs"),
            (Self::Milliseconds, [p50, p99]) => {
                format!("{:.1} / {:.1} ms", p50 / 1000.0, p99 / 1000.0)
            }
            _ => "-".to_owned(),
        }
    }
}

/// The columns of the table of `workload`, after that of the brokers.
fn columns(workload: Workload) -> &'static [Column] {
    match workload {
        Workload::Calls | Workload::Dicts => &CALLS,
        Workload::Signals => &SIGNALS,
        Workload::Latency => &LATENCY,
        Workload::Shared => &SHARED,
    }
}

/// The columns of the method calls, with byte arrays or with dicts.
const CALLS: [Column; 4] = [
    Column {
        header: "1 client",
        fields: &["single_s"],
        unit: Unit::Seconds,
        names: &["1 client"],
    },
    Column {
        header: "20 clients",
        fields: &["multi_s"],
        unit: Unit::Seconds,
        names: &["20 clients"],
    },
    Column {
        header: "CPU time with 20 clients",
        fields: &["multi_broker_cpu_s"],
        unit: Unit::CpuTime,
        names: &["CPU time with 20 clients"],
    },
    PEAK_MEMORY,
];

/// The columns of the signals.
const SIGNALS: [Column; 4] = [
    Column {
        header: "Burst to 1 subscriber",
        fields: &["single_s"],
        unit: Unit::Seconds,
        names: &["burst to 1 subscriber"],
    },
    Column {
        header: "Broadcast to 20",
        fields: &["multi_s"],
        unit: Unit::Seconds,
        names: &["broadcast to 20"],
    },
    Column {
        header: "CPU time of the broadcast",
        fields: &["multi_broker_cpu_s"],
        unit: Unit::CpuTime,
        names: &["CPU time of the broadcast"],
    },
    PEAK_MEMORY,
];

/// The columns of the latencies.
const LATENCY: [Column; 3] = [
    Column {
        header: "Round trip, 1 client",
        fields: &["single_rtt_p50_us", "single_rtt_p99_us"],
        unit: Unit::Microseconds,
        names: &["1 client p50", "1 client p99"],
    },
    Column {
        header: "Round trip, 20 clients",
        fields: &["multi_rtt_p50_us", "multi_rtt_p99_us"],
        unit: Unit::Microseconds,
        names: &["20 clients p50", "20 clients p99"],
    },
    Column {
        header: "Fan-out to 20",
        fields: &["fanout_delivery_p50_us", "fanout_delivery_p99_us"],
        unit: Unit::Milliseconds,
        names: &["fan-out p50", "fan-out p99"],
    },
];

/// The columns of the shared connection.
const SHARED: [Column; 4] = [
    Column {
        header: "100 callers, 1 connection",
        fields: &["single_s"],
        unit: Unit::Seconds,
        names: &["100 callers on 1 connection"],
    },
    Column {
        header: "100 clients",
        fields: &["multi_s"],
        unit: Unit::Seconds,
        names: &["100 clients"],
    },
    Column {
        header: "CPU time with 100 clients",
        fields: &["multi_broker_cpu_s"],
        unit: Unit::CpuTime,
        names: &["CPU time with 100 clients"],
    },
    PEAK_MEMORY,
];

/// The column of the peak memory of the brokers, which ends most tables.
const PEAK_MEMORY: Column = Column {
    header: "Peak memory",
    fields: &["broker_maxrss_kib"],
    unit: Unit::Memory,
    names: &["peak memory"],
};

/// `rows` as a Markdown table under `headers`, padded as the README pads its tables: each column
/// as wide as its widest cell, the first to the left, and the others, of numbers, to the right.
fn render(headers: &[&str], rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let cells = rows.iter().filter_map(|row| row.get(column));
            let widest = cells.map(|cell| cell.chars().count()).max().unwrap_or(0);

            widest.max(header.chars().count())
        })
        .collect();
    let line = |cells: Vec<String>| format!("| {} |", cells.join(" | "));

    let mut lines = Vec::new();
    let header = headers.iter().zip(&widths);
    lines.push(line(
        header
            .map(|(header, &width)| format!("{header:<width$}"))
            .collect(),
    ));
    let rules = widths
        .iter()
        .enumerate()
        .map(|(column, &width)| match column {
            0 => "-".repeat(width + 2),
            _ => format!("{}:", "-".repeat(width + 1)),
        });
    lines.push(format!("|{}|", rules.collect::<Vec<_>>().join("|")));
    for row in rows {
        let cells = row.iter().zip(&widths).enumerate();
        let cells = cells.map(|(column, (cell, &width))| match column {
            0 => format!("{cell:<width$}"),
            _ => format!("{cell:>width$}"),
        });
        lines.push(line(cells.collect()));
    }

    lines.join("\n")
}

/// A line for each broker that busd is compared with, that says by how much busd differs from it
/// on the fields of the table of `report`: by the percent difference of their means over the
/// rounds that both ran in, with its 95% interval and its verdict.
///
/// A field that some runs lack is compared over the pairs that have it, and the line says how many
/// there are. One that no pair has is not compared, and the line says so, as it does of a broker
/// that is compared on no field.
fn gaps(setup: &Setup, report: &Report) -> Vec<String> {
    let others = setup.brokers.iter().filter(|broker| broker.label != BUSD);

    others
        .map(|broker| {
            let label = &broker.label;
            let compared = report.pairs.iter().find(|(other, _)| other == label);
            let Some((_, comparison)) = compared else {
                return format!(
                    "busd against {label}: not compared, as no round has numbers of both in \
                     these columns."
                );
            };

            gap(label, comparison, report.workload)
        })
        .collect()
}

/// The line that says how busd compares with the broker `label` on the fields of the table of
/// `workload`, by `comparison`: on what basis, in which direction, and on each field.
fn gap(label: &str, comparison: &Comparison, workload: Workload) -> String {
    let paired = comparison.paired_rounds.len();
    let mut basis = vec!["+ is more time or memory for busd".to_owned()];
    if paired < 2 {
        basis.push("only 1 pair: no intervals".to_owned());
    } else if paired < pairs::FEW {
        basis.push(format!("only {paired} pairs: the intervals are too narrow"));
    }

    let fields = columns(workload)
        .iter()
        .flat_map(|column| column.fields.iter().zip(column.names));
    let mut interval_named = false;
    let gaps: Vec<String> = fields
        .map(|(field, name)| {
            let Some(metric) = comparison.metric(field) else {
                return format!("{name} not compared");
            };
            let Some(pct) = metric.pct else {
                return format!("{name} n/a");
            };
            let mut about = Vec::new();
            if let Some([low, high]) = metric.ci95 {
                let what = if interval_named { "" } else { "95% CI " };
                interval_named = true;
                about.push(format!("{what}{low:+.1} to {high:+.1}"));
            }
            about.push(metric.verdict.words(comparison.margin));
            let mut about = about.join(", ");
            // Some of the paired rounds lack the field in a run.
            if metric.n != paired {
                about.push_str(&format!("; {}", count_of(metric.n, "pair")));
            }

            format!("{name} {pct:+.1}% ({about})")
        })
        .collect();

    format!(
        "busd against {label}, by the means of {} ({}): {}.",
        count_of(paired, "paired round"),
        basis.join("; "),
        gaps.join(", "),
    )
}
