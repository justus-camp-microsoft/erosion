use anyhow::{Context, Result, ensure};
use tree_sitter::{Node, Parser};

use super::{
    LanguageAdapter,
    tree::{self, SyntaxRules},
};
use crate::metrics::FileAnalysis;

const DECISIONS: &[&str] = &[
    "if_statement",
    "elif_clause",
    "for_statement",
    "while_statement",
    "except_clause",
    "conditional_expression",
    "assert_statement",
    "for_in_clause",
    "if_clause",
    "boolean_operator",
];

pub struct PythonAdapter {
    parser: Parser,
}

impl PythonAdapter {
    pub fn new() -> Result<Self> {
        let grammar = tree_sitter_python::LANGUAGE.into();
        let mut parser = Parser::new();
        parser
            .set_language(&grammar)
            .context("Loading Python grammar")?;
        for kind in DECISIONS.iter().chain(
            [
                "function_definition",
                "lambda",
                "case_clause",
                "else_clause",
                "block",
                "expression_statement",
                "string",
                "string_start",
                "comment",
            ]
            .iter(),
        ) {
            ensure!(
                grammar.id_for_node_kind(kind, true) != 0,
                "Python grammar missing {kind}"
            );
        }
        ensure!(
            grammar.field_id_for_name("body").is_some(),
            "Python grammar missing callable body"
        );
        Ok(Self { parser })
    }
}

// A bare capture, wildcard, or parenthesized/as-bound version is a catch-all.
// Sequence, mapping, class, literal and OR patterns remain conditional cases.
fn irrefutable(mut pattern: Node<'_>) -> bool {
    loop {
        match pattern.kind() {
            "_" | "identifier" => return true,
            "dotted_name" => return pattern.named_child_count() == 1,
            "case_pattern" | "as_pattern" => {
                let Some(child) = pattern.child(0) else {
                    return false;
                };
                pattern = child;
            }
            "tuple_pattern" => {
                let mut cursor = pattern.walk();
                if pattern.named_child_count() != 1
                    || pattern
                        .children(&mut cursor)
                        .any(|child| child.kind() == ",")
                {
                    return false;
                }
                pattern = pattern.named_child(0).expect("single grouped pattern");
            }
            _ => return false,
        }
    }
}

fn ordinary_string(node: Node<'_>, source: &[u8]) -> bool {
    let mut stack = vec![node];
    let mut found = false;
    while let Some(node) = stack.pop() {
        match node.kind() {
            "string" => {
                let Some(start) = node.child(0) else {
                    return false;
                };
                let prefix = &source[start.byte_range()];
                if prefix
                    .iter()
                    .any(|byte| matches!(byte, b'b' | b'B' | b'f' | b'F'))
                {
                    return false;
                }
                found = true;
            }
            "parenthesized_expression" | "concatenated_string" => {
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            "comment" | "line_continuation" => {}
            _ => return false,
        }
    }
    found
}

fn docstring(node: Node<'_>, source: &[u8]) -> bool {
    if node.kind() != "expression_statement" || node.named_child_count() != 1 {
        return false;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    let at_scope_start = parent.kind() == "module"
        || (parent.kind() == "block"
            && parent.parent().is_some_and(|owner| {
                matches!(owner.kind(), "function_definition" | "class_definition")
            }));
    if !at_scope_start {
        return false;
    }
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        if !matches!(sibling.kind(), "comment" | "line_continuation") {
            return false;
        }
        previous = sibling.prev_named_sibling();
    }
    ordinary_string(node.named_child(0).expect("one expression"), source)
}

impl SyntaxRules for PythonAdapter {
    fn callable(node: Node<'_>) -> bool {
        matches!(node.kind(), "function_definition" | "lambda")
            && node.child_by_field_name("body").is_some()
    }

    fn decisions(node: Node<'_>) -> u64 {
        if DECISIONS.contains(&node.kind()) {
            return 1;
        }
        if node.kind() == "else_clause"
            && node.parent().is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    "for_statement" | "while_statement" | "try_statement"
                )
            })
        {
            return 1;
        }
        if node.kind() == "case_clause" {
            let mut cursor = node.walk();
            let mut patterns = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "case_pattern");
            let default = patterns.next().is_some_and(irrefutable) && patterns.next().is_none();
            return u64::from(!default);
        }
        0
    }

    fn name(node: Node<'_>, source: &[u8]) -> String {
        let name = node.child_by_field_name("name").or_else(|| {
            let parent = node.parent()?;
            (parent.kind() == "assignment")
                .then(|| parent.child_by_field_name("left"))
                .flatten()
        });
        name.and_then(|node| node.utf8_text(source).ok())
            .unwrap_or("<lambda>")
            .to_owned()
    }

    fn ignored(node: Node<'_>, source: &[u8]) -> bool {
        node.kind() == "comment" || docstring(node, source)
    }

    fn validation_error(node: Node<'_>) -> Option<&'static str> {
        // The bundled grammar permits empty suites during error recovery without
        // necessarily setting has_error(), but they are not valid Python bodies.
        let mut cursor = node.walk();
        (node.kind() == "block"
            && node
                .named_children(&mut cursor)
                .all(|child| matches!(child.kind(), "comment" | "line_continuation")))
        .then_some("Expected a non-empty Python body")
    }
}

impl LanguageAdapter for PythonAdapter {
    fn analyze(&mut self, source: &[u8]) -> Result<FileAnalysis> {
        tree::analyze::<Self>(&mut self.parser, source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> FileAnalysis {
        let result = PythonAdapter::new()
            .unwrap()
            .analyze(source.as_bytes())
            .unwrap();
        assert!(result.parsed(), "{:?}", result.diagnostics);
        result
    }

    #[test]
    fn functions_async_methods_decorators_lambdas_and_nested_bodies() {
        let file = parse(
            "@decorator\nasync def outer(x):\n    def inner(y):\n        if y:\n            return y\n    return inner(x)\n\nclass C:\n    def method(self): return 1\n\ntransform = lambda x: x if x else 0\n",
        );
        assert_eq!(file.functions.len(), 4);
        assert_eq!(
            file.functions
                .iter()
                .map(|f| f.complexity)
                .collect::<Vec<_>>(),
            [2, 2, 1, 2]
        );
        assert_eq!(file.functions[0].start_line, 2);
        assert_eq!(file.functions[0].source_lines, 5);
        assert_eq!(file.functions[3].name, "transform");
        assert_eq!(
            parse("values = map(lambda x: x + 1, xs)\n").functions[0].name,
            "<lambda>"
        );
    }

    #[test]
    fn branches_booleans_handlers_and_loop_else() {
        let file = parse(
            "def f(x):\n    if x and x or x: return 1\n    elif not x: return 2\n    else: pass\n    for y in x: pass\n    else: pass\n    while x: break\n    else: pass\n    try: work()\n    except ValueError: pass\n    except TypeError: pass\n    else: pass\n    finally: pass\n    assert x\n    with context(): pass\n",
        );
        assert_eq!(file.functions[0].complexity, 13);
        assert_eq!(
            parse("def f():\n    try: work()\n    except* ValueError: recover()\n").functions[0]
                .complexity,
            2
        );
    }

    #[test]
    fn comprehensions_generators_and_async_loops() {
        let file = parse(
            "async def f(xs):\n    async for x in xs:\n        yield x\n    async with context(): pass\n    return [x for x in xs if x and ready(x) for y in xs if y]\n",
        );
        assert_eq!(file.functions[0].complexity, 7);
        assert_eq!(
            parse("def f(xs):\n    return {x: x for x in xs}, {x for x in xs}, (x for x in xs)\n")
                .functions[0]
                .complexity,
            4
        );
        assert_eq!(
            parse("async def f(xs):\n    return [x async for x in xs if x]\n").functions[0]
                .complexity,
            3
        );
    }

    #[test]
    fn match_cases_guards_and_catch_all_patterns() {
        let file = parse(
            "def f(x):\n    match x:\n        case 1 | 2: return 1\n        case str() if x and ready(x): return 2\n        case _: return 0\n",
        );
        assert_eq!(file.functions[0].complexity, 5);
        for pattern in ["_", "captured", "(_)", "captured as alias"] {
            assert_eq!(
                parse(&format!(
                    "def f(x):\n    match x:\n        case {pattern}: return 0\n"
                ))
                .functions[0]
                    .complexity,
                1,
                "{pattern}"
            );
        }
        assert_eq!(parse("def f(x):\n    match x:\n        case (a,): return 1\n        case Color.RED: return 2\n").functions[0].complexity, 3);
        assert_eq!(
            parse(
                "def f(x):\n    match x:\n        case _ if x: return 1\n        case _: return 0\n"
            )
            .functions[0]
                .complexity,
            2
        );
    }

    #[test]
    fn docstrings_are_not_data_strings() {
        let file = parse(
            "\"\"\"module\ntext\"\"\"\n# comment\nclass C:\n    'class docs'\n    def f(self):\n        # comment before docstring\n        r\"\"\"function\n        docs\"\"\"\n        value = \"\"\"data\n        retained\"\"\"\n        return value\n",
        );
        assert_eq!(file.source_lines, 5);
        assert_eq!(file.functions[0].source_lines, 4);
        let file = parse("def f():\n    ('doc' 'string')\n    'later expression'\n    return 0\n");
        assert_eq!(file.functions[0].source_lines, 3);
        for literal in ["b'bytes'", "f'{condition if condition else other}'"] {
            let file = parse(&format!("def f():\n    {literal}\n    return 0\n"));
            assert_eq!(file.functions[0].source_lines, 3);
        }
        assert_eq!(
            parse("def f(): 'doc'; return 0\n").functions[0].source_lines,
            1
        );
        assert_eq!(parse("if True:\n    'not a docstring'\n").source_lines, 2);
        assert_eq!(
            parse("def f():\n    (u'first'\n     # between literals\n     'second') # trailing comment\n    return 0\n")
                .functions[0].source_lines,
            2
        );
    }

    #[test]
    fn encoding_line_endings_and_failures() {
        for source in [
            "\u{feff}def f():\r\n    # ignored\r\n    return 1\r\n",
            "def f():\n    # ignored\n    return 1",
        ] {
            let file = parse(source);
            assert_eq!(
                (file.physical_lines, file.functions[0].source_lines),
                (3, 2)
            );
        }
        let empty = parse("# only a comment\n");
        assert_eq!(empty.source_lines, 0);
        assert!(empty.functions.is_empty());
        let mut adapter = PythonAdapter::new().unwrap();
        for bad in [
            b"def broken(:\n".as_slice(),
            b"def f():\n  return \xff",
            b"def f():\n",
            b"def f():\nreturn 1\n",
            b"def f():\n    # no body\n",
            b"if True:\n",
        ] {
            let result = adapter.analyze(bad).unwrap();
            assert!(!result.parsed(), "{bad:?}");
            assert!(result.functions.is_empty());
        }
    }
}
