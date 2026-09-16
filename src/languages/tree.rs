use anyhow::{Context, Result, ensure};
use tree_sitter::{Node, Parser};

use crate::metrics::{FileAnalysis, FunctionMetrics, ParseDiagnostic};

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
    let source = source.strip_prefix(b"\xef\xbb\xbf").unwrap_or(source);
    let mut analysis = FileAnalysis {
        physical_lines: physical_lines(source),
        source_lines: 0,
        functions: Vec::new(),
        diagnostics: Vec::new(),
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
    let mut cleaned = source.to_vec();
    let mut cursor = root.walk();
    let mut decisions = vec![0_u64];
    let mut finished = false;
    // Nested callable decisions count in their enclosing callable as well as
    // independently. Iterative postorder avoids recursion and per-node maps.
    while !finished {
        let node = cursor.node();
        *decisions.last_mut().expect("root frame") += R::decisions(node);
        if R::ignored(node, source) {
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
        if cursor.goto_first_child() {
            decisions.push(0);
            continue;
        }
        loop {
            let node = cursor.node();
            let count = decisions.pop().expect("current frame");
            if R::callable(node) {
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
