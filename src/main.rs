use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use erosion::{
    Analyzer,
    cache::Cache,
    config::Scope,
    git::Repository,
    history::{self, Checkpoint},
    modules::{self, ModuleSelection},
    output::{self, Format},
};
use std::{io::Write, path::PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Measure code erosion in committed Git source, including whole-module history, without checking out revisions"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure committed HEAD or a selected revision.
    Measure {
        #[command(flatten)]
        common: Common,
        /// Committed revision to measure.
        #[arg(long = "ref", default_value = "HEAD")]
        revision: String,
        /// Show up to N functions contributing complex mass (CC > 10). Not supported in CSV.
        #[arg(long)]
        top: Option<usize>,
    },
    /// Measure first-parent history at UTC calendar checkpoints.
    History {
        #[command(flatten)]
        common: Common,
        /// Head of the first-parent history to traverse.
        #[arg(long = "ref", default_value = "HEAD")]
        revision: String,
        /// Start date (YYYY-MM-DD), or positive duration before the endpoint (e.g. 3y).
        #[arg(long)]
        since: String,
        /// Positive cadence: Nd, Nw, Nmo, Ny (e.g. 6mo).
        #[arg(long, default_value = "6mo")]
        every: String,
        /// End date, inclusive in UTC. Defaults to the selected revision's committer timestamp.
        #[arg(long)]
        until: Option<String>,
    },
    /// Compare two committed snapshots: erosion at TO minus erosion at FROM.
    Delta {
        /// Baseline commit or revision (e.g. HEAD~1).
        from: String,
        /// Target commit or revision (e.g. HEAD).
        to: String,
        #[command(flatten)]
        common: Common,
    },
    /// Measure whole-module history by selecting discovered module paths.
    #[command(after_help = "\
Module directories are discovered across all sampled first-parent commits, including
new and deleted modules. --depth selects directories exactly N levels below the
repository root; filenames never become modules. Repeated --glob patterns
select the union of matching discovered module keys BEFORE their whole contents
are analyzed. Globs are NOT source-file filters: '*' stays within a component and
'**' can cross '/'. With no --glob, all discovered modules are selected.

Entries above the requested directory depth are not analyzed; repository-wide
inventory counts in JSON/CSV report them separately from entries in unselected modules.
Use a shallower depth to include their parent, or measure for repository-root files.

All tracked descendants are inventoried, with no implicit test/generated exclusions.
--exclude-tests applies language-owned path and syntax detection after module selection;
--glob remains module-only. Test-only modules stay present with unavailable scores.
erosion.toml is ignored; scope configuration is not accepted. Unsupported files
count in coverage; symlinks and submodules are counted but never followed.

Absent modules (no_files), absent commits (no_commit), and present modules without
function mass (not_measurable) have null scores, not zero. Changes require adjacent
measurable scores; missing history is never bridged.

Parse failures prevent output by default. --allow-partial explicitly permits
incomplete parsed-source results, but never relaxes Git, cache, or read errors.")]
    Modules {
        #[command(flatten)]
        common: Shared,
        /// Head of the first-parent history to traverse.
        #[arg(long = "ref", default_value = "HEAD")]
        revision: String,
        /// Start date (YYYY-MM-DD), or positive duration before the endpoint (e.g. 3y).
        #[arg(long)]
        since: String,
        /// Positive cadence: Nd, Nw, Nmo, Ny (e.g. 6mo).
        #[arg(long, default_value = "6mo")]
        every: String,
        /// End date, inclusive in UTC. Defaults to the selected revision's committer timestamp.
        #[arg(long)]
        until: Option<String>,
        /// Positive directory depth below the repository root; filenames never become modules. Entries above this depth are counted separately. Discover directory keys, match --glob, then analyze whole contents. erosion.toml is ignored.
        #[arg(long, default_value = "1")]
        depth: usize,
        /// Match discovered module paths, not source-file paths, before analyzing whole contents. Repeat for OR matching; omitted selects all. '*' stays within a component; '**' crosses '/'. erosion.toml is ignored.
        #[arg(long = "glob")]
        globs: Vec<String>,
    },
}

#[derive(Args)]
struct Common {
    #[command(flatten)]
    shared: Shared,
    /// Scope configuration. Defaults to erosion.toml at the repository root, if present.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Exclude repository-relative paths matching GLOB, in addition to configured exclusions. Repeat for multiple globs; quote patterns to prevent shell expansion.
    #[arg(long, value_name = "GLOB")]
    exclude_paths: Vec<String>,
}

#[derive(Args)]
struct Shared {
    /// Repository directory or a directory inside it. Relative to the invocation directory.
    #[arg(long, default_value = ".")]
    repo_path: PathBuf,
    #[arg(long, value_enum, default_value = "table")]
    format: Format,
    /// Show processing and cache diagnostics on stderr.
    #[arg(short, long)]
    verbose: bool,
    /// Exclude language-detected test files and inline tests; included by default.
    #[arg(
        long,
        long_help = "\
Exclude test source using the language-tests-v1 policy. Each language owns its
case-sensitive test paths and syntax rules; disabled by default.

JavaScript/TypeScript/TSX: test/tests/__tests__/__mocks__ directories and
*.test.*, *.tests.*, *.spec.*, *.cy.*;
bound Node test, Jest, Vitest, Mocha, AVA, Tape, Playwright and uvu APIs;
unshadowed ambient suites with test callbacks; test-only local helpers.
Python: test/tests/__tests__ directories; test_*.py, *_test.py and conftest.py
(also pyw/pyi); unittest TestCase/IsolatedAsyncioTestCase subclasses and main;
bound pytest fixtures and marked test_* functions/Test* classes.
Rust: tests directories and tests.rs files; built-in test attributes and known
tokio/async-std/rstest/test-case attributes; syntax provably gated by cfg(test),
including inline modules. Ambiguous conditional compilation remains included.
Gleam: test/tests directories and *_test.gleam; public zero-argument *_test
functions/EUnit *_test_ generators; unshadowed zero-argument gleeunit.main().

Inline detection preserves production code in mixed files and removes test
decisions/source lines from enclosing functions, not just test function rows.
Bindings, aliases and supported static syntax are inspected, never executed.
Uncertain dynamic/custom framework code remains included. Assertions alone are
not tests; benchmarks, generated code and bundles have no separate exclusion.

Scope/module selection happens first; --glob selects module paths, never files.
Only supported regular source files receive language path classification.
Matching test source files are counted but skipped before reading/parsing.
Unsupported assets and links/submodules remain in their normal coverage
categories; links/submodules are never followed. Mixed files must parse fully,
even when apparent test syntax contains an error.

JSON/CSV records per-language policies, whole-file exclusion counts and separate
syntax-removal counts. Removed function/source-line counts cover syntax filtering
only; skipped whole files are not parsed to count their contents. Module
discovery/inventory is unchanged and test-only modules stay present with null
scores. The raw default measurement is unchanged."
    )]
    exclude_tests: bool,
    /// Emit explicitly incomplete results when files fail to parse.
    #[arg(
        long,
        long_help = "Emit explicitly incomplete results when files fail to parse. Strict by default: parse failures prevent all output unless this flag is set. Never relaxes Git, cache, or read errors."
    )]
    allow_partial: bool,
    /// Persistent cache location outside the measured worktree.
    #[arg(long, conflicts_with = "no_cache")]
    cache_dir: Option<PathBuf>,
    /// Disable persistent caching (in-process blob reuse remains enabled).
    #[arg(long)]
    no_cache: bool,
}

enum ReportScope {
    Configured(Scope),
    Modules(ModuleSelection),
}

fn run(cli: Cli) -> Result<()> {
    let (common, top) = match &cli.command {
        Command::Measure { common, top, .. } => {
            ensure!(
                !matches!(common.shared.format, Format::Csv) || top.is_none(),
                "--top cannot be combined with --format csv; use table or JSON"
            );
            (&common.shared, top.unwrap_or(0))
        }
        Command::History { common, .. } | Command::Delta { common, .. } => (&common.shared, 0),
        Command::Modules { common, .. } => (common, 0),
    };
    let repo = Repository::discover(&common.repo_path)?;
    let scope = match &cli.command {
        Command::Measure { common, .. }
        | Command::History { common, .. }
        | Command::Delta { common, .. } => ReportScope::Configured(
            Scope::load(&repo.root, common.config.as_deref())?
                .with_exclusions(&common.exclude_paths)?,
        ),
        Command::Modules { depth, globs, .. } => {
            ReportScope::Modules(ModuleSelection::new(*depth, globs.clone())?)
        }
    };
    let (mode, reference, checkpoints) = match &cli.command {
        Command::Measure { revision, .. } => {
            let reference = repo.resolve(revision)?;
            let checkpoints = vec![Checkpoint {
                cutoff: reference.committed_at,
                commit: Some(reference.clone()),
            }];
            ("measure", reference, checkpoints)
        }
        Command::History {
            revision,
            since,
            every,
            until,
            ..
        }
        | Command::Modules {
            revision,
            since,
            every,
            until,
            ..
        } => {
            let reference = repo.resolve(revision)?;
            let chain = repo.first_parent_chain(&reference)?;
            let checkpoints = history::checkpoints(&chain, since, until.as_deref(), every)?;
            let mode = if matches!(cli.command, Command::Modules { .. }) {
                "modules"
            } else {
                "history"
            };
            (mode, reference, checkpoints)
        }
        Command::Delta { from, to, .. } => {
            let baseline = repo.resolve(from).context("Resolving FROM revision")?;
            let reference = repo.resolve(to).context("Resolving TO revision")?;
            let checkpoints = [baseline, reference.clone()]
                .into_iter()
                .map(|commit| Checkpoint {
                    cutoff: commit.committed_at,
                    commit: Some(commit),
                })
                .collect();
            ("delta", reference, checkpoints)
        }
    };
    let mut analyzer = Analyzer::new(Cache::new(
        common.cache_dir.as_deref(),
        common.no_cache,
        &repo.root,
    )?);
    if common.exclude_tests {
        analyzer = analyzer.exclude_tests()?;
    }
    if common.verbose {
        eprintln!("Processing {} snapshot(s)...", checkpoints.len());
    }
    let rendered = match scope {
        ReportScope::Configured(scope) => {
            let report = analyzer.report(&repo, &scope, reference, &checkpoints, mode, top)?;
            erosion::require_complete(&report, common.allow_partial)?;
            output::render(&report, common.format)?
        }
        ReportScope::Modules(selection) => {
            let report = analyzer.modules_report(&repo, &selection, reference, &checkpoints)?;
            modules::require_complete(&report, common.allow_partial)?;
            output::render_modules(&report, common.format)?
        }
    };
    if common.verbose {
        eprintln!(
            "Blob analysis: {} parsed, {} disk hits, {} in-process hits",
            analyzer.cache.misses, analyzer.cache.disk_hits, analyzer.cache.memory_hits
        );
    }
    std::io::stdout()
        .lock()
        .write_all(&rendered)
        .context("Writing results to stdout")?;
    Ok(())
}

fn main() {
    if let Err(error) = run(Cli::parse()) {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}
