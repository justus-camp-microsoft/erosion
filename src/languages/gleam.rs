use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use tree_sitter::{Node, Parser};

use super::{
    LanguageAdapter, LanguageTestPolicy,
    tree::{self, LanguageRules, SyntaxRules, TestDetector},
};
use crate::metrics::TestRegion;

const TEST_PATHS: &[&str] = &["**/test/**", "**/tests/**", "**/*_test.gleam"];
const TEST_SYNTAX: &[&str] = &[
    "Public zero-argument *_test functions follow gleeunit/EUnit discovery; public zero-argument *_test_ generators follow Erlang EUnit, independent of imports or file name.",
    "Literal gleeunit.main() calls require an unshadowed import, including module and selective-import aliases; a named zero-argument function whose only expression is that call is a test runner unless it has an external override.",
    "Other functions retain their production body around a recognized runner call; piped/use calls with implicit arguments are not zero-argument runners.",
    "Private functions/constants and known-API gleeunit or gleeunit/should imports are excluded only with at least one test reference and no references outside test regions or their own declaration; shared, exported, unused and mutually recursive helpers remain.",
    "Declaration regions include attached attributes and documentation. Assertions, should calls and helper/fixture names alone never establish a test.",
    "gleeunit exposes no callback-registration or fixture DSL. Custom frameworks, dynamic/local runner aliases, cross-file wrappers and ambiguous bindings remain included; target-specific duplicate bindings are not guessed.",
];

struct GleamRules;

struct GleamTestDetector {
    test_paths: tree::TestPaths,
}

pub fn adapter(exclude_tests: bool) -> Result<Box<dyn LanguageAdapter>> {
    tree::adapter::<GleamRules>(parser()?, exclude_tests)
}

fn parser() -> Result<Parser> {
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
    Ok(parser)
}

fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source)
        .expect("test discovery follows UTF-8 and syntax validation")
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn is_public(node: Node<'_>) -> bool {
    named_children(node)
        .iter()
        .any(|child| child.kind() == "visibility_modifier")
}

fn zero_parameters(node: Node<'_>) -> bool {
    node.child_by_field_name("parameters")
        .is_some_and(|parameters| {
            !named_children(parameters)
                .iter()
                .any(|child| child.kind() == "function_parameter")
        })
}

fn declaration_region(node: Node<'_>, reason: &str) -> TestRegion {
    let mut region = tree::test_region(node, reason);
    let mut previous = node.prev_named_sibling();
    while let Some(node) = previous {
        if !matches!(node.kind(), "attribute" | "statement_comment" | "comment") {
            break;
        }
        region.start_byte = node.start_byte();
        region.start_line = node.start_position().row + 1;
        previous = node.prev_named_sibling();
    }
    region
}

fn contains(region: &TestRegion, byte: usize) -> bool {
    region.start_byte <= byte && byte < region.end_byte
}

#[derive(Clone, Copy, Default, PartialEq)]
enum RunnerBinding {
    #[default]
    Unknown,
    Module,
    Main,
}

#[derive(Clone, Copy, Default)]
struct Binding {
    runner: RunnerBinding,
    declaration: Option<usize>,
}

struct Declaration<'tree> {
    node: Node<'tree>,
    helper: bool,
    references: Vec<usize>,
}

struct TestDiscovery<'tree, 'source> {
    source: &'source [u8],
    module: HashMap<&'source str, Binding>,
    scopes: Vec<HashMap<&'source str, Binding>>,
    declarations: Vec<Declaration<'tree>>,
    regions: Vec<TestRegion>,
}

enum Visit<'tree> {
    Node(Node<'tree>),
    BindPattern(Node<'tree>),
    LeaveScope,
}

impl<'tree, 'source> TestDiscovery<'tree, 'source> {
    fn new(root: Node<'tree>, source: &'source [u8]) -> Self {
        let mut discovery = Self {
            source,
            module: HashMap::new(),
            scopes: Vec::new(),
            declarations: Vec::new(),
            regions: Vec::new(),
        };
        let mut stack = named_children(root);
        while let Some(node) = stack.pop() {
            if node.kind() == "target_group" {
                stack.extend(named_children(node));
                continue;
            }
            if node.kind() == "import" {
                discovery.import(node);
            } else if matches!(node.kind(), "function" | "external_function" | "constant")
                && let Some(name) = node.child_by_field_name("name")
            {
                let public = is_public(node);
                let index = discovery.declarations.len();
                discovery.declarations.push(Declaration {
                    node,
                    helper: !public
                        && (node.kind() == "constant"
                            || (node.kind() == "function"
                                && node.child_by_field_name("body").is_some())),
                    references: Vec::new(),
                });
                discovery.module_binding(
                    text(name, source),
                    Binding {
                        declaration: Some(index),
                        ..Binding::default()
                    },
                );
                // EUnit discovers exported zero-arity functions without an
                // assertion import; generators use the additional trailing '_'.
                // https://www.erlang.org/doc/apps/eunit/chapter.html
                if matches!(node.kind(), "function" | "external_function")
                    && public
                    && zero_parameters(node)
                    && (text(name, source).ends_with("_test")
                        || text(name, source).ends_with("_test_"))
                {
                    discovery
                        .regions
                        .push(declaration_region(node, "Gleam public EUnit test function"));
                }
            }
        }
        discovery
    }

    fn module_binding(&mut self, name: &'source str, binding: Binding) {
        // Target-specific declarations can share a spelling. Without choosing a
        // compilation target, neither binding is a safe framework resolution.
        self.module
            .entry(name)
            .and_modify(|existing| *existing = Binding::default())
            .or_insert(binding);
    }

    fn import(&mut self, node: Node<'tree>) {
        let Some(module) = node.child_by_field_name("module") else {
            return;
        };
        let module = text(module, self.source);
        let declaration = if matches!(module, "gleeunit" | "gleeunit/should") {
            let index = self.declarations.len();
            self.declarations.push(Declaration {
                node,
                helper: true,
                references: Vec::new(),
            });
            Some(index)
        } else {
            None
        };
        let alias = node
            .child_by_field_name("alias")
            .map(|alias| text(alias, self.source))
            .unwrap_or_else(|| module.rsplit('/').next().expect("nonempty module"));
        if !alias.starts_with('_') {
            self.module_binding(
                alias,
                Binding {
                    runner: if module == "gleeunit" {
                        RunnerBinding::Module
                    } else {
                        RunnerBinding::Unknown
                    },
                    declaration,
                },
            );
        }
        if let Some(imports) = node.child_by_field_name("imports") {
            for import in named_children(imports) {
                let Some(name) = import.child_by_field_name("name") else {
                    continue;
                };
                let known = match module {
                    "gleeunit" => text(name, self.source) == "main",
                    "gleeunit/should" => matches!(
                        text(name, self.source),
                        "equal"
                            | "not_equal"
                            | "be_ok"
                            | "be_error"
                            | "be_some"
                            | "be_none"
                            | "be_true"
                            | "be_false"
                            | "fail"
                    ),
                    _ => false,
                };
                if let Some(index) = declaration
                    && (!known || name.kind() != "identifier")
                {
                    // Do not erase an import with exports we cannot account
                    // for, particularly type uses outside the value namespace.
                    self.declarations[index].helper = false;
                }
                if name.kind() != "identifier" {
                    continue;
                }
                let alias = import.child_by_field_name("alias").unwrap_or(name);
                self.module_binding(
                    text(alias, self.source),
                    Binding {
                        runner: if module == "gleeunit" && text(name, self.source) == "main" {
                            RunnerBinding::Main
                        } else {
                            RunnerBinding::Unknown
                        },
                        declaration,
                    },
                );
            }
        }
    }

    fn binding(&self, name: &str) -> Binding {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .or_else(|| self.module.get(name))
            .copied()
            .unwrap_or_default()
    }

    fn reference(&mut self, node: Node<'tree>) {
        if let Some(index) = self.binding(text(node, self.source)).declaration {
            self.declarations[index].references.push(node.start_byte());
        }
    }

    fn bind_pattern(&mut self, pattern: Node<'tree>) {
        let mut stack = vec![pattern];
        while let Some(node) = stack.pop() {
            if node.kind() == "bit_array_segment_options" {
                // size(existing_value) reads the outer scope, not a new binder.
                continue;
            }
            if node.kind() == "identifier"
                || (node.kind() == "label"
                    && node.parent().is_some_and(|parent| {
                        parent.kind() == "record_pattern_argument"
                            && parent.child_by_field_name("pattern").is_none()
                    }))
            {
                self.scopes
                    .last_mut()
                    .expect("pattern scope")
                    .insert(text(node, self.source), Binding::default());
            } else {
                stack.extend(named_children(node));
            }
        }
    }

    fn runner_call(&self, node: Node<'tree>) -> bool {
        let Some(function) = node.child_by_field_name("function") else {
            return false;
        };
        let Some(arguments) = node.child_by_field_name("arguments") else {
            return false;
        };
        if named_children(arguments)
            .iter()
            .any(|child| child.kind() == "argument")
        {
            return false;
        }
        // Pipeline and use desugaring supply an argument absent from arguments.
        if node.parent().is_some_and(|parent| {
            (parent.kind() == "use" && parent.child_by_field_name("value") == Some(node))
                || (parent.kind() == "binary_expression"
                    && parent.child_by_field_name("right") == Some(node)
                    && parent
                        .child_by_field_name("operator")
                        .is_some_and(|operator| operator.kind() == "|>"))
        }) {
            return false;
        }
        if function.kind() == "identifier" {
            self.binding(text(function, self.source)).runner == RunnerBinding::Main
        } else if function.kind() == "field_access" {
            function
                .child_by_field_name("record")
                .is_some_and(|record| {
                    record.kind() == "identifier"
                        && self.binding(text(record, self.source)).runner == RunnerBinding::Module
                })
                && function
                    .child_by_field_name("field")
                    .is_some_and(|field| text(field, self.source) == "main")
        } else {
            false
        }
    }

    fn discover(mut self, root: Node<'tree>) -> Vec<TestRegion> {
        let mut stack = vec![Visit::Node(root)];
        while let Some(visit) = stack.pop() {
            let node = match visit {
                Visit::Node(node) => node,
                Visit::BindPattern(pattern) => {
                    self.bind_pattern(pattern);
                    continue;
                }
                Visit::LeaveScope => {
                    self.scopes.pop().expect("matching scope");
                    continue;
                }
            };
            match node.kind() {
                "import" | "external_function" => continue,
                "function" | "anonymous_function" => {
                    self.scopes.push(HashMap::new());
                    stack.push(Visit::LeaveScope);
                    if let Some(parameters) = node.child_by_field_name("parameters") {
                        for parameter in named_children(parameters) {
                            if let Some(name) = parameter.child_by_field_name("name") {
                                self.bind_pattern(name);
                            }
                        }
                    }
                    if let Some(body) = node.child_by_field_name("body") {
                        stack.push(Visit::Node(body));
                    }
                    continue;
                }
                "source_file" | "block" | "case_clause" => {
                    self.scopes.push(HashMap::new());
                    stack.push(Visit::LeaveScope);
                    if node.kind() == "case_clause" {
                        if let Some(value) = node.child_by_field_name("value") {
                            stack.push(Visit::Node(value));
                        }
                        if let Some(guard) = node.child_by_field_name("guard") {
                            stack.push(Visit::Node(guard));
                        }
                        if let Some(patterns) = node.child_by_field_name("patterns") {
                            stack.push(Visit::BindPattern(patterns));
                            self.pattern_reads(patterns, &mut stack);
                        }
                        continue;
                    }
                }
                "constant" => {
                    if let Some(value) = node.child_by_field_name("value") {
                        stack.push(Visit::Node(value));
                    }
                    continue;
                }
                "let" | "let_assert" | "use" => {
                    let pattern = node
                        .child_by_field_name("pattern")
                        .or_else(|| node.child_by_field_name("assignments"));
                    if let Some(pattern) = pattern {
                        stack.push(Visit::BindPattern(pattern));
                        // Pattern aliases (`pattern as name`) can be siblings
                        // of the field designated as the primary pattern.
                        let mut cursor = node.walk();
                        for alias in node.children_by_field_name("assign", &mut cursor) {
                            stack.push(Visit::BindPattern(alias));
                        }
                        self.pattern_reads(pattern, &mut stack);
                    }
                    if let Some(message) = node.child_by_field_name("message") {
                        stack.push(Visit::Node(message));
                    }
                    if let Some(value) = node.child_by_field_name("value") {
                        stack.push(Visit::Node(value));
                    }
                    continue;
                }
                "function_call" if self.runner_call(node) => {
                    let body = node.parent().filter(|parent| parent.kind() == "block");
                    let runner = body.and_then(|body| {
                        body.parent().filter(|function| {
                            function.kind() == "function"
                                && zero_parameters(*function)
                                && named_children(body)
                                    .iter()
                                    .filter(|child| !GleamRules::ignored(**child, self.source))
                                    .count()
                                    == 1
                                && !has_external_attribute(*function, self.source)
                        })
                    });
                    self.regions.push(if let Some(runner) = runner {
                        declaration_region(runner, "Gleam gleeunit main runner")
                    } else {
                        tree::test_region(node, "Gleam gleeunit main call")
                    });
                }
                "identifier" => self.reference(node),
                "field_access" => {
                    if let Some(record) = node.child_by_field_name("record") {
                        stack.push(Visit::Node(record));
                    }
                    continue;
                }
                "argument" | "record_update_argument"
                    if node.child_by_field_name("value").is_none() =>
                {
                    if let Some(label) = node.child_by_field_name("label") {
                        self.reference(label);
                    }
                    continue;
                }
                _ => {}
            }
            stack.extend(named_children(node).into_iter().rev().map(Visit::Node));
        }
        self.exclude_helpers();
        self.regions
    }

    fn pattern_reads(&self, pattern: Node<'tree>, visits: &mut Vec<Visit<'tree>>) {
        let mut stack = vec![pattern];
        while let Some(node) = stack.pop() {
            if node.kind() == "bit_array_segment_options" {
                visits.push(Visit::Node(node));
            } else {
                stack.extend(named_children(node));
            }
        }
    }

    fn exclude_helpers(&mut self) {
        // A fixed point admits helper chains and self-recursion, but not cycles
        // whose only proof of test ownership would be each other.
        loop {
            let mut changed = false;
            for declaration in &mut self.declarations {
                if !declaration.helper {
                    continue;
                }
                let region = declaration_region(declaration.node, "Gleam test-only dependency");
                let mut test_reference = false;
                let exclusively_tests = declaration.references.iter().all(|byte| {
                    if contains(&region, *byte) {
                        return true;
                    }
                    let excluded = self.regions.iter().any(|region| contains(region, *byte));
                    test_reference |= excluded;
                    excluded
                });
                if exclusively_tests && test_reference {
                    self.regions.push(region);
                    declaration.helper = false;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }
}

fn has_external_attribute(function: Node<'_>, source: &[u8]) -> bool {
    let mut previous = function.prev_named_sibling();
    while let Some(node) = previous {
        if !matches!(node.kind(), "attribute" | "statement_comment" | "comment") {
            break;
        }
        if node.kind() == "attribute"
            && node
                .child_by_field_name("name")
                .is_some_and(|name| text(name, source) == "external")
        {
            return true;
        }
        previous = node.prev_named_sibling();
    }
    false
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

impl SyntaxRules for GleamRules {
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

impl LanguageRules for GleamRules {
    type Tests = GleamTestDetector;
}

impl TestDetector for GleamTestDetector {
    fn new() -> Result<Self> {
        let grammar: tree_sitter::Language = tree_sitter_gleam::LANGUAGE.into();
        for kind in [
            "import",
            "unqualified_import",
            "function_call",
            "function_parameter",
            "argument",
            "visibility_modifier",
            "field_access",
            "constant",
            "attribute",
        ] {
            ensure!(
                grammar.id_for_node_kind(kind, true) != 0,
                "Gleam grammar missing {kind}"
            );
        }
        for field in [
            "module",
            "imports",
            "alias",
            "parameters",
            "arguments",
            "function",
            "record",
            "field",
            "pattern",
            "value",
            "assignments",
            "right",
        ] {
            ensure!(
                grammar.field_id_for_name(field).is_some(),
                "Gleam grammar missing {field} field"
            );
        }
        Ok(Self {
            test_paths: tree::TestPaths::new(TEST_PATHS)?,
        })
    }

    fn test_policy(&self) -> LanguageTestPolicy {
        LanguageTestPolicy {
            version: "gleam-tests-v1",
            path_patterns: TEST_PATHS,
            syntax_rules: TEST_SYNTAX,
        }
    }

    fn is_test_file(&self, path: &str) -> bool {
        self.test_paths.matches(path)
    }

    fn test_regions(&self, root: Node<'_>, source: &[u8]) -> Result<Vec<TestRegion>> {
        Ok(TestDiscovery::new(root, source).discover(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::FileAnalysis;

    struct RawGleam;

    impl SyntaxRules for RawGleam {
        fn callable(node: Node<'_>) -> bool {
            GleamRules::callable(node)
        }

        fn decisions(node: Node<'_>) -> u64 {
            GleamRules::decisions(node)
        }

        fn name(node: Node<'_>, source: &[u8]) -> String {
            GleamRules::name(node, source)
        }

        fn ignored(node: Node<'_>, source: &[u8]) -> bool {
            GleamRules::ignored(node, source)
        }
    }

    fn parse(source: &str) -> FileAnalysis {
        let result = adapter(true).unwrap().analyze(source.as_bytes()).unwrap();
        assert!(result.parsed(), "{:?}", result.diagnostics);
        let raw =
            tree::analyze::<RawGleam, _>(&mut parser().unwrap(), source.as_bytes(), &tree::AllCode)
                .unwrap();
        assert_eq!(
            adapter(false).unwrap().analyze(source.as_bytes()).unwrap(),
            raw
        );
        let mut unfiltered = result.clone();
        unfiltered.without_tests = None;
        assert_eq!(unfiltered, raw, "test detection changed raw analysis");
        result
    }

    fn remaining(file: &FileAnalysis) -> Vec<&str> {
        file.without_tests
            .as_ref()
            .expect("syntax test regions")
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect()
    }

    #[test]
    fn raw_mode_keeps_test_code_without_a_filtered_view() {
        let source = "\u{feff}import gleeunit\r\npub fn production() { 1 }\r\npub fn example_test() { assert True }\r\npub fn main() { gleeunit.main() }\r\n";
        let mut raw_adapter = adapter(false).unwrap();
        assert!(raw_adapter.test_policy().is_none());
        assert!(!raw_adapter.is_test_file("tests/example_test.gleam"));
        let raw = raw_adapter.analyze(source.as_bytes()).unwrap();
        assert!(raw.parsed(), "{:?}", raw.diagnostics);
        assert!(raw.without_tests.is_none());
        let mut filtered = parse(source);
        let without_tests = filtered.without_tests.take().expect("recognized tests");
        assert!(without_tests.functions.len() < raw.functions.len());
        assert!(without_tests.source_lines < raw.source_lines);
        assert_eq!(raw, filtered);
    }

    #[test]
    fn language_owned_path_policy_is_component_bounded_and_case_sensitive() {
        let adapter = adapter(true).unwrap();
        let policy = adapter.test_policy().unwrap();
        assert_eq!(policy.version, "gleam-tests-v1");
        assert_eq!(policy.path_patterns, TEST_PATHS);
        assert_eq!(policy.syntax_rules, TEST_SYNTAX);
        for path in [
            "test/helpers.gleam",
            "tests/fixtures/data.gleam",
            "packages/app/test/support/helper.gleam",
            "packages/app/tests/helper.gleam",
            "unit_test.gleam",
            "src/unusual_test.gleam",
        ] {
            assert!(adapter.is_test_file(path), "{path}");
        }
        for path in [
            "src/test.gleam",
            "src/tests.gleam",
            "contest/helper.gleam",
            "src/test_helpers.gleam",
            "src/unit_test.gleam.bak",
            "src/unit_Test.gleam",
            "src/unit_test.GLEAM",
            "Test/helper.gleam",
            "TESTS/helper.gleam",
        ] {
            assert!(!adapter.is_test_file(path), "{path}");
        }
    }

    #[test]
    fn mixed_production_tests_runner_and_fixture_dependencies() {
        let source = r#"//// Module documentation, not evidence.
import gleeunit as unit
import gleeunit/should.{equal as equals}

pub fn production(value) {
  // Production assertions remain.
  let assert Ok(value) = value
  assert value > 0
  value
}

/// The test and its nested callback are excluded.
pub fn production_test() {
  let fixture = setup()
  let check = fn() { fixture |> equals(3) }
  check()
}

fn setup() {
  case seed() {
    0 -> 3
    value -> value
  }
}
fn seed() { 0 }
fn unused() { Nil }

pub fn main() {
  unit.main()
}
"#;
        let file = parse(source);
        assert_eq!(remaining(&file), ["production", "unused"]);
        let filtered = file.without_tests.as_ref().unwrap();
        assert_eq!(filtered.functions[0], file.functions[0]);
        assert_eq!(filtered.functions[0].complexity, 3);
        assert_eq!(filtered.source_lines, 5);
        for fragment in [
            "import gleeunit as unit",
            "import gleeunit/should",
            "pub fn production_test",
            "fn setup",
            "fn seed",
            "pub fn main",
        ] {
            let start = source.find(fragment).unwrap();
            assert!(
                filtered
                    .regions
                    .iter()
                    .any(|region| contains(region, start))
            );
        }
    }

    #[test]
    fn eunit_conventions_require_public_zero_arity_not_assertion_evidence() {
        let file = parse(
            r#"pub fn no_assertions_test(// still zero arguments
) -> Int { 42 }
pub fn generator_test_() { [fn() { assert True }] }
fn private_test() { assert True }
pub fn argument_test(value) { assert value }
pub fn labelled_test(value value: Bool) { assert value }
pub fn discarded_test(_: Bool) { assert True }
pub fn almost_tests() { assert True }
pub fn test_prefix() { assert True }
"#,
        );
        assert_eq!(
            remaining(&file),
            [
                "private_test",
                "argument_test",
                "labelled_test",
                "discarded_test",
                "almost_tests",
                "test_prefix",
            ]
        );
    }

    #[test]
    fn external_test_declarations_and_target_groups_follow_eunit_names() {
        let file = parse(
            r#"@external(erlang, "foreign", "run")
pub fn foreign_test() -> Nil
if erlang {
  pub fn erlang_test_() { [fn() { assert True }] }
  pub fn production() { 1 }
}
if javascript {
  pub fn javascript_test() { assert True }
}
"#,
        );
        assert_eq!(remaining(&file), ["production"]);
        let filtered = file.without_tests.unwrap();
        assert!(
            filtered
                .regions
                .iter()
                .any(|region| region.start_byte == 0 && region.end_line == 2)
        );
    }

    #[test]
    fn runner_import_aliases_and_selective_imports() {
        for source in [
            "import gleeunit\npub fn main() { gleeunit.main() }",
            "import gleeunit as unit\npub fn main() { unit.main() }",
            "import gleeunit.{main as run}\npub fn main() { run() }",
            "import gleeunit.{main as run} as _\npub fn main() { run() }",
            "import gleeunit.{main}\npub fn execute() { main() }",
            "import gleeunit.{main as run} as unit\npub fn execute() { unit.main() }",
            "import gleeunit\nfn execute() { gleeunit.main(// no arguments\n) }",
        ] {
            let file = parse(source);
            assert!(remaining(&file).is_empty(), "{source}");
            assert_eq!(file.without_tests.unwrap().source_lines, 0, "{source}");
        }
    }

    #[test]
    fn assertions_comments_strings_and_unknown_frameworks_are_not_tests() {
        for source in [
            "pub fn validate(x) { assert x }\n",
            "pub fn validate(x) { let assert Ok(value) = x\nvalue }\n",
            "import gleeunit/should\npub fn validate(x) { should.equal(x, 1) }\n",
            "import gleeunit/should.{equal as same}\npub fn validate(x) { same(x, 1) }\n",
            "import gleam/should\npub fn validate(x) { should.be_true(x) }\n",
            "import business/gleeunit\npub fn main() { gleeunit.main() }\n",
            "import business as gleeunit\npub fn main() { gleeunit.main() }\n",
            "pub fn main() { gleeunit.main() }\n",
            "import gleeunit\npub fn main() { gleeunit.test(fn() { assert True }) }\n",
            "import gleeunit\npub fn main() { gleeunit.main(1) }\n",
            "import gleeunit\npub fn main() { Nil |> gleeunit.main() }\n",
            "import gleeunit\npub fn main() { use <- gleeunit.main()\nNil }\n",
            "import gleeunit\npub fn main() { let run = gleeunit.main\nrun() }\n",
            "import gleeunit\npub fn main() { custom_runner(gleeunit.main) }\n",
            "//// import gleeunit\n/// pub fn example_test() {}\n// gleeunit.main()\npub fn value() { \"pub fn x_test() { gleeunit.main() }\" }\n",
            "import gleeunit/should\nfn setup() { should.equal(1, 1) }\nfn teardown() { assert True }\n",
        ] {
            assert!(parse(source).without_tests.is_none(), "{source}");
        }
    }

    #[test]
    fn runner_resolution_respects_lexical_shadowing() {
        for source in [
            "import gleeunit\npub fn main(gleeunit) { gleeunit.main() }",
            "import gleeunit\npub fn main() { let gleeunit = production()\ngleeunit.main() }",
            "import gleeunit.{main as run}\npub fn main(run) { run() }",
            "import gleeunit.{main as run}\npub fn main() { let run = fn() { 1 }\nrun() }",
            "import gleeunit.{main as run}\npub fn main() { fn(run) { run() } }",
            "import gleeunit.{main as run}\npub fn main(value) { case value { Ok(run) -> run()\n_ -> Nil } }",
            "import gleeunit.{main as run}\npub fn main(value) { let assert Ok(run) = value\nrun() }",
            "import gleeunit.{main as run}\npub fn main(value) { let #(run, _) = value\nrun() }",
            "import gleeunit.{main as run}\npub fn main(value) { let value as run = value\nrun() }",
            "import gleeunit.{main as run}\npub fn main(value) { let Holder(run:) = value\nrun() }",
            "import gleeunit.{main as run}\npub fn main() { use run <- acquire()\nrun() }",
            "import gleeunit.{main as run}\nfn run() { 1 }\npub fn main() { run() }",
            "import gleeunit.{main as run}\nimport other.{run}\npub fn main() { run() }",
            "import gleeunit as unit\nimport other as unit\npub fn main() { unit.main() }",
        ] {
            assert!(parse(source).without_tests.is_none(), "{source}");
        }
    }

    #[test]
    fn target_dependent_runner_bindings_are_not_guessed() {
        for source in [
            "@target(erlang)\nimport gleeunit as runner\n@target(javascript)\nimport production as runner\npub fn main() { runner.main() }",
            "if erlang { import gleeunit as runner }\nif javascript { import production as runner }\npub fn main() { runner.main() }",
        ] {
            assert!(parse(source).without_tests.is_none(), "{source}");
        }
    }

    #[test]
    fn unknown_selective_exports_do_not_erase_imports() {
        for unknown in ["type FutureType", "future_api"] {
            let source =
                format!("import gleeunit.{{main as run, {unknown}}}\npub fn main() {{ run() }}");
            let file = parse(&source);
            assert!(remaining(&file).is_empty());
            assert_eq!(file.without_tests.unwrap().source_lines, 1);
        }
    }

    #[test]
    fn nested_runner_regions_reduce_only_the_enclosing_test_lines() {
        let source = r#"import gleeunit.{main as run}
pub fn application(flag) {
  case flag {
    True -> {
      run()
      assert flag
      1
    }
    False -> 0
  }
}
"#;
        let file = parse(source);
        assert_eq!(remaining(&file), ["application"]);
        let filtered = file.without_tests.as_ref().unwrap();
        assert_eq!(
            filtered.functions[0].complexity,
            file.functions[0].complexity
        );
        assert_eq!(
            filtered.functions[0].source_lines,
            file.functions[0].source_lines - 1
        );
        assert_eq!(filtered.source_lines, file.source_lines - 2);
        assert!(filtered.regions.iter().any(|region| {
            &source[region.start_byte..region.end_byte] == "run()"
                && region.reason == "Gleam gleeunit main call"
        }));
    }

    #[test]
    fn local_shadowing_does_not_leak_and_begins_after_the_initializer() {
        let source = r#"import gleeunit.{main as run}
pub fn application(value) {
  { let run = fn() { 1 }
    run() }
  case value {
    Ok(run) -> run()
    _ -> Nil
  }
  let run = run()
  run()
}
"#;
        let file = parse(source);
        assert_eq!(remaining(&file), ["application", "<anonymous>"]);
        let filtered = file.without_tests.as_ref().unwrap();
        let calls = filtered
            .regions
            .iter()
            .filter(|region| region.reason == "Gleam gleeunit main call")
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].start_byte, source.find("= run()").unwrap() + 2);
        // Masking an initializer does not erase the rest of its production line.
        assert_eq!(filtered.functions[0], file.functions[0]);
    }

    #[test]
    fn shared_exported_unused_dynamic_and_cyclic_helpers_are_retained() {
        let source = r#"import gleeunit/should as check
const fixture_value = 1
pub const exported_value = 2
pub fn sample_test() {
  fixture() |> check.equal(fixture_value)
  shared()
  exported()
  cycle_a()
  recursive(2)
  dynamic()
}
fn fixture() { fixture_value }
fn shared() { 1 }
pub fn exported() { exported_value }
fn unused() { 0 }
fn cycle_a() { cycle_b() }
fn cycle_b() { cycle_a() }
fn recursive(n) {
  case n {
    0 -> 0
    _ -> recursive(n - 1)
  }
}
fn dynamic() { 0 }
pub fn production() {
  let callback = dynamic
  check.equal(shared(), exported_value)
  callback()
}
"#;
        let file = parse(source);
        assert_eq!(
            remaining(&file),
            [
                "shared",
                "exported",
                "unused",
                "cycle_a",
                "cycle_b",
                "dynamic",
                "production",
            ]
        );
        let filtered = file.without_tests.unwrap();
        for retained in ["import gleeunit/should", "pub const exported_value"] {
            assert!(
                !filtered
                    .regions
                    .iter()
                    .any(|region| contains(region, source.find(retained).unwrap()))
            );
        }
        assert!(
            filtered
                .regions
                .iter()
                .any(|region| { contains(region, source.find("const fixture_value").unwrap()) })
        );
    }

    #[test]
    fn parameterized_and_mixed_runner_functions_keep_their_production_metrics() {
        for source in [
            "import gleeunit\npub fn execute(value) { gleeunit.main() }",
            "import gleeunit\npub fn main() { assert True\ngleeunit.main()\n42 }",
        ] {
            let file = parse(source);
            let filtered = file.without_tests.as_ref().unwrap();
            assert_eq!(filtered.functions.len(), 1);
            assert_eq!(
                filtered.functions[0].complexity,
                file.functions[0].complexity
            );
            assert!(
                filtered
                    .regions
                    .iter()
                    .any(|region| region.reason == "Gleam gleeunit main call")
            );
            assert!(
                !filtered
                    .regions
                    .iter()
                    .any(|region| region.reason == "Gleam gleeunit main runner")
            );
        }
    }

    #[test]
    fn helper_references_include_label_shorthand_and_pattern_size_reads() {
        for production in [
            "pub fn production() { consume(helper:) }",
            "pub fn production(record) { Holder(..record, helper:) }",
            "pub fn production(value) { let <<part:bytes-size(helper)>> = value\npart }",
            "pub fn production(value) { case value { <<part:bytes-size(helper)>> -> part } }",
        ] {
            let source = format!(
                "const helper = 1\npub fn value_test() {{ assert helper == 1 }}\n{production}"
            );
            let file = parse(&source);
            assert!(
                !file
                    .without_tests
                    .unwrap()
                    .regions
                    .iter()
                    .any(|region| contains(region, 0)),
                "{source}"
            );
        }
    }

    #[test]
    fn attached_attributes_comments_bom_and_crlf_have_original_spans() {
        let source = "\u{feff}//// Module docs\r\n/// Test docs\r\n@target(erlang)\r\npub fn example_test() {\r\n  // test comment\r\n  assert True\r\n}\r\n/// Production docs\r\npub fn value() { 1 }\r\n";
        let file = parse(source);
        assert_eq!(remaining(&file), ["value"]);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.source_lines, 1);
        assert_eq!(filtered.regions.len(), 1);
        let region = &filtered.regions[0];
        assert_eq!(region.start_byte, source.find("/// Test docs").unwrap());
        assert_eq!(region.end_byte, source.find("\r\n/// Production").unwrap());
        assert_eq!((region.start_line, region.end_line), (2, 7));
        assert_eq!(filtered.functions[0], file.functions[1]);
    }

    #[test]
    fn external_runner_overrides_keep_the_function_and_attributes() {
        let file = parse(
            "import gleeunit\n@external(javascript, \"./ffi.mjs\", \"production\")\npub fn main() { gleeunit.main() }\n",
        );
        assert_eq!(remaining(&file), ["main"]);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.functions[0].source_lines, 1);
        assert_eq!(filtered.source_lines, 2);
    }

    #[test]
    fn malformed_tests_still_fail_without_a_filtered_success() {
        for source in [
            b"pub fn invalid_test() { case }".as_slice(),
            b"import gleeunit\npub fn main() { gleeunit.main( }",
            b"pub fn invalid_test() { assert \xff }",
        ] {
            let file = adapter(true).unwrap().analyze(source).unwrap();
            assert!(!file.parsed());
            assert!(!file.diagnostics.is_empty());
            assert!(file.functions.is_empty());
            assert!(file.without_tests.is_none());
        }
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
        let bad = adapter(false).unwrap().analyze(b"fn broken( {").unwrap();
        assert!(!bad.parsed());
        assert!(bad.functions.is_empty());
    }
}
