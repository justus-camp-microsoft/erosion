use super::{git, run, successful_json};
use serde_json::Value;
use std::fs;

#[test]
fn python_routes_history_cache_top_and_mixed_mass() {
    let repo = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    fs::write(
        repo.path().join("below.py"),
        format!(
            "def below(x):\n    \"\"\"not source size\"\"\"\n{}",
            "    if x: work()\n".repeat(9)
        ),
    )
    .unwrap();
    fs::write(
        repo.path().join("above.py"),
        format!("def above(x):\n{}", "    if x: work()\n".repeat(10)),
    )
    .unwrap();
    fs::write(repo.path().join("view.js"), "const view = () => 1;\n").unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "mixed source"]);
    fs::write(repo.path().join("above.py"), "def broken(:\n").unwrap();

    let args = [
        "measure",
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--format",
        "json",
        "--top",
        "5",
    ];
    let cold = run(repo.path(), &args);
    let warm = run(repo.path(), &args);
    assert!(
        cold.status.success(),
        "{}",
        String::from_utf8_lossy(&cold.stderr)
    );
    assert!(warm.status.success());
    assert_eq!(cold.stdout, warm.stdout);
    assert_eq!(
        cold.stdout,
        run(
            repo.path(),
            &["measure", "--no-cache", "--format", "json", "--top", "5"]
        )
        .stdout
    );
    let report: Value = serde_json::from_slice(&cold.stdout).unwrap();
    assert_eq!(report["metric"]["version"], "erosion-v2");
    let snapshot = &report["snapshots"][0];
    let complex_mass = 11.0 * 11.0_f64.sqrt();
    let total = complex_mass + 10.0 * 10.0_f64.sqrt() + 1.0;
    assert!(
        (snapshot["erosion_pct"].as_f64().unwrap() - 100.0 * complex_mass / total).abs() < 1e-12
    );
    assert_eq!(
        snapshot["coverage"]["languages"]["python"]["parsed_files"],
        2
    );
    assert_eq!(
        snapshot["coverage"]["languages"]["javascript"]["parsed_files"],
        1
    );
    assert_eq!(snapshot["top_functions"][0]["language"], "python");
    assert_eq!(snapshot["top_functions"][0]["name"], "above");
    assert_eq!(snapshot["top_functions"][0]["source_lines"], 11);
    assert_eq!(snapshot["functions"], 3);

    let scope = repo.path().join("python.toml");
    fs::write(&scope, "include = ['**/*.py']\n").unwrap();
    let python = successful_json(
        repo.path(),
        &[
            "history",
            "--since",
            "1d",
            "--every",
            "1d",
            "--format",
            "json",
            "--cache-dir",
            cache.path().to_str().unwrap(),
            "--config",
            scope.to_str().unwrap(),
        ],
    );
    assert_eq!(python["snapshots"][0]["status"], "no_commit");
    let head = &python["snapshots"][1];
    assert_eq!(head["functions"], 2);
    assert_eq!(head["coverage"]["excluded_entries"], 1);
    assert!((head["total_mass"].as_f64().unwrap() - (total - 1.0)).abs() < 1e-12);
}

#[test]
fn python_failures_remain_visible_with_partial_output_and_stubs() {
    let repo = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    for (path, source) in [
        ("bad.py", "def broken():\n"),
        ("stub.pyi", "def f() -> None: ...\n"),
        ("script.pyw", "def g():\n    return 1\n"),
    ] {
        fs::write(repo.path().join(path), source).unwrap();
    }
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "python failure"]);
    let strict_args = [
        "measure",
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--format",
        "json",
    ];
    let strict = run(repo.path(), &strict_args);
    assert!(!strict.status.success());
    assert!(strict.stdout.is_empty());
    assert!(String::from_utf8(strict.stderr).unwrap().contains("bad.py"));
    let warm = run(repo.path(), &strict_args);
    assert!(!warm.status.success());
    assert!(warm.stdout.is_empty());
    assert!(String::from_utf8(warm.stderr).unwrap().contains("bad.py"));
    let partial = successful_json(
        repo.path(),
        &[
            "measure",
            "--no-cache",
            "--format",
            "json",
            "--allow-partial",
        ],
    );
    let snapshot = &partial["snapshots"][0];
    assert_eq!(
        partial,
        successful_json(
            repo.path(),
            &[
                "measure",
                "--cache-dir",
                cache.path().to_str().unwrap(),
                "--format",
                "json",
                "--allow-partial",
            ]
        )
    );
    assert_eq!(snapshot["status"], "partial");
    assert_eq!(snapshot["functions"], 2);
    assert_eq!(
        snapshot["coverage"]["languages"]["python"]["selected_files"],
        3
    );
    assert_eq!(
        snapshot["coverage"]["languages"]["python"]["failed_files"],
        1
    );
    assert_eq!(snapshot["coverage"]["failed_physical_lines"], 1);
    assert_eq!(snapshot["failures"][0]["language"], "python");
    let csv = run(
        repo.path(),
        &[
            "measure",
            "--no-cache",
            "--format",
            "csv",
            "--allow-partial",
        ],
    );
    assert!(csv.status.success());
    let mut reader = csv::Reader::from_reader(csv.stdout.as_slice());
    let index = reader
        .headers()
        .unwrap()
        .iter()
        .position(|field| field == "failures_json")
        .unwrap();
    let row = reader.records().next().unwrap().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&row[index]).unwrap()[0]["language"],
        "python"
    );
}
