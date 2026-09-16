use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_include")]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

fn default_include() -> Vec<String> {
    vec!["**".to_owned()]
}

impl Default for Config {
    fn default() -> Self {
        Self {
            include: default_include(),
            exclude: Vec::new(),
        }
    }
}

pub struct Scope {
    pub config: Config,
    pub config_path: Option<PathBuf>,
    pub fingerprint: String,
    include: GlobSet,
    exclude: GlobSet,
}

impl Scope {
    pub fn load(root: &Path, explicit: Option<&Path>) -> Result<Self> {
        let path = explicit
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join("erosion.toml"));
        let (config, config_path) = match std::fs::read_to_string(&path) {
            Ok(text) => (
                toml::from_str(&text)
                    .with_context(|| format!("Invalid config {}", path.display()))?,
                Some(path),
            ),
            Err(error) if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
                (Config::default(), None)
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Reading config {}", path.display()));
            }
        };
        Self::build(config, config_path)
    }

    fn build(config: Config, config_path: Option<PathBuf>) -> Result<Self> {
        fn compile(patterns: &[String]) -> Result<GlobSet> {
            let mut builder = GlobSetBuilder::new();
            for pattern in patterns {
                builder.add(
                    Glob::new(pattern)
                        .with_context(|| format!("Invalid scope glob {pattern:?}"))?,
                );
            }
            builder.build().context("Compiling scope globs")
        }
        let include = compile(&config.include)?;
        let exclude = compile(&config.exclude)?;
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&config)?));
        Ok(Self {
            config,
            config_path,
            fingerprint,
            include,
            exclude,
        })
    }

    pub fn contains(&self, path: &str) -> bool {
        self.include.is_match(path) && !self.exclude.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_scope_no_hidden_exclusions() {
        let scope = Scope::build(Config::default(), None).unwrap();
        assert!(scope.contains("node_modules/generated.test.ts"));
        let scope = Scope::build(
            Config {
                include: vec!["src/**".into()],
                exclude: vec!["**/*.test.*".into()],
            },
            None,
        )
        .unwrap();
        assert!(scope.contains("src/nested/a.ts"));
        assert!(!scope.contains("src/a.test.ts"));
        assert!(!scope.contains("other/a.ts"));
        assert!(
            Scope::build(
                Config {
                    include: vec!["[".into()],
                    exclude: vec![]
                },
                None
            )
            .is_err()
        );
        assert!(toml::from_str::<Config>("incldue = []").is_err());
    }

    #[test]
    fn missing_explicit_config_fails() {
        let root = tempfile::tempdir().unwrap();
        assert!(Scope::load(root.path(), None).is_ok());
        assert!(Scope::load(root.path(), Some(&root.path().join("missing"))).is_err());
    }
}
