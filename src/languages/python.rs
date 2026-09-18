use anyhow::{Context, Result, ensure};
use std::collections::{HashMap, HashSet};
use tree_sitter::{Node, Parser};

use super::{
    LanguageAdapter, LanguageTestPolicy,
    tree::{self, SyntaxRules},
};
use crate::metrics::{FileAnalysis, TestRegion};

const TEST_PATHS: &[&str] = &[
    "**/test/**",
    "**/tests/**",
    "**/__tests__/**",
    "**/test_*.{py,pyw,pyi}",
    "**/*_test.{py,pyw,pyi}",
    "**/conftest.{py,pyw,pyi}",
];

const TEST_POLICY_VERSION: &str = "python-tests-v1";
const TEST_SYNTAX_RULES: &[&str] = &[
    "Whole classes inheriting from statically bound unittest.TestCase or IsolatedAsyncioTestCase, including import aliases and earlier undecorated, unmodified local subclass chains.",
    "Functions (including async functions) decorated with statically bound pytest.fixture or yield_fixture, bare or called, including import aliases.",
    "Functions named test_* and classes named Test* decorated with statically bound pytest.mark.<marker>, bare or called; whole decorated definitions, including class helpers, are excluded. Explicit __test__ overrides or class constructors retain marked definitions; marked methods require a discoverable Test* container.",
    "Standalone calls to statically bound unittest.main, including import aliases; a __name__ == '__main__' guard is excluded only when its entire body is such calls and it has no alternate branch.",
    "Bindings must be unique and precede use. Parameters, assignments, imports, definitions, deletion, captures, global/nonlocal declarations and wildcard imports shadow evidence; framework attribute writes invalidate that framework.",
    "Conservative static subset: assertions, test names or framework imports alone are not tests. Assignment aliases, wildcard-import evidence, custom decorators, pytestmark variables, generic annotation scopes, dynamic bases/metaclasses and decorated or mutated inheritance chains are not resolved. Conditional imports/definitions and multiply bound names remain uncertain.",
];

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
    test_paths: tree::TestPaths,
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
        Ok(Self {
            parser,
            test_paths: tree::TestPaths::new(TEST_PATHS)?,
        })
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestSymbol {
    Unittest,
    UnittestCase,
    UnittestAsyncCase,
    TestCase,
    Main,
    Pytest,
    Fixture,
    Mark,
    Marker,
    Class(usize),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScopeKind {
    Module,
    Class,
    Function,
    Comprehension,
}

struct TestScope<'source> {
    parent: Option<usize>,
    kind: ScopeKind,
    bindings: HashMap<&'source str, Option<(TestSymbol, usize)>>,
    wildcard: bool,
    pytest_class: bool,
}

struct TestBindings<'tree, 'source> {
    source: &'source [u8],
    scopes: Vec<TestScope<'source>>,
    candidates: Vec<(Node<'tree>, usize)>,
    declarations: Vec<(&'source str, usize, bool)>,
    mutations: Vec<(Node<'tree>, usize)>,
    subclasses: HashSet<usize>,
    mutated_classes: HashSet<usize>,
    class_scopes: HashMap<usize, usize>,
    discovery_overrides: HashSet<(usize, &'source str)>,
    mutated_unittest: bool,
    mutated_pytest: bool,
}

fn definition_region(mut node: Node<'_>) -> Node<'_> {
    if let Some(parent) = node.parent()
        && parent.kind() == "decorated_definition"
    {
        node = parent;
    }
    node
}

impl<'tree, 'source> TestBindings<'tree, 'source> {
    fn text(&self, node: Node<'_>) -> &'source str {
        // The shared analyzer validates UTF-8 and the entire syntax tree first.
        std::str::from_utf8(&self.source[node.byte_range()]).expect("validated Python source")
    }

    fn scope(&mut self, parent: Option<usize>, kind: ScopeKind) -> usize {
        let index = self.scopes.len();
        self.scopes.push(TestScope {
            parent,
            kind,
            bindings: HashMap::new(),
            wildcard: false,
            pytest_class: false,
        });
        index
    }

    fn lexical_parent(&self, mut scope: usize) -> usize {
        while self.scopes[scope].kind == ScopeKind::Class {
            scope = self.scopes[scope]
                .parent
                .expect("class has an enclosing scope");
        }
        scope
    }

    fn bind(&mut self, scope: usize, name: &'source str, symbol: Option<(TestSymbol, usize)>) {
        self.scopes[scope]
            .bindings
            .entry(name)
            .and_modify(|binding| *binding = None)
            .or_insert(symbol);
    }

    fn targets(&mut self, node: Node<'tree>, scope: usize) {
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "identifier" => self.bind(scope, self.text(node), None),
                "attribute" | "subscript" => self.mutations.push((node, scope)),
                "pattern_list"
                | "tuple_pattern"
                | "list_pattern"
                | "list_splat_pattern"
                | "dictionary_splat_pattern"
                | "as_pattern_target"
                | "expression_list"
                | "tuple"
                | "list"
                | "parenthesized_expression" => {
                    let mut cursor = node.walk();
                    stack.extend(node.named_children(&mut cursor));
                }
                _ => {}
            }
        }
    }

    fn parameters(&mut self, node: Node<'tree>, scope: usize) {
        let mut cursor = node.walk();
        for parameter in node.named_children(&mut cursor) {
            match parameter.kind() {
                "default_parameter" | "typed_default_parameter" => {
                    if let Some(name) = parameter.child_by_field_name("name") {
                        self.targets(name, scope);
                    }
                }
                "typed_parameter" => {
                    if let Some(name) = parameter.named_child(0) {
                        self.targets(name, scope);
                    }
                }
                _ => self.targets(parameter, scope),
            }
        }
    }

    fn import(&mut self, node: Node<'tree>, scope: usize, conditional: bool) {
        let module = node
            .child_by_field_name("module_name")
            .map(|module| self.text(module));
        let mut cursor = node.walk();
        for item in node.named_children(&mut cursor) {
            if Some(item) == node.child_by_field_name("module_name") {
                continue;
            }
            if item.kind() == "wildcard_import" {
                self.scopes[scope].wildcard = true;
                continue;
            }
            let (name, alias) = if item.kind() == "aliased_import" {
                (
                    item.child_by_field_name("name").expect("import name"),
                    item.child_by_field_name("alias"),
                )
            } else {
                (item, None)
            };
            let name = self.text(name);
            let local = alias.map_or_else(
                || {
                    if module.is_none() {
                        name.split('.').next().expect("import component")
                    } else {
                        name
                    }
                },
                |alias| self.text(alias),
            );
            let symbol = match (module, name) {
                (None, "unittest") => Some(TestSymbol::Unittest),
                (None, "unittest.case") => Some(if alias.is_some() {
                    TestSymbol::UnittestCase
                } else {
                    TestSymbol::Unittest
                }),
                (None, "unittest.async_case") => Some(if alias.is_some() {
                    TestSymbol::UnittestAsyncCase
                } else {
                    TestSymbol::Unittest
                }),
                (None, "pytest") => Some(TestSymbol::Pytest),
                (Some("unittest" | "unittest.case"), "TestCase")
                | (Some("unittest" | "unittest.async_case"), "IsolatedAsyncioTestCase") => {
                    Some(TestSymbol::TestCase)
                }
                (Some("unittest"), "case") => Some(TestSymbol::UnittestCase),
                (Some("unittest"), "async_case") => Some(TestSymbol::UnittestAsyncCase),
                (Some("unittest"), "main") => Some(TestSymbol::Main),
                (Some("pytest"), "fixture" | "yield_fixture") => Some(TestSymbol::Fixture),
                (Some("pytest"), "mark") => Some(TestSymbol::Mark),
                _ => None,
            };
            self.bind(
                scope,
                local,
                symbol
                    .filter(|_| !conditional)
                    .map(|symbol| (symbol, node.end_byte())),
            );
        }
    }

    fn captures(&mut self, pattern: Node<'tree>, scope: usize) {
        let mut stack = vec![pattern];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "dotted_name" => {
                    if node.named_child_count() == 1 {
                        self.targets(node.named_child(0).expect("capture"), scope);
                    }
                }
                "identifier" => self.targets(node, scope),
                "class_pattern" | "keyword_pattern" => {
                    let mut cursor = node.walk();
                    stack.extend(node.named_children(&mut cursor).skip(1));
                }
                "string" | "attribute" => {}
                _ => {
                    let mut cursor = node.walk();
                    stack.extend(node.named_children(&mut cursor));
                }
            }
        }
    }

    fn collect(root: Node<'tree>, source: &'source [u8]) -> Self {
        let mut bindings = Self {
            source,
            scopes: Vec::new(),
            candidates: Vec::new(),
            declarations: Vec::new(),
            mutations: Vec::new(),
            subclasses: HashSet::new(),
            mutated_classes: HashSet::new(),
            class_scopes: HashMap::new(),
            discovery_overrides: HashSet::new(),
            mutated_unittest: false,
            mutated_pytest: false,
        };
        let module = bindings.scope(None, ScopeKind::Module);
        let mut stack = vec![(root, module, false)];
        while let Some((node, mut scope, conditional)) = stack.pop() {
            let mut body_scope = None;
            match node.kind() {
                "function_definition" | "class_definition" | "lambda" => {
                    bindings.candidates.push((node, scope));
                    let class = node.kind() == "class_definition";
                    if let Some(name) = node.child_by_field_name("name") {
                        bindings.bind(
                            scope,
                            bindings.text(name),
                            (class && !conditional)
                                .then_some((TestSymbol::Class(node.id()), node.end_byte())),
                        );
                    }
                    let inner = bindings.scope(
                        Some(bindings.lexical_parent(scope)),
                        if class {
                            ScopeKind::Class
                        } else {
                            ScopeKind::Function
                        },
                    );
                    if class {
                        bindings.class_scopes.insert(node.id(), inner);
                        bindings.scopes[inner].pytest_class = node
                            .child_by_field_name("name")
                            .is_some_and(|name| bindings.text(name).starts_with("Test"));
                    }
                    if let Some(parameters) = node.child_by_field_name("parameters") {
                        bindings.parameters(parameters, inner);
                    }
                    // Type parameters bind names in an annotation scope. Do not
                    // infer framework identity through that separate namespace.
                    if node.child_by_field_name("type_parameters").is_some() {
                        bindings.scopes[inner].wildcard = true;
                    }
                    body_scope = Some(inner);
                }
                "list_comprehension"
                | "set_comprehension"
                | "dictionary_comprehension"
                | "generator_expression" => {
                    scope = bindings.scope(
                        Some(bindings.lexical_parent(scope)),
                        ScopeKind::Comprehension,
                    );
                }
                "import_statement" | "import_from_statement" => {
                    bindings.import(node, scope, conditional);
                    continue;
                }
                "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                    if let Some(left) = node.child_by_field_name("left") {
                        bindings.targets(left, scope);
                    }
                }
                "type_alias_statement" => {
                    if let Some(mut name) = node.child_by_field_name("left") {
                        while matches!(name.kind(), "type" | "generic_type") {
                            let Some(child) = name.named_child(0) else {
                                break;
                            };
                            name = child;
                        }
                        bindings.targets(name, scope);
                    }
                }
                "named_expression" => {
                    let mut owner = scope;
                    while bindings.scopes[owner].kind == ScopeKind::Comprehension {
                        owner = bindings.scopes[owner].parent.expect("comprehension parent");
                    }
                    if let Some(name) = node.child_by_field_name("name") {
                        bindings.targets(name, owner);
                    }
                }
                "as_pattern" => {
                    if let Some(alias) = node.child_by_field_name("alias") {
                        bindings.targets(alias, scope);
                    }
                }
                "case_clause" => {
                    let mut cursor = node.walk();
                    for pattern in node
                        .named_children(&mut cursor)
                        .filter(|child| child.kind() == "case_pattern")
                    {
                        bindings.captures(pattern, scope);
                    }
                }
                "delete_statement" => {
                    let mut cursor = node.walk();
                    for target in node.named_children(&mut cursor) {
                        bindings.targets(target, scope);
                    }
                }
                "global_statement" | "nonlocal_statement" => {
                    let mut cursor = node.walk();
                    for name in node.named_children(&mut cursor) {
                        let name = bindings.text(name);
                        bindings.bind(scope, name, None);
                        bindings.declarations.push((
                            name,
                            scope,
                            node.kind() == "global_statement",
                        ));
                    }
                }
                "expression_statement" | "if_statement" => bindings.candidates.push((node, scope)),
                _ => {}
            }
            let conditional = conditional
                || matches!(
                    node.kind(),
                    "if_statement"
                        | "for_statement"
                        | "while_statement"
                        | "try_statement"
                        | "with_statement"
                        | "match_statement"
                );
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                let is_body = Some(child) == node.child_by_field_name("body");
                stack.push(if let Some(inner) = body_scope.filter(|_| is_body) {
                    (child, inner, false)
                } else {
                    (child, scope, conditional)
                });
            }
        }
        for scope in &mut bindings.scopes {
            if ["__test__", "__init__", "__new__"]
                .iter()
                .any(|name| scope.bindings.contains_key(name))
            {
                scope.pytest_class = false;
            }
        }
        for &(name, scope, global) in &bindings.declarations {
            let mut owner = if global {
                Some(0)
            } else {
                bindings.scopes[scope].parent
            };
            while let Some(index) = owner {
                if global || bindings.scopes[index].bindings.contains_key(name) {
                    bindings.scopes[index].bindings.insert(name, None);
                    break;
                }
                owner = bindings.scopes[index].parent;
            }
        }
        // Attribute mutation through any framework alias makes its API
        // uncertain, including other aliases of the same imported module.
        for &(target, scope) in &bindings.mutations {
            let mut root = target;
            while matches!(
                root.kind(),
                "attribute" | "subscript" | "parenthesized_expression"
            ) {
                let Some(object) = root
                    .child_by_field_name("object")
                    .or_else(|| root.child_by_field_name("value"))
                    .or_else(|| {
                        (root.kind() == "parenthesized_expression")
                            .then(|| root.named_child(0))
                            .flatten()
                    })
                else {
                    break;
                };
                root = object;
            }
            if root.kind() == "identifier"
                && let Some(TestSymbol::Class(id)) =
                    bindings.lookup(bindings.text(root), scope, usize::MAX)
            {
                bindings.mutated_classes.insert(id);
            }
            if root.kind() == "identifier"
                && target
                    .child_by_field_name("attribute")
                    .is_some_and(|attribute| bindings.text(attribute) == "__test__")
            {
                let name = bindings.text(root);
                let mut owner = Some(scope);
                while let Some(index) = owner {
                    if bindings.scopes[index].bindings.contains_key(name) {
                        bindings.discovery_overrides.insert((index, name));
                        if let Some(Some((TestSymbol::Class(id), _))) =
                            bindings.scopes[index].bindings.get(name)
                            && let Some(&body) = bindings.class_scopes.get(id)
                        {
                            bindings.scopes[body].pytest_class = false;
                        }
                        break;
                    }
                    owner = bindings.scopes[index].parent;
                }
            }
            match bindings.resolve(root, scope, usize::MAX) {
                Some(
                    TestSymbol::Unittest
                    | TestSymbol::UnittestCase
                    | TestSymbol::UnittestAsyncCase
                    | TestSymbol::TestCase
                    | TestSymbol::Main,
                ) => bindings.mutated_unittest = true,
                Some(
                    TestSymbol::Pytest
                    | TestSymbol::Fixture
                    | TestSymbol::Mark
                    | TestSymbol::Marker,
                ) => bindings.mutated_pytest = true,
                _ => {}
            }
        }
        bindings
    }

    fn lookup(&self, name: &str, mut scope: usize, before: usize) -> Option<TestSymbol> {
        loop {
            let current = &self.scopes[scope];
            if current.wildcard {
                return None;
            }
            if let Some(binding) = current.bindings.get(name) {
                let (symbol, position) = (*binding)?;
                return (position <= before).then_some(symbol);
            }
            scope = current.parent?;
        }
    }

    fn resolve(&self, mut node: Node<'_>, scope: usize, before: usize) -> Option<TestSymbol> {
        let mut attributes = Vec::new();
        loop {
            match node.kind() {
                "identifier" => break,
                "attribute" => {
                    attributes.push(self.text(node.child_by_field_name("attribute")?));
                    node = node.child_by_field_name("object")?;
                }
                "parenthesized_expression" if node.named_child_count() == 1 => {
                    node = node.named_child(0)?;
                }
                _ => return None,
            }
        }
        let mut symbol = self.lookup(self.text(node), scope, before)?;
        if let TestSymbol::Class(id) = symbol {
            if !self.subclasses.contains(&id) {
                return None;
            }
            symbol = TestSymbol::TestCase;
        }
        for attribute in attributes.into_iter().rev() {
            symbol = match (symbol, attribute) {
                (TestSymbol::Unittest, "TestCase" | "IsolatedAsyncioTestCase")
                | (TestSymbol::UnittestCase, "TestCase")
                | (TestSymbol::UnittestAsyncCase, "IsolatedAsyncioTestCase") => {
                    TestSymbol::TestCase
                }
                (TestSymbol::Unittest, "case") => TestSymbol::UnittestCase,
                (TestSymbol::Unittest, "async_case") => TestSymbol::UnittestAsyncCase,
                (TestSymbol::Unittest, "main") => TestSymbol::Main,
                (TestSymbol::Pytest, "fixture" | "yield_fixture") => TestSymbol::Fixture,
                (TestSymbol::Pytest, "mark") => TestSymbol::Mark,
                (TestSymbol::Mark, marker) if !marker.starts_with('_') => TestSymbol::Marker,
                _ => return None,
            };
        }
        match symbol {
            TestSymbol::Unittest
            | TestSymbol::UnittestCase
            | TestSymbol::UnittestAsyncCase
            | TestSymbol::TestCase
            | TestSymbol::Main
                if self.mutated_unittest =>
            {
                None
            }
            TestSymbol::Pytest | TestSymbol::Fixture | TestSymbol::Mark | TestSymbol::Marker
                if self.mutated_pytest =>
            {
                None
            }
            _ => Some(symbol),
        }
    }

    fn unittest_class(&self, node: Node<'_>, scope: usize) -> bool {
        if node.child_by_field_name("type_parameters").is_some() {
            return false;
        }
        let Some(bases) = node.child_by_field_name("superclasses") else {
            return false;
        };
        let mut cursor = bases.walk();
        let mut found = false;
        for base in bases.named_children(&mut cursor) {
            if base.kind() == "comment" {
                continue;
            }
            let mut expression = base;
            loop {
                match expression.kind() {
                    "identifier" => break,
                    "attribute" => {
                        expression = expression
                            .child_by_field_name("object")
                            .expect("attribute object");
                    }
                    "parenthesized_expression" if expression.named_child_count() == 1 => {
                        expression = expression.named_child(0).expect("parenthesized base");
                    }
                    _ => return false,
                }
            }
            found |= self.resolve(base, scope, node.start_byte()) == Some(TestSymbol::TestCase);
        }
        found
    }

    fn pytest_definition(&self, node: Node<'_>, scope: usize) -> Option<&'static str> {
        let definition = definition_region(node);
        if definition == node {
            return None;
        }
        let name = self.text(node.child_by_field_name("name")?);
        let class = node.kind() == "class_definition";
        let discovered = if class {
            self.class_scopes
                .get(&node.id())
                .is_some_and(|&body| self.scopes[body].pytest_class)
        } else {
            name.starts_with("test_")
                && (self.scopes[scope].kind != ScopeKind::Class || self.scopes[scope].pytest_class)
        } && !self.discovery_overrides.contains(&(scope, name))
            && !self.scopes[0].bindings.contains_key("__test__");
        let mut cursor = definition.walk();
        for decorator in definition
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "decorator")
        {
            let mut expression = decorator.named_child(0)?;
            if expression.kind() == "call" {
                expression = expression.child_by_field_name("function")?;
            }
            match self.resolve(expression, scope, definition.start_byte()) {
                Some(TestSymbol::Fixture) if !class => return Some("pytest fixture"),
                Some(TestSymbol::Marker) if discovered => return Some("pytest marked test"),
                _ => {}
            }
        }
        None
    }

    fn main_call(&self, node: Node<'_>, scope: usize) -> bool {
        if node.kind() != "expression_statement" || node.named_child_count() != 1 {
            return false;
        }
        let call = node.named_child(0).expect("single expression");
        call.kind() == "call"
            && call
                .child_by_field_name("function")
                .is_some_and(|function| {
                    self.resolve(function, scope, node.start_byte()) == Some(TestSymbol::Main)
                })
    }

    fn main_guard(&self, node: Node<'_>, scope: usize) -> bool {
        if node.kind() != "if_statement"
            || node.child_by_field_name("alternative").is_some()
            || self.scopes[scope].kind != ScopeKind::Module
            || self.scopes[scope].bindings.contains_key("__name__")
            || self.scopes[scope].wildcard
        {
            return false;
        }
        let Some(condition) = node.child_by_field_name("condition") else {
            return false;
        };
        if condition.kind() != "comparison_operator" || condition.child_count() != 3 {
            return false;
        }
        let left = condition.child(0).expect("comparison left");
        let operator = condition.child(1).expect("comparison operator");
        let right = condition.child(2).expect("comparison right");
        let name = |node| self.text(node) == "__name__";
        let main = |node| matches!(self.text(node), "\"__main__\"" | "'__main__'");
        if self.text(operator) != "=="
            || !((name(left) && main(right)) || (main(left) && name(right)))
        {
            return false;
        }
        let Some(body) = node.child_by_field_name("consequence") else {
            return false;
        };
        let mut cursor = body.walk();
        let statements: Vec<_> = body
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "comment")
            .collect();
        !statements.is_empty()
            && statements
                .into_iter()
                .all(|statement| self.main_call(statement, scope))
    }

    fn regions(mut self) -> Vec<TestRegion> {
        let mut regions = Vec::new();
        // Source order proves local inheritance without recursively resolving
        // arbitrary user expressions or following cyclic class bindings.
        self.candidates.sort_by_key(|(node, _)| node.start_byte());
        for &(node, scope) in &self.candidates {
            let reason = match node.kind() {
                "class_definition" if self.unittest_class(node, scope) => {
                    if definition_region(node) == node && !self.mutated_classes.contains(&node.id())
                    {
                        self.subclasses.insert(node.id());
                    }
                    Some("unittest TestCase subclass")
                }
                "class_definition" | "function_definition" => self.pytest_definition(node, scope),
                "expression_statement" if self.main_call(node, scope) => {
                    Some("unittest main runner")
                }
                "if_statement" if self.main_guard(node, scope) => Some("unittest main guard"),
                _ => None,
            };
            if let Some(reason) = reason {
                regions.push(tree::test_region(definition_region(node), reason));
            }
        }
        regions
    }
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

    fn test_regions(root: Node<'_>, source: &[u8]) -> Vec<TestRegion> {
        TestBindings::collect(root, source).regions()
    }
}

impl LanguageAdapter for PythonAdapter {
    fn test_policy(&self) -> LanguageTestPolicy {
        LanguageTestPolicy {
            version: TEST_POLICY_VERSION,
            path_patterns: TEST_PATHS,
            syntax_rules: TEST_SYNTAX_RULES,
        }
    }

    fn is_test_file(&self, path: &str) -> bool {
        self.test_paths.matches(path)
    }

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
    fn python_owns_component_bounded_case_sensitive_test_paths() {
        let adapter = PythonAdapter::new().unwrap();
        let policy = adapter.test_policy();
        assert_eq!(policy.version, TEST_POLICY_VERSION);
        assert_eq!(policy.path_patterns, TEST_PATHS);
        assert_eq!(policy.syntax_rules, TEST_SYNTAX_RULES);
        for path in [
            "test/helpers.py",
            "src/tests/helpers.pyw",
            "__tests__/helpers.pyi",
            "test_service.py",
            "src/test_service.pyw",
            "src/service_test.pyi",
            "conftest.py",
            "src/conftest.pyw",
            "src/conftest.pyi",
        ] {
            assert!(adapter.is_test_file(path), "{path}");
        }
        for path in [
            "src/testing/service.py",
            "src/contest/service.py",
            "src/Tests/service.py",
            "src/Test_service.py",
            "src/test_service.PY",
            "src/service_test.py.extra",
            "src/test_service/data.py",
            "src/conftest.py/data.py",
            "src/conftest_helpers.py",
            "src/test_service.rs",
        ] {
            assert!(!adapter.is_test_file(path), "{path}");
        }
    }

    fn retained_names(file: &FileAnalysis) -> Vec<&str> {
        file.without_tests
            .as_ref()
            .expect("syntax test regions")
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .collect()
    }

    #[test]
    fn unittest_whole_classes_remove_helpers_setup_and_decorators() {
        let source = "import unittest as ut
def production(x):
    return x if x else 0
@decorate
@other(flag=True)
class Checks(ut.TestCase):
    'class docs'
    def setUp(self):
        self.value = 1
    def helper(self, value):
        if value: return value
    def test_value(self):
        assert self.helper(self.value)
def test_connection():
    return 'production'
";
        let file = parse(source);
        assert_eq!(file.functions.len(), 5);
        assert_eq!(
            file.functions
                .iter()
                .map(|function| function.complexity)
                .collect::<Vec<_>>(),
            [2, 1, 2, 2, 1]
        );
        assert_eq!(retained_names(&file), ["production", "test_connection"]);
        let filtered = file.without_tests.as_ref().unwrap();
        assert_eq!(filtered.source_lines, 5);
        assert_eq!(filtered.regions.len(), 1);
        let region = &filtered.regions[0];
        assert_eq!(region.start_line, 4);
        assert_eq!(region.end_line, 13);
        assert_eq!(region.reason, "unittest TestCase subclass");
        assert_eq!(
            &source[region.start_byte..region.end_byte],
            "@decorate\n@other(flag=True)\nclass Checks(ut.TestCase):\n    'class docs'\n    def setUp(self):\n        self.value = 1\n    def helper(self, value):\n        if value: return value\n    def test_value(self):\n        assert self.helper(self.value)"
        );
        assert_eq!(
            filtered.functions,
            [file.functions[0].clone(), file.functions[4].clone()]
        );
    }

    #[test]
    fn unittest_import_aliases_and_local_inheritance_are_proven() {
        for import_and_base in [
            ("import unittest", "unittest.TestCase"),
            ("import unittest as ut", "ut.IsolatedAsyncioTestCase"),
            ("from unittest import TestCase as Base", "Base"),
            (
                "from unittest import IsolatedAsyncioTestCase as Base",
                "Base",
            ),
            ("from unittest.case import TestCase as Base", "Base"),
            ("import unittest.case as case", "case.TestCase"),
            ("from unittest import case as case", "case.TestCase"),
            (
                "import unittest.async_case as ac",
                "ac.IsolatedAsyncioTestCase",
            ),
        ] {
            let (import, base) = import_and_base;
            let file = parse(&format!(
                "{import}\nclass Parent({base}):\n    def helper(self): return 1\nclass Child(Parent):\n    async def test_child(self): assert True\nclass Grandchild(Child):\n    def extra(self): return 2\ndef production(): return 3\n"
            ));
            assert_eq!(file.functions.len(), 4, "{import}");
            assert_eq!(retained_names(&file), ["production"], "{import}");
            assert_eq!(file.without_tests.unwrap().regions.len(), 3, "{import}");
        }
    }

    #[test]
    fn pytest_fixtures_marks_aliases_async_and_decorator_shapes() {
        let source = "import pytest as pt
from pytest import fixture as fx, yield_fixture as yf, mark as marks
@wrap
@fx(scope='module')
@other(lambda x: x if x else None)
async def resource():
    yield 1
@pt.yield_fixture
def old_resource():
    yield 2
@yf()
def another_resource(): return 3
@pt.fixture
def fourth_resource(): return 4
@marks.parametrize('value', [1, 2], ids=['one', 'two'])
@pt.mark.asyncio
async def test_value(value):
    assert value
@pt.mark.smoke()
class TestService:
    def helper(self): return 1
    def test_service(self): assert self.helper()
@pt.mark.smoke
def production():
    assert True
def test_connection(): return 1
";
        let file = parse(source);
        assert_eq!(retained_names(&file), ["production", "test_connection"]);
        let filtered = file.without_tests.as_ref().unwrap();
        assert_eq!(filtered.regions.len(), 6);
        assert!(filtered.regions.iter().any(|region| {
            source[region.start_byte..region.end_byte].starts_with("@wrap\n@fx")
        }));
        assert!(
            file.functions
                .iter()
                .any(|function| function.name == "<lambda>")
        );
        assert!(
            filtered
                .functions
                .iter()
                .all(|function| function.name != "<lambda>")
        );
        assert_eq!(filtered.source_lines, 6);
    }

    #[test]
    fn names_assertions_and_unrelated_framework_lookalikes_are_retained() {
        for source in [
            "def test_connection():\n    assert True\n",
            "import pytest\ndef test_connection():\n    assert True\n",
            "import unittest\nclass TestCase:\n    def test_value(self): assert True\n",
            "class TestCase:\n    pass\nclass Production(TestCase):\n    def helper(self): return 1\n",
            "class Production(object.TestCase):\n    def helper(self): return 1\n",
            "from unrelated import TestCase\nclass Production(TestCase):\n    def helper(self): return 1\n",
            "import unrelated as pytest\n@pytest.fixture\ndef production(): return 1\n",
            "import pytest\n@pytest.mark.smoke\nclass Service:\n    def helper(self): return 1\n",
            "import pytest\n@pytest.mark.parametrize('value', [1])\ndef production(value): return value\n",
            "from unittest import *\nclass Checks(TestCase):\n    def test_value(self): assert True\n",
            "import unittest\nfrom custom import *\nclass Checks(unittest.TestCase):\n    def test_value(self): assert True\n",
            "import unittest\nclass Checks(factory(unittest.TestCase)):\n    def helper(self): return 1\n",
            "import unittest\nclass Checks(unittest.TestCase, metaclass=Custom):\n    def helper(self): return 1\n",
            "import unittest\nclass Checks(unittest.TestCase, (factory())):\n    def helper(self): return 1\n",
            "import unittest\nclass Checks(unittest.TestCase, factory().Mixin):\n    def helper(self): return 1\n",
            "from unittest import TestCase as Base\nclass Checks[Base](Base):\n    def helper(self): return 1\n",
            "if enabled:\n    import unittest\nclass Checks(unittest.TestCase):\n    def helper(self): return 1\n",
            "import pytest\nfixture_alias = pytest.fixture\n@fixture_alias\ndef production(): return 1\n",
            "import pytest\n@pytest.mark.__class__\ndef test_connection(): return 1\n",
            "main()\nother.main()\n",
        ] {
            let file = parse(source);
            assert!(file.without_tests.is_none(), "{source}");
        }
    }

    #[test]
    fn parameters_and_scope_local_rebindings_shadow_frameworks() {
        for parameter in [
            "pytest",
            "pytest=None",
            "pytest: object",
            "pytest: object=None",
            "*pytest",
            "**pytest",
        ] {
            let file = parse(&format!(
                "import pytest\ndef production({parameter}):\n    @pytest.fixture\n    def resource(): return 1\n    return resource\n"
            ));
            assert!(file.without_tests.is_none(), "{parameter}");
        }
        for statement in [
            "pytest = other",
            "pytest: object = other",
            "pytest: object",
            "type pytest = object",
            "type pytest[T] = list[T]",
            "pytest += other",
            "pytest, other = pair",
            "[pytest, other] = pair",
            "other, *pytest = values",
            "del pytest",
            "del other, pytest",
            "del (pytest)",
            "import other as pytest",
            "from other import pytest",
            "def pytest(): pass",
            "class pytest: pass",
            "for pytest in values: pass",
            "for other, *pytest in values: pass",
            "with context() as pytest: pass",
            "with context() as (other, pytest): pass",
            "try: work()\n    except Exception as pytest: pass",
            "try: work()\n    except* Exception as pytest: pass",
            "if (pytest := other): pass",
            "match value:\n        case pytest: pass",
            "match value:\n        case {'key': pytest}: pass",
            "match value:\n        case {'key': value, **pytest}: pass",
            "match value:\n        case Container(value=pytest): pass",
            "match value:\n        case Container() as pytest: pass",
            "values = [pytest := item for item in items]",
        ] {
            for before in [true, false] {
                let definition = "    @pytest.fixture\n    def resource(): return 1\n";
                let assignment = format!("    {statement}\n");
                let body = if before {
                    format!("{assignment}{definition}")
                } else {
                    format!("{definition}{assignment}")
                };
                let source = format!("import pytest\ndef production():\n{body}");
                let file = parse(&source);
                assert!(file.without_tests.is_none(), "{source}");
            }
        }
        let source = "from unittest import TestCase as Base
def production(Base):
    class Checks(Base):
        def test_value(self): assert True
    return Checks
";
        assert!(parse(source).without_tests.is_none());
    }

    #[test]
    fn module_reassignment_and_framework_attribute_mutation_are_uncertain() {
        for source in [
            "import unittest as ut\nut = other\nclass C(ut.TestCase):\n    def helper(self): pass\n",
            "from unittest import TestCase as Base\nclass C(Base):\n    def helper(self): pass\nBase = object\n",
            "import unittest as ut\nimport unittest as other\nut.TestCase = object\nclass C(other.TestCase):\n    def helper(self): pass\n",
            "import pytest as pt\nfrom pytest import fixture\npt.fixture = wrapper\n@fixture\ndef resource(): return 1\n",
            "import pytest\npytest.mark.custom = wrapper\n@pytest.mark.custom\ndef test_value(): assert True\n",
            "import pytest\ndef replace():\n    global pytest\n    pytest = other\n@pytest.fixture\ndef resource(): return 1\n",
            "def outer():\n    import pytest\n    def replace():\n        nonlocal pytest\n        pytest = other\n    @pytest.fixture\n    def resource(): return 1\n",
        ] {
            assert!(parse(source).without_tests.is_none(), "{source}");
        }
    }

    #[test]
    fn decorated_or_mutated_local_bases_do_not_prove_inheritance() {
        for parent in [
            "@replace\nclass Base(unittest.TestCase):\n    def helper(self): pass\n",
            "class Base(unittest.TestCase):\n    def helper(self): pass\nBase.__bases__ = (object,)\n",
        ] {
            let file = parse(&format!(
                "import unittest\n{parent}class Production(Base):\n    def keep(self): return 1\n"
            ));
            assert_eq!(retained_names(&file), ["keep"]);
        }
    }

    #[test]
    fn pytest_discovery_overrides_and_non_test_containers_are_retained() {
        for source in [
            "import pytest\n@pytest.mark.smoke\nclass TestService:\n    __test__ = False\n    def helper(self): return 1\n",
            "import pytest\n@pytest.mark.smoke\nclass TestService:\n    def __init__(self): pass\n    @pytest.mark.smoke\n    def test_connection(self): return 1\n",
            "import pytest\n@pytest.mark.smoke\nclass TestService:\n    __new__ = construct\n    def helper(self): return 1\n",
            "import pytest\n@pytest.mark.smoke\nclass TestService:\n    def helper(self): return 1\nTestService.__test__ = False\n",
            "import pytest\nclass Production:\n    @pytest.mark.smoke\n    def test_connection(self): return 1\n",
            "import pytest\n@pytest.mark.smoke\ndef test_connection(): return 1\ntest_connection.__test__ = False\n",
            "import pytest\n__test__ = False\n@pytest.mark.smoke\ndef test_connection(): return 1\n",
        ] {
            assert!(parse(source).without_tests.is_none(), "{source}");
        }
        let file = parse(
            "import pytest\nclass TestService:\n    @pytest.mark.smoke\n    def test_connection(self): assert True\n    def helper(self): return 1\n@pytest.fixture\ndef resource(pytest): return pytest\n",
        );
        assert_eq!(retained_names(&file), ["helper"]);
        assert_eq!(file.without_tests.unwrap().regions.len(), 2);
    }

    #[test]
    fn local_imports_class_scopes_and_comprehensions_do_not_leak() {
        let file = parse(
            "import pytest\nclass Production:\n    pytest = other\n    def method(self):\n        @pytest.fixture\n        def resource(): return 1\n        return resource\nvalues = [pytest for pytest in items]\n@pytest.fixture\ndef outer_resource(): return 2\n",
        );
        assert_eq!(retained_names(&file), ["method"]);
        assert_eq!(file.without_tests.as_ref().unwrap().regions.len(), 2);
        assert!(
            parse(
                "def production():\n    import pytest\n@pytest.fixture\ndef resource(): return 1\n"
            )
            .without_tests
            .is_none()
        );
        let file = parse(
            "def production(flag):\n    from unittest import TestCase as Base\n    class Checks(Base):\n        def helper(self):\n            if flag: return 1\n        def test_value(self): assert flag\n    return flag if flag else None\n",
        );
        assert_eq!(file.source_lines, 7);
        assert_eq!(file.functions[0].complexity, 4);
        assert_eq!(file.functions[0].source_lines, 7);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.source_lines, 3);
        assert_eq!(filtered.functions.len(), 1);
        assert_eq!(filtered.functions[0].complexity, 2);
        assert_eq!(filtered.functions[0].source_lines, 3);
        assert_eq!(filtered.functions[0].start_line, 1);
        assert_eq!(filtered.functions[0].end_line, 7);
    }

    #[test]
    fn nested_pytest_regions_remove_enclosing_decisions_and_source_lines() {
        let file = parse(
            "import pytest\ndef production(flag):\n    @pytest.fixture(params=[1 if flag else 0])\n    def resource():\n        if flag and ready(): return 1\n        return 2\n    @pytest.mark.parametrize('value', [1, 2])\n    async def test_value(value):\n        assert value\n    return flag if flag else None\n",
        );
        assert_eq!(file.functions[0].complexity, 6);
        assert_eq!(file.functions[0].source_lines, 9);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.functions.len(), 1);
        assert_eq!(filtered.functions[0].complexity, 2);
        assert_eq!(filtered.functions[0].source_lines, 2);
        assert_eq!(filtered.source_lines, 3);
    }

    #[test]
    fn unittest_main_runner_guards_and_production_branches() {
        for source in [
            "import unittest\nif __name__ == '__main__':\n    unittest.main()\n",
            "from unittest import main as run\nif \"__main__\" == __name__:\n    # runner\n    run(verbosity=2)\n",
        ] {
            let file = parse(source);
            let filtered = file.without_tests.unwrap();
            assert_eq!(filtered.source_lines, 1);
            assert_eq!(filtered.regions.len(), 1);
            assert_eq!(filtered.regions[0].reason, "unittest main guard");
        }
        for source in [
            "import unittest\nif __name__ == '__main__':\n    production()\n    unittest.main()\n",
            "import unittest\nif __name__ == '__main__':\n    unittest.main()\nelse:\n    production()\n",
            "import unittest\nif __name__ == '__main__':\n    unittest.main()\nelif other:\n    production()\n",
        ] {
            let file = parse(source);
            let filtered = file.without_tests.unwrap();
            assert_eq!(filtered.regions.len(), 1);
            assert_eq!(filtered.regions[0].reason, "unittest main runner");
            assert_eq!(file.source_lines - filtered.source_lines, 1);
        }
        for source in [
            "from unrelated import main\nif __name__ == '__main__':\n    main()\n",
            "import unittest\nunittest = other\nunittest.main()\n",
            "from unittest import main\ndef production(main):\n    main()\n",
        ] {
            assert!(parse(source).without_tests.is_none(), "{source}");
        }
    }

    #[test]
    fn malformed_test_syntax_is_not_hidden_and_raw_metrics_are_unchanged() {
        struct RawPython;
        impl SyntaxRules for RawPython {
            fn callable(node: Node<'_>) -> bool {
                PythonAdapter::callable(node)
            }
            fn decisions(node: Node<'_>) -> u64 {
                PythonAdapter::decisions(node)
            }
            fn name(node: Node<'_>, source: &[u8]) -> String {
                PythonAdapter::name(node, source)
            }
            fn ignored(node: Node<'_>, source: &[u8]) -> bool {
                PythonAdapter::ignored(node, source)
            }
            fn validation_error(node: Node<'_>) -> Option<&'static str> {
                PythonAdapter::validation_error(node)
            }
        }
        for source in [
            "\"module docs\"\nimport unittest\nclass Checks(unittest.TestCase):\n    def test_value(self):\n        'test docs'\n        assert True\ndef production(x):\n    match x:\n        case _: return 0\n",
            "\u{feff}import pytest\r\n@pytest.fixture\r\nasync def resource():\r\n    'docs'\r\n    yield 1\r\n",
            "import unittest\nclass Checks(unittest.TestCase):\n    def broken(:\n",
            "import pytest\n@pytest.fixture\ndef broken():\n    # empty body\n",
        ] {
            let mut adapter = PythonAdapter::new().unwrap();
            let mut file = adapter.analyze(source.as_bytes()).unwrap();
            let raw = tree::analyze::<RawPython>(&mut adapter.parser, source.as_bytes()).unwrap();
            let filtered = file.without_tests.take();
            assert_eq!(file, raw);
            if raw.parsed() {
                let filtered = filtered.expect("recognized tests");
                for region in filtered.regions {
                    let removed = &source[region.start_byte..region.end_byte];
                    assert!(
                        removed.starts_with("class Checks")
                            || removed.starts_with("@pytest.fixture")
                    );
                }
            } else {
                assert!(filtered.is_none());
                assert!(!file.diagnostics.is_empty());
                assert!(file.functions.is_empty());
            }
        }
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
