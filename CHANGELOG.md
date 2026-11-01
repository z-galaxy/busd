# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.6.0 - 2026-11-01

### 👷 CI
- Drop the tests on nightly Rust.
- Build the benchmarks in a container of Ubuntu 22.04.
- Build the benchmarks off the macro runner.
- Skip the benchmarks where they can't measure a change.
- Run the benchmarks on CodSpeed.
- Add a workflow to release with release-plz.
- Adopt shared commitlint config for `gimoji`.

### ♻️ Changed
- Make `tracing` an optional feature.
- Show changelog emojis on section headings only.
- Support skipping the changelog via a commit trailer.

### 📦 Dependencies
- Switch to zbus 6.2.1 & zruntime 0.2.2.
- Update rustix to v1.1.6 (#387).
- Use zruntime's Event instead of event-listener's.
- Drop the enumflags2 dependency.
- Bump the MSRV to 1.89.
- Update ntest to 0.10.0 (#354).
- Update rand to v0.10.3 (#353).
- Update quick-xml to 0.42.0 (#350).
- Update futures-util to v0.3.34 (#348).
- Update clap to v4.6.3 (#344).
- Update fastrand to v2.5.0 (#343).
- Update serde to v1.0.229 (#342).
- Update anyhow to v1.0.104 (#341).
- Update actions/checkout action to v7.
- Update tracing-subscriber to v0.3.23 (#312).

### 📝 Documentation
- Give numbers on how busd performs in the README.
- Add AGENTS.md pointing to CONTRIBUTING.md.
- Document top-down module ordering convention.

### 🐛 Fixed
- Go by the defaults if there's no session configuration.
- Listen in the directory for temporary files without a runtime dir.
- Name the socket that the bus fails to listen on.
- Disconnect clients that don't take a message within 100 ms.
- Don't abort matching a broadcast against a destination rule.
- Deliver signals sent to a well-known name to its owner.
- Don't abort on a broadcast signal to a monitor with rules.
- Use zbus Builder::build_message_stream to avoid dropping Hello.

### ⚙️ Other
- 🦺 Disconnect a client that sends a message with an invalid body. #385
- 👽️ Port to the zbus 6 API.
- 🚨 Fix clippy::useless_borrows_in_formatting.
- 🧵 Don't hold the peers lock during connection setup.
- 🤖 Custom regex for minor version bump.

### ⚡️ Performance
- Set the sender of a client's message without building it anew.
- Forward the messages of each peer in batches.
- Send a message to all of its recipients at once.
- Port from Tokio to zruntime.
- Filter logs with `Targets` instead of `EnvFilter`.

### 🔥 Removed
- Drop the console-subscriber feature.

### 🔒️ Security
- Deliver only the replies that answer a pending call. #386
- Disconnect clients that send messages with too many fds.
- Don't let match rules match messages sent to others.

### ✅ Testing
- Compare busd with other brokers, on the workloads of dbus-bench.
- Benchmark how the bus routes the traffic of its clients.
- Time the stalled subscribers' disconnections during the flood.
- Resolve a TCP port conflict between 2 tests.
- Fix loop condition in a test.
- Increase test timeouts from 15 to 30 seconds.

## 0.5.0 - 2026-01-13

### Added
- 👷 Drop Windows jobs from CI.
- ✨ Implement unix:dir and unix:tmpdir for `--address`.
- 👷 Suggest to copy the git hooks.

### Changed
- 🎨 A minor readability improvement.
- 🚚 Update Github org name.
- 🎨 Fix Cargo.toml formatting.
- ♻️ Extract `Rule` and `Policy` into their own files.
- 🔧 Support `--config`, `--session` (default), and `--system`.
- 🔧 Read XML file(s) into consumer-ready `Config`. #78
- ♻️ Extract `unix_addr()` out of `unix_stream()`.
- 🔧 Change the default address to listen upon.

### Dependencies
- ⬆️ Update quick-xml to 0.39.0 (#288).
- ⬆️ Update tokio to v1.49.0 (#287).
- ⬆️ Update clap to v4.5.54 (#286).
- ⬆️ Update rustix to v1.1.3 (#285).
- ⬆️ Update tracing to v0.1.44 (#284).
- ⬆️ Update ntest to v0.9.5 (#283).
- ⬆️ Update tracing-subscriber to v0.3.22 (#281).
- ⬆️ Update actions/checkout action to v6.
- ⬆️ Update console-subscriber to 0.5.0 (#273).
- ➖ Drop now redundant `nix` dep.
- ➕ Use `rustix` instead of `nix`.
- ⬆️  Update zbus to 5.13.1.
- ⬆️ Update enumflags2 to v0.7.12 (#232).
- ⬆️ Update nix to 0.30.0 (#225).
- ➕ Add the "quick-xml" crate.
- ⬆️ Update fastrand to v2.3.0 (#180).
- ➕ Add "fastrand" crate.
- ➖ Drop hex dependency.
- ➖ rand now a dev-dependency.

### Documentation
- 📝 Link to gimoji's web interface.
- 📝 Remove Windows support claim from the README.
- 📝 Sync with zbus' CONTRIBUTING.md.

### Fixed
- 🐛 Add previous `guid` to new `address`.
- 🐛 Expose the socket address clients can use.

### Other
- 🤖 Automate releasing with release-plz.
- 🚨 Make latest clippy happy.
- 🔊 Make errors and warnings a little bit more descriptive.
- 🚩 Drop `fs` feature of tokio.
- 🚩 Drop default-features of futures-util crate.
- 🚩 Drop uneeded clap default features.

### Performance
- ⚡️ More binary optimizations for release builds (#228).
- ⚡️ Drop docs generation in fdo interfaces.
- ⚡️ Trade compile-time for binary size reduction in release mode.

### Removed
- 🔥 Remove a useless conversion.
- 🔥 Drop support for Windows.

### Security
- 🔒️ Add comprehensive security policy. #147
