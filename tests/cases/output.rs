use std::fs;

use super::{git, repository, run};

#[test]
fn default_measure_is_a_compact_summary_without_debug_metadata() {
    let repo = repository(&[
        ("a.js", "function f() { return 1; }\n"),
        ("b.py", "def g(): return 1\n"),
        ("notes.md", "not source\n"),
    ]);
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);
    let result = run(repo.path(), &["measure", "--no-cache"]);
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!(
            "Erosion: 0.00%\nCommit:  {} (2025-01-01)\n\nFiles: 2    Functions: 2\nLanguages: javascript 1, python 1\nSkipped: 1 unsupported\nParse failures: 0\n\nCommitted source only; uncommitted changes ignored.\n",
            &sha[..10]
        )
    );
}

#[test]
fn verbosity_preserves_every_command_and_format_stdout() {
    let repo = repository(&[("a.py", "def f(): return 1\n")]);
    for command in [
        vec!["measure"],
        vec!["delta", "HEAD", "HEAD"],
        vec!["history", "--since", "1d", "--every", "1d"],
    ] {
        for format in ["table", "json", "csv"] {
            let mut args = command.clone();
            args.extend(["--format", format, "--no-cache"]);
            let quiet = run(repo.path(), &args);
            args.push("--verbose");
            let verbose = run(repo.path(), &args);
            assert!(quiet.status.success());
            assert!(quiet.stderr.is_empty());
            assert!(verbose.status.success());
            assert_eq!(quiet.stdout, verbose.stdout);
            let stderr = String::from_utf8(verbose.stderr).unwrap();
            assert!(stderr.contains("Processing ") && stderr.contains("Blob analysis:"));
            if format == "table" {
                let text = String::from_utf8(quiet.stdout).unwrap();
                assert!(!text.contains("fingerprint") && !text.contains("Analyzer:"));
                assert!(!text.contains("selected_files") && !text.contains("total mass"));
            }
        }
    }
    let short = run(repo.path(), &["measure", "--no-cache", "-v"]);
    assert!(short.status.success());
    assert!(String::from_utf8_lossy(&short.stderr).contains("Blob analysis:"));
}

#[test]
fn failures_are_always_visible_and_partial_null_is_not_zero() {
    let repo = repository(&[
        ("bad.py", "def broken():\n"),
        ("good.py", "def f(): return 1\n"),
    ]);
    for command in [
        vec!["measure"],
        vec!["delta", "HEAD", "HEAD"],
        vec!["history", "--since", "1d", "--every", "1d"],
    ] {
        let mut args = command;
        args.push("--no-cache");
        let failure = run(repo.path(), &args);
        assert_eq!(failure.status.code(), Some(1));
        assert!(failure.stdout.is_empty());
        assert!(String::from_utf8_lossy(&failure.stderr).contains("bad.py"));
        args.push("--allow-partial");
        let partial = run(repo.path(), &args);
        assert!(partial.status.success());
        assert!(String::from_utf8_lossy(&partial.stderr).contains("bad.py"));
        let text = String::from_utf8(partial.stdout).unwrap();
        assert!(text.contains("partial"));
        assert!(text.contains("failed files are excluded"));
        assert!(!text.contains("Blob analysis:"));
    }
    fs::write(repo.path().join("only-bad.toml"), "include = ['bad.py']\n").unwrap();
    let partial = run(
        repo.path(),
        &[
            "measure",
            "--no-cache",
            "--allow-partial",
            "--config",
            "only-bad.toml",
        ],
    );
    assert!(partial.status.success());
    let text = String::from_utf8(partial.stdout).unwrap();
    assert!(text.starts_with("Erosion: not measurable (partial)\n"));
    assert!(text.contains("Parse failures: 1"));
    assert!(!text.contains("Scope:"));
    assert!(!text.contains("0.00%"));
    let invalid = run(
        repo.path(),
        &["measure", "--ref", "not-a-ref", "--no-cache"],
    );
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("does not resolve to a commit"));
}

#[test]
fn history_has_one_row_per_checkpoint_and_explicit_missing_scores() {
    let repo = repository(&[("a.py", "# no functions\n")]);
    let result = run(
        repo.path(),
        &["history", "--since", "3y", "--every", "6mo", "--no-cache"],
    );
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let text = String::from_utf8(result.stdout).unwrap();
    let rows: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("202"))
        .collect();
    assert_eq!(rows.len(), 7);
    assert!(rows[0].contains("no commit"));
    assert!(rows[6].contains("not measurable"));
    assert!(text.contains("No commit: no ancestor"));
    assert!(text.contains("Not measurable: no function mass"));
    assert!(text.contains("Failed") && text.contains("Skipped"));
    assert!(!text.contains("0.00%"));
    assert!(!text.contains("Overall change:"));
}

#[test]
fn top_functions_have_names_locations_and_numeric_measures() {
    let complex = format!("def complex(x):\n{}", "    if x: work()\n".repeat(10));
    let repo = repository(&[("source.py", &complex)]);
    let result = run(repo.path(), &["measure", "--no-cache", "--top", "1"]);
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    let text = String::from_utf8(result.stdout).unwrap();
    assert!(text.starts_with("Erosion: 100.00%\n"));
    assert!(text.contains("Top complex functions (CC > 10, ranked by mass):"));
    assert!(text.contains("1. complex (CC 11, SLOC 11, mass "));
    assert!(text.contains("source.py:1-11"));
}
