use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use erosion::{
    Analyzer,
    cache::Cache,
    config::Scope,
    git::Repository,
    history::{self, Checkpoint},
    output::{self, Format},
};
use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
};

const MIN_PROGRESS_COMMITS: usize = 10;

#[derive(Parser)]
#[command(
    version,
    about = "Measure code erosion in committed Git source, without checking out revisions"
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
        /// Calculate erosion for every commit on the inclusive first-parent range.
        #[arg(long)]
        all_commits: bool,
        #[command(flatten)]
        common: Common,
    },
}

#[derive(Args)]
struct Common {
    /// Repository directory or a directory inside it. Relative to the invocation directory.
    #[arg(long, default_value = ".")]
    repo_path: PathBuf,
    #[arg(long, value_enum, default_value = "table")]
    format: Format,
    /// Show processing and cache diagnostics on stderr.
    #[arg(short, long)]
    verbose: bool,
    /// Scope configuration. Defaults to erosion.toml at the repository root, if present.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Emit explicitly incomplete results when files fail to parse.
    #[arg(long)]
    allow_partial: bool,
    /// Persistent cache location outside the measured worktree.
    #[arg(long, conflicts_with = "no_cache")]
    cache_dir: Option<PathBuf>,
    /// Disable persistent caching (in-process blob reuse remains enabled).
    #[arg(long)]
    no_cache: bool,
}

struct CommitProgress {
    enabled: bool,
    terminal: bool,
}

impl CommitProgress {
    fn advance(&self, completed: usize, total: usize, checkpoint: &Checkpoint) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let sha = &checkpoint.commit.as_ref().context("Missing commit")?.sha[..10];
        let mut stderr = std::io::stderr().lock();
        if self.terminal {
            write!(stderr, "\rProcessing commit {completed}/{total}: {sha}")?;
            stderr.flush()?;
        } else {
            writeln!(stderr, "Processing commit {completed}/{total}: {sha}")?;
        }
        Ok(())
    }

    fn finish(&self) -> Result<()> {
        if self.enabled && self.terminal {
            writeln!(std::io::stderr().lock())?;
        }
        Ok(())
    }
}

fn run(cli: Cli) -> Result<()> {
    let (common, top) = match &cli.command {
        Command::Measure { common, top, .. } => {
            ensure!(
                !matches!(common.format, Format::Csv) || top.is_none(),
                "--top cannot be combined with --format csv; use table or JSON"
            );
            (common, top.unwrap_or(0))
        }
        Command::History { common, .. } | Command::Delta { common, .. } => (common, 0),
    };
    let repo = Repository::discover(&common.repo_path)?;
    let scope = Scope::load(&repo.root, common.config.as_deref())?;
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
        } => {
            let reference = repo.resolve(revision)?;
            let chain = repo.first_parent_chain(&reference)?;
            let checkpoints = history::checkpoints(&chain, since, until.as_deref(), every)?;
            ("history", reference, checkpoints)
        }
        Command::Delta {
            from,
            to,
            all_commits,
            ..
        } => {
            let baseline = repo.resolve(from).context("Resolving FROM revision")?;
            let reference = repo.resolve(to).context("Resolving TO revision")?;
            let commits = if *all_commits {
                let chain = repo.first_parent_chain(&reference)?;
                let Some(from_index) = chain.iter().position(|commit| commit.sha == baseline.sha)
                else {
                    anyhow::bail!(
                        "FROM revision {} is not on the first-parent chain of TO revision {}",
                        baseline.sha,
                        reference.sha
                    );
                };
                chain[..=from_index].iter().rev().cloned().collect()
            } else {
                vec![baseline, reference.clone()]
            };
            let checkpoints = commits
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
    let all_commits = matches!(
        &cli.command,
        Command::Delta {
            all_commits: true,
            ..
        }
    );
    let terminal = std::io::stderr().is_terminal();
    let progress = CommitProgress {
        enabled: all_commits
            && checkpoints.len() >= MIN_PROGRESS_COMMITS
            && (terminal || common.verbose),
        terminal,
    };
    if common.verbose {
        eprintln!("Processing {} snapshot(s)...", checkpoints.len());
    }
    let report = if progress.enabled {
        analyzer.report_with_progress(
            &repo,
            &scope,
            reference,
            &checkpoints,
            mode,
            |completed, total, checkpoint| progress.advance(completed, total, checkpoint),
        )
    } else {
        analyzer.report(&repo, &scope, reference, &checkpoints, mode, top)
    };
    let progress_finished = progress.finish();
    let report = report?;
    progress_finished?;
    erosion::require_complete(&report, common.allow_partial)?;
    let rendered = output::render(&report, common.format)?;
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
