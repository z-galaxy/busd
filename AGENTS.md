# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

For contribution conventions — commit-message format, atomic commits, code layout, and
more — follow the guidelines in [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Project Overview

busd is a pure-Rust D-Bus bus (broker) implementation — the sibling daemon to the [zbus]
library, which it builds on (via zbus's `bus-impl` feature). Being pure Rust, it
cross-builds far more easily than the reference C broker. Status is alpha: only the
essentials of the D-Bus specification are in place.

[zbus]: https://github.com/z-galaxy/zbus

## Development Commands

```bash
# Build
cargo build
cargo build --release

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

busd tracks zbus's git `main` (see the note in `Cargo.toml`), so bus-side changes there
can require updating this crate.
