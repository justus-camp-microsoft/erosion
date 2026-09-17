use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::{
    Analyzer, MetricIdentity, Snapshot, SnapshotAccumulator, Status,
    git::{Commit, Repository},
    history::Checkpoint,
};

#[derive(Clone, Serialize)]
pub struct ModuleConfig {
    pub kind: &'static str,
    pub depth: usize,
    pub globs: Vec<String>,
}

pub struct ModuleSelection {
    pub config: ModuleConfig,
    pub fingerprint: String,
    matcher: GlobSet,
}

impl ModuleSelection {
    pub fn new(depth: usize, globs: Vec<String>) -> Result<Self> {
        ensure!(depth > 0, "Module depth must be greater than zero");
        let mut builder = GlobSetBuilder::new();
        for pattern in &globs {
            builder.add(
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .with_context(|| format!("Invalid module glob {pattern:?}"))?,
            );
        }
        let config = ModuleConfig {
            kind: "directories",
            depth,
            globs,
        };
        let fingerprint = format!("{:x}", Sha256::digest(serde_json::to_vec(&config)?));
        Ok(Self {
            config,
            fingerprint,
            matcher: builder.build().context("Compiling module globs")?,
        })
    }

    fn key<'a>(&self, path: &'a str) -> Option<&'a str> {
        path.match_indices('/')
            .nth(self.config.depth - 1)
            .map(|(end, _)| &path[..end])
    }

    fn matches(&self, key: &str) -> bool {
        self.config.globs.is_empty() || self.matcher.is_match(key)
    }
}

#[derive(Serialize)]
pub struct ModuleSeries {
    pub path: String,
    pub snapshots: Vec<Snapshot>,
}

#[derive(Serialize)]
pub struct ModuleInventory {
    pub cutoff_utc: DateTime<Utc>,
    pub commit: Option<Commit>,
    pub tracked_entries: usize,
    pub outside_depth_entries: usize,
    pub unselected_module_entries: usize,
    pub selected_module_entries: usize,
}

#[derive(Serialize)]
pub struct ModuleReport {
    pub schema_version: u32,
    pub tool_version: &'static str,
    pub mode: &'static str,
    pub reference: Commit,
    pub metric: MetricIdentity,
    pub grouping: ModuleConfig,
    pub grouping_fingerprint: String,
    pub file_scope: &'static str,
    pub discovered_modules: usize,
    pub inventory: Vec<ModuleInventory>,
    pub modules: Vec<ModuleSeries>,
}

impl Analyzer {
    pub fn modules_report(
        &mut self,
        repo: &Repository,
        selection: &ModuleSelection,
        reference: Commit,
        checkpoints: &[Checkpoint],
    ) -> Result<ModuleReport> {
        let mut discovered = BTreeSet::new();
        let mut seen_commits = HashSet::new();
        for checkpoint in checkpoints {
            if let Some(commit) = &checkpoint.commit
                && seen_commits.insert(&commit.sha)
            {
                for entry in repo.entries(&commit.sha)? {
                    if let Some(key) = selection.key(&entry.path) {
                        discovered.insert(key.to_owned());
                    }
                }
            }
        }
        let mut modules: Vec<_> = discovered
            .iter()
            .filter(|path| selection.matches(path))
            .map(|path| ModuleSeries {
                path: path.clone(),
                snapshots: Vec::with_capacity(checkpoints.len()),
            })
            .collect();
        ensure!(
            !modules.is_empty(),
            "No discovered directory modules match at depth {} in the sampled commits (globs: {:?}); files are not modules. Adjust --depth, --glob, or the time range; use measure for repository-root files",
            selection.config.depth,
            selection.config.globs
        );
        let selected: BTreeSet<_> = modules.iter().map(|module| module.path.clone()).collect();
        let mut inventory = Vec::with_capacity(checkpoints.len());
        for checkpoint in checkpoints {
            let mut groups = BTreeMap::new();
            let mut counts = ModuleInventory {
                cutoff_utc: checkpoint.cutoff,
                commit: checkpoint.commit.clone(),
                tracked_entries: 0,
                outside_depth_entries: 0,
                unselected_module_entries: 0,
                selected_module_entries: 0,
            };
            if let Some(commit) = &checkpoint.commit {
                let entries = repo.entries(&commit.sha)?;
                counts.tracked_entries = entries.len();
                let mut reader = repo.blobs()?;
                for entry in entries {
                    let Some(key) = selection.key(&entry.path) else {
                        counts.outside_depth_entries += 1;
                        continue;
                    };
                    if !selected.contains(key) {
                        counts.unselected_module_entries += 1;
                        continue;
                    }
                    counts.selected_module_entries += 1;
                    let accumulator = groups
                        .entry(key.to_owned())
                        .or_insert_with(|| SnapshotAccumulator::new(checkpoint));
                    self.accumulate_entry(accumulator, entry, &mut reader, 0)?;
                }
            }
            for module in &mut modules {
                let mut snapshot = groups
                    .remove(&module.path)
                    .unwrap_or_else(|| SnapshotAccumulator::new(checkpoint))
                    .finish(0);
                if snapshot.commit.is_some() && snapshot.coverage.tracked_entries == 0 {
                    snapshot.status = Status::NoFiles;
                }
                snapshot.change_pp = snapshot
                    .erosion_pct
                    .zip(
                        module
                            .snapshots
                            .last()
                            .and_then(|previous| previous.erosion_pct),
                    )
                    .map(|(now, before)| now - before);
                module.snapshots.push(snapshot);
            }
            inventory.push(counts);
        }
        Ok(ModuleReport {
            schema_version: 2,
            tool_version: env!("CARGO_PKG_VERSION"),
            mode: "modules",
            reference,
            metric: MetricIdentity::current(),
            grouping: selection.config.clone(),
            grouping_fingerprint: selection.fingerprint.clone(),
            file_scope: "all_tracked_entries",
            discovered_modules: discovered.len(),
            inventory,
            modules,
        })
    }
}

pub fn require_complete(report: &ModuleReport, allow_partial: bool) -> Result<()> {
    crate::require_complete_snapshots(
        report
            .modules
            .iter()
            .flat_map(|module| module.snapshots.iter()),
        allow_partial,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_uses_only_directory_components() {
        let selection = ModuleSelection::new(2, vec![]).unwrap();
        assert_eq!(
            selection.key("packages/api/src/main.rs"),
            Some("packages/api")
        );
        assert_eq!(selection.key("src/main.rs"), None);
        assert_eq!(selection.key("main.rs"), None);
        assert_eq!(selection.key("packages/api"), None);
        assert_eq!(
            selection.key("packages/module.ts/a.rs"),
            Some("packages/module.ts")
        );
        assert_eq!(
            ModuleSelection::new(1, vec![]).unwrap().key("src/a.rs"),
            Some("src")
        );
        assert_eq!(
            ModuleSelection::new(usize::MAX, vec![])
                .unwrap()
                .key("src/a.rs"),
            None
        );
        assert!(ModuleSelection::new(0, vec![]).is_err());
    }

    #[test]
    fn filters_match_module_keys_and_respect_separators() {
        let selection = ModuleSelection::new(2, vec!["packages/*".into(), "tools".into()]).unwrap();
        assert!(selection.matches("packages/api"));
        assert!(selection.matches("tools"));
        assert!(!selection.matches("packages/api/src"));
        assert!(!selection.matches("packages"));
        assert!(ModuleSelection::new(2, vec!["[".into()]).is_err());
        assert_ne!(
            selection.fingerprint,
            ModuleSelection::new(3, vec![]).unwrap().fingerprint
        );
        let old_config = serde_json::json!({"depth": 2, "globs": ["packages/*", "tools"]});
        assert_ne!(
            selection.fingerprint,
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&old_config).unwrap())
            )
        );
    }
}
