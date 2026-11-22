# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

For contribution conventions — commit-message format, atomic commits, code layout, and
more — follow the guidelines in [`CONTRIBUTING.md`](CONTRIBUTING.md).

- **Changelog-skip trailer**: end a commit message with a `Changelog: skip` git trailer to
  keep it out of the user-facing changelog (use for AI-workflow artifacts such as design docs
  and implementation plans).
- **Hand-overs across versions**: until busd 1.0, a hand-over only has to work between two
  instances of the same version of busd. Bump the version of the state that it stores whenever
  its format changes, and let a take-over of a state of another version fail, which disconnects
  the clients: keep no code to take over the states of earlier versions, and say so in one
  sentence in the commit message and the pull request. zbus, at 6.x, is another matter: there,
  backwards compatibility matters.

## Project Overview

busd is a pure-Rust D-Bus bus (broker) implementation — the sibling daemon to the [zbus]
library, which it builds on through its `core` and `core-p2p` features, without zbus's
connections. busd reads and writes the messages of its clients itself. Only the tests and
the benchmarks, and the helpers of `src/testing.rs` that the tests share, use zbus's
connections. Being pure Rust, it cross-builds far more easily than the reference C broker.
Status is alpha: only the essentials of the D-Bus specification are in place.

busd runs on a [zruntime] `LocalRuntime`, which the binary drives on its main thread. The
bus, its tasks and the connections of its clients all run there, and the bus spawns its
tasks with `zruntime::spawn_local`. The tests run their buses the same way, through
`busd::testing::block_on`, which returns only once the buses of the test are gone.
Dropping a zruntime `Task` cancels it, so a fire-and-forget task must call `.detach()`.

The tasks share the state of the bus through `Rc`s and change it through `RefCell`s.
Borrow those cells only in synchronous code, in the tests too. If a borrow is held across
an `.await`, such as one of `Peers::routing()`, a task that borrows the same cell
meanwhile panics, unless both borrows only read it.

[zbus]: https://github.com/z-galaxy/zbus
[zruntime]: https://github.com/z-galaxy/zruntime
[zoter]: https://github.com/z-galaxy/zoter

## Development Commands

```bash
# Build
cargo build
cargo build --release

# Build with profile-guided optimization, which needs `rustup component add llvm-tools`
./build-pgo.sh

# Run a session bus and print its address
cargo run -- --print-address

# Test (integration tests live in tests/, e.g. config parsing)
cargo test
# The same from the packaged crate, which lacks the dev-dependency of busd on itself: the tests
# that share the helpers of `src/testing.rs` build only with the `testing` feature
cargo test --features testing

# Format (nightly rustfmt) and lint: the build that ships, then the tests and the `bus`
# benchmark, whose dev-dependencies build the library and the binary with more features
cargo +nightly fmt --all
cargo clippy -- -D warnings
cargo clippy --all-targets -- -D warnings
```

## Architecture

The crate is both a library (`src/lib.rs`) and a thin CLI binary (`src/bin/busd.rs`,
argument parsing via clap). Key modules:

- **`bus/`** — the broker core: accepts peer connections and routes messages between them.
  `listener.rs` is the socket that the bus accepts connections on.
- **`bus/systemd.rs`, `bus/handover.rs`** — running under systemd: socket activation, and
  restarts through systemd's file descriptor store, which hand the clients over to the next
  instance. The protocols of systemd, telling it that the bus has started among them, and the
  layout of a hand-over in the store, are [zoter]'s; busd's record of each connection is an item
  of zoter's hand-over.
  The bus answers the calls to it that it took before it hands over, so nothing is on its way to
  or from the bus itself at that point. The calls that it didn't take go to the next instance with
  the connections of their clients, like everything else that the bus didn't take of them.
  The module docs of `bus/systemd.rs` describe the hand-over, with a diagram of a restart
  (`bus/systemd/restart.mmd`, in mermaid), and name the many kinds of FDs that it takes. The
  tests restart busd through zoter's fake service manager, and the benchmark through a fake
  systemd of its own (`benches/bus/handover.rs`).
- **`connection/`** — the bus's end of each client's connection: its reads (`read.rs`), its
  writes (`write.rs`, and `Connection` in `mod.rs`), its handshake (`handshake.rs`), its
  credentials, and, on Linux, its records for a hand-over (`handover.rs`).
- **`peer/`** — the state of one connection. A connection starts as a peer and can become a
  monitor (an eavesdropper); `client.rs` holds the state that both share.
- **`peers/`** — the set of connected peers, as a handle that the bus and the task that answers
  the calls to it share. `routing.rs` holds the peers, the monitors and the name registry in one
  `RefCell`.
  `serve.rs` is the task that reads a peer's messages, `forward.rs` picks the recipients of each
  message, and `outbox.rs` collects what a batch sends to each recipient. `chores.rs` counts the
  bus's own work, such as the calls to the bus that it has yet to answer, which a hand-over waits
  for, `stats.rs` gathers the statistics, and `handover.rs` builds and restores the state that goes
  to the next instance.
- **`name_registry.rs`** — ownership of well-known bus names.
- **`match_rules.rs`** — match-rule parsing and signal delivery.
- **`fdo/`** — the bus itself, as its clients see it. One task answers the calls to the bus, one
  at a time, out of a queue that the peers' tasks fill (`mod.rs`). The bus forwards its replies
  and signals as it forwards a peer's messages. It serves `org.freedesktop.DBus` (`dbus.rs`),
  monitoring (`monitoring.rs`) and statistics (`stats.rs`) on `/org/freedesktop/DBus`, and the
  standard `Introspectable` (`introspectable.rs`, with the XML), `Properties` (`properties.rs`)
  and `Peer` (`peer.rs`) interfaces. `hook.rs` holds the bus's answers at gates, for the
  hand-over tests.
- **`config/`** — bus configuration file parsing: `xml.rs` (the `.conf` format),
  `policy.rs` and `rule.rs` (the access-control policy, as parsed).
- **`policy/`** — the access-control policy that the bus enforces, built from the parsed one:
  who may connect, own which names, and send and receive which messages; `ids.rs` resolves
  the names of users and groups.
- **`tracing_subscriber.rs`** — logging setup (behind the default `tracing-subscriber`
  feature).
- **`testing.rs`** — the helpers that the tests under `tests/` share: a bus for a test to run
  against, and its clients (behind the `testing` feature, which the tests turn on).

The library is internal to busd: it's only there for the binary, the tests and the
benchmarks to share the code, and is only published along with the binary. Its `pub` items
are public for those alone, so there's no public API to keep stable, to document for
users, or to list as new in pull requests.

busd builds on zbus releases from crates.io, or on a git revision of zbus until a release
has what busd needs, so bus-side changes in zbus can require updating this crate.
