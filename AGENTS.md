# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

For contribution conventions — commit-message format, atomic commits, code layout, and
more — follow the guidelines in [`CONTRIBUTING.md`](CONTRIBUTING.md).

- **Changelog-skip trailer**: end a commit message with a `Changelog: skip` git trailer to
  keep it out of the user-facing changelog (use for AI-workflow artifacts such as design docs
  and implementation plans).

## Project Overview

busd is a pure-Rust D-Bus bus (broker) implementation — the sibling daemon to the [zbus]
library, which it builds on (via zbus's `bus-impl` feature). Being pure Rust, it
cross-builds far more easily than the reference C broker. Status is alpha: only the
essentials of the D-Bus specification are in place.

busd runs on [zruntime], the runtime that zbus brings along with its `default-rt`
feature: the binary drives the bus and all of its connections on the main thread, through
`zruntime::block_on`. Dropping a zruntime `Task` cancels it, so a fire-and-forget task
must call `.detach()`.

[zbus]: https://github.com/z-galaxy/zbus
[zruntime]: https://github.com/z-galaxy/zruntime

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

# Format (nightly rustfmt) and lint
cargo +nightly fmt --all
cargo clippy -- -D warnings
```

## Architecture

The crate is both a library (`src/lib.rs`) and a thin CLI binary (`src/bin/busd.rs`,
argument parsing via clap). Key modules:

- **`bus/`** — the broker core: accepts peer connections and routes messages between them.
- **`systemd.rs`, `bus/systemd.rs`, `bus/handover.rs`** — running under systemd: socket
  activation, readiness notifications, and restarts through systemd's file descriptor store,
  which hand the clients over to the next instance. The module docs of `bus/systemd.rs`
  describe the hand-over, with a diagram of a restart (`bus/systemd/restart.mmd`, in
  mermaid), and name the many kinds of FDs that it takes.
- **`peer/`, `peers.rs`** — per-connection peer state and the set of connected peers,
  including monitor (eavesdropping) support.
- **`name_registry.rs`** — ownership of well-known bus names.
- **`match_rules.rs`** — match-rule parsing and signal delivery.
- **`fdo/`** — the standard `org.freedesktop.DBus` interfaces (`dbus.rs`) and monitoring
  (`monitoring.rs`).
- **`config/`** — bus configuration file parsing: `xml.rs` (the `.conf` format),
  `policy.rs` and `rule.rs` (access-control policy).
- **`tracing_subscriber.rs`** — logging setup (behind the default `tracing-subscriber`
  feature).

The library is internal to busd: it's only there for the binary, the tests and the
benchmarks to share the code, and is only published along with the binary. Its `pub` items
are public for those alone, so there's no public API to keep stable, to document for
users, or to list as new in pull requests.

busd builds on zbus releases from crates.io, so bus-side changes in a new zbus release
can require updating this crate.
