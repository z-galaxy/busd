//! What the client of a run tells the runner: a line of JSON on its standard output as each phase
//! of its workload ends, and nothing else there.
//!
//! The runner reads each line as it comes, and notes how much CPU time the client and the broker
//! have used by then, so that it can tell what each phase cost them, as well as what the client
//! measured.

use std::{collections::BTreeMap, io::Write};

use serde::{Deserialize, Serialize};

/// The line of a phase that has ended: its name, and what the client measured of it.
#[derive(Debug, Deserialize, Serialize)]
pub struct PhaseLine {
    /// The name of the phase, such as `single` or `multi`, which prefixes the names of the CPU
    /// times that the runner works out for it, such as `multi_broker_cpu_s`.
    pub phase: String,
    /// What the client measured, by the name that the results give it, such as `multi_s`, in the
    /// unit that the end of the name says: `_s` for seconds, `_us` for microseconds, and none for
    /// a count.
    pub metrics: BTreeMap<String, f64>,
}

impl PhaseLine {
    /// Writes the line to the standard output, and flushes it, so that the runner gets it as soon
    /// as the phase ends.
    pub fn print(&self) -> serde_json::Result<()> {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, self)?;
        writeln!(stdout)
            .and_then(|()| stdout.flush())
            .map_err(serde_json::Error::io)
    }
}
