use anyhow::{Context, Result, bail, ensure};
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser};

use super::tree::{self, SyntaxRules};
use super::{Language, LanguageAdapter, LanguageTestPolicy};
use crate::metrics::{FileAnalysis, TestRegion};

const TEST_PATHS: &[&str] = &[
    "**/test/**",
    "**/tests/**",
    "**/__tests__/**",
    "**/__mocks__/**",
    "**/*.test.*",
    "**/*.tests.*",
    "**/*.spec.*",
    "**/*.cy.*",
];

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
    test_paths: tree::TestPaths,
}

impl JavaScriptAdapter {
    pub fn new(language: Language) -> Result<Self> {
        let grammar: tree_sitter::Language = match language {
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            _ => bail!("JavaScript adapter does not support {}", language.id()),
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
        Ok(Self {
            parser,
            test_paths: tree::TestPaths::new(TEST_PATHS)?,
        })
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
    fn test_regions(root: Node<'_>, source: &[u8]) -> Vec<TestRegion> {
        TestBindings::new(root, source).regions()
    }

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
    fn test_policy(&self) -> LanguageTestPolicy {
        LanguageTestPolicy {
            version: "javascript-tests-v1",
            path_patterns: TEST_PATHS,
            syntax_rules: &[
                "Node test, Jest globals, Vitest, Mocha, AVA, Tape, Playwright and uvu literal imports/requires",
                "lexically resolved aliases, namespaces, destructuring, modifiers and parameterized tests",
                "unshadowed ambient suite/test callbacks with framework imports or a same-scope suite containing tests",
                "test hooks, registrations, and non-exported local callbacks/helpers used exclusively by test regions",
                "reassigned/mutated bindings, dynamic imports, eval/with and uncertain callback ownership are retained",
                "assertions and arbitrary objects named test are not test regions; benchmarks are not inferred",
            ],
        }
    }

    fn is_test_file(&self, path: &str) -> bool {
        self.test_paths.matches(path)
    }

    fn analyze(&mut self, source: &[u8]) -> Result<FileAnalysis> {
        tree::analyze::<Self>(&mut self.parser, source)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Framework {
    Node,
    Jest,
    Vitest,
    Mocha,
    Ava,
    Tape,
    Playwright,
    Uvu,
}

#[derive(Clone, Copy)]
enum Role {
    Test,
    Suite,
    Hook,
    Configure,
}

#[derive(Clone, Copy)]
enum ApiKind {
    Namespace,
    Callable(Role),
    Factory(Role),
}

#[derive(Clone, Copy)]
struct TestApi {
    framework: Framework,
    kind: ApiKind,
}

impl TestApi {
    fn member(self, name: &str) -> Option<Self> {
        use ApiKind::{Callable, Factory, Namespace};
        use Framework::{Ava, Node, Playwright, Tape, Uvu};
        use Role::{Configure, Hook, Suite, Test};
        let kind = match self.kind {
            Namespace => match name {
                "default" if matches!(self.framework, Node | Ava | Tape) => Callable(Test),
                "test" if !matches!(self.framework, Ava | Tape) => Callable(Test),
                "it" if matches!(
                    self.framework,
                    Node | Framework::Mocha | Framework::Jest | Framework::Vitest
                ) =>
                {
                    Callable(Test)
                }
                "xit" | "xtest" | "fit"
                    if matches!(self.framework, Framework::Jest | Framework::Mocha) =>
                {
                    Callable(Test)
                }
                "describe"
                    if matches!(
                        self.framework,
                        Node | Framework::Mocha | Framework::Jest | Framework::Vitest
                    ) =>
                {
                    Callable(Suite)
                }
                "context" if self.framework == Framework::Mocha => Callable(Suite),
                "xdescribe" | "fdescribe" if self.framework == Framework::Jest => Callable(Suite),
                "xdescribe" | "xcontext" if self.framework == Framework::Mocha => Callable(Suite),
                "suite" if self.framework == Uvu => Factory(Test),
                "suite"
                    if matches!(self.framework, Node | Framework::Mocha | Framework::Vitest) =>
                {
                    Callable(Suite)
                }
                "specify" if self.framework == Framework::Mocha => Callable(Test),
                "beforeEach" | "afterEach"
                    if matches!(
                        self.framework,
                        Node | Framework::Mocha | Framework::Jest | Framework::Vitest
                    ) =>
                {
                    Callable(Hook)
                }
                "before" | "after" if matches!(self.framework, Node | Framework::Mocha | Ava) => {
                    Callable(Hook)
                }
                "beforeEach" | "afterEach" if self.framework == Ava => Callable(Hook),
                "beforeAll" | "afterAll"
                    if matches!(self.framework, Framework::Jest | Framework::Vitest) =>
                {
                    Callable(Hook)
                }
                "setup" | "teardown" | "suiteSetup" | "suiteTeardown"
                    if self.framework == Framework::Mocha =>
                {
                    Callable(Hook)
                }
                "only" | "skip" | "todo" if matches!(self.framework, Node | Ava | Tape) => {
                    Callable(Test)
                }
                "serial" if self.framework == Ava => Callable(Test),
                _ => return None,
            },
            Callable(role) => match name {
                "only" | "skip" | "todo" if matches!(role, Test | Suite) => Callable(role),
                "concurrent" if matches!(self.framework, Framework::Jest | Framework::Vitest) => {
                    Callable(role)
                }
                "serial" if matches!(self.framework, Ava | Playwright) => Callable(role),
                "parallel" if self.framework == Playwright => Callable(role),
                "sequential" | "fails" if self.framework == Framework::Vitest => Callable(role),
                "failing" if matches!(self.framework, Framework::Jest | Ava) => Callable(role),
                "always" if self.framework == Ava && matches!(role, Hook) => Callable(Hook),
                "each" if self.framework == Uvu && matches!(role, Hook) => Callable(Hook),
                "each" if matches!(self.framework, Framework::Jest | Framework::Vitest) => {
                    Factory(role)
                }
                "for" | "skipIf" | "runIf" if self.framework == Framework::Vitest => Factory(role),
                "describe" if matches!(self.framework, Node | Playwright) => Callable(Suite),
                "test" | "it" if self.framework == Node => Callable(Test),
                "before" | "after" if matches!(self.framework, Node | Uvu | Ava) => Callable(Hook),
                "beforeEach" | "afterEach" if matches!(self.framework, Node | Playwright | Ava) => {
                    Callable(Hook)
                }
                "beforeAll" | "afterAll" if self.framework == Playwright => Callable(Hook),
                "extend" if matches!(self.framework, Playwright | Framework::Vitest) => {
                    Factory(Test)
                }
                "use" | "setTimeout" | "configure" | "step" | "fail" | "fixme" | "slow"
                    if self.framework == Playwright =>
                {
                    Callable(Configure)
                }
                "run" if self.framework == Uvu => Callable(Configure),
                _ => return None,
            },
            Factory(_) => return None,
        };
        Some(Self { kind, ..self })
    }

    fn called(self) -> Option<Self> {
        if let ApiKind::Factory(role) = self.kind {
            Some(Self {
                kind: ApiKind::Callable(role),
                ..self
            })
        } else {
            None
        }
    }

    fn registration(self) -> bool {
        !matches!(self.kind, ApiKind::Namespace)
            || matches!(
                self.framework,
                Framework::Node | Framework::Ava | Framework::Tape
            )
    }
}

fn framework(name: &str) -> Option<Framework> {
    match name {
        "node:test" => Some(Framework::Node),
        "@jest/globals" => Some(Framework::Jest),
        "vitest" => Some(Framework::Vitest),
        "mocha" => Some(Framework::Mocha),
        "ava" => Some(Framework::Ava),
        "tape" => Some(Framework::Tape),
        "@playwright/test" => Some(Framework::Playwright),
        "uvu" => Some(Framework::Uvu),
        _ => None,
    }
}

fn literal<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let text = node.utf8_text(source).ok()?;
    let first = *text.as_bytes().first()?;
    (matches!(first, b'\'' | b'"' | b'`')
        && text.len() >= 2
        && text.as_bytes().last() == Some(&first)
        && !text.contains('\\')
        && !text.contains("${"))
    .then(|| &text[1..text.len() - 1])
}

#[derive(Clone)]
enum Binding<'tree> {
    Unknown,
    Api(TestApi),
    Expression(Node<'tree>, usize),
    Member(Node<'tree>, usize, String),
    Function(Node<'tree>),
}

struct Scope<'tree> {
    parent: Option<usize>,
    function: bool,
    bindings: HashMap<String, Binding<'tree>>,
}

struct TestBindings<'tree, 'source> {
    source: &'source [u8],
    scopes: Vec<Scope<'tree>>,
    scope_of: HashMap<usize, usize>,
    nodes: Vec<Node<'tree>>,
    declarations: HashSet<usize>,
    mutated_frameworks: HashSet<Framework>,
    dynamic_scope: bool,
    ambient_framework: bool,
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn binding_names(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => result.push(node),
            "pair_pattern" => pending.extend(node.child_by_field_name("value")),
            "assignment_pattern" | "object_assignment_pattern" => {
                pending.extend(node.child_by_field_name("left"))
            }
            "required_parameter" | "optional_parameter" => {
                pending.extend(
                    node.child_by_field_name("pattern")
                        .or_else(|| node.child_by_field_name("name")),
                );
            }
            "array_pattern" | "object_pattern" | "rest_pattern" | "formal_parameters" => {
                pending.extend(named_children(node))
            }
            _ => {}
        }
    }
    result
}

fn unwrap_expression(mut node: Node<'_>) -> Node<'_> {
    loop {
        let inner = match node.kind() {
            "parenthesized_expression"
            | "as_expression"
            | "satisfies_expression"
            | "non_null_expression"
            | "instantiation_expression" => node.named_child(0),
            "type_assertion" => node.named_child(node.named_child_count().saturating_sub(1)),
            _ => None,
        };
        match inner {
            Some(inner) => node = inner,
            None => return node,
        }
    }
}

impl<'tree, 'source> TestBindings<'tree, 'source> {
    fn new(root: Node<'tree>, source: &'source [u8]) -> Self {
        let mut result = Self {
            source,
            scopes: vec![Scope {
                parent: None,
                function: true,
                bindings: HashMap::new(),
            }],
            scope_of: HashMap::new(),
            nodes: Vec::new(),
            declarations: HashSet::new(),
            mutated_frameworks: HashSet::new(),
            dynamic_scope: false,
            ambient_framework: false,
        };
        let mut pending = vec![(root, 0)];
        while let Some((node, parent)) = pending.pop() {
            let function = CALLABLES.contains(&node.kind()) || node.kind() == "class_static_block";
            let opens = function
                || matches!(
                    node.kind(),
                    "statement_block"
                        | "catch_clause"
                        | "for_statement"
                        | "for_in_statement"
                        | "switch_body"
                        | "class"
                        | "class_declaration"
                        | "internal_module"
                );
            let scope = if opens {
                result.scopes.push(Scope {
                    parent: Some(parent),
                    function,
                    bindings: HashMap::new(),
                });
                result.scopes.len() - 1
            } else {
                parent
            };
            result.scope_of.insert(node.id(), scope);
            result.nodes.push(node);
            pending.extend(
                named_children(node)
                    .into_iter()
                    .rev()
                    .map(|child| (child, scope)),
            );
        }
        result.collect_bindings();
        result.collect_mutations();
        result
    }

    fn text(&self, node: Node<'_>) -> &str {
        node.utf8_text(self.source).expect("validated UTF-8 source")
    }

    fn define(&mut self, scope: usize, node: Node<'tree>, binding: Binding<'tree>) {
        self.declarations.insert(node.id());
        let name = self.text(node).to_owned();
        self.scopes[scope]
            .bindings
            .entry(name)
            .and_modify(|entry| *entry = Binding::Unknown)
            .or_insert(binding);
    }

    fn function_scope(&self, mut scope: usize) -> usize {
        while !self.scopes[scope].function {
            scope = self.scopes[scope].parent.expect("program scope");
        }
        scope
    }

    fn lookup(&self, name: &str, mut scope: usize) -> Option<usize> {
        loop {
            if self.scopes[scope].bindings.contains_key(name) {
                return Some(scope);
            }
            scope = self.scopes[scope].parent?;
        }
    }

    fn collect_bindings(&mut self) {
        for node in self.nodes.clone() {
            let scope = self.scope_of[&node.id()];
            if CALLABLES.contains(&node.kind()) {
                if let Some(parameters) = node
                    .child_by_field_name("parameters")
                    .or_else(|| node.child_by_field_name("parameter"))
                {
                    for name in binding_names(parameters) {
                        self.define(scope, name, Binding::Unknown);
                    }
                }
                if let Some(name) = node.child_by_field_name("name") {
                    if matches!(
                        node.kind(),
                        "function_declaration" | "generator_function_declaration"
                    ) {
                        let parent = self.scopes[scope].parent.expect("function parent");
                        self.define(parent, name, Binding::Function(node));
                    } else if matches!(node.kind(), "function_expression" | "generator_function") {
                        self.define(scope, name, Binding::Function(node));
                    }
                }
            } else if node.kind() == "variable_declarator" {
                let target = if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == "variable_declaration")
                {
                    self.function_scope(scope)
                } else {
                    scope
                };
                if let Some(pattern) = node.child_by_field_name("name") {
                    let value = node.child_by_field_name("value");
                    for name in binding_names(pattern) {
                        let binding = if pattern.kind() == "identifier" {
                            value
                                .map_or(Binding::Unknown, |value| Binding::Expression(value, scope))
                        } else if pattern.kind() == "object_pattern" {
                            let property = match name.parent() {
                                Some(parent) if parent.kind() == "pair_pattern" => {
                                    parent.child_by_field_name("key").and_then(|key| {
                                        literal(key, self.source).or_else(|| {
                                            (key.kind() == "property_identifier")
                                                .then(|| self.text(key))
                                        })
                                    })
                                }
                                Some(parent) if parent.kind() == "object_pattern" => {
                                    Some(self.text(name))
                                }
                                _ => None,
                            };
                            value
                                .zip(property)
                                .map_or(Binding::Unknown, |(value, property)| {
                                    Binding::Member(value, scope, property.to_owned())
                                })
                        } else {
                            Binding::Unknown
                        };
                        self.define(target, name, binding);
                    }
                }
            } else if node.kind() == "import_statement" {
                self.import_bindings(node, scope);
            } else if node.kind() == "catch_clause" {
                if let Some(pattern) = node.child_by_field_name("parameter") {
                    for name in binding_names(pattern) {
                        self.define(scope, name, Binding::Unknown);
                    }
                }
            } else if node.kind() == "for_in_statement" {
                if let Some(kind) = node.child_by_field_name("kind")
                    && let Some(pattern) = node.child_by_field_name("left")
                {
                    let target = if self.text(kind) == "var" {
                        self.function_scope(scope)
                    } else {
                        scope
                    };
                    for name in binding_names(pattern) {
                        self.define(target, name, Binding::Unknown);
                    }
                }
            } else if node.kind() == "class" {
                if let Some(name) = node.child_by_field_name("name") {
                    self.define(scope, name, Binding::Unknown);
                }
            } else if matches!(
                node.kind(),
                "class_declaration" | "enum_declaration" | "function_signature" | "internal_module"
            ) && let Some(name) = node.child_by_field_name("name")
            {
                let target = if matches!(node.kind(), "class_declaration" | "internal_module") {
                    self.scopes[scope].parent.unwrap_or(scope)
                } else {
                    scope
                };
                self.define(target, name, Binding::Unknown);
            }
        }
    }

    fn import_bindings(&mut self, node: Node<'tree>, scope: usize) {
        let mut cursor = node.walk();
        if node
            .children(&mut cursor)
            .any(|child| child.kind() == "type")
        {
            return;
        }
        let source = node.child_by_field_name("source").or_else(|| {
            named_children(node)
                .iter()
                .find_map(|child| child.child_by_field_name("source"))
        });
        let api = source
            .and_then(|source| literal(source, self.source))
            .and_then(framework)
            .map(|framework| TestApi {
                framework,
                kind: ApiKind::Namespace,
            });
        self.ambient_framework |= api.is_some_and(|api| {
            matches!(
                api.framework,
                Framework::Jest | Framework::Vitest | Framework::Mocha
            )
        });
        let mut pending = named_children(node);
        while let Some(child) = pending.pop() {
            match child.kind() {
                "import_specifier" => {
                    let mut cursor = child.walk();
                    if child
                        .children(&mut cursor)
                        .any(|token| token.kind() == "type")
                    {
                        continue;
                    }
                    if let Some(name) = child.child_by_field_name("name") {
                        self.declarations.insert(name.id());
                        let alias = child.child_by_field_name("alias").unwrap_or(name);
                        let imported =
                            literal(name, self.source).unwrap_or_else(|| self.text(name));
                        let binding = api
                            .and_then(|api| api.member(imported))
                            .map_or(Binding::Unknown, Binding::Api);
                        self.define(scope, alias, binding);
                    }
                }
                "namespace_import" | "import_require_clause" => {
                    if let Some(name) = named_children(child)
                        .into_iter()
                        .find(|n| n.kind() == "identifier")
                    {
                        self.define(scope, name, api.map_or(Binding::Unknown, Binding::Api));
                    }
                }
                "import_clause" | "named_imports" => pending.extend(named_children(child)),
                "identifier" => {
                    let default = api.and_then(|api| {
                        api.member("default")
                            .or_else(|| (api.framework == Framework::Mocha).then_some(api))
                    });
                    self.define(scope, child, default.map_or(Binding::Unknown, Binding::Api));
                }
                _ => {}
            }
        }
    }

    fn resolve_api(&self, mut node: Node<'tree>, mut scope: usize) -> Option<TestApi> {
        enum Operation {
            Member(String),
            Call,
        }
        let mut operations = Vec::new();
        let mut seen = HashSet::new();
        let mut api = loop {
            node = unwrap_expression(node);
            match node.kind() {
                "identifier" | "shorthand_property_identifier" => {
                    let name = self.text(node);
                    let owner = self.lookup(name, scope)?;
                    if !seen.insert((owner, name)) {
                        return None;
                    }
                    match &self.scopes[owner].bindings[name] {
                        Binding::Api(api) => break *api,
                        Binding::Expression(value, at) => {
                            node = *value;
                            scope = *at;
                        }
                        Binding::Member(value, at, property) => {
                            operations.push(Operation::Member(property.clone()));
                            node = *value;
                            scope = *at;
                        }
                        _ => return None,
                    }
                }
                "member_expression" | "subscript_expression" => {
                    let property = node
                        .child_by_field_name("property")
                        .map(|n| self.text(n))
                        .or_else(|| {
                            node.child_by_field_name("index")
                                .and_then(|n| literal(n, self.source))
                        })?;
                    operations.push(Operation::Member(property.to_owned()));
                    node = node.child_by_field_name("object")?;
                }
                "call_expression" => {
                    let function = node.child_by_field_name("function")?;
                    if function.kind() == "identifier"
                        && self.text(function) == "require"
                        && self.lookup("require", scope).is_none()
                    {
                        let args = node.child_by_field_name("arguments")?;
                        if args.named_child_count() != 1 {
                            return None;
                        }
                        let framework = framework(literal(args.named_child(0)?, self.source)?)?;
                        break TestApi {
                            framework,
                            kind: ApiKind::Namespace,
                        };
                    }
                    operations.push(Operation::Call);
                    node = function;
                }
                _ => return None,
            }
        };
        for operation in operations.into_iter().rev() {
            api = match operation {
                Operation::Member(name) => api.member(&name)?,
                Operation::Call => api.called()?,
            };
        }
        (!self.mutated_frameworks.contains(&api.framework)).then_some(api)
    }

    fn collect_mutations(&mut self) {
        let mut writes = Vec::new();
        for node in &self.nodes {
            let scope = self.scope_of[&node.id()];
            match node.kind() {
                "with_statement" => self.dynamic_scope = true,
                "call_expression"
                    if node
                        .child_by_field_name("function")
                        .is_some_and(|function| {
                            function.kind() == "identifier"
                                && self.text(function) == "eval"
                                && self.lookup("eval", scope).is_none()
                        }) =>
                {
                    self.dynamic_scope = true;
                }
                "assignment_expression" | "augmented_assignment_expression" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        writes.push((left, scope));
                    }
                }
                "update_expression" => {
                    if let Some(left) = node.child_by_field_name("argument") {
                        writes.push((left, scope));
                    }
                }
                "for_in_statement" if node.child_by_field_name("kind").is_none() => {
                    if let Some(left) = node.child_by_field_name("left") {
                        writes.push((left, scope));
                    }
                }
                "unary_expression"
                    if node
                        .child_by_field_name("operator")
                        .is_some_and(|operator| operator.kind() == "delete") =>
                {
                    if let Some(left) = node.child_by_field_name("argument") {
                        writes.push((left, scope));
                    }
                }
                _ => {}
            }
        }
        let mut names = Vec::new();
        let mut frameworks = HashSet::new();
        while let Some((mut left, scope)) = writes.pop() {
            left = unwrap_expression(left);
            match left.kind() {
                "object_pattern" | "array_pattern" => {
                    writes.extend(named_children(left).into_iter().map(|child| (child, scope)));
                    continue;
                }
                "pair_pattern" | "assignment_pattern" | "object_assignment_pattern" => {
                    if let Some(value) = left
                        .child_by_field_name("value")
                        .or_else(|| left.child_by_field_name("left"))
                    {
                        writes.push((value, scope));
                    }
                    continue;
                }
                "rest_pattern" => {
                    if let Some(value) = left.named_child(0) {
                        writes.push((value, scope));
                    }
                    continue;
                }
                _ => {}
            }
            while matches!(left.kind(), "member_expression" | "subscript_expression") {
                let Some(object) = left.child_by_field_name("object") else {
                    break;
                };
                left = unwrap_expression(object);
                if let Some(api) = self.resolve_api(left, scope) {
                    frameworks.insert(api.framework);
                }
            }
            for name in binding_names(left) {
                if let Some(api) = self.resolve_api(name, scope) {
                    frameworks.insert(api.framework);
                }
                let owner = self.lookup(self.text(name), scope).unwrap_or(0);
                names.push((owner, self.text(name).to_owned()));
            }
        }
        self.mutated_frameworks = frameworks;
        for (scope, name) in names {
            self.scopes[scope].bindings.insert(name, Binding::Unknown);
        }
    }

    fn ambient_role(&self, function: Node<'tree>, scope: usize) -> Option<Role> {
        let mut base = unwrap_expression(function);
        loop {
            if base.kind() == "member_expression" {
                let property = base.child_by_field_name("property")?;
                if !matches!(
                    self.text(property),
                    "only" | "skip" | "concurrent" | "failing" | "each" | "skipIf" | "runIf"
                ) {
                    return None;
                }
                base = unwrap_expression(base.child_by_field_name("object")?);
            } else if base.kind() == "call_expression" {
                let function = base.child_by_field_name("function")?;
                if function.kind() != "member_expression"
                    || !function
                        .child_by_field_name("property")
                        .is_some_and(|property| {
                            matches!(self.text(property), "each" | "skipIf" | "runIf")
                        })
                {
                    return None;
                }
                base = function;
            } else {
                break;
            }
        }
        if base.kind() != "identifier" || self.lookup(self.text(base), scope).is_some() {
            return None;
        }
        match self.text(base) {
            "describe" | "context" | "suite" | "fdescribe" | "xdescribe" | "xcontext" => {
                Some(Role::Suite)
            }
            "test" | "it" | "specify" | "fit" | "xit" | "xtest" => Some(Role::Test),
            "before" | "after" | "beforeEach" | "afterEach" | "beforeAll" | "afterAll"
            | "setup" | "teardown" | "suiteSetup" | "suiteTeardown" => Some(Role::Hook),
            _ => None,
        }
    }

    fn registration_shape(&self, call: Node<'tree>, role: Role) -> bool {
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return false;
        };
        let args = named_children(arguments);
        let callback = args
            .last()
            .is_some_and(|node| CALLABLES.contains(&unwrap_expression(*node).kind()));
        callback
            && (matches!(role, Role::Hook)
                || args
                    .first()
                    .is_some_and(|node| literal(*node, self.source).is_some()))
    }

    fn regions(&self) -> Vec<TestRegion> {
        if self.dynamic_scope {
            return Vec::new();
        }
        let calls: Vec<_> = self
            .nodes
            .iter()
            .copied()
            .filter(|node| node.kind() == "call_expression")
            .collect();
        let ambient_suites: Vec<_> = calls
            .iter()
            .copied()
            .filter(|call| {
                call.child_by_field_name("function")
                    .is_some_and(|function| {
                        matches!(
                            self.ambient_role(function, self.scope_of[&call.id()]),
                            Some(Role::Suite)
                        ) && self.registration_shape(*call, Role::Suite)
                    })
            })
            .collect();
        let ambient_suites: Vec<_> = ambient_suites
            .into_iter()
            .filter(|suite| {
                calls.iter().any(|call| {
                    call.start_byte() > suite.start_byte()
                        && call.end_byte() <= suite.end_byte()
                        && call
                            .child_by_field_name("function")
                            .is_some_and(|function| {
                                matches!(
                                    self.ambient_role(function, self.scope_of[&call.id()]),
                                    Some(Role::Test)
                                ) && self.registration_shape(*call, Role::Test)
                            })
                })
            })
            .collect();
        let mut regions = Vec::new();
        for call in calls {
            let Some(function) = call.child_by_field_name("function") else {
                continue;
            };
            let scope = self.scope_of[&call.id()];
            let ambient = self.ambient_framework
                || ambient_suites.iter().any(|suite| {
                    self.scope_of[&suite.id()] == scope
                        || (suite.start_byte() <= call.start_byte()
                            && call.end_byte() <= suite.end_byte())
                });
            if self
                .resolve_api(function, scope)
                .is_some_and(TestApi::registration)
            {
                regions.push(tree::test_region(call, "bound test-framework registration"));
            } else if ambient
                && let Some(role) = self.ambient_role(function, scope)
                && self.registration_shape(call, role)
            {
                regions.push(tree::test_region(call, "unshadowed ambient test suite"));
            }
        }
        self.exclusive_helpers(&mut regions);
        regions
    }

    fn exclusive_helpers(&self, regions: &mut Vec<TestRegion>) {
        if regions.is_empty() {
            return;
        }
        let inside = |node: Node<'_>, regions: &[TestRegion]| {
            regions.iter().any(|region| {
                region.start_byte <= node.start_byte() && node.end_byte() <= region.end_byte
            })
        };
        let mut references: HashMap<(usize, String), Vec<Node<'tree>>> = HashMap::new();
        for node in &self.nodes {
            if matches!(node.kind(), "identifier" | "shorthand_property_identifier")
                && !self.declarations.contains(&node.id())
                && let Some(owner) = self.lookup(self.text(*node), self.scope_of[&node.id()])
            {
                references
                    .entry((owner, self.text(*node).to_owned()))
                    .or_default()
                    .push(*node);
            }
        }
        let mut candidates = Vec::new();
        for (scope, bindings) in self.scopes.iter().enumerate() {
            for (name, binding) in &bindings.bindings {
                let candidate = match binding {
                    Binding::Function(node) => Some(*node),
                    Binding::Expression(value, at) => {
                        let value = unwrap_expression(*value);
                        if CALLABLES.contains(&value.kind())
                            || self.resolve_api(value, *at).is_some()
                            || (value.kind() == "identifier" && self.local_function(value, *at))
                        {
                            value
                                .parent()
                                .filter(|parent| parent.kind() == "variable_declarator")
                                .or(Some(value))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(mut candidate) = candidate {
                    if candidate.kind() == "variable_declarator"
                        && let Some(parent) = candidate.parent()
                        && named_children(parent)
                            .iter()
                            .filter(|n| n.kind() == "variable_declarator")
                            .count()
                            == 1
                    {
                        candidate = parent;
                    }
                    if candidate
                        .parent()
                        .is_some_and(|parent| parent.kind() == "export_statement")
                    {
                        continue;
                    }
                    if let Some(uses) = references.get(&(scope, name.clone()))
                        && !uses.is_empty()
                    {
                        candidates.push((candidate, uses));
                    }
                }
            }
        }
        candidates.sort_by_key(|(node, _)| (node.start_byte(), node.end_byte()));
        loop {
            let before = regions.len();
            candidates.retain(|(node, uses)| {
                if inside(*node, regions) {
                    return false;
                }
                let test_use = uses.iter().any(|usage| {
                    inside(*usage, regions)
                        && !(node.start_byte() <= usage.start_byte()
                            && usage.end_byte() <= node.end_byte())
                });
                if test_use
                    && uses.iter().all(|usage| {
                        inside(*usage, regions)
                            || (node.start_byte() <= usage.start_byte()
                                && usage.end_byte() <= node.end_byte())
                    })
                {
                    regions.push(tree::test_region(
                        *node,
                        "local helper used exclusively by tests",
                    ));
                    false
                } else {
                    true
                }
            });
            if regions.len() == before {
                break;
            }
        }
    }

    fn local_function(&self, mut node: Node<'tree>, mut scope: usize) -> bool {
        let mut seen = HashSet::new();
        loop {
            node = unwrap_expression(node);
            if CALLABLES.contains(&node.kind()) {
                return true;
            }
            if node.kind() != "identifier" {
                return false;
            }
            let name = self.text(node);
            let Some(owner) = self.lookup(name, scope) else {
                return false;
            };
            if !seen.insert((owner, name)) {
                return false;
            }
            match &self.scopes[owner].bindings[name] {
                Binding::Function(_) => return true,
                Binding::Expression(value, at) => {
                    node = *value;
                    scope = *at;
                }
                _ => return false,
            }
        }
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
        assert!(
            result.parsed(),
            "{language:?}: {source}\n{:?}",
            result.diagnostics
        );
        result
    }

    #[test]
    fn test_paths_are_javascript_owned_and_component_bounded() {
        for language in [Language::JavaScript, Language::TypeScript, Language::Tsx] {
            let adapter = JavaScriptAdapter::new(language).unwrap();
            for path in [
                "tests/a.ts",
                "src/test/a.js",
                "__tests__/a.tsx",
                "__mocks__/client.js",
                "src/a.spec.mts",
                "a.test.cts",
                "a.tests.jsx",
                "a.cy.ts",
            ] {
                assert!(adapter.is_test_file(path), "{path}");
            }
            for path in [
                "contest/main.ts",
                "a.test.ts/main.ts",
                "src/test_helpers/main.js",
                "a.bench.ts",
                "test_main.js",
                "src/main_test.js",
                "src/Tests/main.ts",
                "main.TEST.js",
            ] {
                assert!(!adapter.is_test_file(path), "{path}");
            }
        }
    }

    #[test]
    fn bound_frameworks_aliases_modifiers_and_helpers_are_excluded() {
        for language in [Language::JavaScript, Language::TypeScript, Language::Tsx] {
            for test in [
                "import {test as check, describe as group, beforeEach as setup} from 'node:test'; group('suite',()=>{ setup(()=>{if(x)work()}); check('case',{timeout:1},()=>{if(x)work()}) });",
                "import * as v from 'vitest'; v.describe.each([1,2])('suite',n=>{v.test.concurrent.each([1,2])('case',x=>{if(x)work()})});",
                "import {test} from 'vitest'; const check=test.extend({fixture:()=>{if(x)work()}}); check.for([1,2])('case',()=>{if(x)work()});",
                "import {test} from '@jest/globals'; test.each`value\\n${1}`('case',()=>{if(x)work()});",
                "const {test: check} = require('node:test'); check('case',()=>{if(x)work()});",
                "const node = require('node:test'); const {test: check} = node; check('case',()=>{if(x)work()});",
                "import check from 'node:test'; check('case',()=>{if(x)work()});",
                "const check = require('node:test'); check['skip']('case',()=>{if(x)work()});",
                "import check from 'ava'; check.serial('case',t=>{if(x)work()});",
                "const check=require('ava'); check.before(()=>{if(x)work()}); check('case',t=>{if(x)work()});",
                "import check from 'tape'; check.only('case',t=>{if(x)work()});",
                "import mocha from 'mocha'; mocha.describe('suite',()=>{mocha.it('case',()=>{if(x)work()})});",
                "import {test as base} from '@playwright/test'; const check = base.extend({fixture:async()=>{if(x)work()}}); check.beforeEach(()=>{if(x)work()}); check('case',async()=>{if(x)work()});",
                "import {suite} from 'uvu'; const check = suite('group'); check.before.each(()=>{if(x)work()}); check('case',()=>{if(x)work()}); check.run();",
                "import {test} from '@jest/globals'; const check = test; function helper(){if(x)work()} function example(){helper()} const callback = example; check('case',callback);",
            ] {
                let source = format!("export function production() {{ return 1; }}\n{test}\n");
                let file = parse(&source, language);
                assert!(file.functions.len() > 1, "{test}");
                let filtered = file.without_tests.expect(test);
                assert_eq!(
                    filtered.functions.len(),
                    1,
                    "{language:?}: {test}: {:?}",
                    filtered.functions
                );
                assert_eq!(filtered.functions[0].name, "production");
                assert_eq!(filtered.functions[0].complexity, 1);
                assert_eq!(filtered.functions[0].source_lines, 1);
            }
        }
    }

    #[test]
    fn ambient_suites_require_unshadowed_framework_evidence() {
        for source in [
            "describe('group',()=>{beforeEach(()=>{if(x)work()}); it('case',()=>{if(x)work()})});",
            "describe.each([1,2])('group',()=>{test.concurrent.each([1])('case',()=>{if(x)work()})});",
            "import {expect} from 'vitest'; test('case',()=>{if(x)work()});",
            "import 'mocha'; it('case',()=>{if(x)work()});",
        ] {
            let result = parse(source, Language::TypeScript);
            assert!(
                result.without_tests.expect(source).functions.is_empty(),
                "{source}"
            );
        }
        for source in [
            "test('could be production',()=>{if(x)work()});",
            "describe('could be production',()=>{if(x)work()});",
            "function f(describe,it){describe('x',()=>{it('y',()=>{if(x)work()})})}",
            "const describe = production; describe('x',()=>{it('y',()=>{if(x)work()})});",
            "const regexp = /x/; regexp.test('value');",
        ] {
            assert!(
                parse(source, Language::JavaScript).without_tests.is_none(),
                "{source}"
            );
        }
        let mixed = parse(
            "describe('suite',()=>{it('case',()=>{})}); export function production(){test('unrelated',()=>{if(x)work()})}",
            Language::JavaScript,
        );
        assert_eq!(mixed.without_tests.unwrap().functions.len(), 2);
    }

    #[test]
    fn lexical_shadowing_mutations_and_dynamic_scope_do_not_hide_production() {
        for source in [
            "import {test} from 'node:test'; function f(test) { test('production',()=>{if(x)work()}); }",
            "import {test} from 'node:test'; { const test = production; test('production',()=>{if(x)work()}); }",
            "import {test} from 'node:test'; function f() { test('production',()=>{if(x)work()}); { var test = production; } }",
            "import {test} from 'node:test'; function f({test}) { test('production',()=>{if(x)work()}); }",
            "import {test} from 'node:test'; try {} catch(test) {test('production',()=>{if(x)work()})}",
            "import {test} from 'node:test'; for (const test of values) {test('production',()=>{if(x)work()})}",
            "import {test} from 'node:test'; function f() { function test() {} test('production',()=>{if(x)work()}); }",
            "import {test} from 'node:test'; test = production; test('production',()=>{if(x)work()});",
            "const node = require('node:test'); const alias = node; alias.test = production; node.test('production',()=>{if(x)work()});",
            "function f(require) { const check = require('node:test'); check('production',()=>{if(x)work()}); }",
            "import {test} from './production'; test('production',()=>{if(x)work()});",
            "import {test} from 'node:test'; eval(code); test('uncertain',()=>{if(x)work()});",
            "import {test} from 'node:test'; test[method]('uncertain',()=>{if(x)work()});",
            "import {test} from 'node:test'; let a=b,b=a; a('uncertain',()=>{if(x)work()});",
            "const node=require('node:test'); ({test:node.test}=production); node.test('production',()=>{if(x)work()});",
            "require('node:test').test = production; require('node:test').test('production',()=>{if(x)work()});",
            "import {test} from 'node:test'; const C=class test {static only(){} f(){test.only('production',()=>{if(x)work()})}};",
            "import {it} from 'uvu'; it('unknown API',()=>{if(x)work()});",
            "import test from 'node:test'; test.serial('unknown API',()=>{if(x)work()});",
        ] {
            assert!(
                parse(source, Language::TypeScript).without_tests.is_none(),
                "{source}"
            );
        }
        let static_scope = parse(
            "import {test} from 'node:test'; class C {static {var test=production; test('production',()=>{});}} test('real',()=>{});",
            Language::JavaScript,
        );
        assert_eq!(static_scope.without_tests.unwrap().functions.len(), 1);
    }

    #[test]
    fn mixed_scopes_recompute_enclosing_mass_and_keep_shared_or_exported_callbacks() {
        let source = "\u{feff}import {test} from 'node:test';\r\nexport function production(x) {\r\n if (x) work();\r\n test('case',()=>{\r\n  if(x)work();\r\n  if(x)work();\r\n });\r\n return x;\r\n}\r\n";
        let file = parse(source, Language::TypeScript);
        assert_eq!(file.functions[0].complexity, 4);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.functions.len(), 1);
        assert_eq!(filtered.functions[0].complexity, 2);
        assert_eq!(filtered.functions[0].source_lines, 3);
        assert_eq!(
            &source[filtered.regions[0].start_byte..filtered.regions[0].start_byte + 4],
            "test"
        );
        let file = parse(
            "import {test} from 'node:test'; export function prod(x){if(x)work(); test('case',()=>{if(x)work()}); return x;}",
            Language::TypeScript,
        );
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.functions[0].complexity, 2);
        assert_eq!(filtered.functions[0].source_lines, 1);
        for source in [
            "import {test} from 'node:test'; function shared(){if(x)work()} shared(); test('case',shared);",
            "import {test} from 'node:test'; export function shared(){if(x)work()} test('case',shared);",
            "import {test} from 'node:test'; const prod=function recurse(){if(x)recurse()}; prod(); test('case',()=>{});",
        ] {
            let file = parse(source, Language::JavaScript);
            assert_eq!(file.without_tests.unwrap().functions.len(), 1, "{source}");
        }
    }

    #[test]
    fn typescript_bindings_and_malformed_test_regions_remain_explicit() {
        for language in [Language::TypeScript, Language::Tsx] {
            let file = parse(
                "import test = require('node:test'); test('case',()=>{if(x)work()});",
                language,
            );
            assert!(file.without_tests.unwrap().functions.is_empty());
            let file = parse(
                "import {test} from 'node:test'; type Fn=(name:string,fn:()=>void)=>void; function f(test:Fn){test('prod',()=>{if(x)work()})}",
                language,
            );
            assert!(file.without_tests.is_none());
            let file = parse(
                "import type {test} from 'node:test'; test('uncertain',()=>{if(x)work()});",
                language,
            );
            assert!(file.without_tests.is_none());
            let bad = JavaScriptAdapter::new(language)
                .unwrap()
                .analyze(b"import {test} from 'node:test'; test('broken',()=>{")
                .unwrap();
            assert!(!bad.parsed());
            assert!(bad.without_tests.is_none());
        }
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

    #[test]
    fn typescript_variance_modifiers_are_type_syntax_not_functions() {
        let source = "\
export declare class Producer<out T> { protected constructor(); readonly value: T; }\n\
export interface Consumer<in T> { consume(value: T): void; }\n\
export interface Cell<in out T> { get(): T; set(value: T): void; }\n\
export type Reader<out T> = () => T;\n\
export type Writer<in T> = (value: T) => void;\n\
export interface Options<\n\
  out T extends string = string,\n\
  in out U extends unknown[] = [],\n\
> { readonly value: T; update(value: U): U; }\n";
        for language in [Language::TypeScript, Language::Tsx] {
            let file = parse(source, language);
            assert!(file.functions.is_empty());
            assert_eq!(file.source_lines, 9);
        }
    }

    #[test]
    fn typescript_type_only_star_exports() {
        for language in [Language::TypeScript, Language::Tsx] {
            let file = parse(
                "export type * from './types';\nexport type * as api from './types';\nexport type { Item } from './types';\n",
                language,
            );
            assert!(file.functions.is_empty());
            assert_eq!(file.source_lines, 3);
        }
    }

    #[test]
    fn typescript_contextual_exports_and_tuple_labels_do_not_add_function_mass() {
        let source = "\
export { type, type Item, type as value, value as type } from './types';\n\
type Tuple = [number, boolean?, ...string[]];\n\
const stub = sandbox.stub<[type: string, readonly?: boolean, ...unknown: string[]], void>();\n\
function choose(x) {\n\
  if (x) return 1;\n\
  return 0;\n\
}\n";
        for language in [Language::TypeScript, Language::Tsx] {
            let file = parse(source, language);
            assert_eq!(file.functions.len(), 1);
            assert_eq!(file.functions[0].name, "choose");
            assert_eq!(file.functions[0].complexity, 2);
            assert_eq!(file.functions[0].source_lines, 3);
            assert!(file.without_tests.is_none());
        }
    }

    #[test]
    fn typescript_out_remains_a_contextual_type_parameter_name() {
        let source = "\
interface A<out> { value: string; }\n\
interface Pair<T, out> { first: T; second: out; }\n\
class Q<out> extends Array<out> {}\n\
function identity<out>(value: out): out { return value; }\n\
type Defaulted<out = string> = out;\n\
interface Covariant<out out> { readonly value: out; }\n";
        for language in [Language::TypeScript, Language::Tsx] {
            let file = parse(source, language);
            assert_eq!(file.source_lines, 6);
            assert_eq!(file.functions.len(), 1);
            assert_eq!(file.functions[0].complexity, 1);
            assert_eq!(file.functions[0].source_lines, 1);
        }
    }

    #[test]
    fn typescript_negative_comparison_in_arrow_call_arguments() {
        for language in [Language::TypeScript, Language::Tsx] {
            for expression in ["item.value < -1", "item.value < +1", "item.value < 1"] {
                for suffix in [");", ",);", ", other);"] {
                    let source = format!(
                        "const filtered = items.filter((item: Item) => {expression}{suffix}\n"
                    );
                    let file = parse(&source, language);
                    assert_eq!(file.functions.len(), 1);
                    assert_eq!(file.functions[0].complexity, 1);
                    assert_eq!(file.functions[0].source_lines, 1);
                    let mut adapter = JavaScriptAdapter::new(language).unwrap();
                    let tree = adapter.parser.parse(&source, None).unwrap();
                    let mut stack = vec![tree.root_node()];
                    let mut arrows = 0;
                    while let Some(node) = stack.pop() {
                        if node.kind() == "arrow_function" {
                            let body = node.child_by_field_name("body").unwrap();
                            assert_eq!(body.kind(), "binary_expression");
                            assert_eq!(
                                body.child_by_field_name("right").unwrap().kind(),
                                if expression == "item.value < 1" {
                                    "number"
                                } else {
                                    "unary_expression"
                                }
                            );
                            arrows += 1;
                        }
                        let mut cursor = node.walk();
                        stack.extend(node.named_children(&mut cursor));
                    }
                    assert_eq!(arrows, 1);
                }
            }
            let source = "type Negative = -1;\ntype Boxed = Box<-1,>;\nf<-1>();\nf<-1,>();\nconst specialized = f<-1>;\nf(x < -1,);\n";
            let file = parse(source, language);
            assert!(file.functions.is_empty());
            assert_eq!(file.source_lines, 6);
            let mut adapter = JavaScriptAdapter::new(language).unwrap();
            let tree = adapter.parser.parse(source, None).unwrap();
            let mut stack = vec![tree.root_node()];
            let mut kinds = Vec::new();
            while let Some(node) = stack.pop() {
                kinds.push(node.kind());
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
            for (kind, count) in [
                ("type_arguments", 4),
                ("instantiation_expression", 1),
                ("binary_expression", 1),
            ] {
                assert_eq!(kinds.iter().filter(|&&found| found == kind).count(), count);
            }
        }
    }

    #[test]
    fn typescript_contextual_keywords_as_parameter_names() {
        for language in [Language::TypeScript, Language::Tsx] {
            let file = parse(
                "type Callback = (any) => any;\ninterface Events { readonly handler: (readonly?: boolean, reason?: string) => void; }\n",
                language,
            );
            assert!(file.functions.is_empty());
            assert_eq!(file.source_lines, 2);
            for keyword in [
                "any", "readonly", "number", "boolean", "string", "symbol", "object", "unknown",
                "never",
            ] {
                assert!(
                    parse(&format!("type Callback = ({keyword}) => any;\n"), language)
                        .functions
                        .is_empty()
                );
            }
            assert!(
                parse(
                    "type A = (any);\ntype B = readonly number[];\ntype C = (readonly number[]);\n",
                    language
                )
                .functions
                .is_empty()
            );
            assert_eq!(
                parse(
                    "class C { constructor(readonly value: string) {} }\n",
                    language
                )
                .functions
                .len(),
                1
            );
        }
    }
}
