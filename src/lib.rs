pub mod cache;
pub mod config;
pub mod git;
pub mod history;
pub mod languages;
pub mod metrics;
pub mod modules;
pub mod output;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use cache::Cache;
use config::{Config, Scope};
use git::{BlobReader, Commit, Repository, TreeEntry};
use history::Checkpoint;
use languages::{Language, LanguageAdapter};
use metrics::{COMPLEXITY_THRESHOLD, FunctionMetrics, METRIC_VERSION, ParseDiagnostic, Sum};

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
    fn new(checkpoint: &Checkpoint) -> Self {
        Self {
            snapshot: Snapshot::empty(checkpoint),
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
    pub snapshots: Vec<Snapshot>,
}

pub struct Analyzer {
    pub cache: Cache,
    adapters: HashMap<Language, Box<dyn LanguageAdapter>>,
}

impl Analyzer {
    pub fn new(cache: Cache) -> Self {
        Self {
            cache,
            adapters: HashMap::new(),
        }
    }

    pub fn snapshot(
        &mut self,
        repo: &Repository,
        scope: &Scope,
        checkpoint: &Checkpoint,
        top: usize,
    ) -> Result<Snapshot> {
        let mut accumulator = SnapshotAccumulator::new(checkpoint);
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
        snapshot.coverage.selected_files += 1;
        let language_coverage = snapshot
            .coverage
            .languages
            .entry(language.id().to_owned())
            .or_default();
        language_coverage.selected_files += 1;
        let analysis = match self.cache.get(language, &entry.oid)? {
            Some(cached) => cached,
            None => {
                let commit = snapshot
                    .commit
                    .as_ref()
                    .context("Missing snapshot commit")?;
                let source = reader
                    .read(&entry.oid)
                    .with_context(|| format!("Reading {:?} at {}", entry.path, commit.sha))?;
                if let std::collections::hash_map::Entry::Vacant(slot) =
                    self.adapters.entry(language)
                {
                    slot.insert(languages::adapter(language)?);
                }
                let parsed = self
                    .adapters
                    .get_mut(&language)
                    .context("Missing language adapter")?
                    .analyze(&source)
                    .with_context(|| format!("Analyzing {:?}", entry.path))?;
                self.cache.insert(language, &entry.oid, parsed)?
            }
        };
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
        snapshot.coverage.parsed_source_lines += analysis.source_lines;
        language_coverage.parsed_files += 1;
        for function in &analysis.functions {
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
            schema_version: 1,
            tool_version: env!("CARGO_PKG_VERSION"),
            mode,
            reference,
            metric: MetricIdentity::current(),
            scope: scope.config.clone(),
            scope_fingerprint: scope.fingerprint.clone(),
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
