# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## 0.7.0 - 2026-11-13

### ✨ Added
- Serve the org.freedesktop.DBus.Debug.Stats interface. #453
- Hand over to the next instance of busd through systemd's fdstore, under socket activation.
- Hand the bus over to its next instance.
- Tell systemd once the bus is ready.
- Listen on the socket of systemd's socket activation.
- Create a bus on a socket that listens already.

### ♻️ Changed
- Tell in peer::Stream which messages get their sender disconnected.
- Pass what the task that serves a peer keeps of it in one struct.
- Add a peer, and the bus itself, through one method.
- Create the bus on its listener in a method of its own.
- Put the public methods of Bus above its private helpers.

### 📦 Dependencies
- Update zbus to 6.3.0.
- Update zruntime to v0.2.3 (#474).
- Add zoter 0.1, for the hand-over through systemd's fdstore.

### 📝 Documentation
- Document the policies of the configuration.
- Document how busd restarts through the fd store of systemd.
- Say that the library is internal to busd. #440
- Say in the README how busd runs under systemd.
- Say what busd takes the net feature of rustix for.
- Say that zbus keeps the credentials of each client.
- Say in the README that busd is meant to serve as a system bus too.

### 🐛 Fixed
- Queue the owner that a name is taken over from.
- Notify new name owners when a peer becomes a monitor.
- Keep a peer in the queue of a name at most once.
- List the Monitoring interface in the `Interfaces` property. #452
- Color the logs only on a terminal.
- Raise the soft limit of open file descriptors to the hard one.
- Replace a socket file that no one listens on anymore.
- Answer a call that its callee can't take, for its fds, with an error.
- Pass the messages that go nowhere else on to the monitors.
- Answer a call to a name that no peer has with an error. #393
- Wait 10 s rather than 100 ms for a client to take a message.

### ⚙️ Other
- 🛂 Read the policy again on ReloadConfig.
- 🛂 Enforce the policy of the configuration, but not its deprecated at_console policies. #79
- 🛂 Keep the names that each peer owns or queues for.
- 🛂 Add the policy that the bus is to enforce.
- 🛂 Keep the policies and rules of the configuration that were dropped.
- 📈 Count the connections, match rules and names of the bus.
- 🚨 Fix the warnings of the documentation build.
- 🛂 Let only privileged peers become monitors. #449
- 🦺 Ignore an `<include>` of a file that is being read already. #195
- 🔊 Log info, warnings and errors by default.
- 🧵 Serve a monitor from the task that served its peer, on its stream.
- 🧵 Clean up after a peer that becomes a monitor as it becomes one.
- 🧵 Answer the calls to the bus in the order that they come in.
- 🚩 Ask rustix for its std feature, which busd relies on.

### ⚡️ Performance
- Look the owners of names up without cloning them.
- Add a script that builds busd with PGO. #200
- Look the peers up by hash rather than in a tree. #389
- Keep the peers, the monitors and the names behind one lock.
- Keep the outbox of a peer from one batch to the next. #390

### 🔥 Removed
- Ignore `<auth>`, `<allow_anonymous>` and non-unix `<listen>`.
- Drop the choice of authentication mechanism.
- Drop support for the TCP transport. #433

### 🔒️ Security
- Refuse the name of the bus itself in RequestName and ReleaseName. #450
- Disconnect a client that sends a message reserved for local use. #451
- Limit the objects that the bus keeps for each user. #394

### ✅ Testing
- Benchmark how the bus hands its clients over to its next instance.
- Check that a stalled handshake counts against the quota of its user.
- Check that the bus tells the credentials of a stalled client at once.
- Test that a message brings only the fds that its header declares.

## 0.6.1 - 2026-11-02

### 📝 Documentation
- Link to LICENSE-MIT from CONTRIBUTING.md. #414

### 🐛 Fixed
- Remove the socket file on SIGTERM too. #410
- Don't panic on an abstract UNIX socket address. #409

### ⚙️ Other
- 🔊 Log to stderr rather than stdout. #411
- 🚩 Enable the usage and the error context of clap. #412

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
