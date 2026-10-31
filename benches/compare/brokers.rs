//! The brokers that the comparison runs: busd, as this checkout builds it, the other brokers that
//! the comparison knows and that can run here, or those that the command line names, and busd at
//! another revision of its repository, as `baseline`.

use std::{iter, path::Path};

use crate::{
    calls::Result,
    command::{find_program, quote, CommandLine},
    environment::first_line,
    output::note,
};

/// A broker that the comparison runs.
pub struct Broker {
    /// The label of its runs in the results, such as `dbus-broker`.
    pub label: String,
    /// What the summary calls it, such as `dbus-broker 37`.
    pub name: String,
    /// The command that starts it, as the results note it.
    pub command: String,
    /// The command, split into words.
    pub line: CommandLine,
    /// The first line of what its `--version` prints, for the notes of the results, if it is one
    /// of those that the comparison knows, and it says.
    pub version: Option<String>,
}

/// busd, as `binary` is a build of it, under `label`, and called `name` in the summary.
///
/// busd logs why it disconnects a client at its debug level, and nothing at that level while it
/// forwards messages, so the log that a failed run shows names the cause, at no cost to the others.
pub fn busd(label: &str, name: String, binary: &Path) -> Result<Broker> {
    let binary = quote(&binary.to_string_lossy());
    let command = format!("RUST_LOG=busd=debug {binary} --address {{address}}");
    let line = CommandLine::parse(&command).map_err(|e| format!("{label}, {command}: {e}"))?;

    Ok(Broker {
        label: label.to_owned(),
        name,
        command,
        line,
        version: None,
    })
}

/// The brokers that `requests`, the values of `--broker`, ask for besides busd, or, if there are
/// none, every broker that the comparison knows and that can run here.
///
/// A broker that the comparison knows needs its programs installed. By default, it is also left
/// out if it logs to the journal and there is none, but not if `requests` names it.
///
/// `taken` are the labels of the builds of busd, which no other broker may take.
pub fn others(requests: &[String], taken: &[&str]) -> Result<Vec<Broker>> {
    let mut brokers = Vec::new();
    if requests.is_empty() {
        for known in &KNOWN {
            match known.lacks() {
                None => brokers.push(known.broker()?),
                Some(lack) => note!("{lack}: leaving {} out of the comparison.", known.label),
            }
        }
    }
    for request in requests {
        if let Some((label, command)) = request.split_once('=') {
            brokers.push(custom(label, command)?);
            continue;
        }
        // busd runs anyway: naming it alone leaves the others out.
        if request == BUSD {
            continue;
        }
        let Some(known) = KNOWN.iter().find(|known| known.label == request) else {
            let names: Vec<&str> = iter::once(BUSD)
                .chain(KNOWN.iter().map(|known| known.label))
                .collect();
            let names = names.join(", ");
            return Err(format!(
                "--broker {request}: no broker of that name is known: name one of {names}, or \
                 give LABEL=COMMAND"
            )
            .into());
        };
        if let Some(missing) = known.missing() {
            return Err(format!("--broker {request}: {missing} is not installed").into());
        }
        brokers.push(known.broker()?);
    }

    let mut labels = taken.to_vec();
    for broker in &brokers {
        if labels.contains(&broker.label.as_str()) {
            let label = &broker.label;
            return Err(format!(
                "--broker: the label {label} is taken: each broker needs a label of its own"
            )
            .into());
        }
        labels.push(&broker.label);
    }

    Ok(brokers)
}

/// The label of busd as this checkout builds it, which the comparison compares every other broker
/// with.
pub const BUSD: &str = "busd";

/// The label of busd at the revision that `--baseline` names.
pub const BASELINE: &str = "baseline";

/// A broker that the command line adds, as `--broker LABEL=COMMAND`.
fn custom(label: &str, command: &str) -> Result<Broker> {
    let valid = !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c));
    if !valid {
        return Err(format!(
            "--broker {label}=...: the label names files of the results, so it can only have \
             letters, digits, and . _ + -"
        )
        .into());
    }
    let line = CommandLine::parse(command).map_err(|e| format!("--broker {label}: {e}"))?;

    Ok(Broker {
        label: label.to_owned(),
        name: label.to_owned(),
        command: command.to_owned(),
        line,
        version: None,
    })
}

/// A broker that the comparison knows by name.
struct Known {
    /// Its label, which names it on the command line too.
    label: &'static str,
    /// The programs that it needs, which must be on PATH.
    needs: &'static [&'static str],
    /// Whether it logs to the journal, without which it fails, so that it runs by default only
    /// where `JOURNAL` exists.
    journal: bool,
    /// The command that starts it.
    command: &'static str,
    /// The program whose `--version` tells its version.
    version: &'static str,
}

impl Known {
    /// Why it cannot run here, if it cannot: a program that it needs is not installed, or the
    /// journal, which it logs to, is not there.
    fn lacks(&self) -> Option<String> {
        if let Some(missing) = self.missing() {
            return Some(format!("{missing} is not installed"));
        }
        if self.journal && !Path::new(JOURNAL).exists() {
            return Some(format!("{JOURNAL}, the socket of journald, does not exist"));
        }

        None
    }

    /// The programs that it needs and that are not installed, if there are any, joined with "or".
    fn missing(&self) -> Option<String> {
        let missing: Vec<&str> = self
            .needs
            .iter()
            .copied()
            .filter(|program| find_program(program).is_none())
            .collect();

        (!missing.is_empty()).then(|| missing.join(" or "))
    }

    /// The broker, called by its label and its version in the summary.
    fn broker(&self) -> Result<Broker> {
        let line = CommandLine::parse(self.command)?;
        let version = first_line(self.version, &["--version"]);
        // The number is the last word of the line, as of `dbus-broker 37`, and of
        // `D-Bus Message Bus Daemon 1.16.2`.
        let number = version
            .as_deref()
            .and_then(|version| version.split_whitespace().last());
        let name = match number {
            Some(number) => format!("{} {number}", self.label),
            None => self.label.to_owned(),
        };

        Ok(Broker {
            label: self.label.to_owned(),
            name,
            command: self.command.to_owned(),
            line,
            version,
        })
    }
}

/// The brokers that the comparison knows by name, besides busd, in the order that it runs them by
/// default.
const KNOWN: [Known; 2] = [
    // dbus-broker runs only under its launcher, which takes the socket to listen on from systemd's
    // socket activation, as systemd-socket-activate hands it over, and connects to the bus that it
    // launches through $DBUS_SESSION_BUS_ADDRESS. systemd-socket-activate passes no environment
    // variable on but those given with -E, and without that one, the launcher would connect to the
    // session bus of the desktop instead. The launcher logs to the journal, without which it
    // fails.
    Known {
        label: "dbus-broker",
        needs: &["dbus-broker-launch", "systemd-socket-activate"],
        journal: true,
        command: "systemd-socket-activate -E DBUS_SESSION_BUS_ADDRESS={address} -l {path} \
                  dbus-broker-launch --scope user",
        version: "dbus-broker",
    },
    Known {
        label: "dbus-daemon",
        needs: &["dbus-daemon"],
        journal: false,
        command: "dbus-daemon --session --nofork --address={address}",
        version: "dbus-daemon",
    },
];

/// The socket of journald, which the brokers that log to the journal send their logs to.
const JOURNAL: &str = "/run/systemd/journal/socket";
