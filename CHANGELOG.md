# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
