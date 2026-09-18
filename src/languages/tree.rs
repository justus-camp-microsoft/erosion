use anyhow::{Context, Result, ensure};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use tree_sitter::{Node, Parser};

use crate::metrics::{
    FileAnalysis, FunctionMetrics, ParseDiagnostic, TestFilteredAnalysis, TestRegion,
};

pub(super) struct TestPaths(GlobSet);

impl TestPaths {
    pub fn new(patterns: &[&str]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            builder.add(
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .case_insensitive(false)
                    .build()
                    .with_context(|| format!("Invalid language test path pattern {pattern:?}"))?,
            );
        }
        Ok(Self(
            builder.build().context("Compiling language test paths")?,
        ))
    }

    pub fn matches(&self, path: &str) -> bool {
        self.0.is_match(path)
    }
}

pub(super) fn test_region(node: Node<'_>, reason: &str) -> TestRegion {
    TestRegion {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: last_line(node) + 1,
        reason: reason.to_owned(),
    }
}

pub(super) trait SyntaxRules {
    fn callable(node: Node<'_>) -> bool;
    fn decisions(node: Node<'_>) -> u64;
    fn name(node: Node<'_>, source: &[u8]) -> String;
    fn ignored(node: Node<'_>, _source: &[u8]) -> bool {
        node.kind() == "comment"
    }
    fn validation_error(_node: Node<'_>) -> Option<&'static str> {
        None
    }
    fn test_regions(_root: Node<'_>, _source: &[u8]) -> Vec<TestRegion> {
        Vec::new()
    }
}

fn physical_lines(source: &[u8]) -> usize {
    source.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(!source.is_empty() && source.last() != Some(&b'\n'))
}

fn last_line(node: Node<'_>) -> usize {
    node.end_position()
        .row
        .saturating_sub(usize::from(node.end_position().column == 0))
}

pub(super) fn source_line(line: &[u8]) -> bool {
    let whitespace = |byte: &u8| matches!(*byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let Some(start) = line.iter().position(|byte| !whitespace(byte)) else {
        return false;
    };
    let end = line
        .iter()
        .rposition(|byte| !whitespace(byte))
        .expect("nonblank line");
    !line[start..=end]
        .iter()
        .all(|byte| b"{}[]();,:".contains(byte))
}

pub(super) fn analyze<R: SyntaxRules>(parser: &mut Parser, source: &[u8]) -> Result<FileAnalysis> {
    let bom_bytes = if source.starts_with(b"\xef\xbb\xbf") {
        3
    } else {
        0
    };
    let source = source.strip_prefix(b"\xef\xbb\xbf").unwrap_or(source);
    let mut analysis = FileAnalysis {
        physical_lines: physical_lines(source),
        source_lines: 0,
        functions: Vec::new(),
        diagnostics: Vec::new(),
        without_tests: None,
    };
    if let Err(error) = std::str::from_utf8(source) {
        let prefix = &source[..error.valid_up_to()];
        analysis.diagnostics.push(ParseDiagnostic {
            line: prefix.iter().filter(|byte| **byte == b'\n').count() + 1,
            column: prefix
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(prefix.len() + 1, |position| prefix.len() - position),
            message: format!("Invalid UTF-8: {error}"),
        });
        return Ok(analysis);
    }
    let tree = parser
        .parse(source, None)
        .context("Parser cancelled without a tree")?;
    let root = tree.root_node();
    analysis = measure::<R>(root, source, &[])?;
    if analysis.parsed() {
        let mut regions = normalize_regions(R::test_regions(root, source), source.len())?;
        if !regions.is_empty() {
            let filtered = measure::<R>(root, source, &regions)?;
            ensure!(
                filtered.parsed(),
                "Test filtering changed syntax validation"
            );
            ensure!(
                filtered.source_lines <= analysis.source_lines
                    && filtered.functions.len() <= analysis.functions.len(),
                "Test filtering increased measured source or function counts"
            );
            for region in &mut regions {
                region.start_byte += bom_bytes;
                region.end_byte += bom_bytes;
            }
            analysis.without_tests = Some(TestFilteredAnalysis {
                source_lines: filtered.source_lines,
                functions: filtered.functions,
                regions,
            });
        }
    }
    Ok(analysis)
}

fn normalize_regions(mut regions: Vec<TestRegion>, source_len: usize) -> Result<Vec<TestRegion>> {
    regions.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then_with(|| b.end_byte.cmp(&a.end_byte))
            .then_with(|| a.reason.cmp(&b.reason))
    });
    let mut result: Vec<TestRegion> = Vec::new();
    for region in regions {
        ensure!(
            region.start_byte < region.end_byte && region.end_byte <= source_len,
            "Invalid language test region"
        );
        if let Some(previous) = result.last_mut()
            && region.start_byte < previous.end_byte
        {
            if region.end_byte > previous.end_byte {
                previous.end_byte = region.end_byte;
                previous.end_line = region.end_line;
            }
        } else {
            result.push(region);
        }
    }
    Ok(result)
}

fn contained(node: Node<'_>, regions: &[TestRegion]) -> bool {
    let index = regions.partition_point(|region| region.start_byte <= node.start_byte());
    index > 0 && node.end_byte() <= regions[index - 1].end_byte
}

fn measure<R: SyntaxRules>(
    root: Node<'_>,
    source: &[u8],
    regions: &[TestRegion],
) -> Result<FileAnalysis> {
    let mut analysis = FileAnalysis {
        physical_lines: physical_lines(source),
        source_lines: 0,
        functions: Vec::new(),
        diagnostics: Vec::new(),
        without_tests: None,
    };
    let mut cleaned = source.to_vec();
    for region in regions {
        for byte in &mut cleaned[region.start_byte..region.end_byte] {
            if !matches!(*byte, b'\n' | b'\r') {
                *byte = b' ';
            }
        }
    }
    let mut cursor = root.walk();
    let mut decisions = vec![0_u64];
    let mut finished = false;
    // Nested callable decisions count in their enclosing callable as well as
    // independently. Iterative postorder avoids recursion and per-node maps.
    while !finished {
        let node = cursor.node();
        let excluded = contained(node, regions);
        if !excluded {
            *decisions.last_mut().expect("root frame") += R::decisions(node);
        }
        if !excluded && R::ignored(node, source) {
            for byte in &mut cleaned[node.byte_range()] {
                if !matches!(*byte, b'\n' | b'\r') {
                    *byte = b' ';
                }
            }
        }
        if node.is_error() || node.is_missing() {
            analysis.diagnostics.push(ParseDiagnostic {
                line: node.start_position().row + 1,
                column: node.start_position().column + 1,
                message: if node.is_missing() {
                    format!("Missing {}", node.kind())
                } else {
                    "Unrecognized syntax".to_owned()
                },
            });
        }
        if let Some(message) = R::validation_error(node) {
            analysis.diagnostics.push(ParseDiagnostic {
                line: node.start_position().row + 1,
                column: node.start_position().column + 1,
                message: message.to_owned(),
            });
        }
        if !excluded && cursor.goto_first_child() {
            decisions.push(0);
            continue;
        }
        loop {
            let node = cursor.node();
            let count = decisions.pop().expect("current frame");
            if !contained(node, regions) && R::callable(node) {
                analysis.functions.push(FunctionMetrics {
                    name: R::name(node, source),
                    start_line: node.start_position().row + 1,
                    end_line: last_line(node) + 1,
                    complexity: 1 + count,
                    source_lines: 0,
                });
            }
            if let Some(parent) = decisions.last_mut() {
                *parent += count;
            }
            if cursor.goto_next_sibling() {
                decisions.push(0);
                break;
            }
            if !cursor.goto_parent() {
                finished = true;
                break;
            }
        }
    }
    if root.has_error() {
        ensure!(
            !analysis.diagnostics.is_empty(),
            "Parser returned an unexplained syntax error"
        );
    }
    if !analysis.diagnostics.is_empty() {
        analysis.functions.clear();
        return Ok(analysis);
    }
    let mut prefix = vec![0_usize];
    for line in cleaned
        .split(|byte| *byte == b'\n')
        .take(analysis.physical_lines)
    {
        prefix.push(prefix.last().expect("prefix seed") + usize::from(source_line(line)));
    }
    analysis.source_lines = *prefix.last().expect("prefix seed");
    for function in &mut analysis.functions {
        function.source_lines = prefix[function.end_line] - prefix[function.start_line - 1];
    }
    analysis
        .functions
        .retain(|function| function.source_lines != 0);
    analysis
        .functions
        .sort_by_key(|function| (function.start_line, function.end_line));
    Ok(analysis)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MarkedFunctions;

    impl SyntaxRules for MarkedFunctions {
        fn callable(node: Node<'_>) -> bool {
            node.kind() == "function_declaration"
        }

        fn decisions(node: Node<'_>) -> u64 {
            u64::from(node.kind() == "if_statement")
        }

        fn name(node: Node<'_>, source: &[u8]) -> String {
            node.child_by_field_name("name")
                .unwrap()
                .utf8_text(source)
                .unwrap()
                .to_owned()
        }

        fn test_regions(root: Node<'_>, source: &[u8]) -> Vec<TestRegion> {
            let mut regions = Vec::new();
            let mut nodes = vec![root];
            while let Some(node) = nodes.pop() {
                if Self::callable(node) && Self::name(node, source).starts_with("test_") {
                    regions.push(test_region(node, "marked function"));
                }
                nodes.extend(node.named_children(&mut node.walk()));
            }
            regions
        }

        fn validation_error(node: Node<'_>) -> Option<&'static str> {
            (node.kind() == "debugger_statement").then_some("unsupported fixture syntax")
        }
    }

    fn parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_javascript::LANGUAGE.into())
            .unwrap();
        parser
    }

    #[test]
    fn filtered_subtrees_remove_decisions_and_source_from_retained_ancestors() {
        let source = b"function production(x) {\n if(x)work();\n function test_only(){\n if(x)work();\n }\n return x;\n}\n";
        let mut parser = parser();
        let tree = parser.parse(source, None).unwrap();
        let raw = measure::<MarkedFunctions>(tree.root_node(), source, &[]).unwrap();
        let mut analyzed = analyze::<MarkedFunctions>(&mut parser, source).unwrap();
        let filtered = analyzed.without_tests.take().unwrap();
        assert_eq!(analyzed, raw);
        assert_eq!(raw.functions.len(), 2);
        assert_eq!(raw.functions[0].complexity, 3);
        assert_eq!(filtered.functions.len(), 1);
        assert_eq!(filtered.functions[0].name, "production");
        assert_eq!(filtered.functions[0].complexity, 2);
        assert_eq!(filtered.functions[0].source_lines, 3);
        assert_eq!(filtered.source_lines, 3);
    }

    #[test]
    fn byte_masking_preserves_same_line_production_and_original_blob_offsets() {
        let source = "\u{feff}function test_only(){if(x)work('\u{65e5}\u{672c}\u{8a9e}')} function production(){}\r\n";
        let analyzed = analyze::<MarkedFunctions>(&mut parser(), source.as_bytes()).unwrap();
        assert_eq!(analyzed.physical_lines, 1);
        assert_eq!(analyzed.functions.len(), 2);
        let filtered = analyzed.without_tests.unwrap();
        assert_eq!(filtered.source_lines, 1);
        assert_eq!(filtered.functions.len(), 1);
        assert_eq!(filtered.functions[0].source_lines, 1);
        assert_eq!(filtered.regions[0].start_byte, 3);
        assert_eq!(
            &source[filtered.regions[0].start_byte..filtered.regions[0].end_byte],
            "function test_only(){if(x)work('\u{65e5}\u{672c}\u{8a9e}')}"
        );
        let serialized = serde_json::to_vec(&filtered).unwrap();
        assert_eq!(
            filtered,
            serde_json::from_slice::<TestFilteredAnalysis>(&serialized).unwrap()
        );
    }

    #[test]
    fn wholly_test_source_has_zero_mass_without_hiding_parse_or_language_errors() {
        let analyzed = analyze::<MarkedFunctions>(
            &mut parser(),
            b"function test_outer(){function test_inner(){if(x)work()}}\n",
        )
        .unwrap();
        let filtered = analyzed.without_tests.unwrap();
        assert!(filtered.functions.is_empty());
        assert_eq!(filtered.source_lines, 0);
        assert_eq!(filtered.regions.len(), 1);
        for source in ["function test_bad(){if(}", "function test_bad(){debugger;}"] {
            let analyzed = analyze::<MarkedFunctions>(&mut parser(), source.as_bytes()).unwrap();
            assert!(!analyzed.parsed());
            assert!(analyzed.functions.is_empty());
            assert!(analyzed.without_tests.is_none());
        }
        assert!(
            analyze::<MarkedFunctions>(&mut parser(), b"function production(){}")
                .unwrap()
                .without_tests
                .is_none()
        );
    }

    #[test]
    fn regions_are_deterministic_bounded_and_coalesced_without_losing_coverage() {
        let region = |start_byte, end_byte, reason: &str| TestRegion {
            start_byte,
            end_byte,
            start_line: 1,
            end_line: 1,
            reason: reason.to_owned(),
        };
        let regions = vec![
            region(50, 60, "last"),
            region(9, 13, "overlap"),
            region(3, 5, "nested"),
            region(0, 10, "outer"),
            region(13, 15, "adjacent"),
        ];
        let expected = vec![
            region(0, 13, "outer"),
            region(13, 15, "adjacent"),
            region(50, 60, "last"),
        ];
        assert_eq!(normalize_regions(regions.clone(), 60).unwrap(), expected);
        assert_eq!(
            normalize_regions(regions.into_iter().rev().collect(), 60).unwrap(),
            expected
        );
        for invalid in [region(2, 2, "empty"), region(0, 61, "overrun")] {
            assert!(normalize_regions(vec![invalid], 60).is_err());
        }
    }
}
