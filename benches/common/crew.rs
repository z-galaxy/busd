//! Threads that run rounds of a benchmark together, timed by the thread that tells them to.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::cpus::Cpu;

/// What a thread of a [`Crew`] runs: it sets itself up, and then runs a round each time that
/// [`Rounds::next`] says to, until it says that the crew is done.
pub type Member = Box<dyn FnOnce(Rounds) + Send>;

/// A thread for each of a set of [`Member`]s, which run their rounds together each time that
/// [`Crew::time`] says to.
///
/// The threads are spawned once and kept for every sample, so that spawning them, and whatever
/// they set up, such as their connections, is never timed: a pair of barriers starts a round and
/// waits for it to end, and only the time between the two is measured. A member keeps to one
/// `block_on` of its runtime for as long as it runs, and waits for its rounds inside it, so that
/// its runtime is never handed over to zruntime's helper thread in between, and taken back at the
/// start of the next round. As nothing is sent to a member between two rounds, its runtime has
/// nothing to do while it waits.
///
/// Each thread is pinned to a CPU of its own, and so is the thread that times them, for as long as
/// it does. The threads also start each round together, rather than as the barrier happens to wake
/// them, one after another: a thread that got going first would run part of its round with nobody
/// to contend with.
pub struct Crew {
    /// The CPU that the thread that times the rounds is pinned to, if it is.
    timer: Option<Cpu>,
    /// What the threads of the crew and the thread that times them share.
    shared: Arc<Shared>,
    /// The threads of the crew.
    threads: Vec<JoinHandle<()>>,
}

impl Crew {
    /// Spawns a thread for each of `members`, pinned to the CPU at the same index of `cpus`, where
    /// there is one, and waits until all of them are set up. The thread that times the rounds will
    /// be pinned to `timer`, if there is one.
    ///
    /// # Panics
    ///
    /// If `cpus` has fewer entries than there are members.
    pub fn spawn(members: Vec<Member>, cpus: Vec<Option<Cpu>>, timer: Option<Cpu>) -> Self {
        assert!(
            cpus.len() >= members.len(),
            "a seat for each member of the crew"
        );
        let count = members.len();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            // The crew, and the thread that times it.
            ready: Barrier::new(count + 1),
            start: Barrier::new(count + 1),
            end: Barrier::new(count + 1),
            arrivals: AtomicU64::new(0),
            count: count as u64,
        });
        let threads = members
            .into_iter()
            .zip(cpus)
            .map(|(member, cpu)| {
                let rounds = Rounds {
                    shared: shared.clone(),
                    started: 0,
                };
                thread::spawn(move || {
                    let _abort = AbortOnPanic;
                    let _pinned = cpu.map(Cpu::pin);
                    member(rounds);
                })
            })
            .collect();
        shared.ready.wait();

        Self {
            timer,
            shared,
            threads,
        }
    }

    /// Runs `iters` rounds, one after the other, and the time they took, each from the crew
    /// being told to start it to the last of the crew being done with it.
    pub fn time(&self, iters: u64) -> Duration {
        let _pinned = self.timer.map(Cpu::pin);
        let mut total = Duration::ZERO;
        for _ in 0..iters {
            let started = Instant::now();
            self.shared.start.wait();
            self.shared.end.wait();
            total += started.elapsed();
        }

        total
    }

    /// Lets the threads go with the crew done, so that they return from their member rather than
    /// run a round, and joins them.
    pub fn join(self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.start.wait();
        for thread in self.threads {
            thread
                .join()
                .expect("the members of the crew abort the process on a panic");
        }
    }
}

/// What a member of a [`Crew`] waits for its rounds with.
pub struct Rounds {
    /// What the threads of the crew and the thread that times them share.
    shared: Arc<Shared>,
    /// How many rounds the member has started.
    started: u64,
}

impl Rounds {
    /// Says that the member is done with its round, or with setting itself up before the first,
    /// and waits for the next: true once that is under way on every thread of the crew, false if
    /// the crew is done.
    pub fn next(&mut self) -> bool {
        match self.started {
            0 => self.shared.ready.wait(),
            _ => self.shared.end.wait(),
        };
        self.shared.start.wait();
        if self.shared.stop.load(Ordering::Acquire) {
            return false;
        }
        self.started += 1;
        self.shared.arrivals.fetch_add(1, Ordering::AcqRel);
        // Yields rather than spins, so that a thread that shares its CPU, where there are fewer
        // CPUs than threads, gets there all the same.
        while self.shared.arrivals.load(Ordering::Acquire) < self.started * self.shared.count {
            thread::yield_now();
        }

        true
    }
}

/// Ends the process if the thread that holds it panics.
///
/// The other threads of a benchmark wait for each thread at a barrier, or for what it sends: one
/// that panicked would leave them waiting for good, and the benchmark with them, rather than
/// failing it. The panic is reported, as usual, before this ends the process.
pub struct AbortOnPanic;

impl Drop for AbortOnPanic {
    fn drop(&mut self) {
        if thread::panicking() {
            eprintln!("a thread of the benchmark panicked: ending the process");
            std::process::abort();
        }
    }
}

/// What the threads of a crew and the thread that times them share.
struct Shared {
    /// Whether the crew is done, which its members return on rather than run another round.
    stop: AtomicBool,
    /// What the members wait at once they are set up.
    ready: Barrier,
    /// What starts a round.
    start: Barrier,
    /// What the end of a round waits at.
    end: Barrier,
    /// How many times a member has got to the start of a round. Each does once a round, so the
    /// `n`th round is under way on every thread once this reaches `n` times the count.
    arrivals: AtomicU64,
    /// How many members the crew has.
    count: u64,
}
