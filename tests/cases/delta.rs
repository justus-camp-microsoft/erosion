use std::fs;

use serde_json::Value;

use super::{git, repository, run, successful_json};

fn assert_close(actual: &Value, expected: f64) {
    assert!(
        (actual.as_f64().unwrap() - expected).abs() < 1e-12,
        "{actual} != {expected}"
    );
}

#[test]
fn directional_whole_snapshot_delta_and_read_only_cache_reuse() {
    let repo = repository(&[
        ("changed.js", "function f() { return 0; }\n"),
        ("unchanged.py", "def keep(): return 1\n"),
        ("removed.js", "const removed = () => 0;\n"),
    ]);
    let from = git(repo.path(), &["rev-parse", "HEAD"]);
    let complex = format!("function f(x) {{\n{}}}\n", "if (x) work();\n".repeat(10));
    fs::write(repo.path().join("changed.js"), complex).unwrap();
    fs::remove_file(repo.path().join("removed.js")).unwrap();
    fs::write(repo.path().join("added.py"), "def added(): return 2\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "more complexity"]);
    let to = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("changed.js"), "function { dirty").unwrap();
    fs::write(repo.path().join("untracked.py"), "def broken(:\n").unwrap();
    let status = git(repo.path(), &["status", "--porcelain=v1", "-uall"]);
    let index = fs::read(repo.path().join(".git/index")).unwrap();
    let refs = git(repo.path(), &["show-ref"]);
    let config = fs::read(repo.path().join(".git/config")).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let args = [
        "delta",
        "--verbose",
        "HEAD~1",
        "HEAD",
        "--format",
        "json",
        "--cache-dir",
        cache.path().to_str().unwrap(),
    ];
    let cold = run(repo.path(), &args);
    assert!(
        cold.status.success(),
        "{}",
        String::from_utf8_lossy(&cold.stderr)
    );
    assert!(String::from_utf8_lossy(&cold.stderr).contains("1 in-process hits"));
    let warm = run(repo.path(), &args);
    assert!(warm.status.success());
    assert!(String::from_utf8_lossy(&warm.stderr).contains("5 disk hits"));
    let uncached = run(
        repo.path(),
        &["delta", &from, &to, "--format", "json", "--no-cache"],
    );
    assert!(uncached.status.success());
    assert_eq!(cold.stdout, warm.stdout);
    assert_eq!(cold.stdout, uncached.stdout);
    let report: Value = serde_json::from_slice(&cold.stdout).unwrap();
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["reference"]["sha"], to);
    let rows = report["snapshots"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["commit"]["sha"], from);
    assert_eq!(rows[1]["commit"]["sha"], to);
    assert_eq!(rows[0]["cutoff_utc"], rows[1]["cutoff_utc"]);
    assert!(rows[0]["change_pp"].is_null());
    let complex_mass = 11.0 * 11.0_f64.sqrt();
    let expected = 100.0 * complex_mass / (complex_mass + 2.0);
    assert_close(&rows[0]["erosion_pct"], 0.0);
    assert_close(&rows[1]["erosion_pct"], expected);
    assert_close(&rows[1]["change_pp"], expected);
    for (row, revision) in rows.iter().zip([&from, &to]) {
        let measure = successful_json(
            repo.path(),
            &[
                "measure",
                "--ref",
                revision,
                "--format",
                "json",
                "--no-cache",
            ],
        );
        for field in [
            "commit",
            "coverage",
            "functions",
            "total_mass",
            "complex_mass",
            "erosion_pct",
            "status",
        ] {
            assert_eq!(row[field], measure["snapshots"][0][field], "{field}");
        }
    }
    let reversed = successful_json(
        repo.path(),
        &["delta", &to, &from, "--format", "json", "--no-cache"],
    );
    assert_close(&reversed["snapshots"][1]["change_pp"], -expected);
    let same = successful_json(
        repo.path(),
        &["delta", "HEAD", "HEAD", "--format", "json", "--no-cache"],
    );
    assert_close(&same["snapshots"][1]["change_pp"], 0.0);
    assert_eq!(
        status,
        git(repo.path(), &["status", "--porcelain=v1", "-uall"])
    );
    assert_eq!(to, git(repo.path(), &["rev-parse", "HEAD"]));
    assert_eq!(refs, git(repo.path(), &["show-ref"]));
    assert_eq!(index, fs::read(repo.path().join(".git/index")).unwrap());
    assert_eq!(config, fs::read(repo.path().join(".git/config")).unwrap());
    assert_eq!(
        fs::read_to_string(repo.path().join("changed.js")).unwrap(),
        "function { dirty"
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("untracked.py")).unwrap(),
        "def broken(:\n"
    );
}

#[test]
fn table_csv_and_unmeasurable_delta() {
    let repo = repository(&[("a.ts", "interface Shape { x: number }\n")]);
    fs::write(
        repo.path().join("a.ts"),
        format!(
            "function f(x: boolean) {{\n{}}}\n",
            "if (x) work();\n".repeat(10)
        ),
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "add callable"]);
    for (from, to) in [("HEAD~1", "HEAD"), ("HEAD", "HEAD~1"), ("HEAD~1", "HEAD~1")] {
        let report = successful_json(
            repo.path(),
            &["delta", from, to, "--format", "json", "--no-cache"],
        );
        assert!(report["snapshots"][1]["change_pp"].is_null());
        let table = run(repo.path(), &["delta", from, to, "--no-cache"]);
        assert!(table.status.success());
        let text = String::from_utf8(table.stdout).unwrap();
        assert!(text.contains("FROM ") && text.contains("TO "));
        assert!(text.contains("Erosion delta (TO - FROM): not measurable"));
        let csv = run(
            repo.path(),
            &["delta", from, to, "--format", "csv", "--no-cache"],
        );
        assert!(csv.status.success());
        let mut reader = csv::Reader::from_reader(csv.stdout.as_slice());
        let headers = reader.headers().unwrap().clone();
        let change = headers
            .iter()
            .position(|column| column == "change_pp")
            .unwrap();
        let commit = headers
            .iter()
            .position(|column| column == "commit")
            .unwrap();
        let rows: Vec<_> = reader.records().map(Result::unwrap).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0][change].is_empty() && rows[1][change].is_empty());
        assert_eq!(
            &rows[0][commit],
            report["snapshots"][0]["commit"]["sha"].as_str().unwrap()
        );
        assert_eq!(
            &rows[1][commit],
            report["snapshots"][1]["commit"]["sha"].as_str().unwrap()
        );
    }
    let table = run(repo.path(), &["delta", "HEAD", "HEAD", "--no-cache"]);
    assert!(table.status.success());
    assert!(
        String::from_utf8(table.stdout)
            .unwrap()
            .contains("Erosion delta (TO - FROM): +0.00 percentage points")
    );
}

#[test]
fn parse_failures_identify_both_commits_and_partial_scores() {
    let repo = repository(&[
        ("bad.py", "def broken():\n"),
        ("ok.py", "def f(): return 1\n"),
    ]);
    let from = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("bad.py"), "def broken(:\n").unwrap();
    fs::write(
        repo.path().join("ok.py"),
        format!("def f(x):\n{}", "    if x: work()\n".repeat(10)),
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "another parse failure"]);
    let to = git(repo.path(), &["rev-parse", "HEAD"]);
    let cache = tempfile::tempdir().unwrap();
    let args = [
        "delta",
        &from,
        &to,
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--format",
        "json",
    ];
    for _ in 0..2 {
        let strict = run(repo.path(), &args);
        assert_eq!(strict.status.code(), Some(1));
        assert!(strict.stdout.is_empty());
        let stderr = String::from_utf8(strict.stderr).unwrap();
        for sha in [&from, &to] {
            assert!(stderr.contains(&format!("({sha}): \"bad.py\"")));
        }
    }
    let mut partial_args = args.to_vec();
    partial_args.push("--allow-partial");
    let partial = successful_json(repo.path(), &partial_args);
    for snapshot in partial["snapshots"].as_array().unwrap() {
        assert_eq!(snapshot["status"], "partial");
        assert_eq!(snapshot["coverage"]["failed_files"], 1);
        assert_eq!(snapshot["failures"][0]["path"], "bad.py");
    }
    assert_close(&partial["snapshots"][1]["change_pp"], 100.0);
    let table = run(
        repo.path(),
        &["delta", &from, &to, "--no-cache", "--allow-partial"],
    );
    assert!(table.status.success());
    let text = String::from_utf8(table.stdout).unwrap();
    assert!(text.contains("Erosion delta (TO - FROM): +100.00 percentage points"));
    assert!(text.contains("Partial comparison: failed files are excluded."));
    let csv = run(
        repo.path(),
        &[
            "delta",
            &to,
            &from,
            "--format",
            "csv",
            "--no-cache",
            "--allow-partial",
        ],
    );
    assert!(csv.status.success());
    let mut reader = csv::Reader::from_reader(csv.stdout.as_slice());
    let headers = reader.headers().unwrap().clone();
    let change = headers
        .iter()
        .position(|column| column == "change_pp")
        .unwrap();
    let status = headers
        .iter()
        .position(|column| column == "status")
        .unwrap();
    let rows: Vec<_> = reader.records().map(Result::unwrap).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(&rows[1][change], "-100");
    assert_eq!(&rows[1][status], "partial");
}

#[test]
fn frozen_scope_from_subdirectory_and_explicit_config() {
    let repo = repository(&[
        ("source/a.py", "def f(): return 1\n"),
        ("bad.js", "function {"),
        ("erosion.toml", "include = []\n"),
    ]);
    fs::write(
        repo.path().join("source/a.py"),
        format!("def f(x):\n{}", "    if x: work()\n".repeat(10)),
    )
    .unwrap();
    fs::write(repo.path().join("erosion.toml"), "include = ['**/*.js']\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(
        repo.path(),
        &["commit", "-qm", "different historical config"],
    );
    fs::write(repo.path().join("erosion.toml"), "include = ['**/*.py']\n").unwrap();
    let root = successful_json(
        repo.path(),
        &["delta", "HEAD~1", "HEAD", "--format", "json", "--no-cache"],
    );
    let nested = successful_json(
        &repo.path().join("source"),
        &["delta", "HEAD~1", "HEAD", "--format", "json", "--no-cache"],
    );
    assert_eq!(root, nested);
    assert_eq!(root["scope"]["include"][0], "**/*.py");
    assert_close(&root["snapshots"][1]["change_pp"], 100.0);
    for snapshot in root["snapshots"].as_array().unwrap() {
        assert_eq!(snapshot["coverage"]["selected_files"], 1);
        assert_eq!(snapshot["coverage"]["excluded_entries"], 2);
        assert_eq!(snapshot["status"], "complete");
    }
    fs::write(repo.path().join("source/empty.toml"), "include = []\n").unwrap();
    let empty = successful_json(
        &repo.path().join("source"),
        &[
            "delta",
            "HEAD~1",
            "HEAD",
            "--config",
            "empty.toml",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(empty["scope"]["include"], serde_json::json!([]));
    assert!(empty["snapshots"][1]["change_pp"].is_null());
}

#[test]
fn unrelated_revisions_do_not_traverse_or_reorder_history() {
    let repo = repository(&[("a.py", "def f(): return 1\n")]);
    let original = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(
        repo.path().join("a.py"),
        format!("def f(x):\n{}", "    if x: work()\n".repeat(10)),
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    let tree = git(repo.path(), &["write-tree"]);
    let unrelated = git(repo.path(), &["commit-tree", &tree, "-m", "unrelated root"]);
    let report = successful_json(
        repo.path(),
        &[
            "delta",
            &unrelated,
            &original,
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(report["snapshots"][0]["commit"]["sha"], unrelated);
    assert_close(&report["snapshots"][1]["change_pp"], -100.0);
    // A shallow boundary must not prohibit comparing locally available snapshots.
    fs::write(repo.path().join(".git/shallow"), format!("{original}\n")).unwrap();
    let shallow = successful_json(
        repo.path(),
        &[
            "delta",
            &unrelated,
            &original,
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(report, shallow);
}

#[test]
fn all_commits_reports_each_adjacent_first_parent_delta_in_all_formats() {
    let repo = repository(&[("a.py", "def f(): return 1\n")]);
    let from = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(
        repo.path().join("a.py"),
        format!("def f(x):\n{}", "    if x: work()\n".repeat(10)),
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "increase erosion"]);
    let middle = git(repo.path(), &["rev-parse", "HEAD"]);
    fs::write(repo.path().join("a.py"), "def f(): return 1\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "decrease erosion"]);
    let to = git(repo.path(), &["rev-parse", "HEAD"]);

    let report = successful_json(
        repo.path(),
        &[
            "delta",
            &from,
            &to,
            "--all-commits",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(report["mode"], "delta");
    assert_eq!(report["reference"]["sha"], to);
    let rows = report["snapshots"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["commit"]["sha"], from);
    assert_eq!(rows[1]["commit"]["sha"], middle);
    assert_eq!(rows[2]["commit"]["sha"], to);
    assert!(rows[0]["change_pp"].is_null());
    assert_close(&rows[1]["change_pp"], 100.0);
    assert_close(&rows[2]["change_pp"], -100.0);

    let table = run(
        repo.path(),
        &["delta", &from, &to, "--all-commits", "--no-cache"],
    );
    assert!(table.status.success());
    let text = String::from_utf8(table.stdout).unwrap();
    assert!(text.starts_with("Erosion delta per commit\n"));
    assert!(text.contains("FROM") && text.contains("TO"));
    assert!(text.contains("+100.00pp") && text.contains("-100.00pp"));
    for sha in [&from, &middle, &to] {
        assert!(text.contains(&sha[..10]));
    }

    let csv = run(
        repo.path(),
        &[
            "delta",
            &from,
            &to,
            "--all-commits",
            "--format",
            "csv",
            "--no-cache",
        ],
    );
    assert!(csv.status.success());
    let mut reader = csv::Reader::from_reader(csv.stdout.as_slice());
    let headers = reader.headers().unwrap().clone();
    let commit = headers
        .iter()
        .position(|column| column == "commit")
        .unwrap();
    let change = headers
        .iter()
        .position(|column| column == "change_pp")
        .unwrap();
    let rows: Vec<_> = reader.records().map(Result::unwrap).collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(&rows[0][commit], from);
    assert_eq!(&rows[1][commit], middle);
    assert_eq!(&rows[2][commit], to);
    assert!(rows[0][change].is_empty());
    assert_eq!(&rows[1][change], "100");
    assert_eq!(&rows[2][change], "-100");
}

#[test]
fn all_commits_excludes_side_branches_and_requires_first_parent_ancestry() {
    let repo = repository(&[("a.py", "def f(): return 1\n")]);
    let root = git(repo.path(), &["rev-parse", "HEAD"]);
    let tree = git(repo.path(), &["rev-parse", "HEAD^{tree}"]);
    let main = git(
        repo.path(),
        &["commit-tree", &tree, "-p", &root, "-m", "main"],
    );
    let side = git(
        repo.path(),
        &["commit-tree", &tree, "-p", &root, "-m", "side"],
    );
    let merge = git(
        repo.path(),
        &[
            "commit-tree",
            &tree,
            "-p",
            &main,
            "-p",
            &side,
            "-m",
            "merge",
        ],
    );
    git(repo.path(), &["update-ref", "refs/heads/main", &merge]);

    let report = successful_json(
        repo.path(),
        &[
            "delta",
            &root,
            &merge,
            "--all-commits",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    let commits: Vec<_> = report["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|snapshot| snapshot["commit"]["sha"].as_str().unwrap())
        .collect();
    assert_eq!(commits, [&root, &main, &merge]);
    assert!(!commits.contains(&side.as_str()));

    let outside = tempfile::tempdir().unwrap();
    let cache = outside.path().join("must-not-exist");
    let failure = run(
        repo.path(),
        &[
            "delta",
            &side,
            &merge,
            "--all-commits",
            "--cache-dir",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(failure.status.code(), Some(1));
    assert!(failure.stdout.is_empty());
    assert!(
        String::from_utf8(failure.stderr)
            .unwrap()
            .contains("is not on the first-parent chain")
    );
    assert!(!cache.exists());
}

#[test]
fn invalid_revisions_and_arguments_emit_no_results_or_cache() {
    let repo = repository(&[("a.py", "def f(): return 1\n")]);
    let outside = tempfile::tempdir().unwrap();
    let cache = outside.path().join("must-not-exist");
    let blob = git(repo.path(), &["rev-parse", "HEAD:a.py"]);
    for (from, to, role) in [
        ("missing", "HEAD", "FROM"),
        ("HEAD", "missing", "TO"),
        ("", "HEAD", "FROM"),
        ("HEAD", blob.as_str(), "TO"),
    ] {
        let result = run(
            repo.path(),
            &["delta", from, to, "--cache-dir", cache.to_str().unwrap()],
        );
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8(result.stderr)
                .unwrap()
                .contains(&format!("Resolving {role} revision"))
        );
        assert!(!cache.exists());
    }
    for args in [
        vec!["delta"],
        vec!["delta", "HEAD"],
        vec!["delta", "HEAD", "HEAD", "HEAD"],
        vec!["delta", "HEAD", "HEAD", "--ref", "HEAD"],
        vec!["delta", "HEAD", "HEAD", "--top", "1"],
        vec!["delta", "HEAD", "HEAD", "--since", "1y"],
        vec!["delta", "HEAD", "HEAD", "--no-cache", "--cache-dir", "."],
    ] {
        let result = run(repo.path(), &args);
        assert_eq!(result.status.code(), Some(2), "{args:?}");
        assert!(result.stdout.is_empty());
    }
    let option_like = run(
        repo.path(),
        &["delta", "--no-cache", "--", "--help", "HEAD"],
    );
    assert_eq!(option_like.status.code(), Some(1));
    assert!(option_like.stdout.is_empty());
    let in_repo_cache = run(
        repo.path(),
        &["delta", "HEAD", "HEAD", "--cache-dir", "local-cache"],
    );
    assert_eq!(in_repo_cache.status.code(), Some(1));
    assert!(in_repo_cache.stdout.is_empty());
    assert!(!repo.path().join("local-cache").exists());
}
