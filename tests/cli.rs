use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
use tempfile::TempDir;

#[path = "cases/python.rs"]
mod python;

#[path = "cases/delta.rs"]
mod delta;

#[path = "cases/repo_path.rs"]
mod repo_path;

#[path = "cases/output.rs"]
mod output;

#[path = "cases/typescript.rs"]
mod typescript;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .env("GIT_AUTHOR_DATE", "2025-01-01T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2025-01-01T12:00:00Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn repository(files: &[(&str, &str)]) -> TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q", "--initial-branch=main"]);
    for (path, source) in files {
        let path = repo.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "fixture"]);
    repo
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_erosion"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap()
}

fn successful_json(root: &Path, args: &[&str]) -> Value {
    let output = run(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn adapters_metric_boundary_and_committed_source_only() {
    let ten = format!(
        "function f(x: boolean) {{\n{}}}\n",
        "if(x) work();\n".repeat(9)
    );
    let eleven = format!(
        "function g(x: boolean) {{\n{}}}\n",
        "if(x) work();\n".repeat(10)
    );
    let repo = repository(&[
        ("a.ts", &ten),
        ("b.ts", &eleven),
        ("notes.rs", "// unsupported"),
    ]);
    let before = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("a.ts"), "function { dirty source").unwrap();
    fs::write(repo.path().join("untracked.ts"), "function {").unwrap();
    let status = git(repo.path(), &["status", "--porcelain=v1", "-uall"]);
    let report = successful_json(
        repo.path(),
        &["measure", "--no-cache", "--format", "json", "--top", "2"],
    );
    let snapshot = &report["snapshots"][0];
    let expected =
        100.0 * 11.0 * 11.0_f64.sqrt() / (10.0 * 10.0_f64.sqrt() + 11.0 * 11.0_f64.sqrt());
    assert!((snapshot["erosion_pct"].as_f64().unwrap() - expected).abs() < 1e-12);
    assert_eq!(snapshot["functions"], 2);
    assert_eq!(snapshot["complex_functions"], 1);
    assert_eq!(snapshot["top_functions"][0]["path"], "b.ts");
    assert_eq!(snapshot["coverage"]["unsupported_files"], 1);
    assert_eq!(before, git(repo.path(), &["rev-parse", "HEAD"]));
    assert_eq!(
        status,
        git(repo.path(), &["status", "--porcelain=v1", "-uall"])
    );
}

#[test]
fn cold_warm_and_disabled_cache_are_identical() {
    let repo = repository(&[
        ("a.jsx", "const View = () => <div>Hello</div>;"),
        (
            "b.tsx",
            "const View = (x: boolean) => <div>{x ? 'a' : 'b'}</div>;",
        ),
        ("c.mts", "export const f = () => 1;"),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let args = [
        "measure",
        "--verbose",
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--format",
        "json",
    ];
    let first = run(repo.path(), &args);
    let warm = run(repo.path(), &args);
    let disabled = run(repo.path(), &["measure", "--no-cache", "--format", "json"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(warm.status.success());
    assert!(disabled.status.success());
    assert_eq!(first.stdout, warm.stdout);
    assert_eq!(first.stdout, disabled.stdout);
    assert!(
        String::from_utf8(warm.stderr)
            .unwrap()
            .contains("3 disk hits")
    );
}

#[test]
fn strict_and_partial_failure_with_parse_failure_cache() {
    let repo = repository(&[
        ("good.js", "function good() { return 1; }"),
        ("bad.ts", "function {"),
        ("also-bad.tsx", "const X = () => <div>"),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let args = [
        "measure",
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--format",
        "json",
    ];
    let failure = run(repo.path(), &args);
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
    let stderr = String::from_utf8(failure.stderr).unwrap();
    assert!(stderr.contains("bad.ts") && stderr.contains("also-bad.tsx"));
    let mut partial_args = args.to_vec();
    partial_args.push("--allow-partial");
    let partial = successful_json(repo.path(), &partial_args);
    assert_eq!(partial["snapshots"][0]["status"], "partial");
    assert_eq!(partial["snapshots"][0]["coverage"]["failed_files"], 2);
    assert_eq!(
        partial["snapshots"][0]["failures"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(partial["snapshots"][0]["erosion_pct"], 0.0);
}

#[test]
fn config_is_root_based_frozen_and_explicit() {
    let repo = repository(&[
        ("a.ts", "function a() {}"),
        ("nested/test.ts", "function {"),
    ]);
    fs::write(
        repo.path().join("erosion.toml"),
        "exclude = ['**/test.ts']\n",
    )
    .unwrap();
    let report = successful_json(
        &repo.path().join("nested"),
        &[
            "history",
            "--since",
            "1d",
            "--every",
            "1d",
            "--no-cache",
            "--format",
            "json",
        ],
    );
    assert_eq!(report["scope"]["exclude"][0], "**/test.ts");
    assert_eq!(report["snapshots"][1]["coverage"]["excluded_entries"], 1);
    fs::write(repo.path().join("erosion.toml"), "unknown = true\n").unwrap();
    let failure = run(repo.path(), &["measure", "--no-cache"]);
    assert!(!failure.status.success());
    assert!(failure.stdout.is_empty());
}

#[test]
fn formats_pre_history_and_zero_mass() {
    let repo = repository(&[("a.ts", "export interface Shape { x: number; }\n")]);
    let args = [
        "history",
        "--since",
        "3y",
        "--every",
        "6mo",
        "--no-cache",
        "--format",
        "json",
    ];
    let report = successful_json(repo.path(), &args);
    let rows = report["snapshots"].as_array().unwrap();
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[0]["status"], "no_commit");
    assert!(rows[0]["erosion_pct"].is_null());
    assert_eq!(rows[6]["status"], "not_measurable");
    assert!(rows[6]["erosion_pct"].is_null());
    let csv = run(repo.path(), &["measure", "--no-cache", "--format", "csv"]);
    assert!(csv.status.success());
    let mut reader = csv::Reader::from_reader(csv.stdout.as_slice());
    let headers = reader.headers().unwrap().clone();
    let score = headers
        .iter()
        .position(|column| column == "erosion_pct")
        .unwrap();
    let records: Vec<_> = reader.records().map(Result::unwrap).collect();
    assert_eq!(records.len(), 1);
    assert_eq!(&records[0][score], "");
    let table = run(repo.path(), &["measure", "--no-cache"]);
    assert!(table.status.success());
    assert!(
        String::from_utf8(table.stdout)
            .unwrap()
            .contains("not measurable")
    );
    let failure = run(
        repo.path(),
        &["measure", "--no-cache", "--format", "csv", "--top", "1"],
    );
    assert_eq!(failure.status.code(), Some(1));
    assert!(failure.stdout.is_empty());
}

#[test]
fn runtime_and_argument_errors_do_not_emit_results() {
    let repo = repository(&[("a.ts", "function f() {}")]);
    for args in [
        vec!["measure", "--ref", "not-a-ref", "--no-cache"],
        vec!["measure", "--ref=--help", "--no-cache"],
        vec!["measure", "--cache-dir", repo.path().to_str().unwrap()],
        vec!["history", "--since", "3y", "--every", "0d", "--no-cache"],
        vec!["history", "--since", "3y", "--every", "1m", "--no-cache"],
        vec![
            "history",
            "--since",
            "3y",
            "--compare-common-files",
            "--no-cache",
        ],
    ] {
        let result = run(repo.path(), &args);
        assert!(!result.status.success(), "{args:?}");
        assert!(result.stdout.is_empty(), "{args:?}");
    }
    let empty = tempfile::tempdir().unwrap();
    let invalid_cadence = run(
        repo.path(),
        &["history", "--since", "3y", "--every", "1m", "--no-cache"],
    );
    assert_eq!(invalid_cadence.status.code(), Some(1));
    let conflicting_flags = run(repo.path(), &["measure", "--cache-dir", ".", "--no-cache"]);
    assert_eq!(conflicting_flags.status.code(), Some(2));
    assert!(conflicting_flags.stdout.is_empty());
    assert!(
        !run(empty.path(), &["measure", "--no-cache"])
            .status
            .success()
    );
    git(empty.path(), &["init", "-q"]);
    assert!(
        !run(empty.path(), &["measure", "--no-cache"])
            .status
            .success()
    );
}

#[test]
fn history_does_not_sample_a_merged_side_branch() {
    let repo = repository(&[("a.js", "function f() {}")]);
    let root = git(repo.path(), &["rev-parse", "HEAD"]);
    let tree = git(repo.path(), &["rev-parse", "HEAD^{tree}"]);
    let commit = |parents: &[&str], date: &str, message: &str| {
        let mut command = Command::new("git");
        command.arg("-C").arg(repo.path()).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            &tree,
            "-m",
            message,
        ]);
        for parent in parents {
            command.args(["-p", parent]);
        }
        let output = command
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    let main = commit(&[&root], "2025-02-01T12:00:00Z", "main");
    let side = commit(&[&root], "2025-02-15T12:00:00Z", "side");
    let merge = commit(&[&main, &side], "2025-03-01T12:00:00Z", "merge");
    git(repo.path(), &["update-ref", "refs/heads/main", &merge]);
    let report = successful_json(
        repo.path(),
        &[
            "history",
            "--since",
            "2025-02-15",
            "--until",
            "2025-03-01",
            "--every",
            "1w",
            "--no-cache",
            "--format",
            "json",
        ],
    );
    let snapshots = report["snapshots"].as_array().unwrap();
    assert_eq!(snapshots[0]["commit"]["sha"], main);
    assert_eq!(snapshots.last().unwrap()["commit"]["sha"], merge);
    assert!(
        snapshots
            .iter()
            .all(|snapshot| snapshot["commit"]["sha"] != side)
    );
}
