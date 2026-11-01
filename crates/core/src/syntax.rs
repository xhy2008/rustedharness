//! Tree-sitter backed extraction for the L0 index.
//!
//! Every supported language is parsed with its official tree-sitter grammar.
//! Definitions are selected by AST node kind and `name`-style fields; references
//! are identifier leaves. There are no line-oriented or regex-based parsers
//! here.

use tree_sitter::{Language, Node, Parser};

const MAX_REFS_PER_FILE: usize = 20_000;

#[derive(Debug, Clone)]
pub struct Definition {
    pub name: String,
    pub kind: &'static str,
    /// 0-based line.
    pub line: usize,
    /// 0-based Unicode scalar column.
    pub column: usize,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Clone)]
pub struct Reference {
    pub name: String,
    /// 0-based line.
    pub line: usize,
    /// 0-based Unicode scalar column.
    pub column: usize,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Default)]
pub struct FileSyntax {
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
}

pub fn is_supported(language: &str) -> bool {
    spec(language).is_some()
}

/// Parse a file and return its definitions and identifier references. Returns
/// `None` for languages without a bundled grammar or when tree-sitter rejects
/// the grammar/language pair.
pub fn parse_file(language: &str, source: &str) -> Option<FileSyntax> {
    let spec = spec(language)?;
    let language = make_language(language)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let bytes = source.as_bytes();
    let starts = line_starts(source);
    let mut collector = Collector {
        source: bytes,
        starts: &starts,
        spec,
        out: FileSyntax::default(),
        definition_spans: Vec::new(),
    };
    collector.walk(tree.root_node());
    let spans = collector.definition_spans;
    collector.out.references.retain(|r| {
        !spans
            .iter()
            .any(|(start, end)| r.byte_start == *start && r.byte_end == *end)
    });
    collector.out.references.sort_by_key(|r| (r.line, r.column));
    collector
        .out
        .references
        .dedup_by(|a, b| a.name == b.name && a.line == b.line && a.column == b.column);
    collector
        .out
        .definitions
        .sort_by_key(|d| (d.line, d.column));
    collector
        .out
        .definitions
        .dedup_by(|a, b| a.name == b.name && a.line == b.line && a.column == b.column);
    Some(collector.out)
}

/// Resolve the identifier under a 1-based line/column position using the AST
/// rather than scanning the line manually.
pub fn identifier_at(language: &str, source: &str, line: u32, column: u32) -> Option<String> {
    let language = make_language(language)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let starts = line_starts(source);
    let row = line.saturating_sub(1) as usize;
    let start = *starts.get(row)?;
    let end = starts.get(row + 1).copied().unwrap_or(source.len());
    let line_text = source.get(start..end)?;
    let char_col = column.saturating_sub(1) as usize;
    let byte_in_line = char_to_byte(line_text, char_col)?;
    let offset = start + byte_in_line;

    let root = tree.root_node();
    for candidate in [offset, offset.saturating_sub(1)] {
        if candidate > source.len() {
            continue;
        }
        let range_end = (candidate + 1).min(source.len());
        if candidate == range_end {
            continue;
        }
        let Some(node) = root.descendant_for_byte_range(candidate, range_end) else {
            continue;
        };
        if is_reference_kind(node.kind()) {
            if let Some(name) = identifier_text(node, source.as_bytes()) {
                return Some(name.to_string());
            }
        }
        // Cursors can land on punctuation inside a qualified path; climb to the
        // nearest named ancestor that is itself an identifier token.
        let mut current = node;
        while let Some(parent) = current.parent() {
            if is_reference_kind(parent.kind()) {
                if let Some(name) = identifier_text(parent, source.as_bytes()) {
                    return Some(name.to_string());
                }
            }
            current = parent;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// AST walking
// ---------------------------------------------------------------------------

struct Collector<'a> {
    source: &'a [u8],
    starts: &'a [usize],
    spec: &'static Spec,
    out: FileSyntax,
    definition_spans: Vec<(usize, usize)>,
}

impl<'a> Collector<'a> {
    fn walk(&mut self, node: Node<'a>) {
        if let Some(rule) = self.spec.rule(node.kind()) {
            for (name, label) in definition_nodes(node, *rule, self.source) {
                if let Some(text) = identifier_text(name, self.source) {
                    let (line, column) = position(self.starts, self.source, name.start_byte());
                    self.out.definitions.push(Definition {
                        name: text.to_string(),
                        kind: label,
                        line,
                        column,
                        byte_start: name.start_byte(),
                        byte_end: name.end_byte(),
                    });
                    self.definition_spans
                        .push((name.start_byte(), name.end_byte()));
                }
            }
        }

        if node.child_count() == 0 {
            if self.out.references.len() < MAX_REFS_PER_FILE {
                if let Some(text) = identifier_text(node, self.source) {
                    if is_reference_kind(node.kind()) && !is_noise(text) {
                        let (line, column) = position(self.starts, self.source, node.start_byte());
                        self.out.references.push(Reference {
                            name: text.to_string(),
                            line,
                            column,
                            byte_start: node.start_byte(),
                            byte_end: node.end_byte(),
                        });
                    }
                }
            }
            return;
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child);
        }
    }
}

#[derive(Clone, Copy)]
struct Rule {
    node: &'static str,
    label: &'static str,
    name: NameRule,
}

#[derive(Clone, Copy)]
enum NameRule {
    /// A named field (`name`, `declarator`, `pattern`, ...). Multiple values
    /// are accepted (Go const specs, C declarator lists).
    Field(&'static str),
    /// First identifier leaf anywhere inside the node.
    FirstIdentifier,
    /// C/C++ declarators: unwrap pointer/function/parenthesized layers.
    Declarator,
    /// Elixir `def`/`defmodule` style calls.
    ElixirCall,
}

impl Spec {
    fn rule(&self, kind: &str) -> Option<&'static Rule> {
        self.rules.iter().find(|rule| rule.node == kind)
    }
}

fn definition_nodes<'a>(
    node: Node<'a>,
    rule: Rule,
    source: &'a [u8],
) -> Vec<(Node<'a>, &'static str)> {
    let mut out = Vec::new();
    let mut push = |name: Node<'a>| out.push((name, rule.label));
    match rule.name {
        NameRule::Field(field) => {
            let mut cursor = node.walk();
            for child in node.children_by_field_name(field, &mut cursor) {
                if let Some(name) = resolve_name(child, source) {
                    push(name);
                }
            }
        }
        NameRule::FirstIdentifier => {
            if let Some(name) = first_identifier(node, source) {
                push(name);
            }
        }
        NameRule::Declarator => {
            let mut cursor = node.walk();
            let declarators: Vec<Node<'a>> = node
                .children_by_field_name("declarator", &mut cursor)
                .collect();
            if declarators.is_empty() {
                if let Some(name) = declarator_name(node, source) {
                    push(name);
                }
            } else {
                for declarator in declarators {
                    if let Some(name) = declarator_name(declarator, source) {
                        push(name);
                    }
                }
            }
        }
        NameRule::ElixirCall => {
            if let Some((name, kind)) = elixir_call(node, source) {
                out.push((name, kind));
            }
        }
    }
    out
}

/// Resolve a named-field value to the identifier token it denotes. Non-leaf
/// values (qualified names, patterns, user types) fall back to their last
/// identifier leaf.
fn resolve_name<'a>(node: Node<'a>, source: &'a [u8]) -> Option<Node<'a>> {
    if node.child_count() == 0 {
        return identifier_token(node, source);
    }
    last_identifier(node, source)
}

fn declarator_name<'a>(node: Node<'a>, source: &'a [u8]) -> Option<Node<'a>> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier" => {
            if node.child_count() == 0 {
                return identifier_token(node, source);
            }
            // A qualified declarator resolves to its final component.
            node.child_by_field_name("name")
                .and_then(|n| declarator_name(n, source))
                .or_else(|| last_identifier(node, source))
        }
        "function_declarator"
        | "pointer_declarator"
        | "parenthesized_declarator"
        | "init_declarator"
        | "attributed_declarator"
        | "array_declarator"
        | "reference_declarator"
        | "structured_binding_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|n| declarator_name(n, source))
            .or_else(|| first_identifier(node, source)),
        _ => first_identifier(node, source),
    }
}

fn first_identifier<'a>(node: Node<'a>, source: &'a [u8]) -> Option<Node<'a>> {
    if node.child_count() == 0 {
        return identifier_token(node, source);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = first_identifier(child, source) {
            return Some(found);
        }
    }
    None
}

fn last_identifier<'a>(node: Node<'a>, source: &'a [u8]) -> Option<Node<'a>> {
    if node.child_count() == 0 {
        return identifier_token(node, source);
    }
    let mut cursor = node.walk();
    let children: Vec<Node<'a>> = node.named_children(&mut cursor).collect();
    for child in children.into_iter().rev() {
        if let Some(found) = last_identifier(child, source) {
            return Some(found);
        }
    }
    None
}

fn identifier_token<'a>(node: Node<'a>, source: &'a [u8]) -> Option<Node<'a>> {
    identifier_text(node, source).map(|_| node)
}

fn identifier_text<'a>(node: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    if node.child_count() > 0 {
        return None;
    }
    let text = std::str::from_utf8(&source[node.start_byte()..node.end_byte()]).ok()?;
    let text = text.strip_prefix('$').unwrap_or(text);
    valid_identifier(text).then_some(text)
}

fn valid_identifier(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$' | '?' | '!'))
}

fn is_reference_kind(kind: &str) -> bool {
    matches!(
        kind,
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "property_identifier"
            | "private_property_identifier"
            | "shorthand_property_identifier"
            | "shorthand_property_identifier_pattern"
            | "simple_identifier"
            | "constant"
            | "alias"
            | "name"
            | "variable_name"
            | "namespace_name"
            | "package_identifier"
            | "operator_identifier"
            | "constant_identifier"
    ) || kind.ends_with("_identifier")
}

fn is_noise(name: &str) -> bool {
    matches!(
        name,
        "self"
            | "Self"
            | "this"
            | "super"
            | "cls"
            | "crate"
            | "null"
            | "None"
            | "nil"
            | "true"
            | "false"
            | "undefined"
    )
}

fn elixir_call<'a>(node: Node<'a>, source: &'a [u8]) -> Option<(Node<'a>, &'static str)> {
    if node.kind() != "call" {
        return None;
    }
    let target = node.child_by_field_name("target")?;
    let keyword = identifier_text(target, source)?;
    let label = match keyword {
        "def" | "defp" | "defguard" | "defguardp" => "function",
        "defmodule" => "module",
        "defprotocol" => "protocol",
        "defimpl" => "implementation",
        "defmacro" | "defmacrop" => "macro",
        "defstruct" => "struct",
        _ => return None,
    };
    let mut cursor = node.walk();
    let args = node
        .children(&mut cursor)
        .find(|child| child.kind() == "arguments")?;
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        let candidate = if child.kind() == "call" {
            child.child_by_field_name("target").unwrap_or(child)
        } else {
            child
        };
        if let Some(name) = identifier_token(candidate, source) {
            return Some((name, label));
        }
        if let Some(name) = first_identifier(child, source) {
            return Some((name, label));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Position helpers
// ---------------------------------------------------------------------------

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn position(starts: &[usize], source: &[u8], offset: usize) -> (usize, usize) {
    let row = match starts.binary_search(&offset) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let line_start = starts.get(row).copied().unwrap_or(0);
    let slice = source.get(line_start..offset).unwrap_or_default();
    let column = String::from_utf8_lossy(slice).chars().count();
    (row, column)
}

fn char_to_byte(line: &str, char_index: usize) -> Option<usize> {
    if char_index == 0 {
        return Some(0);
    }
    line.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .or_else(|| (line.chars().count() == char_index).then_some(line.len()))
}

// ---------------------------------------------------------------------------
// Language registry
// ---------------------------------------------------------------------------

struct Spec {
    rules: &'static [Rule],
}

fn make_language(id: &str) -> Option<Language> {
    Some(match id {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "kotlin" => tree_sitter_kotlin_ng::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        "scala" => tree_sitter_scala::LANGUAGE.into(),
        "lua" => tree_sitter_lua::LANGUAGE.into(),
        "elixir" => tree_sitter_elixir::LANGUAGE.into(),
        _ => return None,
    })
}

fn spec(id: &str) -> Option<&'static Spec> {
    let spec = match id {
        "rust" => &RUST,
        "c" => &C,
        "cpp" => &CPP,
        "go" => &GO,
        "typescript" | "tsx" => &TYPESCRIPT,
        "javascript" => &JAVASCRIPT,
        "python" => &PYTHON,
        "java" => &JAVA,
        "kotlin" => &KOTLIN,
        "csharp" => &CSHARP,
        "ruby" => &RUBY,
        "php" => &PHP,
        "swift" => &SWIFT,
        "scala" => &SCALA,
        "lua" => &LUA,
        "elixir" => &ELIXIR,
        _ => return None,
    };
    Some(spec)
}

const fn rule(node: &'static str, label: &'static str, name: NameRule) -> Rule {
    Rule { node, label, name }
}

const RUST_RULES: &[Rule] = &[
    rule("function_item", "fn", NameRule::Field("name")),
    rule("function_signature_item", "fn", NameRule::Field("name")),
    rule("struct_item", "struct", NameRule::Field("name")),
    rule("enum_item", "enum", NameRule::Field("name")),
    rule("enum_variant", "variant", NameRule::Field("name")),
    rule("trait_item", "trait", NameRule::Field("name")),
    rule("type_item", "type", NameRule::Field("name")),
    rule("const_item", "const", NameRule::Field("name")),
    rule("static_item", "static", NameRule::Field("name")),
    rule("mod_item", "module", NameRule::Field("name")),
    rule("union_item", "union", NameRule::Field("name")),
    rule("macro_definition", "macro", NameRule::Field("name")),
    rule("field_declaration", "field", NameRule::Field("name")),
];
const RUST: Spec = Spec { rules: RUST_RULES };

const PYTHON_RULES: &[Rule] = &[
    rule("function_definition", "fn", NameRule::Field("name")),
    rule("class_definition", "class", NameRule::Field("name")),
    rule("type_alias_statement", "type", NameRule::Field("left")),
];
const PYTHON: Spec = Spec {
    rules: PYTHON_RULES,
};

const JAVASCRIPT_RULES: &[Rule] = &[
    rule("function_declaration", "fn", NameRule::Field("name")),
    rule(
        "generator_function_declaration",
        "fn",
        NameRule::Field("name"),
    ),
    rule("class_declaration", "class", NameRule::Field("name")),
    rule("method_definition", "method", NameRule::Field("name")),
    rule("field_definition", "field", NameRule::Field("property")),
    rule("variable_declarator", "variable", NameRule::Field("name")),
];
const JAVASCRIPT: Spec = Spec {
    rules: JAVASCRIPT_RULES,
};

const TYPESCRIPT_RULES: &[Rule] = &[
    rule("function_declaration", "fn", NameRule::Field("name")),
    rule(
        "generator_function_declaration",
        "fn",
        NameRule::Field("name"),
    ),
    rule("function_signature", "fn", NameRule::Field("name")),
    rule("class_declaration", "class", NameRule::Field("name")),
    rule(
        "abstract_class_declaration",
        "class",
        NameRule::Field("name"),
    ),
    rule(
        "interface_declaration",
        "interface",
        NameRule::Field("name"),
    ),
    rule("type_alias_declaration", "type", NameRule::Field("name")),
    rule("enum_declaration", "enum", NameRule::Field("name")),
    rule("method_definition", "method", NameRule::Field("name")),
    rule(
        "abstract_method_signature",
        "method",
        NameRule::Field("name"),
    ),
    rule("method_signature", "method", NameRule::Field("name")),
    rule("property_signature", "property", NameRule::Field("name")),
    rule("field_definition", "field", NameRule::Field("property")),
    rule("variable_declarator", "variable", NameRule::Field("name")),
    rule("module", "module", NameRule::Field("name")),
    rule("internal_module", "module", NameRule::Field("name")),
];
const TYPESCRIPT: Spec = Spec {
    rules: TYPESCRIPT_RULES,
};

const GO_RULES: &[Rule] = &[
    rule("function_declaration", "fn", NameRule::Field("name")),
    rule("method_declaration", "method", NameRule::Field("name")),
    rule("type_spec", "type", NameRule::Field("name")),
    rule("type_alias", "type", NameRule::Field("name")),
    rule("var_spec", "variable", NameRule::Field("name")),
    rule("const_spec", "const", NameRule::Field("name")),
    rule("field_declaration", "field", NameRule::Field("name")),
];
const GO: Spec = Spec { rules: GO_RULES };

const JAVA_RULES: &[Rule] = &[
    rule("class_declaration", "class", NameRule::Field("name")),
    rule(
        "interface_declaration",
        "interface",
        NameRule::Field("name"),
    ),
    rule("enum_declaration", "enum", NameRule::Field("name")),
    rule("record_declaration", "record", NameRule::Field("name")),
    rule(
        "annotation_type_declaration",
        "annotation",
        NameRule::Field("name"),
    ),
    rule("method_declaration", "method", NameRule::Field("name")),
    rule(
        "constructor_declaration",
        "constructor",
        NameRule::Field("name"),
    ),
    rule(
        "compact_constructor_declaration",
        "constructor",
        NameRule::Field("name"),
    ),
    rule("field_declaration", "field", NameRule::Field("declarator")),
    rule("variable_declarator", "variable", NameRule::Field("name")),
    rule("enum_constant", "const", NameRule::Field("name")),
];
const JAVA: Spec = Spec { rules: JAVA_RULES };

const C_RULES: &[Rule] = &[
    rule("function_definition", "fn", NameRule::Declarator),
    rule("struct_specifier", "struct", NameRule::Field("name")),
    rule("union_specifier", "union", NameRule::Field("name")),
    rule("enum_specifier", "enum", NameRule::Field("name")),
    rule("enumerator", "const", NameRule::Field("name")),
    rule("type_definition", "type", NameRule::Declarator),
    rule("declaration", "declaration", NameRule::Declarator),
    rule("preproc_def", "macro", NameRule::Field("name")),
    rule("preproc_function_def", "macro", NameRule::Field("name")),
];
const C: Spec = Spec { rules: C_RULES };

const CPP_RULES: &[Rule] = &[
    rule("function_definition", "fn", NameRule::Declarator),
    rule("class_specifier", "class", NameRule::Field("name")),
    rule("struct_specifier", "struct", NameRule::Field("name")),
    rule("union_specifier", "union", NameRule::Field("name")),
    rule("enum_specifier", "enum", NameRule::Field("name")),
    rule("enumerator", "const", NameRule::Field("name")),
    rule("namespace_definition", "namespace", NameRule::Field("name")),
    rule("type_definition", "type", NameRule::Declarator),
    rule("alias_declaration", "type", NameRule::Field("name")),
    rule("concept_definition", "concept", NameRule::Field("name")),
    rule("declaration", "declaration", NameRule::Declarator),
    rule("preproc_def", "macro", NameRule::Field("name")),
    rule("preproc_function_def", "macro", NameRule::Field("name")),
];
const CPP: Spec = Spec { rules: CPP_RULES };

const CSHARP_RULES: &[Rule] = &[
    rule("class_declaration", "class", NameRule::Field("name")),
    rule(
        "interface_declaration",
        "interface",
        NameRule::Field("name"),
    ),
    rule("struct_declaration", "struct", NameRule::Field("name")),
    rule("enum_declaration", "enum", NameRule::Field("name")),
    rule("record_declaration", "record", NameRule::Field("name")),
    rule("method_declaration", "method", NameRule::Field("name")),
    rule("local_function_statement", "fn", NameRule::Field("name")),
    rule(
        "constructor_declaration",
        "constructor",
        NameRule::Field("name"),
    ),
    rule(
        "destructor_declaration",
        "destructor",
        NameRule::Field("name"),
    ),
    rule("property_declaration", "property", NameRule::Field("name")),
    rule("delegate_declaration", "delegate", NameRule::Field("name")),
    rule(
        "namespace_declaration",
        "namespace",
        NameRule::Field("name"),
    ),
    rule("event_declaration", "event", NameRule::Field("name")),
    rule("enum_member_declaration", "const", NameRule::Field("name")),
    rule("variable_declarator", "variable", NameRule::Field("name")),
];
const CSHARP: Spec = Spec {
    rules: CSHARP_RULES,
};

const RUBY_RULES: &[Rule] = &[
    rule("method", "method", NameRule::Field("name")),
    rule("singleton_method", "method", NameRule::Field("name")),
    rule("class", "class", NameRule::Field("name")),
    rule("module", "module", NameRule::Field("name")),
];
const RUBY: Spec = Spec { rules: RUBY_RULES };

const PHP_RULES: &[Rule] = &[
    rule("function_definition", "fn", NameRule::Field("name")),
    rule("class_declaration", "class", NameRule::Field("name")),
    rule(
        "interface_declaration",
        "interface",
        NameRule::Field("name"),
    ),
    rule("trait_declaration", "trait", NameRule::Field("name")),
    rule("enum_declaration", "enum", NameRule::Field("name")),
    rule("method_declaration", "method", NameRule::Field("name")),
    rule("property_element", "property", NameRule::Field("name")),
    rule("namespace_definition", "namespace", NameRule::Field("name")),
    rule("enum_case", "const", NameRule::Field("name")),
];
const PHP: Spec = Spec { rules: PHP_RULES };

const SWIFT_RULES: &[Rule] = &[
    rule("function_declaration", "fn", NameRule::Field("name")),
    rule("init_declaration", "initializer", NameRule::Field("name")),
    rule("class_declaration", "type", NameRule::Field("name")),
    rule("protocol_declaration", "protocol", NameRule::Field("name")),
    rule("typealias_declaration", "type", NameRule::Field("name")),
    rule("enum_entry", "const", NameRule::Field("name")),
    rule("property_declaration", "property", NameRule::Field("name")),
    rule(
        "protocol_function_declaration",
        "fn",
        NameRule::Field("name"),
    ),
];
const SWIFT: Spec = Spec { rules: SWIFT_RULES };

const SCALA_RULES: &[Rule] = &[
    rule("function_definition", "fn", NameRule::Field("name")),
    rule("function_declaration", "fn", NameRule::Field("name")),
    rule("class_definition", "class", NameRule::Field("name")),
    rule("object_definition", "object", NameRule::Field("name")),
    rule("trait_definition", "trait", NameRule::Field("name")),
    rule("type_definition", "type", NameRule::Field("name")),
    rule("enum_definition", "enum", NameRule::Field("name")),
    rule("val_definition", "val", NameRule::Field("pattern")),
    rule("var_definition", "var", NameRule::Field("pattern")),
    rule("given_definition", "given", NameRule::Field("name")),
];
const SCALA: Spec = Spec { rules: SCALA_RULES };

const LUA_RULES: &[Rule] = &[
    rule("function_declaration", "fn", NameRule::Field("name")),
    rule(
        "variable_declaration",
        "variable",
        NameRule::FirstIdentifier,
    ),
    rule("field", "field", NameRule::Field("name")),
];
const LUA: Spec = Spec { rules: LUA_RULES };

const KOTLIN_RULES: &[Rule] = &[
    rule("function_declaration", "fn", NameRule::Field("name")),
    rule("class_declaration", "class", NameRule::Field("name")),
    rule("object_declaration", "object", NameRule::Field("name")),
    rule("companion_object", "object", NameRule::Field("name")),
    rule("type_alias", "type", NameRule::FirstIdentifier),
    rule(
        "variable_declaration",
        "property",
        NameRule::FirstIdentifier,
    ),
    rule("enum_entry", "const", NameRule::FirstIdentifier),
];
const KOTLIN: Spec = Spec {
    rules: KOTLIN_RULES,
};

const ELIXIR_RULES: &[Rule] = &[rule("call", "symbol", NameRule::ElixirCall)];
const ELIXIR: Spec = Spec {
    rules: ELIXIR_RULES,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_ignores_definitions_inside_strings_and_comments() {
        let source = r#"
// fn ghost() {}
const TEXT: &str = "fn also_ghost() {}";
pub fn real(x: i32) -> i32 {
    let nested = x + 1;
    nested
}
struct Thing { field: i32 }
"#;
        let syntax = parse_file("rust", source).expect("rust grammar");
        let names: Vec<&str> = syntax.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"real"));
        assert!(names.contains(&"Thing"));
        assert!(names.contains(&"field"));
        assert!(!names.contains(&"ghost"));
        assert!(!names.contains(&"also_ghost"));
        assert!(syntax.references.iter().any(|r| r.name == "nested"));
        assert!(!syntax.references.iter().any(|r| r.name == "ghost"));
    }

    #[test]
    fn extracts_multiple_languages() {
        let python =
            parse_file("python", "def gamma():\n    pass\nclass Box:\n    pass\n").expect("python");
        assert!(python.definitions.iter().any(|d| d.name == "gamma"));
        assert!(python.definitions.iter().any(|d| d.name == "Box"));

        let go = parse_file("go", "package p\nfunc Alpha() {}\ntype Beta struct{}\n").expect("go");
        assert!(go.definitions.iter().any(|d| d.name == "Alpha"));
        assert!(go.definitions.iter().any(|d| d.name == "Beta"));

        let ts = parse_file(
            "typescript",
            "export function hello() {}\ninterface Shape { area(): number }\n",
        )
        .expect("typescript");
        assert!(ts.definitions.iter().any(|d| d.name == "hello"));
        assert!(ts.definitions.iter().any(|d| d.name == "Shape"));

        let c = parse_file("c", "int add(int a, int b) { return a + b; }\n").expect("c");
        assert!(c.definitions.iter().any(|d| d.name == "add"));
        assert!(!c.definitions.iter().any(|d| d.name == "a"));
    }

    #[test]
    fn every_bundled_grammar_extracts_a_definition() {
        let cases: &[(&str, &str, &str)] = &[
            ("rust", "pub fn alpha() {}\n", "alpha"),
            ("c", "int alpha(void) { return 0; }\n", "alpha"),
            ("cpp", "class Alpha {};\n", "Alpha"),
            ("go", "package p\nfunc Alpha() {}\n", "Alpha"),
            ("typescript", "export interface Alpha {}\n", "Alpha"),
            (
                "tsx",
                "export function Alpha() { return <div />; }\n",
                "Alpha",
            ),
            ("javascript", "class Alpha {}\n", "Alpha"),
            ("python", "def alpha():\n    pass\n", "alpha"),
            ("java", "class Alpha {}\n", "Alpha"),
            ("kotlin", "fun alpha() {}\n", "alpha"),
            ("csharp", "class Alpha {}\n", "Alpha"),
            ("ruby", "def alpha\nend\n", "alpha"),
            ("php", "<?php\nfunction alpha() {}\n", "alpha"),
            ("swift", "func alpha() {}\n", "alpha"),
            ("scala", "def alpha(): Unit = ()\n", "alpha"),
            ("lua", "function alpha() end\n", "alpha"),
            (
                "elixir",
                "defmodule Alpha do\n  def beta do\n    :ok\n  end\nend\n",
                "Alpha",
            ),
        ];
        for (language, source, expected) in cases {
            let parsed = parse_file(language, source)
                .unwrap_or_else(|| panic!("{language} grammar failed to parse"));
            assert!(
                parsed.definitions.iter().any(|d| d.name == *expected),
                "{language} did not extract {expected}: {:?}",
                parsed
                    .definitions
                    .iter()
                    .map(|d| (&d.name, d.kind))
                    .collect::<Vec<_>>()
            );
        }
        let elixir = parse_file(
            "elixir",
            "defmodule Alpha do\n  def beta do\n    :ok\n  end\nend\n",
        )
        .unwrap();
        assert!(elixir
            .definitions
            .iter()
            .any(|d| d.name == "beta" && d.kind == "function"));
    }

    #[test]
    fn identifier_at_uses_ast_and_unicode_columns() {
        let source = "fn main() { let 名字 = alpha_beta(); }\n";
        // Column 25 is inside `alpha_beta` (1-based Unicode scalar column).
        assert_eq!(
            identifier_at("rust", source, 1, 25).as_deref(),
            Some("alpha_beta")
        );
        assert_eq!(
            identifier_at("rust", source, 1, 22).as_deref(),
            Some("alpha_beta")
        );
        assert!(identifier_at("rust", source, 1, 1).is_none());
    }

    #[test]
    fn unsupported_language_returns_none() {
        assert!(!is_supported("brainfuck"));
        assert!(parse_file("brainfuck", "+++").is_none());
        assert!(identifier_at("brainfuck", "+++", 1, 1).is_none());
    }
}
