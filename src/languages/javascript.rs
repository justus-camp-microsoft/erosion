use anyhow::{Context, Result, bail, ensure};
use tree_sitter::{Node, Parser};

use super::tree::{self, SyntaxRules};
use super::{Language, LanguageAdapter};
use crate::metrics::FileAnalysis;

const CALLABLES: &[&str] = &[
    "function_declaration",
    "function_expression",
    "generator_function_declaration",
    "generator_function",
    "arrow_function",
    "method_definition",
];
const DECISIONS: &[&str] = &[
    "catch_clause",
    "if_statement",
    "switch_statement",
    "ternary_expression",
    "do_statement",
    "for_in_statement",
    "for_statement",
    "while_statement",
];

pub struct JavaScriptAdapter {
    parser: Parser,
}

impl JavaScriptAdapter {
    pub fn new(language: Language) -> Result<Self> {
        let grammar: tree_sitter::Language = match language {
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::Python => bail!("Python requires its own language adapter"),
            Language::Rust => bail!("Rust requires its own language adapter"),
            Language::Gleam => bail!("Gleam requires its own language adapter"),
        };
        for kind in CALLABLES
            .iter()
            .chain(DECISIONS)
            .chain(["binary_expression", "comment"].iter())
        {
            ensure!(
                grammar.id_for_node_kind(kind, true) != 0,
                "Grammar contract changed: {} has no {kind}",
                language.id()
            );
        }
        for field in ["body", "operator"] {
            ensure!(
                grammar.field_id_for_name(field).is_some(),
                "Grammar missing {field} field"
            );
        }
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .context("Loading bundled grammar")?;
        Ok(Self { parser })
    }
}

fn name(node: Node<'_>, source: &[u8]) -> String {
    let named = node.child_by_field_name("name").or_else(|| {
        let parent = node.parent()?;
        (parent.kind() == "variable_declarator")
            .then(|| parent.child_by_field_name("name"))
            .flatten()
    });
    named
        .and_then(|node| node.utf8_text(source).ok())
        .unwrap_or("<anonymous>")
        .to_owned()
}

fn own_decisions(node: Node<'_>) -> u64 {
    u64::from(DECISIONS.contains(&node.kind()))
        + u64::from(
            node.kind() == "binary_expression"
                && node
                    .child_by_field_name("operator")
                    .is_some_and(|operator| matches!(operator.kind(), "&&" | "||")),
        )
}

impl SyntaxRules for JavaScriptAdapter {
    fn callable(node: Node<'_>) -> bool {
        CALLABLES.contains(&node.kind()) && node.child_by_field_name("body").is_some()
    }

    fn decisions(node: Node<'_>) -> u64 {
        own_decisions(node)
    }

    fn name(node: Node<'_>, source: &[u8]) -> String {
        name(node, source)
    }
}

impl LanguageAdapter for JavaScriptAdapter {
    fn analyze(&mut self, source: &[u8]) -> Result<FileAnalysis> {
        tree::analyze::<Self>(&mut self.parser, source)
    }
}

#[cfg(test)]
mod tests {
    use super::tree::source_line;
    use super::*;

    fn parse(source: &str, language: Language) -> FileAnalysis {
        let result = JavaScriptAdapter::new(language)
            .unwrap()
            .analyze(source.as_bytes())
            .unwrap();
        assert!(result.parsed(), "{:?}", result.diagnostics);
        result
    }

    #[test]
    fn all_grammars_and_callable_forms() {
        for language in [Language::JavaScript, Language::TypeScript, Language::Tsx] {
            let file = parse(
                "const a = function(x) { return x ? 1 : 0; };\nconst b = () => 2;\n[1].map(x => x + 1);",
                language,
            );
            assert_eq!(file.functions.len(), 3);
            assert_eq!(file.functions[0].complexity, 2);
            assert_eq!(parse("function* g() { yield 1; }\nconst g2 = function*() { yield 2; };\nclass C { method() { return 1; } }", language).functions.len(), 3);
        }
        assert_eq!(
            parse(
                "const View = () => <div>{true ? 'a' : 'b'}</div>;",
                Language::Tsx
            )
            .functions[0]
                .complexity,
            2
        );
        assert_eq!(
            parse("const View = () => <div>Hello</div>;", Language::JavaScript)
                .functions
                .len(),
            1
        );
        assert_eq!(
            parse(
                "function f(x: boolean) {\n if (x) return 1;\n return 0;\n}\n",
                Language::TypeScript
            )
            .functions[0]
                .source_lines,
            3
        );
    }

    #[test]
    fn nested_and_operator_conventions() {
        let nested = parse(
            "function outer() {\n function inner(x) { if (x) return 1; }\n return inner(0);\n}",
            Language::JavaScript,
        );
        assert_eq!(
            nested
                .functions
                .iter()
                .map(|f| f.complexity)
                .collect::<Vec<_>>(),
            [2, 2]
        );
        assert_eq!(nested.functions[0].source_lines, 3);
        let file = parse(
            "function f(x) { switch(x) { case 1: break; case 2: break; } return x && x || x ?? x?.y; }",
            Language::TypeScript,
        );
        assert_eq!(file.functions[0].complexity, 4);
        let loops = parse(
            "function f(xs) { for (const x of xs) {} for (const x in xs) {} for (;;) { break; } while (false) {} do {} while(false); try {} catch(e) {} return xs ? 1 : 0; }",
            Language::JavaScript,
        );
        assert_eq!(loops.functions[0].complexity, 8);
    }

    #[test]
    fn comments_punctuation_bom_and_line_endings() {
        assert!(!source_line(b"\x0b\t\x0c"));
        assert!(!source_line(b"\x0b}\x0c"));
        assert!(source_line("\u{a0}".as_bytes()));
        let file = parse(
            "// comment\nconst x = 1; /* note */\n/* multi\nline */\n",
            Language::JavaScript,
        );
        assert_eq!((file.physical_lines, file.source_lines), (4, 1));
        for ending in ["\n", "\r\n"] {
            let source = ["function f() {", "/* comment */", "return 1;", "}", ""].join(ending);
            let file = parse(&source, Language::JavaScript);
            assert_eq!(
                (file.physical_lines, file.functions[0].source_lines),
                (4, 2)
            );
        }
        let file = parse("\u{feff}const f = () => 1;", Language::TypeScript);
        assert_eq!(file.functions.len(), 1);
        assert_eq!(parse("", Language::TypeScript).physical_lines, 0);
        assert_eq!(
            parse("let x = 1;\n", Language::TypeScript).physical_lines,
            1
        );
    }

    #[test]
    fn invalid_input_is_not_success() {
        let mut adapter = JavaScriptAdapter::new(Language::TypeScript).unwrap();
        for source in [b"function {".as_slice(), b"\xff"] {
            let result = adapter.analyze(source).unwrap();
            assert!(!result.parsed());
            assert!(result.functions.is_empty());
        }
    }
}
