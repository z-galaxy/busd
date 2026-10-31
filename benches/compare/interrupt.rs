//! Ctrl-C, SIGTERM and SIGHUP, which ask the runner to stop: a thread listens for them, and notes
//! that one came, which the runner looks at between the steps of its runs.
//!
//! A terminal sends the SIGINT of Ctrl-C to every process of its foreground process group: the
//! runner, and the client of the run under way, which ends there and then. Each broker runs in a
//! process group of its own and gets nothing from the terminal: the runner stops it, as it stops
//! every broker. Then the runner writes the results of the runs that finished, and exits with
//! status 130.

use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
    },
    thread,
};

use zruntime::{
    signal::{Signal, Signals},
    SharedRuntime,
};

use crate::{calls::Result, procfs};

/// Whether the runner has been asked to stop.
pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// Starts the thread that listens for the signals that ask the runner to stop, and returns once it
/// listens.
///
/// SIGINT is taken whatever the runner was started with for it. A shell that starts a command in
/// the background has it ignore SIGINT, and the brokers would inherit that, and not stop on the
/// SIGINT that is meant to stop them, whereas a process with a handler for a signal starts its
/// children with the default action for it. SIGTERM and SIGHUP are left alone if they are ignored,
/// as `nohup` has SIGHUP ignored on purpose.
pub fn listen() -> Result<()> {
    let mut signals = vec![Signal::Interrupt];
    for (signal, number) in [
        (Signal::Terminate, rustix::process::Signal::TERM.as_raw()),
        (Signal::Hangup, rustix::process::Signal::HUP.as_raw()),
    ] {
        if !procfs::ignores(number) {
            signals.push(signal);
        }
    }

    let (listening, listens) = mpsc::channel();
    thread::Builder::new()
        .name("signals".to_owned())
        .spawn(move || zruntime::block_on(note(signals, listening)))?;
    let Ok(listened) = listens.recv() else {
        return Err("the thread that listens for signals ended before it listened".into());
    };
    listened.map_err(|e| format!("cannot listen for signals: {e}"))?;

    Ok(())
}

/// Listens for `signals`, says on `listening` whether it does, and notes it in `INTERRUPTED` as one
/// comes, for as long as the runner runs.
async fn note(signals: Vec<Signal>, listening: Sender<io::Result<()>>) {
    let signals = SharedRuntime::current().and_then(|runtime| Signals::new(&runtime, signals));
    let mut signals = match signals {
        Ok(signals) => signals,
        Err(e) => {
            let _ = listening.send(Err(e));
            return;
        }
    };
    let _ = listening.send(Ok(()));

    while signals.recv().await.is_ok() {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }
}

/// Whether one of the signals that ask the runner to stop has come.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
