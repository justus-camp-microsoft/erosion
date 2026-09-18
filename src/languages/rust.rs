use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use tree_sitter::{Node, Parser};

use super::{
    LanguageAdapter, LanguageTestPolicy,
    tree::{self, SyntaxRules},
};
use crate::metrics::{FileAnalysis, TestRegion};

const TEST_PATHS: &[&str] = &["**/tests/**", "**/tests.rs"];
const TEST_SYNTAX: &[&str] = &[
    "Unshadowed built-in #[test] functions; tokio::test, async_std::test, rstest::rstest and test_case::test_case functions, including explicit use/grouped-use and extern-crate aliases.",
    "Outer cfg/cfg_attr gates on syntax and inner gates on crates, modules, impls and function/block bodies are excluded only when definitely absent outside test mode and potentially present in test mode; all, any and not use conservative three-valued evaluation.",
    "Attached outer attributes and intervening comments belong to the excluded item; nested test regions are measured on the original AST.",
    "tests directories and dedicated tests.rs files only; no generic test/spec suffixes or benchmark exclusion.",
    "No macro expansion, Cargo/environment evaluation or cross-file resolution. Bare framework macro names need explicit imports; custom attributes, attributed imports, wildcard-import ambiguity, conflicting bindings and unresolved crate/self/super reexports are retained.",
    "Inline modules do not inherit ordinary parent imports; block scopes do. Local module/type names and macro/import bindings conservatively shadow qualified attributes. Absolute known crate paths bypass local imports but respect explicit extern-crate renames.",
    "Unknown cfg predicates are independent unknowns, not a SAT solver; unsupported meta subexpressions and alias/cfg recursion beyond 64 levels are treated as unknown.",
];

const DECISIONS: &[&str] = &[
    "if_expression",
    "for_expression",
    "while_expression",
    "loop_expression",
];

pub struct RustAdapter {
    parser: Parser,
    test_paths: tree::TestPaths,
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
                "attribute_item",
                "inner_attribute_item",
                "attribute",
                "token_tree",
                "use_declaration",
                "use_as_clause",
                "scoped_use_list",
                "use_list",
                "use_wildcard",
                "extern_crate_declaration",
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
        Ok(Self {
            parser,
            test_paths: tree::TestPaths::new(TEST_PATHS)?,
        })
    }
}

fn text<'s>(node: Node<'_>, source: &'s [u8]) -> &'s str {
    node.utf8_text(source).expect("validated Rust UTF-8")
}

fn comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

#[derive(Clone, Debug)]
struct AttributePath {
    absolute: bool,
    parts: Vec<String>,
}

fn path(node: Node<'_>, source: &[u8]) -> Option<AttributePath> {
    let mut pending = vec![node];
    let mut parts = Vec::new();
    let mut absolute = false;
    while let Some(node) = pending.pop() {
        match node.kind() {
            "identifier" | "type_identifier" | "crate" | "self" | "super" => {
                parts.push(text(node, source).trim_start_matches("r#").to_owned());
            }
            "::" => absolute |= parts.is_empty(),
            "scoped_identifier" | "scoped_type_identifier" => {
                let mut cursor = node.walk();
                let start = pending.len();
                pending.extend(node.children(&mut cursor));
                pending[start..].reverse();
            }
            _ if comment(node) => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then_some(AttributePath { absolute, parts })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Truth {
    False,
    Unknown,
    True,
}

impl Truth {
    fn not(self) -> Self {
        match self {
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
            Self::True => Self::False,
        }
    }

    fn all(values: impl IntoIterator<Item = Self>) -> Self {
        values.into_iter().fold(Self::True, |left, right| {
            if left == Self::False || right == Self::False {
                Self::False
            } else if left == Self::Unknown || right == Self::Unknown {
                Self::Unknown
            } else {
                Self::True
            }
        })
    }

    fn any(values: impl IntoIterator<Item = Self>) -> Self {
        Self::all(values.into_iter().map(Self::not)).not()
    }
}

struct Meta<'tree> {
    path: AttributePath,
    arguments: Option<Node<'tree>>,
    bare: bool,
}

fn meta<'tree>(nodes: &[Node<'tree>], source: &[u8]) -> Option<Meta<'tree>> {
    let (&first, mut rest) = nodes.split_first()?;
    let (mut attribute_path, consumed) = if first.kind() == "::" {
        let mut attribute_path = path(*rest.first()?, source)?;
        attribute_path.absolute = true;
        (attribute_path, 1)
    } else {
        (path(first, source)?, 0)
    };
    rest = &rest[consumed..];
    while rest.first().is_some_and(|node| node.kind() == "::") {
        let part = path(*rest.get(1)?, source)?;
        if part.absolute {
            return None;
        }
        attribute_path.parts.extend(part.parts);
        rest = &rest[2..];
    }
    let arguments = match rest {
        [] => None,
        [arguments] if arguments.kind() == "token_tree" => Some(*arguments),
        [equals, _value] if equals.kind() == "=" => None,
        _ => return None,
    };
    Some(Meta {
        path: attribute_path,
        arguments,
        bare: rest.is_empty(),
    })
}

fn attribute_meta<'tree>(node: Node<'tree>, source: &[u8]) -> Option<Meta<'tree>> {
    let mut cursor = node.walk();
    let attribute = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "attribute")?;
    let mut cursor = attribute.walk();
    let tokens: Vec<_> = attribute
        .children(&mut cursor)
        .filter(|child| !comment(*child))
        .collect();
    meta(&tokens, source)
}

fn arguments<'tree>(node: Node<'tree>, source: &[u8]) -> Option<Vec<Meta<'tree>>> {
    let mut cursor = node.walk();
    let tokens: Vec<_> = node
        .children(&mut cursor)
        .filter(|child| !comment(*child))
        .collect();
    if tokens.first()?.kind() != "(" || tokens.last()?.kind() != ")" {
        return None;
    }
    let tokens = &tokens[1..tokens.len() - 1];
    let mut result = Vec::new();
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind() == "," {
            result.push(meta(&tokens[start..index], source)?);
            start = index + 1;
        }
    }
    if start < tokens.len() {
        result.push(meta(&tokens[start..], source)?);
    }
    Some(result)
}

fn cfg_value(meta: &Meta<'_>, source: &[u8], test: bool, depth: usize) -> Truth {
    if depth >= 64 || meta.path.absolute || meta.path.parts.len() != 1 {
        return Truth::Unknown;
    }
    let name = meta.path.parts[0].as_str();
    if meta.bare && name == "test" {
        return if test { Truth::True } else { Truth::False };
    }
    let Some(values) = meta.arguments.and_then(|node| arguments(node, source)) else {
        return Truth::Unknown;
    };
    let evaluated = || {
        values
            .iter()
            .map(|value| cfg_value(value, source, test, depth + 1))
    };
    match name {
        "all" => Truth::all(evaluated()),
        "any" => Truth::any(evaluated()),
        "not" if values.len() == 1 => cfg_value(&values[0], source, test, depth + 1).not(),
        _ => Truth::Unknown,
    }
}

#[derive(Clone)]
enum Binding {
    Import(AttributePath),
    Crate(&'static str),
    Unknown,
}

#[derive(Default)]
struct Scope {
    parent: Option<usize>,
    module: bool,
    imports: BTreeMap<String, Binding>,
    externs: BTreeMap<String, Binding>,
    types: BTreeSet<String>,
    macros: BTreeSet<String>,
    wildcard: bool,
}

fn framework(name: &str) -> Option<&'static str> {
    match name {
        "tokio" => Some("tokio"),
        "async_std" => Some("async_std"),
        "rstest" => Some("rstest"),
        "test_case" => Some("test_case"),
        _ => None,
    }
}

fn bind(bindings: &mut BTreeMap<String, Binding>, name: String, value: Binding) {
    bindings
        .entry(name)
        .and_modify(|binding| *binding = Binding::Unknown)
        .or_insert(value);
}

fn attached_attributes(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" => result.push(sibling),
            _ if comment(sibling) => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    result
}

fn collect_use(node: Node<'_>, scope: &mut Scope, source: &[u8], uncertain: bool) {
    let Some(argument) = node.child_by_field_name("argument") else {
        return;
    };
    let mut pending = vec![(
        argument,
        AttributePath {
            absolute: false,
            parts: Vec::new(),
        },
    )];
    while let Some((node, prefix)) = pending.pop() {
        match node.kind() {
            "use_list" => {
                let mut cursor = node.walk();
                for child in node
                    .named_children(&mut cursor)
                    .filter(|node| !comment(*node))
                {
                    pending.push((child, prefix.clone()));
                }
            }
            "scoped_use_list" => {
                let mut prefix = prefix;
                if let Some(node) = node.child_by_field_name("path") {
                    let Some(part) = path(node, source) else {
                        scope.wildcard = true;
                        continue;
                    };
                    prefix.absolute |= part.absolute;
                    prefix.parts.extend(part.parts);
                } else {
                    prefix.absolute = true;
                }
                if let Some(list) = node.child_by_field_name("list") {
                    pending.push((list, prefix));
                }
            }
            "use_wildcard" => scope.wildcard = true,
            _ => {
                let target = if node.kind() == "use_as_clause" {
                    node.child_by_field_name("path")
                } else {
                    Some(node)
                };
                let Some(mut target) = target.and_then(|node| path(node, source)) else {
                    scope.wildcard = true;
                    continue;
                };
                let mut combined = prefix;
                combined.absolute |= target.absolute;
                if target.parts == ["self"] && !combined.parts.is_empty() {
                    target.parts.clear();
                }
                combined.parts.extend(target.parts);
                let alias = node
                    .child_by_field_name("alias")
                    .map(|node| text(node, source).trim_start_matches("r#").to_owned())
                    .or_else(|| combined.parts.last().cloned());
                if let Some(alias) = alias.filter(|alias| alias != "_") {
                    bind(
                        &mut scope.imports,
                        alias,
                        if uncertain {
                            Binding::Unknown
                        } else {
                            Binding::Import(combined)
                        },
                    );
                }
            }
        }
    }
}

fn make_scope(node: Node<'_>, parent: Option<usize>, source: &[u8]) -> Scope {
    let mut scope = Scope {
        parent,
        module: node.kind() == "source_file"
            || node
                .parent()
                .is_some_and(|parent| parent.kind() == "mod_item"),
        ..Scope::default()
    };
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "use_declaration" => {
                let uncertain = !attached_attributes(child).is_empty();
                collect_use(child, &mut scope, source, uncertain);
            }
            "extern_crate_declaration" => {
                if let Some(name) = child.child_by_field_name("name") {
                    let binding = if attached_attributes(child).is_empty() {
                        framework(text(name, source).trim_start_matches("r#"))
                            .map_or(Binding::Unknown, Binding::Crate)
                    } else {
                        Binding::Unknown
                    };
                    let alias = child.child_by_field_name("alias").unwrap_or(name);
                    let alias = text(alias, source).trim_start_matches("r#").to_owned();
                    bind(&mut scope.imports, alias.clone(), binding.clone());
                    bind(&mut scope.externs, alias, binding);
                }
            }
            "mod_item" | "struct_item" | "enum_item" | "union_item" | "type_item"
            | "trait_item" => {
                if let Some(name) = child.child_by_field_name("name") {
                    scope
                        .types
                        .insert(text(name, source).trim_start_matches("r#").to_owned());
                }
            }
            "macro_definition" => {
                if let Some(name) = child.child_by_field_name("name") {
                    scope
                        .macros
                        .insert(text(name, source).trim_start_matches("r#").to_owned());
                }
            }
            _ => {}
        }
    }
    if let Some(parameters) = node
        .parent()
        .and_then(|owner| owner.child_by_field_name("type_parameters"))
    {
        let mut cursor = parameters.walk();
        for parameter in parameters.named_children(&mut cursor) {
            if parameter.kind() == "type_parameter"
                && let Some(name) = parameter.child_by_field_name("name")
            {
                scope
                    .types
                    .insert(text(name, source).trim_start_matches("r#").to_owned());
            }
        }
    }
    scope
}

#[derive(Clone, Copy, PartialEq)]
enum Resolved {
    Crate(&'static str),
    Test,
    Unknown,
}

fn member(base: Resolved, parts: &[String]) -> Resolved {
    if parts.is_empty() {
        return base;
    }
    match (base, parts) {
        (Resolved::Crate("tokio" | "async_std"), [name]) if name == "test" => Resolved::Test,
        (Resolved::Crate("rstest"), [name]) if name == "rstest" => Resolved::Test,
        (Resolved::Crate("test_case"), [name]) if name == "test_case" => Resolved::Test,
        _ => Resolved::Unknown,
    }
}

fn external_crate(name: &str, scopes: &[Scope]) -> Resolved {
    match scopes[0].externs.get(name) {
        Some(Binding::Crate(name)) => Resolved::Crate(name),
        Some(_) => Resolved::Unknown,
        None => framework(name).map_or(Resolved::Unknown, Resolved::Crate),
    }
}

fn resolve_path(
    path: &AttributePath,
    scope: usize,
    scopes: &[Scope],
    source_scope: Option<(&str, usize)>,
    depth: usize,
) -> Resolved {
    if depth >= 64 {
        return Resolved::Unknown;
    }
    let Some(first) = path.parts.first() else {
        return Resolved::Unknown;
    };
    if matches!(first.as_str(), "crate" | "self" | "super") {
        return Resolved::Unknown;
    }
    if path.absolute {
        return member(external_crate(first, scopes), &path.parts[1..]);
    }
    let mut current = Some(scope);
    while let Some(index) = current {
        let local = &scopes[index];
        if local.types.contains(first) || local.macros.contains(first) {
            return Resolved::Unknown;
        }
        if let Some(binding) = local.imports.get(first) {
            let mut base = match binding {
                Binding::Crate(name) => Resolved::Crate(name),
                Binding::Unknown => Resolved::Unknown,
                Binding::Import(import) => {
                    // A same-named macro import (`use rstest::rstest`) does
                    // not replace the external crate in the type namespace.
                    if source_scope == Some((first.as_str(), index)) {
                        external_crate(first, scopes)
                    } else {
                        resolve_path(import, index, scopes, Some((first, index)), depth + 1)
                    }
                }
            };
            if base == Resolved::Test && path.parts.len() > 1 {
                base = external_crate(first, scopes);
            }
            return member(base, &path.parts[1..]);
        }
        if local.wildcard {
            return Resolved::Unknown;
        }
        current = if local.module { None } else { local.parent };
    }
    member(external_crate(first, scopes), &path.parts[1..])
}

fn builtin(name: &str, scope: usize, scopes: &[Scope]) -> bool {
    let mut current = Some(scope);
    let mut inherited_macros_only = false;
    while let Some(index) = current {
        let local = &scopes[index];
        if local.macros.contains(name)
            || (!inherited_macros_only && (local.imports.contains_key(name) || local.wildcard))
        {
            return false;
        }
        inherited_macros_only |= local.module;
        current = local.parent;
    }
    true
}

struct AttributeContext<'a> {
    scope: usize,
    scopes: &'a [Scope],
    source: &'a [u8],
}

fn attribute_survival(
    meta: &Meta<'_>,
    function: bool,
    context: &AttributeContext<'_>,
    test: bool,
    depth: usize,
) -> Truth {
    if depth >= 64 {
        return Truth::Unknown;
    }
    let name =
        (!meta.path.absolute && meta.path.parts.len() == 1).then(|| meta.path.parts[0].as_str());
    if let Some(name @ ("cfg" | "cfg_attr")) = name {
        let Some(values) = meta
            .arguments
            .and_then(|node| arguments(node, context.source))
        else {
            return Truth::Unknown;
        };
        return match (name, values.as_slice()) {
            ("cfg", [value]) => cfg_value(value, context.source, test, depth + 1),
            ("cfg_attr", [condition, attributes @ ..]) if !attributes.is_empty() => {
                let condition = cfg_value(condition, context.source, test, depth + 1);
                let applied = Truth::all(attributes.iter().map(|attribute| {
                    attribute_survival(attribute, function, context, test, depth + 1)
                }));
                Truth::any([condition.not(), applied])
            }
            _ => Truth::Unknown,
        };
    }
    if function
        && !test
        && ((name == Some("test") && meta.bare && builtin("test", context.scope, context.scopes))
            || ((meta.bare || meta.arguments.is_some())
                && resolve_path(&meta.path, context.scope, context.scopes, None, 0)
                    == Resolved::Test))
    {
        Truth::False
    } else {
        Truth::True
    }
}

fn test_only(
    attributes: &[Node<'_>],
    function: bool,
    scope: usize,
    scopes: &[Scope],
    source: &[u8],
) -> bool {
    let context = AttributeContext {
        scope,
        scopes,
        source,
    };
    let survival = |test| {
        Truth::all(attributes.iter().map(|attribute| {
            attribute_meta(*attribute, source).map_or(Truth::Unknown, |meta| {
                attribute_survival(&meta, function, &context, test, 0)
            })
        }))
    };
    survival(false) == Truth::False && survival(true) != Truth::False
}

fn region_with_attributes(node: Node<'_>, reason: &str) -> TestRegion {
    let mut region = tree::test_region(node, reason);
    if let Some(attribute) = attached_attributes(node).last() {
        region.start_byte = attribute.start_byte();
        region.start_line = attribute.start_position().row + 1;
    }
    region
}

fn embedded_attributes(node: Node<'_>) -> Vec<Node<'_>> {
    if !matches!(
        node.kind(),
        "match_arm" | "field_initializer" | "shorthand_field_initializer"
    ) {
        return Vec::new();
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .take_while(|child| {
            comment(*child) || matches!(child.kind(), "attribute_item" | "inner_attribute_item")
        })
        .filter(|child| !comment(*child))
        .collect()
}

fn inner_target(node: Node<'_>) -> Node<'_> {
    if let Some(parent) = node.parent()
        && parent.child_by_field_name("body") == Some(node)
        && matches!(
            parent.kind(),
            "mod_item"
                | "impl_item"
                | "trait_item"
                | "function_item"
                | "closure_expression"
                | "foreign_mod_item"
        )
    {
        parent
    } else {
        node
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

    fn test_regions(root: Node<'_>, source: &[u8]) -> Vec<TestRegion> {
        let mut regions = Vec::new();
        let mut scopes = Vec::new();
        let mut pending = vec![(root, None)];
        while let Some((node, mut scope)) = pending.pop() {
            if comment(node)
                || matches!(
                    node.kind(),
                    "attribute_item" | "inner_attribute_item" | "token_tree"
                )
            {
                continue;
            }
            if let Some(index) = scope {
                let mut attributes = attached_attributes(node);
                attributes.extend(embedded_attributes(node));
                if !attributes.is_empty()
                    && test_only(
                        &attributes,
                        node.kind() == "function_item",
                        index,
                        &scopes,
                        source,
                    )
                {
                    let mut region =
                        region_with_attributes(node, "Rust test attribute or test-only cfg gate");
                    if node.kind() == "visibility_modifier"
                        && node
                            .parent()
                            .is_some_and(|parent| parent.kind() == "ordered_field_declaration_list")
                    {
                        let mut next = node.next_named_sibling();
                        while next.is_some_and(comment) {
                            next = next.and_then(|node| node.next_named_sibling());
                        }
                        if let Some(field_type) = next {
                            let end = tree::test_region(field_type, "");
                            region.end_byte = end.end_byte;
                            region.end_line = end.end_line;
                        }
                    }
                    regions.push(region);
                    continue;
                }
            }
            if matches!(node.kind(), "macro_definition" | "macro_invocation") {
                continue;
            }
            if matches!(node.kind(), "source_file" | "declaration_list" | "block") {
                let index = scopes.len();
                scopes.push(make_scope(node, scope, source));
                scope = Some(index);
                let mut cursor = node.walk();
                let attributes: Vec<_> = node
                    .named_children(&mut cursor)
                    .filter(|child| child.kind() == "inner_attribute_item")
                    .collect();
                if !attributes.is_empty() && test_only(&attributes, false, index, &scopes, source) {
                    regions.push(region_with_attributes(
                        inner_target(node),
                        "Rust inner test-only cfg gate",
                    ));
                    continue;
                }
            }
            let mut cursor = node.walk();
            let start = pending.len();
            pending.extend(node.named_children(&mut cursor).map(|child| (child, scope)));
            pending[start..].reverse();
        }
        regions
    }
}

impl LanguageAdapter for RustAdapter {
    fn test_policy(&self) -> LanguageTestPolicy {
        LanguageTestPolicy {
            version: "rust-tests-v1",
            path_patterns: TEST_PATHS,
            syntax_rules: TEST_SYNTAX,
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
        let result = RustAdapter::new()
            .unwrap()
            .analyze(source.as_bytes())
            .unwrap();
        assert!(result.parsed(), "{:?}", result.diagnostics);
        result
    }

    fn retained_names(file: &FileAnalysis) -> Vec<&str> {
        file.without_tests
            .as_ref()
            .map_or(&file.functions, |filtered| &filtered.functions)
            .iter()
            .map(|function| function.name.as_str())
            .collect()
    }

    #[test]
    fn rust_owned_path_policy() {
        let adapter = RustAdapter::new().unwrap();
        assert_eq!(adapter.test_policy().path_patterns, TEST_PATHS);
        assert_eq!(adapter.test_policy().syntax_rules, TEST_SYNTAX);
        assert_eq!(adapter.test_policy().version, "rust-tests-v1");
        for path in [
            "tests.rs",
            "src/tests.rs",
            "tests/basic.rs",
            "crate/tests/helpers/mod.rs",
        ] {
            assert!(adapter.is_test_file(path), "{path}");
        }
        for path in [
            "src/test.rs",
            "src/test_helpers.rs",
            "src/foo_test.rs",
            "src/foo.test.rs",
            "src/foo.spec.rs",
            "src/__tests__/helper.rs",
            "src/Tests.rs",
            "Tests/basic.rs",
            "contest/tests_support.rs",
            "benches/speed.rs",
            "test/basic.rs",
        ] {
            assert!(!adapter.is_test_file(path), "{path}");
        }
    }

    #[test]
    fn builtins_and_qualified_framework_attributes() {
        for attribute in [
            "test",
            "r#test",
            "tokio::test",
            "async_std::test",
            "rstest::rstest",
            "test_case::test_case(1, 2)",
            "tokio::test(flavor = \"multi_thread\")",
            "::tokio::test",
            "tokio /* path comment */ :: test",
        ] {
            let file = parse(&format!(
                "#[{attribute}] async fn check() {{}} fn production() {{}}"
            ));
            assert_eq!(retained_names(&file), ["production"], "{attribute}");
            assert_eq!(file.functions.len(), 2, "{attribute}");
        }
    }

    #[test]
    fn aliases_grouped_imports_and_extern_crates() {
        for source in [
            "use tokio::test as check; #[check] fn removed() {}",
            "use tokio as runtime; #[runtime::test] fn removed() {}",
            "use tokio::{self as runtime, test as check}; #[runtime::test] fn first() {} #[check] fn second() {}",
            "use {rstest::{rstest as check}}; #[check] fn removed() {}",
            "use {async_std as runtime}; use runtime::test as check; #[check] fn removed() {}",
            "use test_case::test_case; #[test_case(1)] fn removed() {}",
            "use rstest::rstest; #[rstest] fn first() {} #[rstest::rstest] fn second() {}",
            "extern crate tokio as runtime; #[runtime::test] fn removed() {}",
            "extern crate async_std as runtime; #[::runtime::test] fn removed() {}",
            "use ::tokio::{self, test as check}; #[tokio::test] fn first() {} #[check] fn second() {}",
            "use tokio; #[tokio::test] fn removed() {}",
            "use tokio as first; use first as second; #[second::test] fn removed() {}",
            "#[check] fn removed() {} use tokio::test as check;",
            "use tokio::{test as r#check}; #[r#check] fn removed() {}",
        ] {
            let file = parse(source);
            assert!(retained_names(&file).is_empty(), "{source}");
            assert!(file.without_tests.is_some(), "{source}");
        }
    }

    #[test]
    fn unknown_names_shadowing_and_ambiguous_bindings_are_retained() {
        for source in [
            "#[custom::test] fn production() {}",
            "#[my::tokio::test] fn production() {}",
            "#[crate::tokio::test] fn production() {}",
            "#[rstest] fn production() {}",
            "#[test_case(1)] fn production() {}",
            "use custom::test; #[test] fn production() {}",
            "use custom::test as check; #[check] fn production() {}",
            "use custom as tokio; #[tokio::test] fn production() {}",
            "mod tokio {} #[tokio::test] fn production() {}",
            "struct tokio; #[tokio::test] fn production() {}",
            "type tokio = (); #[tokio::test] fn production() {}",
            "use custom::*; #[test] fn production() {}",
            "use custom::*; #[tokio::test] fn production() {}",
            "use tokio::test as check; use custom::check; #[check] fn production() {}",
            "#[cfg(feature = \"x\")] use tokio::test as check; #[check] fn production() {}",
            "use tokio::test as check; mod child { #[check] fn production() {} }",
            "use first as second; use second as first; #[first::test] fn production() {}",
            "macro_rules! test { () => {} } #[test] fn production() {}",
            "extern crate custom as tokio; #[::tokio::test] fn production() {}",
            "use tokio::test as check; mod child { use super::check; #[check] fn production() {} }",
            "#[cfg_attr(test, tokio::test)] fn production() {}",
            "#[tokio::test = \"custom\"] fn production() {}",
        ] {
            let file = parse(source);
            assert!(file.without_tests.is_none(), "{source}");
            assert_eq!(retained_names(&file), ["production"], "{source}");
        }
    }

    #[test]
    fn namespace_and_block_scope_boundaries() {
        let file = parse(
            "use ::tokio::test as check;
             fn outer() {
                 #[check] fn removed() {}
                 { use custom::check; #[check] fn kept() {} }
                 let test = 1;
                 #[test] fn also_removed() {}
             }
             mod child { #[check] fn kept_in_module() {} #[test] fn removed_in_module() {} }
             mod tokio {}
             #[::tokio::test] fn absolute_removed() {}
             fn test() {}
             mod test {}
             #[test] fn builtin_removed() {}",
        );
        assert_eq!(
            retained_names(&file),
            ["outer", "kept", "kept_in_module", "test"]
        );
    }

    #[test]
    fn cfg_requires_test_mode() {
        for predicate in [
            "test",
            "all(test, feature = \"x\")",
            "all(feature = r#\"x\"#, test,)",
            "any(test, all(test, feature = \"x\"))",
            "not(not(test))",
            "all(any(test, feature = \"x\"), not(any(not(test), unknown)))",
            "all(/* cfg comment */ test, not(not(test)))",
        ] {
            let file = parse(&format!(
                "#[cfg({predicate})] fn removed() {{}} fn production() {{}}"
            ));
            assert_eq!(retained_names(&file), ["production"], "{predicate}");
        }
        for predicate in [
            "any(test, feature = \"x\")",
            "not(test)",
            "feature = \"x\"",
            "not(all(test, feature = \"x\"))",
            "any(not(test), feature = \"x\")",
            "all()",
            "any()",
            "all(test, not(test))",
            "test = \"yes\"",
            "custom(test)",
            "any(test, not(feature = \"x\"))",
            "not(test, unknown)",
            "all(test,,)",
            "not()",
        ] {
            let file = parse(&format!("#[cfg({predicate})] fn production() {{}}"));
            assert!(file.without_tests.is_none(), "{predicate}");
        }
    }

    #[test]
    fn cfg_attr_and_multiple_attributes_are_logical_not_textual() {
        for attributes in [
            "#[cfg_attr(not(test), cfg(test))]",
            "#[cfg_attr(all(), test)]",
            "#[cfg_attr(not(test), allow(dead_code), cfg(any()))]",
            "#[cfg_attr(all(), cfg_attr(not(test), cfg(test)))]",
            "#[cfg(any(test, feature = \"x\"))] #[cfg(test)]",
            "#[cfg_attr(all(), tokio::test)]",
            "#[cfg_attr(all(), cfg(all(test, feature = \"x\")))]",
        ] {
            let file = parse(&format!("{attributes} fn removed() {{}}"));
            assert!(retained_names(&file).is_empty(), "{attributes}");
            assert_eq!(file.without_tests.unwrap().source_lines, 0, "{attributes}");
        }
        for attributes in [
            "#[cfg_attr(test, test)]",
            "#[cfg_attr(test, cfg(test))]",
            "#[cfg_attr(feature = \"x\", test)]",
            "#[cfg_attr(any(test, feature = \"x\"), test)]",
            "#[cfg_attr(not(test), allow(dead_code))]",
            "#[cfg_attr(not(test), cfg_attr(test, test))]",
            "#[custom::cfg(test)]",
            "#[doc = \"#[test]\"] #[allow(dead_code)]",
        ] {
            let file = parse(&format!("{attributes} fn production() {{}}"));
            assert!(file.without_tests.is_none(), "{attributes}");
        }
    }

    #[test]
    fn nested_tests_remove_ancestor_decisions_and_lines() {
        let file = parse(
            "fn outer() {\n\
                 if ready() { work(); }\n\
                 #[test]\n\
                 fn nested() { if a() && b() { work(); } }\n\
                 #[cfg(test)]\n\
                 mod tests { fn helper() { while ready() { work(); } } }\n\
                 work();\n\
             }\n",
        );
        assert_eq!(file.functions.len(), 3);
        assert_eq!(file.functions[0].complexity, 5);
        assert_eq!(file.functions[0].source_lines, 7);
        assert_eq!(file.source_lines, 7);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.functions.len(), 1);
        assert_eq!(filtered.functions[0].name, "outer");
        assert_eq!(filtered.functions[0].complexity, 2);
        assert_eq!(filtered.functions[0].source_lines, 3);
        assert_eq!(filtered.source_lines, 3);
        assert_eq!(filtered.regions.len(), 2);
    }

    #[test]
    fn nested_statements_fields_and_match_arms_are_complete_regions() {
        let file = parse(
            "fn outer(value: i32) {
                 #[cfg(test)] let helper = || { if ready() { work(); } };
                 match value {
                     #[cfg(test)] 1 => if ready() { work(); },
                     _ => work(),
                 }
                 let value = Value {
                     #[cfg(test)] test: || { while ready() { work(); } },
                     production: 1,
                 };
             }",
        );
        assert_eq!(file.functions[0].complexity, 5);
        assert_eq!(file.functions.len(), 3);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.functions.len(), 1);
        assert_eq!(filtered.functions[0].complexity, 1);
        assert_eq!(filtered.regions.len(), 3);
        assert_eq!(filtered.source_lines, 5);

        let file = parse(
            "struct Tuple(
                 #[cfg(test)] pub fn(),
                 i32,
             );
             struct Named {
                 #[cfg(test)] test: fn(),
                 production: i32,
             }",
        );
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.source_lines, 4);
        assert_eq!(filtered.regions.len(), 2);
    }

    #[test]
    fn uncertain_deep_cfg_and_aliases_are_bounded() {
        let nested = format!("{}test{}", "not(".repeat(80), ")".repeat(80));
        assert!(
            parse(&format!("#[cfg({nested})] fn production() {{}}"))
                .without_tests
                .is_none()
        );
        let mut source = "use tokio as alias0;\n".to_owned();
        for index in 1..80 {
            source.push_str(&format!("use alias{} as alias{index};\n", index - 1));
        }
        source.push_str("#[alias79::test] fn production() {}");
        assert!(parse(&source).without_tests.is_none());
    }

    #[test]
    fn cfg_items_and_inner_gates_cover_their_owner() {
        for source in [
            "#![cfg(test)] use custom::*; fn helper() {}",
            "#![cfg_attr(not(test), cfg(test))] fn helper() {}",
            "#[cfg(test)] mod tests { #[test] fn check() {} fn helper() {} }",
            "mod tests { #![cfg(test)] use super::*; fn helper() {} }",
            "#[cfg(test)] impl Value { fn helper() {} }",
            "impl Value { #![cfg(test)] fn helper() {} }",
            "trait Value { #![cfg(test)] fn helper() {} }",
            "fn helper() { #![cfg(test)] if ready() { work(); } }",
            "#[cfg(test)] const HELPER: fn() = || { if ready() { work(); } };",
            "#[cfg(test)] use custom::helper;",
            "#[cfg(test)] macro_rules! helper { () => { fn test() {} } }",
        ] {
            let file = parse(source);
            let filtered = file.without_tests.as_ref().expect(source);
            assert!(filtered.functions.is_empty(), "{source}");
            assert_eq!(filtered.source_lines, 0, "{source}");
            assert_eq!(filtered.regions.len(), 1, "{source}");
            assert_eq!(filtered.regions[0].start_byte, 0, "{source}");
            assert_eq!(filtered.regions[0].end_byte, source.len(), "{source}");
        }
        let file = parse("fn outer() { { #![cfg(test)] if ready() { work(); } } work(); }");
        assert_eq!(file.functions[0].complexity, 2);
        assert_eq!(file.without_tests.unwrap().functions[0].complexity, 1);
    }

    #[test]
    fn region_boundaries_preserve_same_line_production() {
        let source = "fn first() {} #[allow(dead_code)] /* attached */ #[test] // attached too\n\
                      fn check() { if ready() { work(); } } fn last() {}";
        let file = parse(source);
        assert_eq!(retained_names(&file), ["first", "last"]);
        assert_eq!(file.functions.len(), 3);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.source_lines, file.source_lines);
        assert_eq!(filtered.source_lines, 2);
        let region = &filtered.regions[0];
        assert_eq!(region.start_byte, source.find("#[allow").unwrap());
        assert_eq!(region.end_byte, source.find(" fn last").unwrap());
        assert_eq!((region.start_line, region.end_line), (1, 2));
        assert!(region.reason.contains("Rust"));
    }

    #[test]
    fn bom_crlf_and_original_byte_offsets() {
        let source =
            "\u{feff}#[test]\r\nfn check() { let label = \"\u{e9}\"; }\r\nfn production() {}\r\n";
        let file = parse(source);
        assert_eq!(file.physical_lines, 3);
        assert_eq!(file.source_lines, 3);
        assert_eq!(retained_names(&file), ["production"]);
        let filtered = file.without_tests.unwrap();
        assert_eq!(filtered.source_lines, 1);
        assert_eq!(filtered.regions[0].start_byte, 3);
        assert_eq!(
            filtered.regions[0].end_byte,
            source.find("\r\nfn production").unwrap()
        );
        assert_eq!(
            (filtered.regions[0].start_line, filtered.regions[0].end_line),
            (1, 2)
        );
    }

    #[test]
    fn no_test_regions_leave_the_raw_view_unchanged() {
        let source = "#[allow(dead_code)]\nfn test() { if ready() { work(); } }\n\
                      fn production() { let test = || 1; assert!(test() == 1); }\n\
                      macro_rules! generate { () => { #[test] fn generated() {} } }\n\
                      generate!();";
        let mut adapter = RustAdapter::new().unwrap();
        let before = adapter.analyze(source.as_bytes()).unwrap();
        assert!(before.parsed());
        assert!(before.without_tests.is_none());
        assert_eq!(before.functions[0].complexity, 2);
        assert!(adapter.is_test_file("tests/example.rs"));
        assert_eq!(adapter.analyze(source.as_bytes()).unwrap(), before);
    }

    #[test]
    fn malformed_test_code_still_fails_raw_validation() {
        for source in [
            "#[test] fn broken( {",
            "#[cfg(test)] mod tests { fn broken() { if } }",
            "#![cfg(test)] fn broken() { let x = ; }",
            "#[tokio::test] async fn broken() {",
            "#[cfg_attr(all(), test)] fn broken() { let = 1; }",
        ] {
            let file = RustAdapter::new()
                .unwrap()
                .analyze(source.as_bytes())
                .unwrap();
            assert!(!file.parsed(), "{source}");
            assert!(!file.diagnostics.is_empty(), "{source}");
            assert!(file.functions.is_empty(), "{source}");
            assert!(file.without_tests.is_none(), "{source}");
        }
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
