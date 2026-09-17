use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::languages::{Language, PARSER_VERSIONS};
use crate::metrics::{FileAnalysis, METRIC_VERSION};

pub const CACHE_SCHEMA: u32 = 1;
const ANALYZER_SOURCES: &[(&str, &str)] = &[
    ("src/metrics.rs", include_str!("metrics.rs")),
    ("src/languages/mod.rs", include_str!("languages/mod.rs")),
    (
        "src/languages/javascript.rs",
        include_str!("languages/javascript.rs"),
    ),
    ("src/languages/gleam.rs", include_str!("languages/gleam.rs")),
    (
        "src/languages/python.rs",
        include_str!("languages/python.rs"),
    ),
    ("src/languages/rust.rs", include_str!("languages/rust.rs")),
    ("src/languages/tree.rs", include_str!("languages/tree.rs")),
];

pub fn analyzer_fingerprint() -> String {
    let mut hash = Sha256::new();
    hash.update(METRIC_VERSION);
    hash.update(PARSER_VERSIONS);
    hash.update(tree_sitter_typescript::SOURCE_FINGERPRINT);
    hash.update(CACHE_SCHEMA.to_le_bytes());
    for (path, source) in ANALYZER_SOURCES {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update(source.as_bytes());
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    analyzer: String,
    oid: String,
    language: Language,
    analysis_sha256: String,
    analysis: FileAnalysis,
}

pub struct Cache {
    directory: Option<PathBuf>,
    fingerprint: String,
    memory: HashMap<(Language, String), Arc<FileAnalysis>>,
    pub memory_hits: usize,
    pub disk_hits: usize,
    pub misses: usize,
}

// Resolve existing ancestors as well as '..', so a cache cannot accidentally
// be directed into the measured worktree through a symlink or relative path.
fn resolved_destination(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut result = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => {
                result.push(other.as_os_str());
                match result.canonicalize() {
                    Ok(resolved) => result = resolved,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error).context("Resolving cache directory"),
                }
            }
        }
    }
    Ok(result)
}

impl Cache {
    pub fn new(directory: Option<&Path>, disabled: bool, repository: &Path) -> Result<Self> {
        let directory = if disabled {
            None
        } else {
            let requested = directory
                .map(Path::to_path_buf)
                .or_else(|| dirs::cache_dir().map(|path| path.join("erosion")))
                .context("No user cache directory available; use --cache-dir or --no-cache")?;
            let resolved = resolved_destination(&requested)?;
            ensure!(
                !resolved.starts_with(repository.canonicalize()?),
                "Cache must be outside the measured repository; use --cache-dir or --no-cache"
            );
            Some(resolved)
        };
        Ok(Self {
            directory,
            fingerprint: analyzer_fingerprint(),
            memory: HashMap::new(),
            memory_hits: 0,
            disk_hits: 0,
            misses: 0,
        })
    }

    fn path(&self, language: Language, oid: &str) -> Result<Option<PathBuf>> {
        ensure!(
            (oid.len() == 40 || oid.len() == 64)
                && oid.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid blob object ID"
        );
        Ok(self.directory.as_ref().map(|directory| {
            directory
                .join(&self.fingerprint)
                .join(language.id())
                .join(&oid[..2])
                .join(format!("{oid}.json"))
        }))
    }

    pub fn get(&mut self, language: Language, oid: &str) -> Result<Option<Arc<FileAnalysis>>> {
        let key = (language, oid.to_owned());
        if let Some(analysis) = self.memory.get(&key) {
            self.memory_hits += 1;
            return Ok(Some(Arc::clone(analysis)));
        }
        if let Some(path) = self.path(language, oid)? {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let record: Record = serde_json::from_slice(&bytes).with_context(|| {
                        format!(
                            "Invalid cache {}; remove it or use --no-cache",
                            path.display()
                        )
                    })?;
                    ensure!(
                        record.schema == CACHE_SCHEMA
                            && record.analyzer == self.fingerprint
                            && record.oid == oid
                            && record.language == language,
                        "Cache identity mismatch at {}; remove it or use --no-cache",
                        path.display()
                    );
                    ensure!(
                        record.analysis_sha256
                            == format!(
                                "{:x}",
                                Sha256::digest(serde_json::to_vec(&record.analysis)?)
                            ),
                        "Cache payload checksum mismatch at {}; remove it or use --no-cache",
                        path.display()
                    );
                    let analysis = Arc::new(record.analysis);
                    self.memory.insert(key, Arc::clone(&analysis));
                    self.disk_hits += 1;
                    return Ok(Some(analysis));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("Reading cache {}", path.display()));
                }
            }
        }
        self.misses += 1;
        Ok(None)
    }

    pub fn insert(
        &mut self,
        language: Language,
        oid: &str,
        analysis: FileAnalysis,
    ) -> Result<Arc<FileAnalysis>> {
        if let Some(path) = self.path(language, oid)? {
            let parent = path.parent().context("Missing cache parent")?;
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Creating cache {}", parent.display()))?;
            let record = Record {
                schema: CACHE_SCHEMA,
                analyzer: self.fingerprint.clone(),
                oid: oid.to_owned(),
                language,
                analysis: analysis.clone(),
                analysis_sha256: format!("{:x}", Sha256::digest(serde_json::to_vec(&analysis)?)),
            };
            let mut temporary =
                tempfile::NamedTempFile::new_in(parent).context("Creating atomic cache file")?;
            serde_json::to_writer(temporary.as_file_mut(), &record)?;
            temporary.flush()?;
            // Cache entries are reconstructible. Atomic publication and payload
            // checksums suffice; a device flush per blob makes large cold runs
            // much slower, especially on macOS.
            temporary
                .persist(&path)
                .with_context(|| format!("Persisting cache {}", path.display()))?;
        }
        let analysis = Arc::new(analysis);
        self.memory
            .insert((language, oid.to_owned()), Arc::clone(&analysis));
        Ok(analysis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_and_corrupt_cache_are_explicit() {
        let repo = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let oid = "a".repeat(40);
        let data = FileAnalysis {
            physical_lines: 1,
            source_lines: 0,
            functions: vec![],
            diagnostics: vec![crate::metrics::ParseDiagnostic {
                line: 1,
                column: 1,
                message: "test".into(),
            }],
        };
        let mut cache = Cache::new(Some(dir.path()), false, repo.path()).unwrap();
        assert!(cache.get(Language::TypeScript, &oid).unwrap().is_none());
        cache
            .insert(Language::TypeScript, &oid, data.clone())
            .unwrap();
        let path = cache.path(Language::TypeScript, &oid).unwrap().unwrap();
        let mut warm = Cache::new(Some(dir.path()), false, repo.path()).unwrap();
        assert_eq!(
            *warm.get(Language::TypeScript, &oid).unwrap().unwrap(),
            data
        );
        assert_eq!(warm.disk_hits, 1);
        let mut corrupted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        corrupted["analysis"]["physical_lines"] = 100.into();
        std::fs::write(&path, serde_json::to_vec(&corrupted).unwrap()).unwrap();
        assert!(
            Cache::new(Some(dir.path()), false, repo.path())
                .unwrap()
                .get(Language::TypeScript, &oid)
                .is_err()
        );
        std::fs::write(&path, b"{").unwrap();
        assert!(
            Cache::new(Some(dir.path()), false, repo.path())
                .unwrap()
                .get(Language::TypeScript, &oid)
                .is_err()
        );
        assert!(Cache::new(Some(repo.path()), false, repo.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn cache_inside_worktree_via_symlink_is_rejected() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = outside.path().join("alias");
        std::os::unix::fs::symlink(repo.path(), &link).unwrap();
        assert!(Cache::new(Some(&link.join("cache")), false, repo.path()).is_err());
        assert!(!repo.path().join("cache").exists());
    }

    #[test]
    fn typescript_patch_matches_the_locked_git_revision() {
        let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
        let patch = &manifest["patch"]["crates-io"]["tree-sitter-typescript"];
        let git = patch["git"].as_str().unwrap();
        let revision = patch["rev"].as_str().unwrap();
        assert_eq!(
            git,
            "https://github.com/justus-camp-microsoft/tree-sitter-typescript"
        );
        assert_eq!(revision.len(), 40);
        assert!(
            revision
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        for moving_or_local_source in ["branch", "tag", "path"] {
            assert!(patch.get(moving_or_local_source).is_none());
        }
        let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
        let packages: Vec<_> = lock["package"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|package| package["name"].as_str() == Some("tree-sitter-typescript"))
            .collect();
        assert_eq!(packages.len(), 1);
        assert_eq!(
            packages[0]["source"].as_str().unwrap(),
            format!("git+{git}?rev={revision}#{revision}")
        );
    }

    #[test]
    fn fingerprint_covers_analyzer_sources_and_pinned_grammars() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut expected = vec!["src/metrics.rs".to_owned()];
        for entry in std::fs::read_dir(root.join("src/languages")).unwrap() {
            let path = entry.unwrap().path();
            assert!(
                path.is_file(),
                "New analyzer directory must be added to fingerprint"
            );
            if path.extension().is_some_and(|ext| ext == "rs") {
                expected.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .replace('\\', "/"),
                );
            }
        }
        expected.sort();
        let mut actual: Vec<_> = ANALYZER_SOURCES
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        actual.sort();
        assert_eq!(actual, expected);
        let lock: toml::Value = toml::from_str(include_str!("../Cargo.lock")).unwrap();
        for (name, version) in [
            ("tree-sitter", "0.25.2"),
            ("tree-sitter-javascript", "0.25.0"),
            ("tree-sitter-typescript", "0.23.2-erosion.1"),
            ("tree-sitter-python", "0.25.0"),
        ] {
            let packages = lock["package"].as_array().unwrap();
            assert!(packages.iter().any(
                |p| p["name"].as_str() == Some(name) && p["version"].as_str() == Some(version)
            ));
        }
        assert_eq!(tree_sitter_typescript::SOURCE_FINGERPRINT.len(), 64);
        assert!(
            tree_sitter_typescript::SOURCE_FINGERPRINT
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
    }
}
