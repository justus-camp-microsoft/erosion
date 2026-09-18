use std::fs;

use serde_json::Value;

use super::{commit_at, git, repository, run, successful_json};

fn commands() -> [Vec<&'static str>; 4] {
    [
        vec!["measure"],
        vec!["history", "--since", "1d"],
        vec!["delta", "HEAD", "HEAD"],
        vec!["modules", "--since", "1d"],
    ]
}

fn snapshots(report: &Value) -> Vec<&Value> {
    if let Some(modules) = report["modules"].as_array() {
        modules
            .iter()
            .flat_map(|module| module["snapshots"].as_array().unwrap())
            .collect()
    } else {
        report["snapshots"].as_array().unwrap().iter().collect()
    }
}

#[test]
fn all_commands_apply_language_policies_without_changing_defaults() {
    let repo = repository(&[
        ("pkg/main.js", "const f = () => 1;"),
        ("pkg/main.ts", "const f = () => 1;"),
        ("pkg/view.tsx", "const f = () => <div />;"),
        ("pkg/main.py", "def f(): return 1\n"),
        (
            "pkg/main.rs",
            "fn main() {}\n#[cfg(test)] mod tests { #[test] fn inline_test() {} }\n",
        ),
        ("pkg/main.gleam", "pub fn main() { 1 }\n"),
        ("pkg/bench/main.ts", "const f = () => 1;"),
        ("pkg/bundle/main.js", "const f = () => 1;"),
        ("pkg/generated/main.ts", "const f = () => 1;"),
        ("pkg/tests/integration.rs", "fn {"),
        ("pkg/tests/helper.ts", "function {"),
        ("pkg/__tests__/view.tsx", "function {"),
        ("pkg/main.spec.js", "function {"),
        ("pkg/main.test.ts", "function {"),
        ("pkg/test_math.py", "def broken(:"),
        ("pkg/math_test.gleam", "pub fn broken("),
        ("pkg/tests/data.json", "fixture"),
    ]);
    let mut policy = None;
    for mut args in commands() {
        args.extend(["--format", "json", "--no-cache"]);
        let strict = run(repo.path(), &args);
        assert_eq!(strict.status.code(), Some(1));
        assert!(strict.stdout.is_empty());
        let mut baseline_args = args.clone();
        baseline_args.push("--allow-partial");
        let baseline = successful_json(repo.path(), &baseline_args);
        assert!(baseline.get("test_exclusion").is_none());
        assert!(
            snapshots(&baseline)
                .iter()
                .all(|point| point["coverage"].get("test_excluded_entries").is_none())
        );
        args.push("--exclude-tests");
        let result = run(repo.path(), &args);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(result.stderr.is_empty());
        let report: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["metric"], baseline["metric"]);
        assert_eq!(report["test_exclusion"]["version"], "language-tests-v1");
        let languages = report["test_exclusion"]["languages"].as_object().unwrap();
        assert_eq!(languages.len(), 6);
        for language in ["javascript", "typescript", "tsx", "python", "rust", "gleam"] {
            assert!(
                languages[language]["version"]
                    .as_str()
                    .unwrap()
                    .ends_with("-tests-v1")
            );
            assert!(
                !languages[language]["path_patterns"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(
                !languages[language]["syntax_rules"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        if let Some(previous) = &policy {
            assert_eq!(previous, &report["test_exclusion"]);
        } else {
            policy = Some(report["test_exclusion"].clone());
        }
        let fingerprint = if args[0] == "modules" {
            assert_eq!(report["schema_version"], 4);
            assert_eq!(report["file_scope"], "exclude_language_tests");
            assert_eq!(report["inventory"], baseline["inventory"]);
            "grouping_fingerprint"
        } else {
            assert_eq!(report["schema_version"], 3);
            assert_eq!(report["scope"], baseline["scope"]);
            "scope_fingerprint"
        };
        assert_ne!(report[fingerprint], baseline[fingerprint]);
        for point in snapshots(&report) {
            let coverage = &point["coverage"];
            if point["commit"].is_null() {
                assert_eq!(point["status"], "no_commit");
                assert_eq!(coverage["test_excluded_entries"], 0);
                assert_eq!(coverage["syntax_test_files"], 0);
                assert_eq!(coverage["test_excluded_functions"], 0);
                assert_eq!(coverage["test_excluded_source_lines"], 0);
                continue;
            }
            assert_eq!(point["status"], "complete");
            assert_eq!(point["functions"], 9);
            assert_eq!(coverage["tracked_entries"], 17);
            assert_eq!(coverage["parsed_files"], 9);
            assert_eq!(coverage["failed_files"], 0);
            assert_eq!(coverage["excluded_entries"], 7);
            assert_eq!(coverage["test_excluded_entries"], 7);
            assert_eq!(coverage["unsupported_files"], 1);
            assert_eq!(coverage["syntax_test_files"], 1);
            assert_eq!(coverage["test_excluded_functions"], 1);
        }
    }
}

#[test]
fn inline_tests_recompute_all_commands_and_share_path_independent_raw_filtered_cache_views() {
    let javascript = format!(
        "import {{test as check}} from 'node:test';\nexport function production(x) {{\n if(x)work();\n check('case',()=>{{\n{} }});\n return x;\n}}\n",
        "  if(x)work();\n".repeat(10)
    );
    let python = format!(
        "from unittest import TestCase\n\
         def production(x):\n if x: return 1\n return 0\n\
         class Checks(TestCase):\n def test_case(self):\n{}",
        "  if self: pass\n".repeat(10)
    );
    let rust = format!(
        "pub fn production(x: bool) -> bool {{ if x {{ return true; }} false }}\n\
         #[cfg(test)]\nmod checks {{\n #[test]\n fn example() {{\n{} }}\n}}\n",
        "  if true { work(); }\n".repeat(10)
    );
    let gleam = "import gleeunit/should\npub fn production(x: Bool) { x }\npub fn example_test() { 1 |> should.equal(1) }\n";
    let repo = repository(&[
        ("pkg/main.js", &javascript),
        ("pkg/main.ts", &javascript),
        ("pkg/main.tsx", &javascript),
        ("pkg/main.py", &python),
        ("pkg/main.rs", &rust),
        ("pkg/main.gleam", gleam),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let cache_path = cache.path().to_str().unwrap();
    let raw = successful_json(
        repo.path(),
        &[
            "measure",
            "--top",
            "99",
            "--format",
            "json",
            "--cache-dir",
            cache_path,
        ],
    );
    assert_eq!(raw["snapshots"][0]["functions"], 12);
    assert_eq!(raw["snapshots"][0]["complex_functions"], 8);
    let fingerprint = raw["metric"]["analyzer_fingerprint"].as_str().unwrap();
    let mut records = Vec::new();
    let mut removed_lines = 0;
    for (extension, language) in [
        ("js", "javascript"),
        ("ts", "typescript"),
        ("tsx", "tsx"),
        ("py", "python"),
        ("rs", "rust"),
        ("gleam", "gleam"),
    ] {
        let oid = git(
            repo.path(),
            &["rev-parse", &format!("HEAD:pkg/main.{extension}")],
        );
        let path = cache
            .path()
            .join(fingerprint)
            .join(language)
            .join(&oid[..2])
            .join(format!("{oid}.json"));
        let bytes = fs::read(&path).unwrap();
        let record: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(record["schema"], 2);
        let analysis = &record["analysis"];
        assert_eq!(analysis["functions"].as_array().unwrap().len(), 2);
        assert_eq!(
            analysis["without_tests"]["functions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(
            !analysis["without_tests"]["regions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        removed_lines += analysis["source_lines"].as_u64().unwrap()
            - analysis["without_tests"]["source_lines"].as_u64().unwrap();
        records.push((path, bytes));
    }
    for command in commands() {
        let mut args: Vec<&str> = command.clone();
        args.extend([
            "--exclude-tests",
            "--format",
            "json",
            "--cache-dir",
            cache_path,
        ]);
        let filtered = successful_json(repo.path(), &args);
        assert_eq!(filtered["metric"], raw["metric"]);
        for point in snapshots(&filtered)
            .into_iter()
            .filter(|point| !point["commit"].is_null())
        {
            assert_eq!(point["functions"], 6);
            assert_eq!(point["complex_functions"], 0);
            assert_eq!(point["erosion_pct"], 0.0);
            assert!(
                (point["total_mass"].as_f64().unwrap() - (8.0 * 3.0_f64.sqrt() + 3.0)).abs()
                    < 1e-12
            );
            assert!(point["top_functions"].as_array().unwrap().is_empty());
            assert_eq!(point["coverage"]["tracked_entries"], 6);
            assert_eq!(point["coverage"]["parsed_files"], 6);
            assert_eq!(point["coverage"]["test_excluded_entries"], 0);
            assert_eq!(point["coverage"]["syntax_test_files"], 6);
            assert_eq!(point["coverage"]["test_excluded_functions"], 6);
            assert_eq!(
                point["coverage"]["test_excluded_source_lines"],
                removed_lines
            );
        }
        let mut args = command;
        args.extend(["--exclude-tests", "--format", "json", "--no-cache"]);
        assert_eq!(filtered, successful_json(repo.path(), &args));
    }
    let filtered_top = successful_json(
        repo.path(),
        &[
            "measure",
            "--top",
            "99",
            "--exclude-tests",
            "--format",
            "json",
            "--cache-dir",
            cache_path,
        ],
    );
    assert!(
        filtered_top["snapshots"][0]["top_functions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        raw,
        successful_json(
            repo.path(),
            &[
                "measure",
                "--top",
                "99",
                "--format",
                "json",
                "--cache-dir",
                cache_path,
            ]
        )
    );
    fs::rename(
        repo.path().join("pkg/main.ts"),
        repo.path().join("pkg/main.test.ts"),
    )
    .unwrap();
    commit_at(repo.path(), "2025-01-02T12:00:00Z");
    let renamed = successful_json(
        repo.path(),
        &[
            "measure",
            "--exclude-tests",
            "--format",
            "json",
            "--cache-dir",
            cache_path,
        ],
    );
    assert_eq!(renamed["snapshots"][0]["functions"], 5);
    assert_eq!(
        renamed["snapshots"][0]["coverage"]["test_excluded_entries"],
        1
    );
    assert_eq!(renamed["snapshots"][0]["coverage"]["syntax_test_files"], 5);
    for (path, before) in records {
        assert_eq!(before, fs::read(path).unwrap());
    }
}

#[test]
fn excluded_tests_leave_present_modules_unmeasurable_and_reset_changes() {
    let complex = format!("function f(x) {{\n{}}}\n", "if (x) work();\n".repeat(10));
    let repo = repository(&[
        ("pkg/active.ts", &complex),
        ("pkg/always.test.ts", &complex),
        ("tests/unit.ts", &complex),
    ]);
    fs::rename(
        repo.path().join("pkg/active.ts"),
        repo.path().join("pkg/active.test.ts"),
    )
    .unwrap();
    commit_at(repo.path(), "2025-01-02T12:00:00Z");
    fs::remove_file(repo.path().join("pkg/active.test.ts")).unwrap();
    fs::write(repo.path().join("pkg/active.ts"), "const f = () => 1;").unwrap();
    commit_at(repo.path(), "2025-01-03T12:00:00Z");
    for command in ["history", "modules"] {
        let report = successful_json(
            repo.path(),
            &[
                command,
                "--since",
                "3d",
                "--every",
                "1d",
                "--exclude-tests",
                "--format",
                "json",
                "--no-cache",
            ],
        );
        let points = if command == "modules" {
            assert_eq!(report["modules"].as_array().unwrap().len(), 2);
            let tests = &report["modules"][1];
            assert_eq!(tests["path"], "tests");
            for point in tests["snapshots"].as_array().unwrap().iter().skip(1) {
                assert_eq!(point["status"], "not_measurable");
                assert_eq!(point["coverage"]["tracked_entries"], 1);
                assert_eq!(point["coverage"]["test_excluded_entries"], 1);
                assert!(point["erosion_pct"].is_null());
            }
            report["modules"][0]["snapshots"].as_array().unwrap()
        } else {
            report["snapshots"].as_array().unwrap()
        };
        assert_eq!(
            points
                .iter()
                .map(|point| point["status"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["no_commit", "complete", "not_measurable", "complete"]
        );
        assert_eq!(points[1]["erosion_pct"], 100.0);
        assert!(points[2]["erosion_pct"].is_null());
        assert_eq!(points[3]["erosion_pct"], 0.0);
        assert!(points.iter().all(|point| point["change_pp"].is_null()));
    }
    let delta = successful_json(
        repo.path(),
        &[
            "delta",
            "HEAD~2",
            "HEAD",
            "--exclude-tests",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(delta["snapshots"][1]["change_pp"], -100.0);
    let gap = successful_json(
        repo.path(),
        &[
            "delta",
            "HEAD~1",
            "HEAD",
            "--exclude-tests",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert!(gap["snapshots"][1]["change_pp"].is_null());
}

#[test]
fn test_exclusion_respects_scope_order_but_modules_ignore_config() {
    let repo = repository(&[
        ("pkg/main.ts", "const f = () => 1;"),
        ("pkg/tests/a.ts", "function {"),
        ("pkg/tests/b.ts", "function {"),
        ("pkg/extra.spec.ts", "function {"),
        ("other/bad.ts", "function {"),
    ]);
    fs::write(
        repo.path().join("erosion.toml"),
        "include = ['pkg/**']\nexclude = ['pkg/tests/a.ts']\n",
    )
    .unwrap();
    let report = successful_json(
        repo.path(),
        &[
            "measure",
            "--exclude-tests",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    let coverage = &report["snapshots"][0]["coverage"];
    assert_eq!(coverage["tracked_entries"], 5);
    assert_eq!(coverage["excluded_entries"], 4);
    assert_eq!(coverage["test_excluded_entries"], 2);
    assert_eq!(coverage["parsed_files"], 1);
    fs::write(repo.path().join("erosion.toml"), "invalid [").unwrap();
    let failure = run(repo.path(), &["measure", "--exclude-tests", "--no-cache"]);
    assert_eq!(failure.status.code(), Some(1));
    assert!(failure.stdout.is_empty());
    let modules = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--glob",
            "pkg",
            "--exclude-tests",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    let point = modules["modules"][0]["snapshots"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(point["coverage"]["tracked_entries"], 4);
    assert_eq!(point["coverage"]["test_excluded_entries"], 3);
    assert_eq!(point["coverage"]["parsed_files"], 1);
    assert_eq!(modules["inventory"][1]["unselected_module_entries"], 1);
}

#[test]
fn excluded_top_functions_and_cache_records_do_not_leak_into_reports() {
    let complex = format!("function f(x) {{\n{}}}\n", "if (x) work();\n".repeat(10));
    let more_complex = format!("function test(x) {{\n{}}}\n", "if (x) work();\n".repeat(20));
    let repo = repository(&[
        ("pkg/main.ts", &complex),
        ("pkg/simple.ts", "const f = () => 1;"),
        ("pkg/main.test.ts", &more_complex),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let mut args = vec![
        "measure",
        "--top",
        "5",
        "--cache-dir",
        cache.path().to_str().unwrap(),
        "--format",
        "json",
    ];
    let baseline = successful_json(repo.path(), &args);
    assert_eq!(
        baseline["snapshots"][0]["top_functions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let fingerprint = baseline["metric"]["analyzer_fingerprint"].as_str().unwrap();
    let source_oid = git(repo.path(), &["rev-parse", "HEAD:pkg/main.ts"]);
    let test_oid = git(repo.path(), &["rev-parse", "HEAD:pkg/main.test.ts"]);
    let source_record = cache
        .path()
        .join(fingerprint)
        .join("typescript")
        .join(&source_oid[..2])
        .join(format!("{source_oid}.json"));
    let test_record = cache
        .path()
        .join(fingerprint)
        .join("typescript")
        .join(&test_oid[..2])
        .join(format!("{test_oid}.json"));
    let before = fs::read(&source_record).unwrap();
    fs::write(&test_record, "invalid cache").unwrap();
    args.push("--exclude-tests");
    let filtered = run(repo.path(), &args);
    assert!(
        filtered.status.success(),
        "{}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    let report: Value = serde_json::from_slice(&filtered.stdout).unwrap();
    assert_eq!(report["metric"], baseline["metric"]);
    assert_eq!(
        report["snapshots"][0]["top_functions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        report["snapshots"][0]["top_functions"][0]["path"],
        "pkg/main.ts"
    );
    let point = &report["snapshots"][0];
    let complex_mass = 11.0 * 11.0_f64.sqrt();
    assert_eq!(point["functions"], 2);
    assert_eq!(point["complex_functions"], 1);
    assert!((point["complex_mass"].as_f64().unwrap() - complex_mass).abs() < 1e-12);
    assert!((point["total_mass"].as_f64().unwrap() - (complex_mass + 1.0)).abs() < 1e-12);
    assert!(
        (point["erosion_pct"].as_f64().unwrap() - 100.0 * complex_mass / (complex_mass + 1.0))
            .abs()
            < 1e-12
    );
    assert_eq!(before, fs::read(&source_record).unwrap());
    let uncached = run(
        repo.path(),
        &[
            "measure",
            "--top",
            "5",
            "--exclude-tests",
            "--no-cache",
            "--format",
            "json",
        ],
    );
    assert!(uncached.status.success());
    assert_eq!(filtered.stdout, uncached.stdout);
    fs::write(source_record, "invalid cache").unwrap();
    args.push("--allow-partial");
    let failure = run(repo.path(), &args);
    assert_eq!(failure.status.code(), Some(1));
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("Invalid cache"));
}

#[test]
fn kept_parse_and_blob_errors_remain_explicit() {
    let repo = repository(&[
        (
            "src/main.ts",
            "import {test} from 'node:test'; test('broken',()=>{",
        ),
        ("src/main.rs", "#[cfg(test)] mod checks { fn broken("),
        (
            "src/main.py",
            "from unittest import TestCase\nclass Checks(TestCase):\n def broken(:\n",
        ),
        (
            "src/main.gleam",
            "import gleeunit/should\npub fn broken_test(",
        ),
        ("src/main.test.ts", "function {"),
    ]);
    for mut args in commands() {
        args.extend(["--exclude-tests", "--no-cache"]);
        let failure = run(repo.path(), &args);
        assert_eq!(failure.status.code(), Some(1));
        assert!(failure.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&failure.stderr);
        assert!(stderr.contains("src/main.ts"));
        assert!(!stderr.contains("src/main.test.ts"));
        args.extend(["--allow-partial", "--format", "json"]);
        let report = successful_json(repo.path(), &args);
        for point in snapshots(&report)
            .iter()
            .filter(|point| !point["commit"].is_null())
        {
            assert_eq!(point["status"], "partial");
            assert_eq!(point["coverage"]["failed_files"], 4);
            assert_eq!(point["coverage"]["test_excluded_entries"], 1);
            assert_eq!(point["coverage"]["syntax_test_files"], 0);
        }
    }
    let oid = git(repo.path(), &["rev-parse", "HEAD:src/main.ts"]);
    fs::remove_file(
        repo.path()
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]),
    )
    .unwrap();
    let failure = run(
        repo.path(),
        &[
            "measure",
            "--exclude-tests",
            "--allow-partial",
            "--no-cache",
        ],
    );
    assert_eq!(failure.status.code(), Some(1));
    assert!(failure.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("Reading"));
}

#[test]
fn all_command_formats_and_help_expose_exclusion_without_default_noise() {
    let repo = repository(&[
        ("pkg/main.ts", "const f = () => 1;"),
        ("pkg/main.spec.ts", "const f = () => 1;"),
    ]);
    for command in commands() {
        let help = run(repo.path(), &[command[0], "--help"]);
        assert!(help.status.success());
        let help = String::from_utf8(help.stdout).unwrap();
        for phrase in [
            "--exclude-tests",
            "case-sensitive",
            "language-tests-v1",
            "*.spec.*",
            "Inline",
            "JSON/CSV",
            "unittest",
            "pytest",
            "Rust",
            "Gleam",
            "mixed",
        ] {
            assert!(
                help.to_lowercase().contains(&phrase.to_lowercase()),
                "{phrase}: {help}"
            );
        }
        let short = run(repo.path(), &[command[0], "-h"]);
        assert!(String::from_utf8_lossy(&short.stdout).contains("--exclude-tests"));
        for format in ["table", "json", "csv"] {
            let mut args = command.clone();
            args.extend(["--no-cache", "--format", format]);
            let baseline = run(repo.path(), &args);
            assert!(baseline.status.success());
            assert!(!String::from_utf8_lossy(&baseline.stdout).contains("test_exclusion"));
            args.push("--exclude-tests");
            let result = run(repo.path(), &args);
            assert!(result.status.success());
            assert!(result.stderr.is_empty());
            args.push("--verbose");
            let verbose = run(repo.path(), &args);
            assert!(verbose.status.success());
            assert_eq!(result.stdout, verbose.stdout);
            assert!(!verbose.stderr.is_empty());
            if format == "table" {
                let text = String::from_utf8(result.stdout).unwrap();
                assert!(text.lines().next().unwrap().contains("(tests excluded)"));
                assert!(!text.contains("language-tests-v1"));
                if command[0] == "modules" {
                    assert!(!text.contains("Repository inventory") && !text.contains("Grouping:"));
                }
            } else if format == "csv" {
                let mut reader = csv::Reader::from_reader(result.stdout.as_slice());
                let headers = reader.headers().unwrap().clone();
                let policy = headers
                    .iter()
                    .position(|name| name == "test_exclusion_json")
                    .unwrap();
                let excluded = headers
                    .iter()
                    .position(|name| name == "test_excluded_entries")
                    .unwrap();
                let commit = headers.iter().position(|name| name == "commit").unwrap();
                let total_excluded = headers
                    .iter()
                    .position(|name| name == "excluded_entries")
                    .unwrap();
                for row in reader.records() {
                    let row = row.unwrap();
                    let expected = if row[commit].is_empty() { "0" } else { "1" };
                    assert_eq!(&row[excluded], expected);
                    assert_eq!(&row[total_excluded], expected);
                    let policy: Value = serde_json::from_str(&row[policy]).unwrap();
                    assert_eq!(policy["version"], "language-tests-v1");
                    for counter in [
                        "syntax_test_files",
                        "test_excluded_functions",
                        "test_excluded_source_lines",
                    ] {
                        let column = headers.iter().position(|name| name == counter).unwrap();
                        assert_eq!(&row[column], "0");
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn unsupported_and_non_regular_entries_have_no_inferred_testing_language() {
    let repo = repository(&[
        ("pkg/main.ts", "const f = () => 1;"),
        ("pkg/tests/fixture.json", "test fixture"),
    ]);
    std::os::unix::fs::symlink("/missing/external", repo.path().join("pkg/tests/link.ts")).unwrap();
    std::os::unix::fs::symlink("/missing/external", repo.path().join("pkg/link")).unwrap();
    git(repo.path(), &["add", "."]);
    let oid = git(repo.path(), &["rev-parse", "HEAD"]);
    git(
        repo.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &oid,
            "pkg/tests/submodule.rs",
        ],
    );
    git(repo.path(), &["commit", "-qm", "links"]);
    let report = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--exclude-tests",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    let coverage = &report["modules"][0]["snapshots"][1]["coverage"];
    assert_eq!(coverage["tracked_entries"], 5);
    assert_eq!(coverage["test_excluded_entries"], 0);
    assert_eq!(coverage["excluded_entries"], 0);
    assert_eq!(coverage["non_regular_entries"], 3);
    assert_eq!(coverage["unsupported_files"], 1);
    assert_eq!(coverage["parsed_files"], 1);
    assert_eq!(report["inventory"][1]["selected_module_entries"], 5);
}
