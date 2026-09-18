use std::fs;

use serde_json::Value;

use super::{commit_at, git, repository, run, successful_json};

fn series<'a>(report: &'a Value, path: &str) -> &'a Vec<Value> {
    report["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|module| module["path"] == path)
        .unwrap()["snapshots"]
        .as_array()
        .unwrap()
}

#[test]
fn directory_depth_never_promotes_loose_files_or_drops_module_contents() {
    let files = [
        ("root.ts", "function { outside depth"),
        ("packages/dds/README.md", "documentation"),
        ("packages/dds/SchemaVersioning.md", "documentation"),
        ("packages/dds/loose.ts", "const a = () => 1;"),
        ("packages/dds/module.ts/src/index.ts", "const a = () => 1;"),
        ("packages/dds/tree/src/index.ts", "const a = () => 1;"),
        ("packages/dds/tree/README.md", "documentation"),
        ("packages/dds/tree/generated/out.ts", "const a = () => 1;"),
        ("packages/dds/tree/tests/main.ts", "const a = () => 1;"),
        ("packages/dds-old/other/src/x.ts", "function { unselected"),
        ("packages/docs/about.md", "documentation"),
    ];
    let repo = repository(&files);
    let report = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--depth",
            "3",
            "--glob",
            "packages/dds/*",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    let paths: Vec<_> = report["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|module| module["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["packages/dds/module.ts", "packages/dds/tree"]);
    assert_eq!(report["schema_version"], 2);
    assert_eq!(report["grouping"]["kind"], "directories");
    assert_eq!(report["discovered_modules"], 3);
    let tree = series(&report, "packages/dds/tree").last().unwrap();
    assert_eq!(tree["coverage"]["tracked_entries"], 4);
    assert_eq!(tree["coverage"]["parsed_files"], 3);
    assert_eq!(tree["coverage"]["unsupported_files"], 1);
    assert_eq!(tree["functions"], 3);
    let inventories = report["inventory"].as_array().unwrap();
    assert!(inventories[0]["commit"].is_null());
    assert_eq!(inventories[0]["tracked_entries"], 0);
    let last = inventories.last().unwrap();
    assert_eq!(last["tracked_entries"], files.len());
    assert_eq!(last["outside_depth_entries"], 5);
    assert_eq!(last["unselected_module_entries"], 1);
    assert_eq!(last["selected_module_entries"], 5);
    for (index, inventory) in inventories.iter().enumerate() {
        let selected: u64 = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|module| {
                module["snapshots"][index]["coverage"]["tracked_entries"]
                    .as_u64()
                    .unwrap()
            })
            .sum();
        assert_eq!(inventory["selected_module_entries"], selected);
        assert_eq!(
            inventory["tracked_entries"].as_u64().unwrap(),
            selected
                + inventory["outside_depth_entries"].as_u64().unwrap()
                + inventory["unselected_module_entries"].as_u64().unwrap()
        );
    }
    let parent = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--depth",
            "2",
            "--glob",
            "packages/dds",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    let point = series(&parent, "packages/dds").last().unwrap();
    assert_eq!(point["coverage"]["tracked_entries"], 8);
    assert_eq!(point["coverage"]["parsed_files"], 5);
    assert_eq!(point["coverage"]["unsupported_files"], 3);
    assert_eq!(point["functions"], 5);
    assert_eq!(parent["inventory"][1]["outside_depth_entries"], 1);
    let file_glob = run(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--depth",
            "3",
            "--glob",
            "packages/dds/loose.ts",
            "--no-cache",
        ],
    );
    assert_eq!(file_glob.status.code(), Some(1));
    assert!(file_glob.stdout.is_empty());
    assert!(String::from_utf8_lossy(&file_glob.stderr).contains("files are not modules"));
}

#[test]
fn module_filter_selects_whole_contents_and_ignores_file_scope() {
    let repo = repository(&[
        ("packages/api/a.ts", "const f = () => 1;\n"),
        ("packages/api/tests/a.test.ts", "const f = () => 1;\n"),
        ("packages/api/generated/a.py", "def f():\n    return 1\n"),
        ("packages/api/README.md", "Not source\n"),
        ("packages/api2/bad.ts", "function {"),
    ]);
    fs::write(repo.path().join("erosion.toml"), "exclude = ['**']\n").unwrap();
    fs::write(repo.path().join("packages/api/a.ts"), "function { dirty").unwrap();
    fs::write(repo.path().join("packages/api/untracked.ts"), "function {").unwrap();
    let before = git(repo.path(), &["status", "--porcelain=v1", "-uall"]);
    let cache = tempfile::tempdir().unwrap();
    let args = [
        "modules",
        "--since",
        "1d",
        "--depth",
        "2",
        "--glob",
        "packages/api",
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
    assert!(cold.stderr.is_empty());
    let warm = run(repo.path(), &args);
    assert!(warm.status.success());
    assert_eq!(cold.stdout, warm.stdout);
    fs::write(repo.path().join("erosion.toml"), "not valid toml [").unwrap();
    let uncached = run(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--depth",
            "2",
            "--glob",
            "packages/api",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert!(uncached.status.success());
    assert_eq!(cold.stdout, uncached.stdout);
    let report: Value = serde_json::from_slice(&cold.stdout).unwrap();
    assert_eq!(report["mode"], "modules");
    assert_eq!(report["file_scope"], "all_tracked_entries");
    assert_eq!(report["modules"].as_array().unwrap().len(), 1);
    assert_eq!(report["grouping"]["depth"], 2);
    assert_eq!(
        report["grouping"]["globs"],
        serde_json::json!(["packages/api"])
    );
    let point = series(&report, "packages/api").last().unwrap();
    assert_eq!(point["status"], "complete");
    assert_eq!(point["functions"], 3);
    assert_eq!(point["coverage"]["tracked_entries"], 4);
    assert_eq!(point["coverage"]["parsed_files"], 3);
    assert_eq!(point["coverage"]["unsupported_files"], 1);
    assert_eq!(point["coverage"]["excluded_entries"], 0);
    assert_eq!(
        before,
        git(repo.path(), &["status", "--porcelain=v1", "-uall"])
    );
}

#[test]
fn discovers_deleted_modules_and_preserves_gaps_without_zero_deltas() {
    let complex = format!(
        "function f(x) {{\n{}return x;\n}}\n",
        "if (x) x--;\n".repeat(10)
    );
    let repo = repository(&[
        ("always/a.ts", "const a = () => 1;\n"),
        ("gone/a.ts", "const a = () => 1;\n"),
        ("docs/README.md", "unsupported"),
        ("types/a.ts", "interface A { x: number }\n"),
    ]);
    fs::create_dir(repo.path().join("new")).unwrap();
    fs::write(repo.path().join("new/a.ts"), complex).unwrap();
    commit_at(repo.path(), "2025-01-02T12:00:00Z");
    fs::remove_file(repo.path().join("new/a.ts")).unwrap();
    commit_at(repo.path(), "2025-01-03T12:00:00Z");
    fs::write(repo.path().join("new/a.ts"), "const a = () => 1;\n").unwrap();
    commit_at(repo.path(), "2025-01-04T12:00:00Z");
    fs::remove_file(repo.path().join("gone/a.ts")).unwrap();
    commit_at(repo.path(), "2025-01-05T12:00:00Z");
    let report = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "5d",
            "--every",
            "1d",
            "--depth",
            "1",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    let paths: Vec<_> = report["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|module| module["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["always", "docs", "gone", "new", "types"]);
    let new = series(&report, "new");
    assert_eq!(
        new.iter()
            .map(|p| p["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "no_commit",
            "no_files",
            "complete",
            "no_files",
            "complete",
            "complete"
        ]
    );
    assert_eq!(new[2]["erosion_pct"], 100.0);
    assert_eq!(new[4]["erosion_pct"], 0.0);
    for point in &new[..5] {
        assert!(point["change_pp"].is_null());
    }
    assert_eq!(new[5]["change_pp"], 0.0);
    for index in [0, 1, 3] {
        assert!(new[index]["erosion_pct"].is_null());
    }
    assert_eq!(
        series(&report, "gone").last().unwrap()["status"],
        "no_files"
    );
    for path in ["docs", "types"] {
        let point = series(&report, path).last().unwrap();
        assert_eq!(point["status"], "not_measurable");
        assert_eq!(point["coverage"]["tracked_entries"], 1);
        assert!(point["erosion_pct"].is_null());
    }
}

#[test]
fn grouping_combines_mass_not_module_or_file_percentages() {
    let complex = format!(
        "function f(x) {{\n{}return x;\n}}\n",
        "if (x) x--;\n".repeat(10)
    );
    let repo = repository(&[
        ("pkg/complex.ts", &complex),
        ("pkg/simple.ts", "const a = () => 1;\n"),
    ]);
    let report = successful_json(
        repo.path(),
        &["modules", "--since", "1d", "--format", "json", "--no-cache"],
    );
    let point = series(&report, "pkg").last().unwrap();
    let expected_complex = 11.0 * 12.0_f64.sqrt();
    let expected = 100.0 * expected_complex / (expected_complex + 1.0);
    assert!((point["erosion_pct"].as_f64().unwrap() - expected).abs() < 1e-12);
    let measured = successful_json(repo.path(), &["measure", "--format", "json", "--no-cache"]);
    assert_eq!(point, &measured["snapshots"][0]);
}

#[test]
fn selected_failures_are_strict_and_partial_is_local_to_the_module() {
    let repo = repository(&[
        ("broken/tests/test.ts", "function {"),
        ("good/a.ts", "const a = () => 1;\n"),
    ]);
    fs::write(
        repo.path().join("erosion.toml"),
        "exclude = ['**/tests/**']\n",
    )
    .unwrap();
    let args = ["modules", "--since", "1d", "--format", "json", "--no-cache"];
    let strict = run(repo.path(), &args);
    assert_eq!(strict.status.code(), Some(1));
    assert!(strict.stdout.is_empty());
    assert!(String::from_utf8_lossy(&strict.stderr).contains("broken/tests/test.ts"));
    let mut partial_args = args.to_vec();
    partial_args.push("--allow-partial");
    let report = successful_json(repo.path(), &partial_args);
    let broken = series(&report, "broken").last().unwrap();
    assert_eq!(broken["status"], "partial");
    assert_eq!(broken["coverage"]["failed_files"], 1);
    assert!(broken["erosion_pct"].is_null());
    assert_eq!(
        series(&report, "good").last().unwrap()["status"],
        "complete"
    );
    let mut filtered_args = args.to_vec();
    filtered_args.extend(["--glob", "good"]);
    assert!(run(repo.path(), &filtered_args).status.success());
}

#[test]
fn invalid_options_and_unmatched_module_globs_fail_without_results() {
    let repo = repository(&[("pkg/nested/a.ts", "const a = () => 1;")]);
    for extra in [
        vec!["--depth", "0"],
        vec!["--glob", "["],
        vec!["--depth", "1", "--glob", "pkg/**/*.ts"],
        vec!["--glob", "missing"],
        vec!["--depth", "3"],
        vec!["--config", "erosion.toml"],
    ] {
        let mut args = vec!["modules", "--since", "1d", "--no-cache"];
        args.extend(extra);
        let output = run(repo.path(), &args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    for option in ["--help", "-h"] {
        let help = run(repo.path(), &["modules", option]);
        assert!(help.status.success());
        let text = String::from_utf8(help.stdout).unwrap();
        for phrase in [
            "module",
            "erosion.toml",
            "--depth",
            "--glob",
            "--since",
            "tracked",
            "directory",
            "filenames never become modules",
            "repository-wide",
        ] {
            assert!(text.contains(phrase), "{text}");
        }
        assert!(!text.contains("--config"));
    }
}

#[test]
fn formats_quote_module_paths_and_keep_absence_distinct() {
    let path = "odd,\nmodule";
    let repo = repository(&[
        (&format!("{path}/a.ts"), "const a = () => 1;"),
        ("shallow.ts", "function { above directory depth"),
    ]);
    let args = ["modules", "--since", "1d", "--no-cache", "--format"];
    let mut csv_args = args.to_vec();
    csv_args.push("csv");
    let csv_output = run(repo.path(), &csv_args);
    assert!(csv_output.status.success());
    let mut reader = csv::Reader::from_reader(csv_output.stdout.as_slice());
    let headers = reader.headers().unwrap().clone();
    let module_column = headers.iter().position(|name| name == "module").unwrap();
    let status_column = headers.iter().position(|name| name == "status").unwrap();
    let tracked_column = headers
        .iter()
        .position(|name| name == "tracked_entries")
        .unwrap();
    let outside_column = headers
        .iter()
        .position(|name| name == "outside_depth_entries")
        .unwrap();
    let repository_column = headers
        .iter()
        .position(|name| name == "repository_tracked_entries")
        .unwrap();
    let selected_column = headers
        .iter()
        .position(|name| name == "selected_module_entries")
        .unwrap();
    let score_column = headers
        .iter()
        .position(|name| name == "erosion_pct")
        .unwrap();
    let rows: Vec<_> = reader.records().map(Result::unwrap).collect();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| &row[module_column] == path));
    assert_eq!(&rows[0][status_column], "no_commit");
    assert_eq!(&rows[0][score_column], "");
    assert_eq!(&rows[1][status_column], "complete");
    assert_eq!(&rows[1][tracked_column], "1");
    assert_eq!(&rows[0][outside_column], "0");
    assert_eq!(&rows[1][outside_column], "1");
    assert_eq!(&rows[1][repository_column], "2");
    assert_eq!(&rows[1][selected_column], "1");
    let table = run(repo.path(), &["modules", "--since", "1d", "--no-cache"]);
    assert!(table.status.success());
    let text = String::from_utf8_lossy(&table.stdout);
    assert!(text.contains("odd,\\nmodule"));
    assert!(text.contains("no commit"));
    assert!(!text.contains("Repository inventory"));
    assert!(!text.contains("Outside depth"));
    assert!(!text.contains("Module: shallow.ts"));
    assert_eq!(
        text.lines().filter(|line| line.starts_with("202")).count(),
        2
    );
    let verbose = run(
        repo.path(),
        &["modules", "--since", "1d", "--no-cache", "--verbose"],
    );
    assert!(verbose.status.success());
    assert_eq!(table.stdout, verbose.stdout);
    assert!(table.stderr.is_empty());
    assert!(!verbose.stderr.is_empty());
}

#[test]
fn module_tables_show_only_measurements() {
    let repo = repository(&[
        ("good/a.ts", "const a = () => 1;"),
        ("bad/a.ts", "function {"),
        ("docs/README.md", "unsupported"),
    ]);
    for extra in [vec!["--glob", "good"], vec!["--allow-partial"]] {
        let mut args = vec!["modules", "--since", "1d", "--no-cache"];
        args.extend(extra);
        let result = run(repo.path(), &args);
        assert!(result.status.success());
        let text = String::from_utf8(result.stdout).unwrap();
        assert!(text.contains("Module: good") && text.contains("complete"));
        let last = text.lines().rfind(|line| !line.is_empty()).unwrap();
        assert!(last.starts_with("2025-01-01") && last.ends_with("complete"));
        for unwanted in [
            "Grouping:",
            "Modules:",
            "Repository inventory",
            "Outside depth",
            "Other modules",
            "Filters match",
            "Whole modules include",
            "Files = parsed",
            "Symlinks and submodules",
            "No files:",
            "Not measurable:",
            "Partial results:",
            "-- = unavailable",
            "Committed source only;",
        ] {
            assert!(!text.contains(unwanted), "{unwanted}: {text}");
        }
        if args.contains(&"--allow-partial") {
            assert!(text.contains("partial") && text.contains("not measurable"));
            assert!(String::from_utf8_lossy(&result.stderr).contains("bad/a.ts"));
        } else {
            assert!(result.stderr.is_empty());
        }
    }
}

#[test]
fn file_directory_transitions_create_gaps_without_collapsing_deeper_modules() {
    let repo = repository(&[
        ("area", "historical root file"),
        ("area-other/nested/a.ts", "const a = () => 1;"),
    ]);
    fs::remove_file(repo.path().join("area")).unwrap();
    fs::create_dir_all(repo.path().join("area/nested")).unwrap();
    fs::write(repo.path().join("area/nested/a.ts"), "const a = () => 1;").unwrap();
    commit_at(repo.path(), "2025-01-02T12:00:00Z");
    fs::remove_file(repo.path().join("area/nested/a.ts")).unwrap();
    fs::remove_dir(repo.path().join("area/nested")).unwrap();
    fs::remove_dir(repo.path().join("area")).unwrap();
    fs::write(repo.path().join("area"), "root file again").unwrap();
    commit_at(repo.path(), "2025-01-03T12:00:00Z");
    let report = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "2d",
            "--every",
            "1d",
            "--depth",
            "2",
            "--glob",
            "area/nested",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(report["modules"].as_array().unwrap().len(), 1);
    let points = series(&report, "area/nested");
    assert_eq!(points[0]["status"], "no_files");
    assert_eq!(points[0]["coverage"]["tracked_entries"], 0);
    assert_eq!(points[1]["coverage"]["parsed_files"], 1);
    assert_eq!(points[1]["functions"], 1);
    assert_eq!(points[2]["status"], "no_files");
    assert!(points.iter().all(|point| point["change_pp"].is_null()));
    assert_eq!(report["inventory"][0]["outside_depth_entries"], 1);
    assert_eq!(report["inventory"][1]["outside_depth_entries"], 0);
    assert_eq!(report["inventory"][2]["outside_depth_entries"], 1);
    let all = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "2d",
            "--every",
            "1d",
            "--depth",
            "2",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(all["modules"].as_array().unwrap().len(), 2);
}

#[test]
fn unreadable_selected_blobs_are_not_partial_success() {
    let repo = repository(&[("pkg/a.ts", "const a = () => 1;")]);
    let oid = git(repo.path(), &["rev-parse", "HEAD:pkg/a.ts"]);
    fs::remove_file(
        repo.path()
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]),
    )
    .unwrap();
    let output = run(
        repo.path(),
        &["modules", "--since", "1d", "--allow-partial", "--no-cache"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Reading"));
}

#[test]
fn changing_grouping_reuses_blob_cache_and_cache_errors_are_not_partial() {
    let repo = repository(&[
        ("pkg/a/source.ts", "const a = () => 1;"),
        ("pkg/b/source.ts", "const a = () => 1;"),
        ("other/c/source.ts", "const a = () => 1;"),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let base = [
        "modules",
        "--since",
        "1d",
        "--format",
        "json",
        "--cache-dir",
        cache.path().to_str().unwrap(),
    ];
    let mut args = base.to_vec();
    args.extend(["--glob", "pkg"]);
    let whole = successful_json(repo.path(), &args);
    let mut split_args = base.to_vec();
    split_args.extend([
        "--depth", "2", "--glob", "pkg/a", "--glob", "pkg/b", "--glob", "pkg/a",
    ]);
    let split = successful_json(repo.path(), &split_args);
    assert_eq!(whole["metric"], split["metric"]);
    assert_ne!(whole["grouping_fingerprint"], split["grouping_fingerprint"]);
    assert_eq!(split["modules"].as_array().unwrap().len(), 2);
    assert_eq!(split["discovered_modules"], 3);
    let oid = git(repo.path(), &["rev-parse", "HEAD:pkg/a/source.ts"]);
    let record = cache
        .path()
        .join(whole["metric"]["analyzer_fingerprint"].as_str().unwrap())
        .join("raw")
        .join("typescript")
        .join(&oid[..2])
        .join(format!("{oid}.json"));
    let bytes = fs::read(&record).unwrap();
    let cached: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(cached["schema"], 3);
    assert_eq!(cached["oid"], oid);
    assert!(cached.get("grouping").is_none());
    fs::write(record, "{}").unwrap();
    split_args.push("--allow-partial");
    let failed = run(repo.path(), &split_args);
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("Invalid cache"));
    let inside = run(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--allow-partial",
            "--cache-dir",
            repo.path().to_str().unwrap(),
        ],
    );
    assert_eq!(inside.status.code(), Some(1));
    assert!(inside.stdout.is_empty());
    assert!(String::from_utf8_lossy(&inside.stderr).contains("outside the measured repository"));
}

#[cfg(unix)]
#[test]
fn module_inventory_counts_but_never_follows_links_or_submodules() {
    let repo = repository(&[
        ("pkg/a.ts", "const a = () => 1;"),
        ("pkg/deep/a.ts", "const a = () => 1;"),
    ]);
    std::os::unix::fs::symlink("/missing/external.ts", repo.path().join("pkg/link.ts")).unwrap();
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
            "pkg/submodule",
        ],
    );
    git(repo.path(), &["commit", "-qm", "links"]);
    let report = successful_json(
        repo.path(),
        &["modules", "--since", "1d", "--format", "json", "--no-cache"],
    );
    let point = series(&report, "pkg").last().unwrap();
    assert_eq!(point["coverage"]["tracked_entries"], 4);
    assert_eq!(point["coverage"]["non_regular_entries"], 2);
    assert_eq!(point["coverage"]["parsed_files"], 2);
    let deep = successful_json(
        repo.path(),
        &[
            "modules",
            "--since",
            "1d",
            "--depth",
            "2",
            "--format",
            "json",
            "--no-cache",
        ],
    );
    assert_eq!(deep["discovered_modules"], 1);
    assert_eq!(deep["modules"][0]["path"], "pkg/deep");
    assert_eq!(deep["inventory"][1]["outside_depth_entries"], 3);
    assert_eq!(deep["inventory"][1]["selected_module_entries"], 1);
}
