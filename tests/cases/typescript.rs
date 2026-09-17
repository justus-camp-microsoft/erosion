use std::fs;

use super::{git, repository, run, successful_json};

const SOURCE: &str = "\
export type * from './types';\n\
export type * as api from './types';\n\
export interface Box<out T> { readonly value: T; }\n\
export interface Cell<in out T> { get(): T; set(value: T): void; }\n\
export interface Named<out> { value: out; }\n\
export type Callback = (any) => any;\n\
export interface Events { readonly changed: (readonly?: boolean) => void; }\n\
export function choose(x: boolean) {\n\
  if (x && ready()) return 1;\n\
  return x ? 2 : 3;\n\
}\n\
const selected = items.filter((item: Item) => item.value < -1,);\n";

#[test]
fn modern_typescript_routes_all_extensions_and_reuses_cache() {
    let repo = repository(&[
        ("api.ts", SOURCE),
        ("view.tsx", SOURCE),
        ("esm.mts", SOURCE),
        ("common.cts", SOURCE),
        ("types.d.cts", "export type * from './types';\n"),
    ]);
    let cache = tempfile::tempdir().unwrap();
    let args = [
        "measure",
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
    let uncached = run(repo.path(), &["measure", "--format", "json", "--no-cache"]);
    assert!(warm.status.success() && uncached.status.success());
    assert_eq!(cold.stdout, warm.stdout);
    assert_eq!(cold.stdout, uncached.stdout);
    let report: serde_json::Value = serde_json::from_slice(&cold.stdout).unwrap();
    let snapshot = &report["snapshots"][0];
    assert_eq!(snapshot["status"], "complete");
    assert_eq!(snapshot["coverage"]["parsed_files"], 5);
    assert_eq!(snapshot["coverage"]["failed_files"], 0);
    assert_eq!(
        snapshot["coverage"]["languages"]["typescript"]["parsed_files"],
        4
    );
    assert_eq!(snapshot["coverage"]["languages"]["tsx"]["parsed_files"], 1);
    assert_eq!(snapshot["functions"], 8);
    assert_eq!(snapshot["erosion_pct"], 0.0);
    let expected_mass = 4.0 * (4.0 * 3.0_f64.sqrt() + 1.0);
    assert!((snapshot["total_mass"].as_f64().unwrap() - expected_mass).abs() < 1e-12);
    fs::write(
        repo.path().join("api.ts"),
        format!("{SOURCE}\nconst extra = () => 1;\n"),
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "new callable"]);
    let delta = successful_json(
        repo.path(),
        &[
            "delta",
            "HEAD~1",
            "HEAD",
            "--format",
            "json",
            "--cache-dir",
            cache.path().to_str().unwrap(),
        ],
    );
    assert_eq!(delta["snapshots"][0]["functions"], 8);
    assert_eq!(delta["snapshots"][1]["functions"], 9);
    assert_eq!(delta["snapshots"][1]["change_pp"], 0.0);
}

#[test]
fn malformed_typescript_is_still_a_strict_failure() {
    let repo = repository(&[
        ("good.ts", SOURCE),
        ("bad.ts", "export type * from ;\n"),
        ("bad.tsx", "interface Broken<out T> { value: }\n"),
        (
            "bad-arrow.ts",
            "const selected = items.filter((item: Item) => item.value < ,);\n",
        ),
    ]);
    let strict = run(repo.path(), &["measure", "--no-cache", "--format", "json"]);
    assert_eq!(strict.status.code(), Some(1));
    assert!(strict.stdout.is_empty());
    let stderr = String::from_utf8(strict.stderr).unwrap();
    for path in ["bad.ts", "bad.tsx", "bad-arrow.ts"] {
        assert!(stderr.contains(path), "{stderr}");
    }
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
    assert_eq!(partial["snapshots"][0]["status"], "partial");
    assert_eq!(partial["snapshots"][0]["coverage"]["failed_files"], 3);
    assert_eq!(partial["snapshots"][0]["coverage"]["parsed_files"], 1);
}
