//! Read-only access to committed Git objects.
//!
//! Every Git invocation in this crate happens here, is restricted to plumbing
//! verbs with fixed argument lists, and never mutates the worktree, index,
//! refs or object store. Configured helpers (hooks, filters, textconv, credential
//! helpers, pagers) are not used to read source, and missing objects are never
//! lazily fetched from a promisor remote.
//!
//! Contracts enforced here:
//!
//! - Discovery requires a non-bare worktree with at least one commit, and
//!   refuses grafted history and partial clones. History traversal additionally
//!   refuses shallow clones; measuring an available shallow HEAD is allowed.
//! - User-supplied revisions are validated once through
//!   `rev-parse --verify --end-of-options <rev>^{commit}`; every other command
//!   receives only validated hexadecimal object ids.
//! - Commit metadata uses the committer UNIX timestamp, never a rendered local
//!   time, so no timezone rendering enters the measurement.
//! - Blob reads run through a single persistent `cat-file --batch` child, one
//!   request at a time, with exact framed reads.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Minimum supported Git release.
///
/// `git --no-lazy-fetch` and the `GIT_NO_LAZY_FETCH` environment variable were
/// introduced in Git 2.45 (`git.c`, `NO_LAZY_FETCH_ENVIRONMENT`). Older releases
/// silently ignore both and would fetch missing objects from a promisor remote
/// while measuring, so they are refused rather than trusted.
pub const MINIMUM_GIT_VERSION: (u32, u32) = (2, 45);

/// A commit identified by its full object id and committer timestamp.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub committed_at: DateTime<Utc>,
}

/// One entry of a recursive tree listing.
///
/// Trees are listed recursively, so `path` is always repository-root relative
/// and `mode` distinguishes regular files from symlinks (`120000`) and
/// submodule gitlinks (`160000`). Symlinks and submodules are reported so they
/// remain visible in coverage; they are never followed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub oid: String,
    pub mode: String,
}

impl TreeEntry {
    /// True for regular blobs (`100644`) and executable blobs (`100755`).
    pub fn is_regular(&self) -> bool {
        self.mode == "100644" || self.mode == "100755"
    }
}

/// A validated, non-bare worktree without promisor fetches.
#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
}

impl Repository {
    /// Discovers the worktree containing `cwd` and validates that its history
    /// can be measured without mutation or network access.
    ///
    /// Fails explicitly when `cwd` is not inside a Git worktree, when the
    /// repository is bare, when `HEAD` is unborn, when history is grafted, and
    /// when the repository is a partial clone with a promisor remote.
    pub fn discover(cwd: &Path) -> Result<Self> {
        ensure_git_version()?;
        if !cwd.is_dir() {
            bail!("{} is not an existing directory", cwd.display());
        }
        let probe = capture(cwd, &["rev-parse", "--is-bare-repository"])?;
        if !probe.status.success() {
            bail!(
                "{} is not inside a usable Git worktree: {}",
                cwd.display(),
                stderr(&probe)
            );
        }
        if text(probe)? == "true" {
            bail!(
                "{} is a bare repository; erosion measures committed trees from a worktree",
                cwd.display()
            );
        }
        let inside = require(cwd, &["rev-parse", "--is-inside-work-tree"])?;
        if inside != "true" {
            bail!(
                "{} is inside the Git directory, not a worktree; run erosion from a worktree",
                cwd.display()
            );
        }
        let root = PathBuf::from(require(cwd, &["rev-parse", "--show-toplevel"])?);
        if !root.is_dir() {
            bail!(
                "Git reported worktree root {} which is not a directory",
                root.display()
            );
        }
        let repository = Self { root };
        repository.reject_grafts()?;
        repository.reject_partial_clone()?;
        repository.reject_unborn_head()?;
        Ok(repository)
    }

    /// Resolves a user-supplied revision to a commit.
    ///
    /// The revision is the only untrusted string ever handed to Git, and it is
    /// separated from options by `--end-of-options`. Tags and other peelable
    /// objects are peeled to a commit; anything else is an error.
    pub fn resolve(&self, revision: &str) -> Result<Commit> {
        if revision.is_empty() {
            bail!("Empty revision");
        }
        if revision.contains('\0') || revision.contains('\n') {
            bail!("Revision {revision:?} contains a control character");
        }
        let peeled = format!("{revision}^{{commit}}");
        let output = capture(
            &self.root,
            &["rev-parse", "--verify", "--end-of-options", peeled.as_str()],
        )?;
        if !output.status.success() {
            bail!(
                "Revision {revision:?} does not resolve to a commit in {}: {}",
                self.root.display(),
                stderr(&output)
            );
        }
        let sha = validate_oid(text(output)?.as_str())?;
        let mut commits = self.commits(&[
            "rev-list",
            "--max-count=1",
            "--no-commit-header",
            "--format=%H %ct",
            sha.as_str(),
        ])?;
        if commits.len() != 1 {
            bail!(
                "Expected one commit record for {sha}, got {}",
                commits.len()
            );
        }
        Ok(commits.remove(0))
    }

    /// Returns the first-parent chain starting at `head`, newest first.
    ///
    /// The whole chain is traversed and materialized before any checkpoint is
    /// selected: a truncated or partially readable history is an error, never a
    /// shorter series. Committer timestamps are returned as recorded and are not
    /// sorted, because Git does not require them to be monotonic.
    pub fn first_parent_chain(&self, head: &Commit) -> Result<Vec<Commit>> {
        let sha = validate_oid(&head.sha)?;
        self.reject_shallow()?;
        let commits = self.commits(&[
            "rev-list",
            "--first-parent",
            "--no-commit-header",
            "--format=%H %ct",
            sha.as_str(),
        ])?;
        match commits.first() {
            Some(first) if first.sha == sha => Ok(commits),
            Some(first) => bail!("First-parent traversal of {sha} started at {}", first.sha),
            None => bail!("First-parent traversal of {sha} produced no commits"),
        }
    }

    /// Lists every entry of the commit's tree, recursively, sorted by path.
    ///
    /// Non-UTF-8 paths are reported as errors rather than lossily decoded, so a
    /// measurement never silently attributes lines to a renamed path.
    pub fn entries(&self, sha: &str) -> Result<Vec<TreeEntry>> {
        let sha = validate_oid(sha)?;
        let output = capture(
            &self.root,
            &["ls-tree", "-r", "-z", "--full-tree", sha.as_str()],
        )?;
        if !output.status.success() {
            bail!("Listing the tree of {sha} failed: {}", stderr(&output));
        }
        let mut entries = Vec::new();
        for record in output.stdout.split(|byte| *byte == 0) {
            if record.is_empty() {
                continue;
            }
            entries.push(parse_tree_entry(record, &sha)?);
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    /// Starts a persistent blob reader for this repository.
    pub fn blobs(&self) -> Result<BlobReader> {
        BlobReader::start(&self.root)
    }

    fn commits(&self, args: &[&str]) -> Result<Vec<Commit>> {
        let output = capture(&self.root, args)?;
        if !output.status.success() {
            bail!(
                "Reading commit metadata failed ({}): {}",
                args.join(" "),
                stderr(&output)
            );
        }
        let listing =
            String::from_utf8(output.stdout).context("Git printed non-UTF-8 commit metadata")?;
        let mut commits = Vec::new();
        for line in listing.lines() {
            if line.is_empty() {
                continue;
            }
            let (sha, seconds) = line
                .split_once(' ')
                .ok_or_else(|| anyhow!("Malformed commit record {line:?}"))?;
            let sha = validate_oid(sha)?;
            let seconds: i64 = seconds
                .parse()
                .with_context(|| format!("Malformed committer timestamp in {line:?}"))?;
            let committed_at = DateTime::from_timestamp(seconds, 0).ok_or_else(|| {
                anyhow!("Commit {sha} has an out-of-range committer timestamp {seconds}")
            })?;
            commits.push(Commit { sha, committed_at });
        }
        Ok(commits)
    }

    fn reject_shallow(&self) -> Result<()> {
        if require(&self.root, &["rev-parse", "--is-shallow-repository"])? == "true" {
            bail!(
                "{} is a shallow clone; its history is truncated and cannot be measured. \
                 Unshallow the repository first",
                self.root.display()
            );
        }
        Ok(())
    }

    fn reject_grafts(&self) -> Result<()> {
        let grafts = self.root.join(require(
            &self.root,
            &["rev-parse", "--git-path", "info/grafts"],
        )?);
        if grafts.exists() {
            bail!(
                "{} rewrites history with a grafts file ({}); measured history would not match the \
                 recorded commits",
                self.root.display(),
                grafts.display()
            );
        }
        Ok(())
    }

    fn reject_partial_clone(&self) -> Result<()> {
        let output = capture(&self.root, &["config", "--list", "-z"])?;
        if !output.status.success() {
            bail!("Reading Git configuration failed: {}", stderr(&output));
        }
        let listing =
            String::from_utf8(output.stdout).context("Git printed non-UTF-8 configuration")?;
        for record in listing.split('\0') {
            let (key, value) = match record.split_once('\n') {
                Some(pair) => pair,
                None if record.is_empty() => continue,
                None => (record, ""),
            };
            let promisor = key == "extensions.partialclone"
                || (key.starts_with("remote.") && key.ends_with(".partialclonefilter"))
                || (key.starts_with("remote.")
                    && key.ends_with(".promisor")
                    && !matches!(value, "false" | "0" | ""));
            if promisor {
                bail!(
                    "{} is a partial clone ({key}); measuring it would lazily fetch missing objects. \
                     Use a complete clone",
                    self.root.display()
                );
            }
            if key == "core.graftfile" {
                bail!(
                    "{} rewrites history with core.graftFile={value}; measured history would not \
                     match the recorded commits",
                    self.root.display()
                );
            }
        }
        Ok(())
    }

    fn reject_unborn_head(&self) -> Result<()> {
        let output = capture(
            &self.root,
            &["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"],
        )?;
        if !output.status.success() {
            bail!(
                "{} has no commits on HEAD: {}",
                self.root.display(),
                stderr(&output)
            );
        }
        Ok(())
    }
}

/// A persistent `git cat-file --batch` child process.
///
/// Requests are strictly lockstep: one object id is written and flushed, its
/// header and exact payload are read back, and only then may another request be
/// sent. Nothing is speculatively queued, so the reader can never deadlock
/// against a full pipe. The child's stderr is drained by a dedicated thread so
/// diagnostics are preserved without blocking the protocol.
pub struct BlobReader {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    errors: Arc<Mutex<Vec<u8>>>,
    drain: Option<JoinHandle<()>>,
    failed: bool,
}

impl BlobReader {
    fn start(root: &Path) -> Result<Self> {
        let mut command = git(root);
        command
            .arg("cat-file")
            .arg("--batch")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("Starting git cat-file --batch in {}", root.display()))?;
        let stdin = child.stdin.take().context("git cat-file has no stdin")?;
        let stdout = child.stdout.take().context("git cat-file has no stdout")?;
        let mut stderr = child.stderr.take().context("git cat-file has no stderr")?;
        let errors = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        let drain = std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            if let Ok(mut shared) = sink.lock() {
                shared.extend_from_slice(&buffer);
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            errors,
            drain: Some(drain),
            failed: false,
        })
    }

    /// Reads one object's bytes verbatim.
    ///
    /// The content is the stored blob: no clean/smudge filter, textconv or
    /// end-of-line conversion is applied, and a missing object is an error
    /// rather than a fetch. A protocol failure poisons the reader, because the
    /// stream position can no longer be trusted.
    pub fn read(&mut self, oid: &str) -> Result<Vec<u8>> {
        if self.failed {
            bail!("The blob reader is unusable after an earlier failure");
        }
        let oid = validate_oid(oid)?;
        match self.exchange(&oid) {
            Ok(bytes) => Ok(bytes),
            Err(error) => {
                self.failed = true;
                let diagnostics = self.diagnostics();
                if diagnostics.is_empty() {
                    Err(error)
                } else {
                    Err(error.context(format!("git cat-file reported: {diagnostics}")))
                }
            }
        }
    }

    fn exchange(&mut self, oid: &str) -> Result<Vec<u8>> {
        let stdin = self.stdin.as_mut().context("The blob reader is closed")?;
        stdin
            .write_all(format!("{oid}\n").as_bytes())
            .with_context(|| format!("Requesting object {oid}"))?;
        stdin
            .flush()
            .with_context(|| format!("Requesting object {oid}"))?;
        let mut header = String::new();
        let read = self
            .stdout
            .read_line(&mut header)
            .with_context(|| format!("Reading the response header for {oid}"))?;
        if read == 0 {
            bail!("git cat-file exited before answering the request for {oid}");
        }
        let header = header.trim_end_matches('\n');
        let mut fields = header.split(' ');
        let echoed = fields.next().unwrap_or_default();
        let kind = fields
            .next()
            .ok_or_else(|| anyhow!("Malformed cat-file header {header:?}"))?;
        if kind == "missing" {
            bail!("Object {oid} is missing from the repository");
        }
        if kind != "blob" {
            bail!("Object {oid} is a {kind}, not a blob");
        }
        if echoed != oid {
            bail!("git cat-file answered for {echoed}, not {oid}");
        }
        let size: usize = fields
            .next()
            .ok_or_else(|| anyhow!("Malformed cat-file header {header:?}"))?
            .parse()
            .with_context(|| format!("Malformed object size in {header:?}"))?;
        if fields.next().is_some() {
            bail!("Malformed cat-file header {header:?}");
        }
        let mut content = vec![0; size];
        self.stdout
            .read_exact(&mut content)
            .with_context(|| format!("Reading {size} bytes of object {oid}"))?;
        let mut terminator = [0; 1];
        self.stdout
            .read_exact(&mut terminator)
            .with_context(|| format!("Reading the record terminator of object {oid}"))?;
        if terminator[0] != b'\n' {
            bail!("Object {oid} was not terminated by a newline");
        }
        Ok(content)
    }

    fn diagnostics(&self) -> String {
        let captured = match self.errors.lock() {
            Ok(shared) => shared.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        String::from_utf8_lossy(&captured).trim().to_owned()
    }
}

impl Drop for BlobReader {
    fn drop(&mut self) {
        // Close the request pipe first so a healthy child exits on its own, then
        // guarantee termination so neither the process nor the stderr drain
        // thread can outlive the reader.
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

/// Builds a Git invocation with the fixed read-only environment.
///
/// `--no-pager` and `--no-replace-objects` keep output machine readable and
/// unrewritten; `--no-lazy-fetch` plus `GIT_NO_LAZY_FETCH` refuse promisor
/// fetches; `GIT_OPTIONAL_LOCKS=0`, `gc.auto`, `maintenance.auto` and
/// `core.fsmonitor` keep the invocation from writing to the repository;
/// `GIT_TERMINAL_PROMPT=0` prevents an interactive stall.
fn git(root: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root.as_os_str())
        .args([
            "--no-pager",
            "--no-replace-objects",
            "--no-lazy-fetch",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=0",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn capture(root: &Path, args: &[&str]) -> Result<Output> {
    git(root)
        .args(args.iter().map(OsStr::new))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("Running git {} in {}", args.join(" "), root.display()))
}

fn require(root: &Path, args: &[&str]) -> Result<String> {
    let output = capture(root, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed in {}: {}",
            args.join(" "),
            root.display(),
            stderr(&output)
        );
    }
    text(output)
}

fn text(output: Output) -> Result<String> {
    let text = String::from_utf8(output.stdout).context("Git printed non-UTF-8 output")?;
    Ok(text.trim_end_matches(['\n', '\r']).to_owned())
}

fn stderr(output: &Output) -> String {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        format!("exit status {}", output.status)
    } else {
        message
    }
}

fn ensure_git_version() -> Result<()> {
    let output = Command::new("git")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Running git --version; is Git installed and on PATH?")?;
    if !output.status.success() {
        bail!("git --version failed: {}", stderr(&output));
    }
    let banner = text(output)?;
    let (major, minor) = parse_version(&banner)
        .ok_or_else(|| anyhow!("Could not parse the Git version from {banner:?}"))?;
    if (major, minor) < MINIMUM_GIT_VERSION {
        bail!(
            "Git {major}.{minor} is too old; erosion requires Git {}.{} for --no-lazy-fetch, \
             without which missing objects would be fetched from a promisor remote",
            MINIMUM_GIT_VERSION.0,
            MINIMUM_GIT_VERSION.1
        );
    }
    Ok(())
}

fn parse_version(banner: &str) -> Option<(u32, u32)> {
    let digits = banner
        .split_whitespace()
        .find(|field| field.starts_with(|character: char| character.is_ascii_digit()))?;
    let mut parts = digits.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0");
    let minor = minor
        .trim_end_matches(|character: char| !character.is_ascii_digit())
        .parse()
        .ok()?;
    Some((major, minor))
}

fn validate_oid(oid: &str) -> Result<String> {
    let valid = matches!(oid.len(), 40 | 64)
        && oid
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    if !valid {
        bail!("{oid:?} is not a full hexadecimal object id");
    }
    Ok(oid.to_owned())
}

fn parse_tree_entry(record: &[u8], sha: &str) -> Result<TreeEntry> {
    // `<mode> SP <type> SP <oid> TAB <path>`; with -z the path is verbatim, so
    // the first tab is the only safe split point.
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| anyhow!("Malformed ls-tree record in {sha}"))?;
    let header = std::str::from_utf8(&record[..tab])
        .with_context(|| format!("Malformed ls-tree record in {sha}"))?;
    let path = std::str::from_utf8(&record[tab + 1..]).map_err(|_| {
        anyhow!(
            "Commit {sha} contains the non-UTF-8 path {:?}; erosion refuses to guess its encoding",
            String::from_utf8_lossy(&record[tab + 1..])
        )
    })?;
    let mut fields = header.split(' ');
    let mode = fields
        .next()
        .ok_or_else(|| anyhow!("Malformed ls-tree record {header:?} in {sha}"))?;
    let kind = fields
        .next()
        .ok_or_else(|| anyhow!("Malformed ls-tree record {header:?} in {sha}"))?;
    let oid = fields
        .next()
        .ok_or_else(|| anyhow!("Malformed ls-tree record {header:?} in {sha}"))?;
    if fields.next().is_some() {
        bail!("Malformed ls-tree record {header:?} in {sha}");
    }
    if mode.len() != 6 || !mode.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("Malformed mode {mode:?} in {sha}");
    }
    if path.is_empty() {
        bail!("Empty path in ls-tree record {header:?} in {sha}");
    }
    if !matches!(kind, "blob" | "commit" | "tree") {
        bail!("Unexpected object type {kind:?} in {sha}");
    }
    Ok(TreeEntry {
        path: path.to_owned(),
        oid: validate_oid(oid)?,
        mode: mode.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    fn scratch() -> TempDir {
        // Fixtures stay inside the build directory: the crate never writes test
        // data outside its own tree.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch");
        fs::create_dir_all(&root).expect("create scratch root");
        TempDir::new_in(root).expect("create scratch directory")
    }

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            // Isolate fixtures from the developer's own Git configuration.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Erosion Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Erosion Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("utf-8 git output")
            .trim_end()
            .to_owned()
    }

    fn commit(dir: &Path, message: &str, date: &str) {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["commit", "-q", "--no-gpg-sign", "-m", message])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Erosion Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Erosion Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .expect("run git commit");
        assert!(
            output.status.success(),
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init(dir: &Path) {
        run(dir, &["init", "-q", "--initial-branch=main", "."]);
        run(dir, &["config", "core.autocrlf", "false"]);
    }

    /// Two dated commits; the second adds an executable file.
    fn fixture() -> TempDir {
        let scratch = scratch();
        let dir = scratch.path();
        init(dir);
        fs::write(dir.join("first.js"), b"function a() {}\n").expect("write");
        run(dir, &["add", "-A"]);
        commit(dir, "first", "2024-01-01T00:00:00+0000");
        fs::write(dir.join("second.js"), b"function b() {}\n").expect("write");
        let script = dir.join("run.sh");
        fs::write(&script, b"#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        run(dir, &["add", "-A"]);
        commit(dir, "second", "2024-03-01T12:30:00+0000");
        scratch
    }

    #[test]
    fn parses_git_version_banners() {
        assert_eq!(
            parse_version("git version 2.50.1 (Apple Git-155)"),
            Some((2, 50))
        );
        assert_eq!(parse_version("git version 2.45.0"), Some((2, 45)));
        assert_eq!(parse_version("git version 2.39.3.windows.1"), Some((2, 39)));
        assert_eq!(parse_version("git version 3"), Some((3, 0)));
        assert_eq!(parse_version("not a version"), None);
    }

    #[test]
    fn rejects_partial_and_uppercase_object_ids() {
        assert!(validate_oid("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(validate_oid("0123456789ABCDEF0123456789abcdef01234567").is_err());
        assert!(validate_oid("abc").is_err());
        assert!(validate_oid("").is_err());
    }

    #[test]
    fn resolves_revisions_and_first_parent_history() {
        let scratch = fixture();
        let repository = Repository::discover(scratch.path()).expect("discover");
        assert_eq!(
            repository.root.canonicalize().ok(),
            scratch.path().canonicalize().ok()
        );
        let head = repository.resolve("HEAD").expect("resolve HEAD");
        assert_eq!(head.committed_at.timestamp(), 1_709_296_200);
        let chain = repository.first_parent_chain(&head).expect("chain");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].sha, head.sha);
        assert_eq!(chain[1].committed_at.timestamp(), 1_704_067_200);
        let older = repository.resolve("HEAD~1").expect("resolve HEAD~1");
        assert_eq!(older.sha, chain[1].sha);
        assert_eq!(
            repository.first_parent_chain(&older).expect("chain").len(),
            1
        );
    }

    #[test]
    fn rejects_unknown_revisions() {
        let scratch = fixture();
        let repository = Repository::discover(scratch.path()).expect("discover");
        let error = repository
            .resolve("v9.9.9-missing")
            .expect_err("unknown revision");
        assert!(
            error.to_string().contains("does not resolve to a commit"),
            "{error}"
        );
        assert!(repository.resolve("").is_err());
        assert!(repository.resolve("HEAD\n--help").is_err());
        // A tree is not a commit.
        let tree = run(scratch.path(), &["rev-parse", "HEAD^{tree}"]);
        assert!(repository.resolve(&tree).is_err());
    }

    #[test]
    fn lists_tree_entries_sorted_and_ignores_the_worktree() {
        let scratch = fixture();
        let dir = scratch.path();
        fs::write(dir.join("first.js"), b"uncommitted garbage\n").expect("write");
        fs::write(dir.join("untracked.js"), b"function c() {}\n").expect("write");
        let repository = Repository::discover(dir).expect("discover");
        let head = repository.resolve("HEAD").expect("resolve");
        let entries = repository.entries(&head.sha).expect("entries");
        let paths: Vec<_> = entries.iter().map(|entry| entry.path.as_str()).collect();
        assert_eq!(paths, vec!["first.js", "run.sh", "second.js"]);
        let mut reader = repository.blobs().expect("blobs");
        let first = entries
            .iter()
            .find(|entry| entry.path == "first.js")
            .expect("entry");
        assert_eq!(reader.read(&first.oid).expect("read"), b"function a() {}\n");
        #[cfg(unix)]
        {
            let script = entries
                .iter()
                .find(|entry| entry.path == "run.sh")
                .expect("entry");
            assert_eq!(script.mode, "100755");
            assert!(script.is_regular());
        }
        assert!(entries.iter().all(|entry| entry.is_regular()));
    }

    #[test]
    fn reports_symlinks_and_gitlinks_without_following_them() {
        let scratch = fixture();
        let dir = scratch.path();
        let head = run(dir, &["rev-parse", "HEAD"]);
        #[cfg(unix)]
        std::os::unix::fs::symlink("first.js", dir.join("link.js")).expect("symlink");
        run(dir, &["add", "-A"]);
        // Staged after `add`, which would otherwise record the absent submodule
        // directory as a deletion.
        run(
            dir,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{head},module"),
            ],
        );
        commit(dir, "links", "2024-04-01T00:00:00+0000");
        let repository = Repository::discover(dir).expect("discover");
        let commit = repository.resolve("HEAD").expect("resolve");
        let entries = repository.entries(&commit.sha).expect("entries");
        let gitlink = entries
            .iter()
            .find(|entry| entry.path == "module")
            .expect("gitlink");
        assert_eq!(gitlink.mode, "160000");
        assert_eq!(gitlink.oid, head);
        assert!(!gitlink.is_regular());
        #[cfg(unix)]
        {
            let link = entries
                .iter()
                .find(|entry| entry.path == "link.js")
                .expect("symlink");
            assert_eq!(link.mode, "120000");
            assert!(!link.is_regular());
            let mut reader = repository.blobs().expect("blobs");
            // The symlink blob holds its target, never the target's content.
            assert_eq!(reader.read(&link.oid).expect("read"), b"first.js");
        }
    }

    #[cfg(unix)]
    #[test]
    fn frames_paths_containing_tabs_and_newlines() {
        let scratch = fixture();
        let dir = scratch.path();
        fs::write(dir.join("tab\there.js"), b"function tabbed() {}\n").expect("write");
        fs::write(dir.join("line\nbreak.js"), b"function broken() {}\n").expect("write");
        run(dir, &["add", "-A"]);
        commit(dir, "awkward names", "2024-05-01T00:00:00+0000");
        let repository = Repository::discover(dir).expect("discover");
        let head = repository.resolve("HEAD").expect("resolve");
        let entries = repository.entries(&head.sha).expect("entries");
        let tabbed = entries
            .iter()
            .find(|entry| entry.path == "tab\there.js")
            .expect("tab path");
        let broken = entries
            .iter()
            .find(|entry| entry.path == "line\nbreak.js")
            .expect("lf path");
        assert!(tabbed.is_regular() && broken.is_regular());
        let mut reader = repository.blobs().expect("blobs");
        assert_eq!(
            reader.read(&broken.oid).expect("read"),
            b"function broken() {}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;
        let scratch = fixture();
        let dir = scratch.path();
        let existing = run(dir, &["rev-parse", "HEAD:first.js"]);
        let name = OsStr::from_bytes(b"bad\xff.js");
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["update-index", "--add", "--cacheinfo", "100644", &existing])
            .arg(name)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("update-index");
        assert!(
            output.status.success(),
            "update-index failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let tree = run(dir, &["write-tree"]);
        let head = run(dir, &["rev-parse", "HEAD"]);
        let created = run(dir, &["commit-tree", &tree, "-p", &head, "-m", "non-utf8"]);
        let repository = Repository::discover(dir).expect("discover");
        let error = repository.entries(&created).expect_err("non-UTF-8 path");
        assert!(error.to_string().contains("non-UTF-8 path"), "{error}");
    }

    #[test]
    fn reads_blobs_in_lockstep_and_reports_missing_objects() {
        let scratch = fixture();
        let repository = Repository::discover(scratch.path()).expect("discover");
        let head = repository.resolve("HEAD").expect("resolve");
        let entries = repository.entries(&head.sha).expect("entries");
        let mut reader = repository.blobs().expect("blobs");
        for entry in &entries {
            let content = reader.read(&entry.oid).expect("read");
            assert!(!content.is_empty());
            // Repeated reads of the same object stay in sync.
            assert_eq!(reader.read(&entry.oid).expect("re-read"), content);
        }
        let missing = "0".repeat(head.sha.len());
        let error = reader.read(&missing).expect_err("missing object");
        assert!(error.to_string().contains("is missing"), "{error}");
        let error = reader.read(&entries[0].oid).expect_err("poisoned reader");
        assert!(error.to_string().contains("unusable"), "{error}");
        assert!(reader.read("not-an-oid").is_err());
    }

    #[test]
    fn reads_blobs_without_content_filters() {
        let scratch = fixture();
        let dir = scratch.path();
        fs::write(dir.join(".gitattributes"), b"*.js text eol=crlf\n").expect("write");
        fs::write(dir.join("binary.js"), b"a\r\nb\x00c").expect("write");
        run(dir, &["add", "-A"]);
        commit(dir, "attributes", "2024-06-01T00:00:00+0000");
        let repository = Repository::discover(dir).expect("discover");
        let head = repository.resolve("HEAD").expect("resolve");
        let entries = repository.entries(&head.sha).expect("entries");
        let binary = entries
            .iter()
            .find(|entry| entry.path == "binary.js")
            .expect("entry");
        let mut reader = repository.blobs().expect("blobs");
        // The stored bytes are returned verbatim: `eol=crlf` would rewrite them
        // on checkout, and measurement must not see that rewrite.
        assert_eq!(reader.read(&binary.oid).expect("read"), b"a\nb\x00c");
        assert_eq!(
            fs::read(dir.join("binary.js")).expect("worktree"),
            b"a\r\nb\x00c"
        );
    }

    #[test]
    fn rejects_shallow_history() {
        let scratch = fixture();
        let dir = scratch.path();
        let repository = Repository::discover(dir).expect("discover");
        let head = repository.resolve("HEAD").expect("resolve");
        let git_dir = dir.join(run(dir, &["rev-parse", "--git-dir"]));
        fs::write(git_dir.join("shallow"), format!("{}\n", head.sha)).expect("write shallow");
        let error = repository
            .first_parent_chain(&head)
            .expect_err("shallow chain");
        assert!(error.to_string().contains("shallow clone"), "{error}");
        let shallow = Repository::discover(dir).expect("shallow HEAD can be measured");
        assert_eq!(shallow.resolve("HEAD").unwrap().sha, head.sha);
        assert!(!shallow.entries(&head.sha).unwrap().is_empty());
    }

    #[test]
    fn rejects_grafted_history() {
        let scratch = fixture();
        let dir = scratch.path();
        let head = run(dir, &["rev-parse", "HEAD"]);
        let git_dir = dir.join(run(dir, &["rev-parse", "--git-dir"]));
        fs::create_dir_all(git_dir.join("info")).expect("info directory");
        fs::write(git_dir.join("info").join("grafts"), format!("{head}\n")).expect("write grafts");
        let error = Repository::discover(dir).expect_err("grafted repository");
        assert!(error.to_string().contains("grafts file"), "{error}");
    }

    #[test]
    fn rejects_partial_clones() {
        let scratch = fixture();
        let dir = scratch.path();
        run(dir, &["config", "remote.origin.promisor", "true"]);
        let error = Repository::discover(dir).expect_err("partial clone");
        assert!(error.to_string().contains("partial clone"), "{error}");
    }

    #[test]
    fn rejects_bare_and_unborn_repositories() {
        let bare = scratch();
        run(bare.path(), &["init", "-q", "--bare", "."]);
        let error = Repository::discover(bare.path()).expect_err("bare repository");
        assert!(error.to_string().contains("bare repository"), "{error}");

        let unborn = scratch();
        init(unborn.path());
        let error = Repository::discover(unborn.path()).expect_err("unborn HEAD");
        assert!(error.to_string().contains("no commits"), "{error}");
    }

    #[test]
    fn rejects_unusable_directories() {
        let scratch = scratch();
        let missing = scratch.path().join("absent");
        let error = Repository::discover(&missing).expect_err("missing directory");
        assert!(
            error.to_string().contains("not an existing directory"),
            "{error}"
        );

        let broken = scratch.path().join("broken");
        fs::create_dir_all(&broken).expect("create");
        fs::write(broken.join(".git"), b"gitdir: /nonexistent\n").expect("write gitfile");
        assert!(Repository::discover(&broken).is_err());
    }
}
