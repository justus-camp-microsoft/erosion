use anyhow::{Context, Result, ensure};
use tree_sitter::{Node, Parser};

use super::{
    LanguageAdapter,
    tree::{self, SyntaxRules},
};
use crate::metrics::FileAnalysis;

const DECISIONS: &[&str] = &[
    "if_expression",
    "for_expression",
    "while_expression",
    "loop_expression",
];

pub struct RustAdapter {
    parser: Parser,
}

impl RustAdapter {
    pub fn new() -> Result<Self> {
        let grammar: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        for kind in DECISIONS.iter().chain(
            [
                "function_item",
                "closure_expression",
                "match_arm",
                "let_declaration",
                "binary_expression",
                "line_comment",
                "block_comment",
            ]
            .iter(),
        ) {
            ensure!(
                grammar.id_for_node_kind(kind, true) != 0,
                "Rust grammar missing {kind}"
            );
        }
        for field in ["body", "name", "operator", "pattern", "alternative"] {
            ensure!(
                grammar.field_id_for_name(field).is_some(),
                "Rust grammar missing {field} field"
            );
        }
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .context("Loading Rust grammar")?;
        Ok(Self { parser })
    }
}

fn wildcard_match_arm(node: Node<'_>) -> bool {
    let Some(mut pattern) = node.child_by_field_name("pattern") else {
        return false;
    };
    loop {
        if pattern.kind() == "_" {
            return true;
        }
        if pattern.child_count() != 1 {
            return false;
        }
        pattern = pattern.child(0).expect("single pattern child");
    }
}

impl SyntaxRules for RustAdapter {
    fn callable(node: Node<'_>) -> bool {
        matches!(node.kind(), "function_item" | "closure_expression")
            && node.child_by_field_name("body").is_some()
    }

    fn decisions(node: Node<'_>) -> u64 {
        u64::from(
            DECISIONS.contains(&node.kind())
                || (node.kind() == "match_arm" && !wildcard_match_arm(node))
                || (node.kind() == "let_declaration"
                    && node.child_by_field_name("alternative").is_some())
                || (node.kind() == "binary_expression"
                    && node
                        .child_by_field_name("operator")
                        .is_some_and(|operator| matches!(operator.kind(), "&&" | "||"))),
        )
    }

    fn name(node: Node<'_>, source: &[u8]) -> String {
        let named = node.child_by_field_name("name").or_else(|| {
            let parent = node.parent()?;
            (parent.kind() == "let_declaration")
                .then(|| parent.child_by_field_name("pattern"))
                .flatten()
                .filter(|pattern| pattern.kind() == "identifier")
        });
        named
            .and_then(|name| name.utf8_text(source).ok())
            .unwrap_or("<closure>")
            .to_owned()
    }

    fn ignored(node: Node<'_>, _source: &[u8]) -> bool {
        matches!(node.kind(), "line_comment" | "block_comment")
    }
}

impl LanguageAdapter for RustAdapter {
    fn analyze(&mut self, source: &[u8]) -> Result<FileAnalysis> {
        tree::analyze::<Self>(&mut self.parser, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> FileAnalysis {
        let result = RustAdapter::new()
            .unwrap()
            .analyze(source.as_bytes())
            .unwrap();
        assert!(result.parsed(), "{:?}", result.diagnostics);
        result
    }

    #[test]
    fn functions_closures_and_control_flow() {
        let file = parse(
            "fn outer(xs: &[i32]) -> i32 {\n    let choose = |x| if x > 0 { x } else { 0 };\n    for x in xs { if *x > 1 && ready() { work(); } }\n    match xs.first() { Some(x) => *x, None => 0 }\n}\n",
        );
        assert_eq!(file.functions.len(), 2);
        assert_eq!(file.functions[0].name, "outer");
        assert_eq!(file.functions[0].complexity, 7);
        assert_eq!(file.functions[1].name, "choose");
        assert_eq!(file.functions[1].complexity, 2);
    }

    #[test]
    fn wildcard_arms_let_else_comments_and_failures() {
        let file = parse(
            "fn f(value: Option<i32>) -> i32 {\n    // ignored\n    let Some(x) = value else { return 0; };\n    match x { 1 => 1, _ => 0 }\n}\n",
        );
        assert_eq!(file.functions[0].complexity, 3);
        assert_eq!(file.functions[0].source_lines, 3);
        let bad = RustAdapter::new()
            .unwrap()
            .analyze(b"fn broken( {")
            .unwrap();
        assert!(!bad.parsed());
        assert!(bad.functions.is_empty());
    }
}
