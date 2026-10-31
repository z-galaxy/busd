//! The builds of busd that the comparison runs: this checkout's, and that of another revision of
//! its repository, each built as busd ships, rather than as the bench build has it.
//!
//! cargo bench builds busd with the features of the dev-dependencies, which it unifies into those
//! of busd's own dependencies, so each busd is built apart, with `cargo build --release`, in a
//! target directory of its own. The builds run before the first run, as the processes of git and
//! cargo would otherwise be counted with a client or a broker (see `usage.rs`).

use std::{
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::Value;

use crate::{calls::Result, output::note};

/// busd as this checkout builds it, built unless it is up to date: the path of its binary.
pub fn this_checkout() -> Result<PathBuf> {
    let checkout = Path::new(CHECKOUT);

    build(checkout, &checkout.join("target/compare/build"))
}

/// The directory of this checkout of busd, which the bench build was made from.
pub const CHECKOUT: &str = env!("CARGO_MANIFEST_DIR");

/// A build of busd at another revision of the repository of this checkout.
pub struct Revision {
    /// The full hash of its commit.
    pub hash: String,
    /// The directory that its sources are exported to, with their Cargo.lock.
    pub dir: PathBuf,
    /// The path of its binary.
    pub binary: PathBuf,
}

impl Revision {
    /// busd at `rev`, exported and built unless that was done already.
    ///
    /// `rev` is whatever git takes for a commit of this checkout, or else of
    /// https://github.com/z-galaxy/busd, which it is fetched from: a branch, a tag, a hash, or a
    /// ref such as `refs/pull/396/head`, the head of a pull request.
    ///
    /// The sources are exported to `target/compare/busd-<short hash>`, and built there. They are
    /// exported under another name first, which becomes theirs once they are whole, so that an
    /// export that stops halfway is not taken for a whole one by the next comparison, which reuses
    /// the export and its build.
    pub fn build(rev: &str) -> Result<Self> {
        let hash = resolve(rev)?;
        let short = hash.get(..7).unwrap_or(&hash);
        let dir = Path::new(CHECKOUT).join(format!("target/compare/busd-{short}"));
        if !dir.exists() {
            export(&hash, &dir)?;
        }
        let binary = build(&dir, &dir.join("target"))?;

        Ok(Self { hash, dir, binary })
    }

    /// The short hash of its commit, as the names of its directory and of its row in the summary
    /// have it.
    pub fn short(&self) -> &str {
        self.hash.get(..7).unwrap_or(&self.hash)
    }
}

/// The revision of this checkout, as `git describe --always --dirty` names it, with `abbrev`
/// digits of the hash of its commit, if git can tell.
pub fn describe(abbrev: u8) -> Option<String> {
    let abbrev = format!("--abbrev={abbrev}");

    git(&["describe", "--always", "--dirty", &abbrev]).ok()
}

/// The cargo that builds busd: the one that runs this benchmark, as cargo says in `$CARGO`, or
/// else the one on PATH.
pub fn cargo() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

/// Builds busd from the sources in `dir`, as it ships, in the target directory `target`, and
/// returns the path of its binary, as cargo names it.
///
/// What cargo prints for people, its diagnostics included, goes to the standard error, as the
/// standard output is the summary's. Its messages for programs, in JSON, come on its standard
/// output instead, and that of the binary names its path: cargo puts the binary in
/// `<triple>/release` of `target` rather than in `release` if its configuration names a target to
/// build for, as `build.target` does.
fn build(dir: &Path, target: &Path) -> Result<PathBuf> {
    note!("Building busd in {}", dir.display());
    let output = Command::new(cargo())
        .args([
            "build",
            "--release",
            "--locked",
            "--bin",
            "busd",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
        ])
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(target)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo could not build busd in {}", dir.display()).into());
    }
    let messages = String::from_utf8_lossy(&output.stdout);
    let Some(binary) = messages.lines().find_map(binary) else {
        let dir = dir.display();
        return Err(format!("cargo built busd in {dir}, but named no binary of it").into());
    };

    Ok(binary)
}

/// The path of the binary of busd, if `message`, a message of cargo in JSON, is the one that says
/// that it built it.
fn binary(message: &str) -> Option<PathBuf> {
    let message: Value = serde_json::from_str(message).ok()?;
    let target = &message["target"];
    let is_busd = message["reason"] == "compiler-artifact"
        && target["name"] == "busd"
        && target["kind"].as_array()?.iter().any(|kind| kind == "bin");
    if !is_busd {
        return None;
    }

    message["executable"].as_str().map(PathBuf::from)
}

/// The full hash of the commit `rev`: one that the checkout has, or else the one that
/// https://github.com/z-galaxy/busd has under that name, which is fetched.
fn resolve(rev: &str) -> Result<String> {
    let commit = format!("{rev}^{{commit}}");
    if let Ok(hash) = git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &commit,
    ]) {
        return Ok(hash);
    }

    note!("Fetching {rev} from {REPOSITORY}");
    git(&["fetch", "-q", "--end-of-options", REPOSITORY, rev])?;
    let hash = git(&["rev-parse", "--verify", "--quiet", "FETCH_HEAD^{commit}"])
        .map_err(|e| format!("{rev} is no commit of {REPOSITORY}: {e}"))?;

    Ok(hash)
}

/// The repository that a revision that the checkout does not have is fetched from.
const REPOSITORY: &str = "https://github.com/z-galaxy/busd";

/// Exports the sources of the commit `hash` to `dir`, which must not exist, through a directory of
/// another name, which becomes `dir` once the export is whole.
fn export(hash: &str, dir: &Path) -> Result<()> {
    let partial = dir.with_extension("new");
    match fs::remove_dir_all(&partial) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    fs::create_dir_all(&partial)?;

    note!("Exporting {hash} to {}", dir.display());
    let mut archive = Command::new("git")
        .arg("-C")
        .arg(CHECKOUT)
        .args(["archive", "--end-of-options", hash])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let Some(sources) = archive.stdout.take() else {
        unreachable!("the standard output of git archive is piped");
    };
    let extracted = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(&partial)
        .stdin(sources)
        .status();
    let archived = archive.wait()?;
    let extracted = extracted.map_err(|e| format!("cannot run tar: {e}"))?;
    if !archived.success() || !extracted.success() {
        return Err(format!("cannot export {hash} to {}", partial.display()).into());
    }
    fs::rename(&partial, dir)?;

    Ok(())
}

/// What git prints, trimmed, when run in the checkout with `args`, or why it failed.
fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(CHECKOUT)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {} failed: {}", args.join(" "), stderr.trim()).into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
