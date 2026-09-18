pub mod cache;
pub mod config;
pub mod git;
pub mod history;
pub mod languages;
pub mod metrics;
pub mod modules;
pub mod output;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use cache::Cache;
use config::{Config, Scope};
use git::{BlobReader, Commit, Repository, TreeEntry};
use history::Checkpoint;
use languages::{Language, LanguageAdapter, TestExclusionPolicy};
use metrics::{
    COMPLEXITY_THRESHOLD, FunctionMetrics, METRIC_VERSION, ParseDiagnostic, Sum,
    TestExclusionAnalysis,
};

#[derive(Default, Debug, Serialize)]
pub struct LanguageCoverage {
    selected_files: usize,
    parsed_files: usize,
    failed_files: usize,
}

#[derive(Default, Debug, Serialize)]
pub struct Coverage {
    pub tracked_entries: usize,
    pub excluded_entries: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_excluded_entries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syntax_test_files: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_excluded_functions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_excluded_source_lines: Option<usize>,
    pub non_regular_entries: usize,
    pub unsupported_files: usize,
    pub unsupported_extensions: BTreeMap<String, usize>,
    pub selected_files: usize,
    pub parsed_files: usize,
    pub failed_files: usize,
    pub parsed_physical_lines: usize,
    pub failed_physical_lines: usize,
    pub parsed_source_lines: usize,
    pub languages: BTreeMap<String, LanguageCoverage>,
}

#[derive(Debug, Serialize)]
pub struct FailedFile {
    pub path: String,
    pub language: Language,
    pub physical_lines: usize,
    pub diagnostics: Vec<ParseDiagnostic>,
}

#[derive(Debug, Serialize)]
pub struct TopFunction {
    pub path: String,
    pub language: Language,
    #[serde(flatten)]
    pub function: FunctionMetrics,
    pub mass: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Complete,
    Partial,
    NotMeasurable,
    NoCommit,
    NoFiles,
}

impl Status {
    pub fn id(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::NotMeasurable => "not_measurable",
            Self::NoCommit => "no_commit",
            Self::NoFiles => "no_files",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub cutoff_utc: DateTime<Utc>,
    pub commit: Option<Commit>,
    pub status: Status,
    pub coverage: Coverage,
    pub functions: usize,
    pub complex_functions: usize,
    pub total_mass: f64,
    pub complex_mass: f64,
    pub erosion_pct: Option<f64>,
    pub change_pp: Option<f64>,
    pub failures: Vec<FailedFile>,
    pub top_functions: Vec<TopFunction>,
}

impl Snapshot {
    fn empty(checkpoint: &Checkpoint) -> Self {
        Self {
            cutoff_utc: checkpoint.cutoff,
            commit: checkpoint.commit.clone(),
            status: if checkpoint.commit.is_none() {
                Status::NoCommit
            } else {
                Status::NotMeasurable
            },
            coverage: Coverage::default(),
            functions: 0,
            complex_functions: 0,
            total_mass: 0.0,
            complex_mass: 0.0,
            erosion_pct: None,
            change_pp: None,
            failures: Vec::new(),
            top_functions: Vec::new(),
        }
    }
}

struct SnapshotAccumulator {
    snapshot: Snapshot,
    total: Sum,
    complex: Sum,
}

impl SnapshotAccumulator {
    fn new(checkpoint: &Checkpoint, exclude_tests: bool) -> Self {
        let mut snapshot = Snapshot::empty(checkpoint);
        snapshot.coverage.test_excluded_entries = exclude_tests.then_some(0);
        snapshot.coverage.syntax_test_files = exclude_tests.then_some(0);
        snapshot.coverage.test_excluded_functions = exclude_tests.then_some(0);
        snapshot.coverage.test_excluded_source_lines = exclude_tests.then_some(0);
        Self {
            snapshot,
            total: Sum::default(),
            complex: Sum::default(),
        }
    }

    fn finish(mut self, top: usize) -> Snapshot {
        let snapshot = &mut self.snapshot;
        snapshot.total_mass = self.total.total();
        snapshot.complex_mass = self.complex.total();
        snapshot.erosion_pct = (snapshot.total_mass > 0.0)
            .then(|| 100.0 * snapshot.complex_mass / snapshot.total_mass);
        if snapshot.commit.is_some() {
            snapshot.status = if snapshot.coverage.failed_files > 0 {
                Status::Partial
            } else if snapshot.erosion_pct.is_none() {
                Status::NotMeasurable
            } else {
                Status::Complete
            };
        }
        snapshot.top_functions.sort_by(|a, b| {
            b.mass
                .total_cmp(&a.mass)
                .then_with(|| a.path.cmp(&b.path))
                .then(a.function.start_line.cmp(&b.function.start_line))
                .then(a.function.end_line.cmp(&b.function.end_line))
        });
        snapshot.top_functions.truncate(top);
        self.snapshot
    }
}

#[derive(Serialize)]
pub struct MetricIdentity {
    pub name: &'static str,
    pub version: &'static str,
    pub analyzer_fingerprint: String,
    pub parser_versions: &'static str,
    pub complexity_threshold: u64,
    pub formula: &'static str,
}

impl MetricIdentity {
    fn current() -> Self {
        Self {
            name: "erosion",
            version: METRIC_VERSION,
            analyzer_fingerprint: cache::analyzer_fingerprint(),
            parser_versions: languages::PARSER_VERSIONS,
            complexity_threshold: COMPLEXITY_THRESHOLD,
            formula: "100 * sum(CC * sqrt(SLOC) where CC > 10) / sum(CC * sqrt(SLOC))",
        }
    }
}

#[derive(Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool_version: &'static str,
    pub mode: &'static str,
    pub reference: Commit,
    pub metric: MetricIdentity,
    pub scope: Config,
    pub scope_fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_exclusion: Option<TestExclusionPolicy>,
    pub snapshots: Vec<Snapshot>,
}

pub struct Analyzer {
    pub cache: Cache,
    adapters: HashMap<Language, Box<dyn LanguageAdapter>>,
    test_exclusion: Option<TestExclusionPolicy>,
    test_fingerprints: HashMap<Language, String>,
}

impl Analyzer {
    pub fn new(cache: Cache) -> Self {
        Self {
            cache,
            adapters: HashMap::new(),
            test_exclusion: None,
            test_fingerprints: HashMap::new(),
        }
    }

    pub fn exclude_tests(mut self) -> Result<Self> {
        let mut policies = BTreeMap::new();
        for language in Language::ALL {
            let adapter = languages::adapter(language, true)?;
            let policy = adapter
                .test_policy()
                .context("Missing language test policy")?;
            self.test_fingerprints
                .insert(language, policy.fingerprint()?);
            policies.insert(language.id().to_owned(), policy);
            self.adapters.insert(language, adapter);
        }
        self.test_exclusion = Some(TestExclusionPolicy {
            version: "language-tests-v1",
            languages: policies,
        });
        Ok(self)
    }

    fn scope_fingerprint(&self, fingerprint: &str) -> Result<String> {
        self.test_exclusion.as_ref().map_or_else(
            || Ok(fingerprint.to_owned()),
            |exclusion| {
                Ok(format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&(fingerprint, exclusion))?)
                ))
            },
        )
    }

    pub fn snapshot(
        &mut self,
        repo: &Repository,
        scope: &Scope,
        checkpoint: &Checkpoint,
        top: usize,
    ) -> Result<Snapshot> {
        let mut accumulator = SnapshotAccumulator::new(checkpoint, self.test_exclusion.is_some());
        let Some(commit) = &checkpoint.commit else {
            return Ok(accumulator.finish(top));
        };
        let entries = repo.entries(&commit.sha)?;
        let mut reader = repo.blobs()?;
        for entry in entries {
            if !scope.contains(&entry.path) {
                accumulator.snapshot.coverage.tracked_entries += 1;
                accumulator.snapshot.coverage.excluded_entries += 1;
                continue;
            }
            self.accumulate_entry(&mut accumulator, entry, &mut reader, top)?;
        }
        Ok(accumulator.finish(top))
    }

    fn accumulate_entry(
        &mut self,
        accumulator: &mut SnapshotAccumulator,
        entry: TreeEntry,
        reader: &mut BlobReader,
        top: usize,
    ) -> Result<()> {
        let snapshot = &mut accumulator.snapshot;
        snapshot.coverage.tracked_entries += 1;
        if !entry.is_regular() {
            snapshot.coverage.non_regular_entries += 1;
            return Ok(());
        }
        let Some(language) = Language::for_path(&entry.path) else {
            snapshot.coverage.unsupported_files += 1;
            let extension = Path::new(&entry.path)
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or("<none>")
                .to_ascii_lowercase();
            *snapshot
                .coverage
                .unsupported_extensions
                .entry(extension)
                .or_default() += 1;
            return Ok(());
        };
        if self.test_exclusion.is_some()
            && self
                .adapters
                .get(&language)
                .context("Missing language test policy")?
                .is_test_file(&entry.path)
        {
            snapshot.coverage.excluded_entries += 1;
            *snapshot
                .coverage
                .test_excluded_entries
                .as_mut()
                .context("Missing test exclusion coverage")? += 1;
            return Ok(());
        }
        snapshot.coverage.selected_files += 1;
        let language_coverage = snapshot
            .coverage
            .languages
            .entry(language.id().to_owned())
            .or_default();
        language_coverage.selected_files += 1;
        let mut analysis = self.cache.get(language, &entry.oid)?;
        let policy = self.test_fingerprints.get(&language);
        let mut exclusion = match policy {
            Some(policy) if analysis.as_ref().is_none_or(|raw| raw.parsed()) => self
                .cache
                .get_test_exclusion(language, &entry.oid, policy)?,
            _ => None,
        };
        if analysis.is_none()
            || (policy.is_some()
                && analysis.as_ref().is_some_and(|raw| raw.parsed())
                && exclusion.is_none())
        {
            if analysis.is_some() {
                // A raw hit still needs source analysis when the derived view is absent.
                self.cache.misses += 1;
            }
            let commit = snapshot
                .commit
                .as_ref()
                .context("Missing snapshot commit")?;
            let source = reader
                .read(&entry.oid)
                .with_context(|| format!("Reading {:?} at {}", entry.path, commit.sha))?;
            if let std::collections::hash_map::Entry::Vacant(slot) = self.adapters.entry(language) {
                slot.insert(languages::adapter(language, policy.is_some())?);
            }
            let mut parsed = self
                .adapters
                .get_mut(&language)
                .context("Missing language adapter")?
                .analyze(&source)
                .with_context(|| format!("Analyzing {:?}", entry.path))?;
            let derived = policy.map(|policy| {
                (
                    policy,
                    TestExclusionAnalysis {
                        without_tests: parsed.without_tests.take(),
                    },
                )
            });
            if let Some(raw) = &analysis {
                ensure!(
                    **raw == parsed,
                    "Raw analysis disagrees with cached data for {:?}; remove it or use --no-cache",
                    entry.path
                );
            } else {
                analysis = Some(self.cache.insert(language, &entry.oid, parsed)?);
            }
            if let Some((policy, derived)) = derived
                && analysis.as_ref().is_some_and(|raw| raw.parsed())
            {
                if let Some(cached) = &exclusion {
                    ensure!(
                        **cached == derived,
                        "Test exclusion disagrees with cached data for {:?}; remove it or use --no-cache",
                        entry.path
                    );
                } else {
                    exclusion = Some(
                        self.cache
                            .insert_test_exclusion(language, &entry.oid, policy, derived)?,
                    );
                }
            }
        }
        let analysis = analysis.context("Missing raw analysis")?;
        if !analysis.parsed() {
            snapshot.coverage.failed_files += 1;
            snapshot.coverage.failed_physical_lines += analysis.physical_lines;
            language_coverage.failed_files += 1;
            snapshot.failures.push(FailedFile {
                path: entry.path,
                language,
                physical_lines: analysis.physical_lines,
                diagnostics: analysis.diagnostics.clone(),
            });
            return Ok(());
        }
        snapshot.coverage.parsed_files += 1;
        snapshot.coverage.parsed_physical_lines += analysis.physical_lines;
        let filtered = exclusion
            .as_ref()
            .and_then(|analysis| analysis.without_tests.as_ref());
        let (source_lines, functions) = if let Some(filtered) = filtered {
            *snapshot
                .coverage
                .syntax_test_files
                .as_mut()
                .context("Missing syntax test coverage")? += 1;
            *snapshot
                .coverage
                .test_excluded_functions
                .as_mut()
                .context("Missing test function coverage")? += analysis
                .functions
                .len()
                .checked_sub(filtered.functions.len())
                .context("Invalid cached test-filtered function count")?;
            *snapshot
                .coverage
                .test_excluded_source_lines
                .as_mut()
                .context("Missing test line coverage")? += analysis
                .source_lines
                .checked_sub(filtered.source_lines)
                .context("Invalid cached test-filtered source count")?;
            (filtered.source_lines, &filtered.functions)
        } else {
            (analysis.source_lines, &analysis.functions)
        };
        snapshot.coverage.parsed_source_lines += source_lines;
        language_coverage.parsed_files += 1;
        for function in functions {
            snapshot.functions += 1;
            let mass = function.mass();
            accumulator.total.add(mass);
            if function.complexity > COMPLEXITY_THRESHOLD {
                snapshot.complex_functions += 1;
                accumulator.complex.add(mass);
                if top != 0 {
                    snapshot.top_functions.push(TopFunction {
                        path: entry.path.clone(),
                        language,
                        function: function.clone(),
                        mass,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn report(
        &mut self,
        repo: &Repository,
        scope: &Scope,
        reference: Commit,
        checkpoints: &[Checkpoint],
        mode: &'static str,
        top: usize,
    ) -> Result<Report> {
        let mut snapshots = Vec::new();
        let mut previous = None;
        for checkpoint in checkpoints {
            let mut snapshot = self.snapshot(repo, scope, checkpoint, top)?;
            snapshot.change_pp = snapshot
                .erosion_pct
                .zip(previous)
                .map(|(now, before)| now - before);
            previous = snapshot.erosion_pct;
            snapshots.push(snapshot);
        }
        Ok(Report {
            schema_version: if self.test_exclusion.is_some() { 3 } else { 1 },
            tool_version: env!("CARGO_PKG_VERSION"),
            mode,
            reference,
            metric: MetricIdentity::current(),
            scope: scope.config.clone(),
            scope_fingerprint: self.scope_fingerprint(&scope.fingerprint)?,
            test_exclusion: self.test_exclusion.clone(),
            snapshots,
        })
    }
}

pub fn require_complete(report: &Report, allow_partial: bool) -> Result<()> {
    require_complete_snapshots(report.snapshots.iter(), allow_partial)
}

fn require_complete_snapshots<'a>(
    snapshots: impl Iterator<Item = &'a Snapshot>,
    allow_partial: bool,
) -> Result<()> {
    let mut failed = 0;
    for snapshot in snapshots {
        for file in &snapshot.failures {
            failed += 1;
            eprintln!(
                "Parse failure at {} ({}): {:?} ({} physical lines)",
                snapshot.cutoff_utc,
                snapshot
                    .commit
                    .as_ref()
                    .map_or("no commit", |commit| &commit.sha),
                file.path,
                file.physical_lines
            );
            for diagnostic in &file.diagnostics {
                eprintln!(
                    "  {}:{} {}",
                    diagnostic.line, diagnostic.column, diagnostic.message
                );
            }
        }
    }
    if failed > 0 && !allow_partial {
        bail!(
            "{failed} file/checkpoint parse failures; no results emitted. Use --allow-partial to explicitly report incomplete measurements"
        );
    }
    Ok(())
}
