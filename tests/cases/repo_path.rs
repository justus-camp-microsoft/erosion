use std::{fs, path::Path};

use super::{git, repository, run, successful_json};

#[test]
fn all_commands_accept_absolute_relative_and_nested_repository_paths() {
    let repo = repository(&[("nested space/a.py", "def f(): return 1\n")]);
    fs::write(
        repo.path().join("nested space/a.py"),
        format!("def f(x):\n{}", "    if x: work()\n".repeat(10)),
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-qm", "more complexity"]);
    fs::write(repo.path().join("nested space/a.py"), "def dirty(:\n").unwrap();
    let status = git(repo.path(), &["status", "--porcelain=v1", "-uall"]);
    let index = fs::read(repo.path().join(".git/index")).unwrap();
    let refs = git(repo.path(), &["show-ref"]);
    let config = fs::read(repo.path().join(".git/config")).unwrap();
    let outside = tempfile::tempdir().unwrap();
    assert_eq!(outside.path().parent(), repo.path().parent());
    let relative = Path::new("..").join(repo.path().file_name().unwrap());
    let nested = repo.path().join("nested space");
    for args in [
        vec![
            "measure",
            "--ref",
            "HEAD~1",
            "--no-cache",
            "--format",
            "json",
        ],
        vec![
            "history",
            "--since",
            "1d",
            "--every",
            "1d",
            "--no-cache",
            "--format",
            "json",
        ],
        vec!["delta", "HEAD~1", "HEAD", "--no-cache", "--format", "json"],
    ] {
        let expected = run(repo.path(), &args);
        assert!(expected.status.success());
        for path in [repo.path(), relative.as_path(), nested.as_path()] {
            let mut explicit = args.clone();
            explicit.extend(["--repo-path", path.to_str().unwrap()]);
            let result = run(outside.path(), &explicit);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(expected.stdout, result.stdout);
        }
    }
    assert_eq!(
        status,
        git(repo.path(), &["status", "--porcelain=v1", "-uall"])
    );
    assert_eq!(index, fs::read(repo.path().join(".git/index")).unwrap());
    assert_eq!(refs, git(repo.path(), &["show-ref"]));
    assert_eq!(config, fs::read(repo.path().join(".git/config")).unwrap());
    assert_eq!(
        fs::read_to_string(repo.path().join("nested space/a.py")).unwrap(),
        "def dirty(:\n"
    );
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn scope_and_cache_paths_stay_relative_to_caller_not_selected_repository() {
    let repo = repository(&[
        ("a.py", "def f(): return 1\n"),
        ("bad.js", "function {"),
        ("erosion.toml", "include = ['**/*.py']\n"),
        ("scope.toml", "include = []\n"),
    ]);
    let caller = repository(&[
        ("other.js", "function other() {}\n"),
        ("erosion.toml", "invalid config\n"),
        ("scope.toml", "include = ['**/*.py']\n"),
    ]);
    let head = git(repo.path(), &["rev-parse", "HEAD"]);
    let implicit = successful_json(
        caller.path(),
        &[
            "measure",
            "--repo-path",
            repo.path().to_str().unwrap(),
            "--no-cache",
            "--format",
            "json",
        ],
    );
    assert_eq!(implicit["reference"]["sha"], head);
    assert_eq!(implicit["scope"]["include"][0], "**/*.py");
    assert_eq!(implicit["snapshots"][0]["functions"], 1);
    let args = [
        "measure",
        "--repo-path",
        repo.path().to_str().unwrap(),
        "--config",
        "scope.toml",
        "--cache-dir",
        "analysis-cache",
        "--verbose",
        "--format",
        "json",
    ];
    let cold = successful_json(caller.path(), &args);
    let warm = run(caller.path(), &args);
    assert!(warm.status.success());
    assert!(String::from_utf8_lossy(&warm.stderr).contains("1 disk hits"));
    assert_eq!(cold, implicit);
    assert_eq!(
        cold,
        serde_json::from_slice::<serde_json::Value>(&warm.stdout).unwrap()
    );
    assert!(caller.path().join("analysis-cache").is_dir());
    assert!(!repo.path().join("analysis-cache").exists());
    let fingerprint = cold["metric"]["analyzer_fingerprint"].as_str().unwrap();
    let oid = git(repo.path(), &["rev-parse", "HEAD:a.py"]);
    assert!(
        caller
            .path()
            .join("analysis-cache")
            .join(fingerprint)
            .join("python")
            .join(&oid[..2])
            .join(format!("{oid}.json"))
            .is_file()
    );
    let missing = run(
        caller.path(),
        &[
            "measure",
            "--repo-path",
            repo.path().to_str().unwrap(),
            "--config",
            "missing.toml",
            "--no-cache",
        ],
    );
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Reading config missing.toml"));
}

#[test]
fn invalid_repo_paths_never_fall_back_or_create_cache() {
    let caller = repository(&[("a.py", "def f(): return 1\n")]);
    let outside = tempfile::tempdir().unwrap();
    let missing = outside.path().join("missing");
    let file = caller.path().join("a.py");
    let cache = outside.path().join("cache");
    for command in [
        vec!["measure"],
        vec!["history", "--since", "1d"],
        vec!["delta", "HEAD", "HEAD"],
    ] {
        for path in [missing.as_path(), outside.path(), file.as_path()] {
            let mut args = command.clone();
            args.extend([
                "--repo-path",
                path.to_str().unwrap(),
                "--cache-dir",
                cache.to_str().unwrap(),
            ]);
            let result = run(caller.path(), &args);
            assert_eq!(result.status.code(), Some(1), "{args:?}");
            assert!(result.stdout.is_empty());
            assert!(!cache.exists());
        }
    }
    let missing_value = run(caller.path(), &["measure", "--repo-path"]);
    assert_eq!(missing_value.status.code(), Some(2));
    assert!(missing_value.stdout.is_empty());
    let empty_value = run(
        caller.path(),
        &[
            "measure",
            "--repo-path",
            "",
            "--cache-dir",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(empty_value.status.code(), Some(2));
    assert!(empty_value.stdout.is_empty());
    assert!(!cache.exists());
}

#[test]
fn cache_must_remain_outside_explicitly_selected_worktree() {
    let repo = repository(&[("a.py", "def f(): return 1\n")]);
    let caller = tempfile::tempdir().unwrap();
    let cache = repo.path().join("forbidden-cache");
    let result = run(
        caller.path(),
        &[
            "measure",
            "--repo-path",
            repo.path().to_str().unwrap(),
            "--cache-dir",
            cache.to_str().unwrap(),
        ],
    );
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("Cache must be outside"));
    assert!(!cache.exists());

    #[cfg(unix)]
    {
        let alias = caller.path().join("repository-link");
        std::os::unix::fs::symlink(repo.path(), &alias).unwrap();
        let report = successful_json(
            caller.path(),
            &[
                "measure",
                "--repo-path",
                "repository-link",
                "--no-cache",
                "--format",
                "json",
            ],
        );
        assert_eq!(
            report["reference"]["sha"],
            git(repo.path(), &["rev-parse", "HEAD"])
        );
        let result = run(
            caller.path(),
            &[
                "measure",
                "--repo-path",
                "repository-link",
                "--cache-dir",
                "repository-link/forbidden-cache",
            ],
        );
        assert_eq!(result.status.code(), Some(1));
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr).contains("Cache must be outside"));
        assert!(!cache.exists());
    }
}
