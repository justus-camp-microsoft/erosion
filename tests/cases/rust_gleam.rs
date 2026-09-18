use super::{repository, run, successful_json};

#[test]
fn rust_and_gleam_route_into_mixed_reports() {
    let repo = repository(&[
        (
            "src/main.rs",
            "fn main() {\n    if ready() { work(); }\n}\n",
        ),
        (
            "src/main.gleam",
            "pub fn main() {\n  case True {\n    True -> 1\n    False -> 0\n  }\n}\n",
        ),
        (
            "src/main.ts",
            "export type * from './types';\ninterface Box<out T> { readonly value: T; }\nconst selected = items.filter((item: Item) => item.value < -1,);\n",
        ),
    ]);
    let report = successful_json(
        repo.path(),
        &["measure", "--no-cache", "--format", "json", "--top", "5"],
    );
    assert_eq!(report["metric"]["version"], "erosion-v3");
    assert!(
        report["metric"]["parser_versions"]
            .as_str()
            .unwrap()
            .contains("rust=0.24.2-erosion.1;gleam=git-cefbd686")
    );
    assert!(
        report["metric"]["parser_versions"]
            .as_str()
            .unwrap()
            .contains("typescript=0.23.2-erosion.2")
    );
    let snapshot = &report["snapshots"][0];
    assert_eq!(snapshot["status"], "complete");
    assert_eq!(snapshot["functions"], 3);
    assert_eq!(snapshot["coverage"]["failed_files"], 0);
    assert_eq!(
        snapshot["coverage"]["languages"]["typescript"]["parsed_files"],
        1
    );
    assert_eq!(snapshot["coverage"]["languages"]["rust"]["parsed_files"], 1);
    assert_eq!(
        snapshot["coverage"]["languages"]["gleam"]["parsed_files"],
        1
    );
    assert_eq!(snapshot["coverage"]["unsupported_files"], 0);
}

#[test]
fn modern_rust_grammar_routes_all_commands_and_preserves_test_regions() {
    let repo = repository(&[
        (
            "src/dynamic.rs",
            "type Object = Box<dyn 'static + Send>;\nfn choose(x: bool) -> bool {\n if x { return true; }\n false\n}\n",
        ),
        (
            "src/pattern.rs",
            "fn unpack(s: Pair) -> i32 {\n let Pair {\n  #[cfg(test)]\n  a: _,\n  b,\n  ..\n } = s;\n b\n}\n",
        ),
        (
            "src/externs.rs",
            "unsafe extern \"C\" { pub safe static ITEM: u8; pub safe fn value() -> u8; }\n",
        ),
        (
            "src/generic.rs",
            "fn unwrap(w: Wrapper<u8>) -> u8 {\n let Wrapper::<u8> { value } = w;\n value\n}\n",
        ),
        (
            "src/macros.rs",
            "macro_rules! str { ($value:literal) => { $value }; }\nfn render() -> &'static str {\n str!(\"value\")\n}\n",
        ),
    ]);
    let cache = tempfile::tempdir().unwrap();
    for command in [
        vec!["measure"],
        vec!["history", "--since", "1d"],
        vec!["delta", "HEAD", "HEAD"],
        vec!["modules", "--since", "1d"],
    ] {
        for exclude_tests in [false, true] {
            let mut args: Vec<&str> = command.clone();
            args.extend([
                "--format",
                "json",
                "--cache-dir",
                cache.path().to_str().unwrap(),
            ]);
            if exclude_tests {
                args.push("--exclude-tests");
            }
            let first = run(repo.path(), &args);
            assert!(
                first.status.success(),
                "{}",
                String::from_utf8_lossy(&first.stderr)
            );
            assert!(first.stderr.is_empty());
            let warm = run(repo.path(), &args);
            assert!(warm.status.success());
            assert_eq!(first.stdout, warm.stdout);
            let report: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
            let snapshots = if command[0] == "modules" {
                report["modules"][0]["snapshots"].as_array().unwrap()
            } else {
                report["snapshots"].as_array().unwrap()
            };
            for point in snapshots.iter().filter(|point| !point["commit"].is_null()) {
                assert_eq!(point["status"], "complete");
                assert_eq!(point["coverage"]["parsed_files"], 5);
                assert_eq!(point["coverage"]["failed_files"], 0);
                assert_eq!(point["functions"], 4);
                assert_eq!(point["erosion_pct"], 0.0);
                if exclude_tests {
                    assert_eq!(point["coverage"]["test_excluded_entries"], 0);
                    assert_eq!(point["coverage"]["syntax_test_files"], 1);
                    assert_eq!(point["coverage"]["test_excluded_functions"], 0);
                    assert_eq!(point["coverage"]["test_excluded_source_lines"], 2);
                }
            }
        }
    }
}

#[test]
fn malformed_rust_neighbors_are_not_hidden_by_test_exclusion_or_cache() {
    let repo = repository(&[
        ("src/bad-dynamic.rs", "type Object = Box<dyn >;"),
        (
            "src/bad-pattern.rs",
            "fn unpack(s: Pair) { let Pair { #[cfg(test)] : } = s; }",
        ),
        ("src/bad-macro.rs", "fn render() { str!( }"),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let args = [
        "measure",
        "--exclude-tests",
        "--format",
        "json",
        "--cache-dir",
        cache.path().to_str().unwrap(),
    ];
    for _ in 0..2 {
        let strict = run(repo.path(), &args);
        assert_eq!(strict.status.code(), Some(1));
        assert!(strict.stdout.is_empty());
        let stderr = String::from_utf8(strict.stderr).unwrap();
        for path in ["bad-dynamic.rs", "bad-pattern.rs", "bad-macro.rs"] {
            assert!(stderr.contains(path), "{stderr}");
        }
    }
    let mut partial_args = args.to_vec();
    partial_args.push("--allow-partial");
    let report = successful_json(repo.path(), &partial_args);
    assert_eq!(report["snapshots"][0]["status"], "partial");
    assert_eq!(report["snapshots"][0]["coverage"]["failed_files"], 3);
    assert_eq!(report["snapshots"][0]["coverage"]["syntax_test_files"], 0);
    assert_eq!(report["snapshots"][0]["functions"], 0);
    assert!(report["snapshots"][0]["erosion_pct"].is_null());
}
