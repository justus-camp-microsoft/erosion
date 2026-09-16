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
    let snapshot = &report["snapshots"][0];
    assert_eq!(snapshot["functions"], 2);
    assert_eq!(snapshot["coverage"]["languages"]["rust"]["parsed_files"], 1);
    assert_eq!(
        snapshot["coverage"]["languages"]["gleam"]["parsed_files"],
        1
    );
    assert_eq!(snapshot["coverage"]["unsupported_files"], 0);
}
