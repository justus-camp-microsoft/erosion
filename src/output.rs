use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use std::fmt::Write;

use crate::{Report, Snapshot, Status, modules::ModuleReport};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Format {
    Table,
    Json,
    Csv,
}

pub fn render(report: &Report, format: Format) -> Result<Vec<u8>> {
    match format {
        Format::Json => {
            let mut output = serde_json::to_vec_pretty(report)?;
            output.push(b'\n');
            Ok(output)
        }
        Format::Csv => csv(report),
        Format::Table => table(report),
    }
}

pub fn render_modules(report: &ModuleReport, format: Format) -> Result<Vec<u8>> {
    match format {
        Format::Json => {
            let mut output = serde_json::to_vec_pretty(report)?;
            output.push(b'\n');
            Ok(output)
        }
        Format::Csv => modules_csv(report),
        Format::Table => modules_table(report),
    }
}

fn modules_table(report: &ModuleReport) -> Result<Vec<u8>> {
    let mut text = String::new();
    writeln!(text, "Erosion whole-module history\n")?;
    for module in &report.modules {
        writeln!(text, "Module: {}", module.path.escape_debug())?;
        header(&mut text, "Checkpoint")?;
        for snapshot in &module.snapshots {
            row(
                &mut text,
                &snapshot.cutoff_utc.format("%Y-%m-%d").to_string(),
                snapshot,
            )?;
        }
        writeln!(text)?;
    }
    Ok(text.into_bytes())
}

fn table(report: &Report) -> Result<Vec<u8>> {
    let mut text = String::new();
    match report.mode {
        "measure" => {
            let [snapshot] = report.snapshots.as_slice() else {
                bail!("Measure output requires exactly one snapshot");
            };
            measure(&mut text, snapshot)?;
        }
        "delta" => {
            let [from, to] = report.snapshots.as_slice() else {
                bail!("Delta output requires exactly two snapshots");
            };
            match to.change_pp {
                Some(change) => writeln!(
                    text,
                    "Erosion delta (TO - FROM): {change:+.2} percentage points\n"
                )?,
                None => writeln!(
                    text,
                    "Erosion delta (TO - FROM): not measurable (one or both scores unavailable)\n"
                )?,
            }
            from.commit
                .as_ref()
                .context("Delta FROM commit is missing")?;
            to.commit.as_ref().context("Delta TO commit is missing")?;
            header(&mut text, "Snapshot")?;
            row(&mut text, "FROM", from)?;
            row(&mut text, "TO", to)?;
        }
        "history" => {
            writeln!(text, "Erosion history\n")?;
            header(&mut text, "Checkpoint")?;
            for snapshot in &report.snapshots {
                row(
                    &mut text,
                    &snapshot.cutoff_utc.format("%Y-%m-%d").to_string(),
                    snapshot,
                )?;
            }
            if let (Some(first), Some(last)) = (report.snapshots.first(), report.snapshots.last())
                && report.snapshots.len() > 1
                && let Some((first, last)) = first.erosion_pct.zip(last.erosion_pct)
            {
                writeln!(
                    text,
                    "\nOverall change: {:+.2} percentage points",
                    last - first
                )?;
            }
        }
        mode => bail!("Unknown report mode {mode:?}"),
    }
    if report.mode != "measure" {
        writeln!(
            text,
            "\nFiles = parsed; skipped = unsupported, excluded, or links/submodules."
        )?;
    }
    if report
        .snapshots
        .iter()
        .any(|snapshot| matches!(snapshot.status, Status::Partial))
    {
        let label = if report.mode == "delta" {
            "Partial comparison"
        } else {
            "Partial results"
        };
        writeln!(
            text,
            "{label}: failed files are excluded. Scores cover parsed source only."
        )?;
    }
    if report
        .snapshots
        .iter()
        .any(|snapshot| snapshot.commit.is_some() && snapshot.erosion_pct.is_none())
    {
        writeln!(
            text,
            "Not measurable: no function mass in the parsed source."
        )?;
    }
    if report
        .snapshots
        .iter()
        .any(|snapshot| matches!(snapshot.status, Status::NoCommit))
    {
        writeln!(text, "No commit: no ancestor at or before the checkpoint.")?;
    }
    if report.scope.include != ["**"] || !report.scope.exclude.is_empty() {
        writeln!(
            text,
            "Scope: custom include/exclude rules (details in JSON/CSV)."
        )?;
    }
    writeln!(
        text,
        "\nCommitted source only; uncommitted changes ignored."
    )?;
    Ok(text.into_bytes())
}

fn number(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn score(snapshot: &Snapshot) -> String {
    snapshot
        .erosion_pct
        .map_or("--".to_owned(), |value| format!("{value:.2}%"))
}

fn header(text: &mut String, label: &str) -> Result<()> {
    writeln!(
        text,
        "{label:<10}  {:<10} {:>8} {:>9} {:>6} {:>9} {:>6} {:>7} Status",
        "Commit", "Erosion", "Change", "Files", "Functions", "Failed", "Skipped"
    )?;
    Ok(())
}

fn row(text: &mut String, label: &str, snapshot: &Snapshot) -> Result<()> {
    let sha = snapshot
        .commit
        .as_ref()
        .map_or("--", |commit| &commit.sha[..10]);
    let change = snapshot
        .change_pp
        .map_or("--".to_owned(), |value| format!("{value:+.2}pp"));
    let status = match snapshot.status {
        Status::Complete => "complete",
        Status::Partial => "partial",
        Status::NotMeasurable => "not measurable",
        Status::NoCommit => "no commit",
        Status::NoFiles => "no files",
    };
    let coverage = &snapshot.coverage;
    let skipped =
        coverage.unsupported_files + coverage.excluded_entries + coverage.non_regular_entries;
    writeln!(
        text,
        "{label:<10}  {sha:<10} {:>8} {change:>9} {:>6} {:>9} {:>6} {:>7} {status}",
        score(snapshot),
        number(coverage.parsed_files),
        number(snapshot.functions),
        number(coverage.failed_files),
        number(skipped)
    )?;
    Ok(())
}

fn measure(text: &mut String, snapshot: &Snapshot) -> Result<()> {
    let commit = snapshot
        .commit
        .as_ref()
        .context("Measure commit is missing")?;
    let score = if snapshot.erosion_pct.is_none() {
        "not measurable".to_owned()
    } else {
        score(snapshot)
    };
    let partial = if matches!(snapshot.status, Status::Partial) {
        " (partial)"
    } else {
        ""
    };
    writeln!(text, "Erosion: {score}{partial}")?;
    writeln!(
        text,
        "Commit:  {} ({})\n",
        &commit.sha[..10],
        commit.committed_at.format("%Y-%m-%d")
    )?;
    let coverage = &snapshot.coverage;
    writeln!(
        text,
        "Files: {}    Functions: {}",
        number(coverage.parsed_files),
        number(snapshot.functions)
    )?;
    if !coverage.languages.is_empty() {
        let languages: Vec<_> = coverage
            .languages
            .iter()
            .map(|(language, counts)| format!("{language} {}", number(counts.parsed_files)))
            .collect();
        writeln!(text, "Languages: {}", languages.join(", "))?;
    }
    let skipped: Vec<_> = [
        (coverage.unsupported_files, "unsupported"),
        (coverage.excluded_entries, "excluded"),
        (coverage.non_regular_entries, "links/submodules"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, label)| format!("{} {label}", number(count)))
    .collect();
    if !skipped.is_empty() {
        writeln!(text, "Skipped: {}", skipped.join(", "))?;
    }
    writeln!(text, "Parse failures: {}", number(coverage.failed_files))?;
    if !snapshot.top_functions.is_empty() {
        writeln!(text, "\nTop complex functions (CC > 10, ranked by mass):")?;
        for (index, function) in snapshot.top_functions.iter().enumerate() {
            writeln!(
                text,
                "  {}. {} (CC {}, SLOC {}, mass {:.1})",
                index + 1,
                function.function.name.escape_debug(),
                function.function.complexity,
                number(function.function.source_lines),
                function.mass
            )?;
            writeln!(
                text,
                "     {}:{}-{}",
                function.path.escape_debug(),
                function.function.start_line,
                function.function.end_line
            )?;
        }
    }
    Ok(())
}

fn csv(report: &Report) -> Result<Vec<u8>> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record([
        "schema_version",
        "tool_version",
        "metric_version",
        "analyzer_fingerprint",
        "parser_versions",
        "scope_fingerprint",
        "scope_json",
        "reference_sha",
        "cutoff_utc",
        "commit",
        "committed_at",
        "status",
        "selected_files",
        "parsed_files",
        "failed_files",
        "unsupported_files",
        "excluded_entries",
        "non_regular_entries",
        "parsed_physical_lines",
        "failed_physical_lines",
        "parsed_source_lines",
        "functions",
        "complex_functions",
        "total_mass",
        "complex_mass",
        "erosion_pct",
        "change_pp",
        "languages_json",
        "unsupported_extensions_json",
        "failures_json",
    ])?;
    for snapshot in &report.snapshots {
        let coverage = &snapshot.coverage;
        writer.write_record([
            report.schema_version.to_string(),
            report.tool_version.to_owned(),
            report.metric.version.to_owned(),
            report.metric.analyzer_fingerprint.clone(),
            report.metric.parser_versions.to_owned(),
            report.scope_fingerprint.clone(),
            serde_json::to_string(&report.scope)?,
            report.reference.sha.clone(),
            snapshot.cutoff_utc.to_rfc3339(),
            snapshot
                .commit
                .as_ref()
                .map(|commit| commit.sha.clone())
                .unwrap_or_default(),
            snapshot
                .commit
                .as_ref()
                .map(|commit| commit.committed_at.to_rfc3339())
                .unwrap_or_default(),
            snapshot.status.id().to_owned(),
            coverage.selected_files.to_string(),
            coverage.parsed_files.to_string(),
            coverage.failed_files.to_string(),
            coverage.unsupported_files.to_string(),
            coverage.excluded_entries.to_string(),
            coverage.non_regular_entries.to_string(),
            coverage.parsed_physical_lines.to_string(),
            coverage.failed_physical_lines.to_string(),
            coverage.parsed_source_lines.to_string(),
            snapshot.functions.to_string(),
            snapshot.complex_functions.to_string(),
            snapshot.total_mass.to_string(),
            snapshot.complex_mass.to_string(),
            snapshot
                .erosion_pct
                .map(|value| value.to_string())
                .unwrap_or_default(),
            snapshot
                .change_pp
                .map(|value| value.to_string())
                .unwrap_or_default(),
            serde_json::to_string(&coverage.languages)?,
            serde_json::to_string(&coverage.unsupported_extensions)?,
            serde_json::to_string(&snapshot.failures)?,
        ])?;
    }
    writer.flush()?;
    Ok(writer.into_inner()?)
}

fn modules_csv(report: &ModuleReport) -> Result<Vec<u8>> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record([
        "schema_version",
        "tool_version",
        "mode",
        "metric_version",
        "analyzer_fingerprint",
        "parser_versions",
        "metric_json",
        "grouping_fingerprint",
        "grouping_json",
        "file_scope",
        "discovered_modules",
        "reference_sha",
        "reference_committed_at",
        "reference_json",
        "module",
        "cutoff_utc",
        "commit",
        "committed_at",
        "status",
        "tracked_entries",
        "selected_files",
        "parsed_files",
        "failed_files",
        "unsupported_files",
        "excluded_entries",
        "non_regular_entries",
        "parsed_physical_lines",
        "failed_physical_lines",
        "parsed_source_lines",
        "coverage_json",
        "repository_tracked_entries",
        "outside_depth_entries",
        "unselected_module_entries",
        "selected_module_entries",
        "functions",
        "complex_functions",
        "total_mass",
        "complex_mass",
        "erosion_pct",
        "change_pp",
        "failures_json",
    ])?;
    let metric_json = serde_json::to_string(&report.metric)?;
    let grouping_json = serde_json::to_string(&report.grouping)?;
    let reference_json = serde_json::to_string(&report.reference)?;
    for module in &report.modules {
        for (index, snapshot) in module.snapshots.iter().enumerate() {
            let inventory = report
                .inventory
                .get(index)
                .context("Missing module inventory checkpoint")?;
            let coverage = &snapshot.coverage;
            writer.write_record([
                report.schema_version.to_string(),
                report.tool_version.to_owned(),
                report.mode.to_owned(),
                report.metric.version.to_owned(),
                report.metric.analyzer_fingerprint.clone(),
                report.metric.parser_versions.to_owned(),
                metric_json.clone(),
                report.grouping_fingerprint.clone(),
                grouping_json.clone(),
                report.file_scope.to_owned(),
                report.discovered_modules.to_string(),
                report.reference.sha.clone(),
                report.reference.committed_at.to_rfc3339(),
                reference_json.clone(),
                module.path.clone(),
                snapshot.cutoff_utc.to_rfc3339(),
                snapshot
                    .commit
                    .as_ref()
                    .map(|commit| commit.sha.clone())
                    .unwrap_or_default(),
                snapshot
                    .commit
                    .as_ref()
                    .map(|commit| commit.committed_at.to_rfc3339())
                    .unwrap_or_default(),
                snapshot.status.id().to_owned(),
                coverage.tracked_entries.to_string(),
                coverage.selected_files.to_string(),
                coverage.parsed_files.to_string(),
                coverage.failed_files.to_string(),
                coverage.unsupported_files.to_string(),
                coverage.excluded_entries.to_string(),
                coverage.non_regular_entries.to_string(),
                coverage.parsed_physical_lines.to_string(),
                coverage.failed_physical_lines.to_string(),
                coverage.parsed_source_lines.to_string(),
                serde_json::to_string(coverage)?,
                inventory.tracked_entries.to_string(),
                inventory.outside_depth_entries.to_string(),
                inventory.unselected_module_entries.to_string(),
                inventory.selected_module_entries.to_string(),
                snapshot.functions.to_string(),
                snapshot.complex_functions.to_string(),
                snapshot.total_mass.to_string(),
                snapshot.complex_mass.to_string(),
                snapshot
                    .erosion_pct
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                snapshot
                    .change_pp
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
                serde_json::to_string(&snapshot.failures)?,
            ])?;
        }
    }
    writer.flush()?;
    Ok(writer.into_inner()?)
}
