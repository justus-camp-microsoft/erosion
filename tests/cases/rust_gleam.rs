use super::{repository, successful_json};

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
            .contains("rust=0.24.2;gleam=git-cefbd686")
    );
    assert!(
        report["metric"]["parser_versions"]
            .as_str()
            .unwrap()
            .contains("typescript=0.23.2-erosion.1")
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
