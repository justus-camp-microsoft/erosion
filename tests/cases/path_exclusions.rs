use std::fs;

use serde_json::Value;

use super::{commit_at, git, repository, run, successful_json};

fn commands() -> [Vec<&'static str>; 3] {
    [
        vec!["measure"],
        vec!["history", "--since", "1d", "--every", "1d"],
        vec!["delta", "HEAD~1", "HEAD"],
    ]
}

#[test]
fn repeated_globs_extend_config_consistently_across_commands_and_checkpoints() {
    let repo = repository(&[
        ("pkg/keep.js", "function kept() {}"),
        ("pkg/nested/keep.MIN.js", "function also_kept() {}"),
        ("pkg/nested/bundle.min.js", "function {"),
        ("root.min.js", "function {"),
        ("pkg/generated/out.js", "function {"),
        ("pkg/snapshots/state.snap", "fixture"),
        ("pkg/bundle.map", "fixture"),
        ("pkg/tests/keep.test.js", "function {"),
        ("other/out.js", "function {"),
    ]);
    fs::write(repo.path().join("pkg/nested/later.min.js"), "function {").unwrap();
    commit_at(repo.path(), "2025-01-02T12:00:00Z");
    fs::write(
        repo.path().join("erosion.toml"),
        "include = ['pkg/**']\nexclude = ['**/generated/**']\n",
    )
    .unwrap();
    let config = tempfile::tempdir().unwrap();
    let equivalent = config.path().join("equivalent.toml");
    fs::write(
        &equivalent,
        "include = ['pkg/**']\nexclude = ['**/generated/**', '*.min.js', '**/*.{map,snap}']\n",
    )
    .unwrap();
    for command in commands() {
        let mut baseline = command.clone();
        baseline.extend(["--exclude-tests", "--no-cache"]);
        let failure = run(repo.path(), &baseline);
        assert_eq!(failure.status.code(), Some(1));
        assert!(failure.stdout.is_empty());

        let mut args = command.clone();
        args.extend([
            "--exclude-tests",
            "--exclude-paths",
            "*.min.js",
            "--exclude-paths",
            "**/*.{map,snap}",
            "--no-cache",
            "--format",
            "json",
        ]);
        let report = successful_json(&repo.path().join("pkg/nested"), &args);
        assert_eq!(report["scope"]["include"], serde_json::json!(["pkg/**"]));
        assert_eq!(
            report["scope"]["exclude"],
            serde_json::json!(["**/generated/**", "*.min.js", "**/*.{map,snap}"])
        );
        for point in report["snapshots"].as_array().unwrap() {
            assert_eq!(point["status"], "complete");
            assert_eq!(point["functions"], 2);
            assert_eq!(point["coverage"]["parsed_files"], 2);
            assert_eq!(point["coverage"]["failed_files"], 0);
            assert_eq!(point["coverage"]["unsupported_files"], 0);
            assert_eq!(point["coverage"]["test_excluded_entries"], 1);
            assert_eq!(
                point["coverage"]["excluded_entries"].as_u64().unwrap(),
                point["coverage"]["tracked_entries"].as_u64().unwrap() - 2
            );
        }
        let mut equivalent_args = command;
        equivalent_args.extend([
            "--exclude-tests",
            "--config",
            equivalent.to_str().unwrap(),
            "--no-cache",
            "--format",
            "json",
        ]);
        assert_eq!(report, successful_json(repo.path(), &equivalent_args));
        for format in ["table", "csv"] {
            *args.last_mut().unwrap() = format;
            *equivalent_args.last_mut().unwrap() = format;
            let cli = run(repo.path(), &args);
            let configured = run(repo.path(), &equivalent_args);
            assert!(cli.status.success());
            assert!(configured.status.success());
            assert_eq!(cli.stdout, configured.stdout);
            if format == "table" {
                assert!(!String::from_utf8_lossy(&cli.stdout).contains("Scope:"));
            }
        }
    }
}

#[test]
fn cli_only_exclusions_skip_root_and_nested_files_and_reuse_blob_cache() {
    let repo = repository(&[
        ("kept.js", "function kept() {}"),
        ("root.min.js", "function {"),
        ("nested/bundle.min.js", "function {"),
    ]);
    commit_at(repo.path(), "2025-01-02T12:00:00Z");
    let cache = tempfile::tempdir().unwrap();
    let baseline = successful_json(
        repo.path(),
        &[
            "measure",
            "--allow-partial",
            "--format",
            "json",
            "--cache-dir",
            cache.path().to_str().unwrap(),
        ],
    );
    let fingerprint = baseline["metric"]["analyzer_fingerprint"].as_str().unwrap();
    let excluded_oid = git(repo.path(), &["rev-parse", "HEAD:root.min.js"]);
    let excluded_record = cache
        .path()
        .join(fingerprint)
        .join("raw")
        .join("javascript")
        .join(&excluded_oid[..2])
        .join(format!("{excluded_oid}.json"));
    fs::write(&excluded_record, "invalid excluded cache").unwrap();
    for mut command in commands() {
        command.extend([
            "--exclude-paths",
            "*.min.js",
            "--cache-dir",
            cache.path().to_str().unwrap(),
            "--format",
            "json",
            "--verbose",
        ]);
        let first = run(repo.path(), &command);
        assert!(
            first.status.success(),
            "{}",
            String::from_utf8_lossy(&first.stderr)
        );
        assert!(String::from_utf8_lossy(&first.stderr).contains("0 parsed"));
        let warm = run(repo.path(), &command);
        assert!(warm.status.success());
        assert_eq!(first.stdout, warm.stdout);
        let report: Value = serde_json::from_slice(&first.stdout).unwrap();
        assert_eq!(report["metric"], baseline["metric"]);
        assert_ne!(report["scope_fingerprint"], baseline["scope_fingerprint"]);
        for point in report["snapshots"].as_array().unwrap() {
            assert_eq!(point["status"], "complete");
            assert_eq!(point["functions"], 1);
            assert_eq!(point["coverage"]["excluded_entries"], 2);
            assert!(point["coverage"].get("test_excluded_entries").is_none());
        }
    }
    assert_eq!(
        fs::read(excluded_record).unwrap(),
        b"invalid excluded cache"
    );
    let no_matches = successful_json(
        repo.path(),
        &[
            "measure",
            "--exclude-paths",
            "not-present/**",
            "--allow-partial",
            "--no-cache",
            "--format",
            "json",
        ],
    );
    assert_eq!(no_matches["snapshots"], baseline["snapshots"]);
}

#[test]
fn invalid_exclusion_arguments_are_explicit_and_modules_does_not_accept_them() {
    let repo = repository(&[("pkg/kept.js", "function kept() {}")]);
    commit_at(repo.path(), "2025-01-02T12:00:00Z");
    let outside = tempfile::tempdir().unwrap();
    let cache = outside.path().join("unused-cache");
    for mut command in commands() {
        command.extend([
            "--exclude-paths",
            "[",
            "--allow-partial",
            "--cache-dir",
            cache.to_str().unwrap(),
        ]);
        let failure = run(repo.path(), &command);
        assert_eq!(failure.status.code(), Some(1));
        assert!(failure.stdout.is_empty());
        assert!(String::from_utf8_lossy(&failure.stderr).contains("Invalid scope glob"));
        assert!(!cache.exists());
    }
    for command in ["measure", "history", "delta"] {
        let help = run(repo.path(), &[command, "--help"]);
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains("--exclude-paths <GLOB>"));
    }
    let missing = run(repo.path(), &["measure", "--exclude-paths"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    let help = run(repo.path(), &["modules", "--help"]);
    assert!(help.status.success());
    assert!(!String::from_utf8_lossy(&help.stdout).contains("--exclude-paths"));
    let modules = run(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--exclude-paths",
            "*.min.js",
            "--cache-dir",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(modules.status.code(), Some(2));
    assert!(modules.stdout.is_empty());
    assert!(String::from_utf8_lossy(&modules.stderr).contains("--exclude-paths"));
    assert!(!cache.exists());
}
