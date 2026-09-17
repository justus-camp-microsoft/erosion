use anyhow::{Context, Result, ensure};
use tree_sitter::{Node, Parser};

use super::{
    LanguageAdapter,
    tree::{self, SyntaxRules},
};
use crate::metrics::FileAnalysis;

pub struct GleamAdapter {
    parser: Parser,
}

impl GleamAdapter {
    pub fn new() -> Result<Self> {
        let grammar: tree_sitter::Language = tree_sitter_gleam::LANGUAGE.into();
        for kind in [
            "function",
            "anonymous_function",
            "case_clause",
            "case_clause_patterns",
            "case_clause_pattern",
            "binary_expression",
            "assert",
            "let_assert",
            "module_comment",
            "statement_comment",
            "comment",
        ] {
            ensure!(
                grammar.id_for_node_kind(kind, true) != 0,
                "Gleam grammar missing {kind}"
            );
        }
        for field in ["body", "name", "operator", "patterns", "guard"] {
            ensure!(
                grammar.field_id_for_name(field).is_some(),
                "Gleam grammar missing {field} field"
            );
        }
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .context("Loading Gleam grammar")?;
        Ok(Self { parser })
    }
}

fn irrefutable_clause(node: Node<'_>) -> bool {
    if node.child_by_field_name("guard").is_some() {
        return false;
    }
    let Some(patterns) = node.child_by_field_name("patterns") else {
        return false;
    };
    if patterns.named_child_count() != 1 {
        return false;
    }
    let Some(pattern) = patterns.named_child(0) else {
        return false;
    };
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "case_clause_pattern" | "identifier" | "discard" => {}
            _ => return false,
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    true
}

impl SyntaxRules for GleamAdapter {
    fn callable(node: Node<'_>) -> bool {
        matches!(node.kind(), "function" | "anonymous_function")
            && node.child_by_field_name("body").is_some()
    }

    fn decisions(node: Node<'_>) -> u64 {
        u64::from(
            (node.kind() == "case_clause" && !irrefutable_clause(node))
                || (matches!(node.kind(), "assert" | "let_assert") && node.is_named())
                || (node.kind() == "binary_expression"
                    && node
                        .child_by_field_name("operator")
                        .is_some_and(|operator| matches!(operator.kind(), "&&" | "||"))),
        )
    }

    fn name(node: Node<'_>, source: &[u8]) -> String {
        node.child_by_field_name("name")
            .and_then(|name| name.utf8_text(source).ok())
            .unwrap_or("<anonymous>")
            .to_owned()
    }

    fn ignored(node: Node<'_>, _source: &[u8]) -> bool {
        matches!(
            node.kind(),
            "module_comment" | "statement_comment" | "comment"
        )
    }
}

impl LanguageAdapter for GleamAdapter {
    fn analyze(&mut self, source: &[u8]) -> Result<FileAnalysis> {
        tree::analyze::<Self>(&mut self.parser, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> FileAnalysis {
        let result = GleamAdapter::new()
            .unwrap()
            .analyze(source.as_bytes())
            .unwrap();
        assert!(result.parsed(), "{:?}", result.diagnostics);
        result
    }

    #[test]
    fn functions_anonymous_functions_cases_and_booleans() {
        let file = parse(
            "pub fn choose(value) {\n  let mapper = fn(x) { x && value || False }\n  case value {\n    True if value.status.ready == True -> mapper(True)\n    False -> False\n    other -> other\n  }\n}\n",
        );
        assert_eq!(file.functions.len(), 2);
        assert_eq!(file.functions[0].name, "choose");
        assert_eq!(file.functions[0].complexity, 5);
        assert_eq!(file.functions[1].name, "<anonymous>");
        assert_eq!(file.functions[1].complexity, 3);
    }

    #[test]
    fn assertions_comments_and_failures() {
        let file = parse(
            "fn validate(value) {\n  // ignored\n  let assert Ok(x) = value\n  assert x == 1\n  x\n}\n",
        );
        assert_eq!(file.functions[0].complexity, 3);
        assert_eq!(file.functions[0].source_lines, 4);
        let bad = GleamAdapter::new()
            .unwrap()
            .analyze(b"fn broken( {")
            .unwrap();
        assert!(!bad.parsed());
        assert!(bad.functions.is_empty());
    }
}
