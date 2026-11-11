//! Groovy parser plugin — full-parse mode.
//!
//! Handles `.groovy` and `.gradle` files.
//! The plugin parses source with Tree-sitter inside Rust/Wasm.

use intentumdiff_plugin_sdk::{
    cst::CstNode,
    hash::structural_hash_with_memo,
    tree::{SemanticNode, SemanticNodeBuilder},
};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct GroovyParser;

const TRIVIA: &[&str] = &["comment", "line_comment", "block_comment", "whitespace"];

const SEMANTIC_TYPES: &[&str] = &[
    // Root
    "compilation_unit",
    "script",
    // Type declarations
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "annotation_type_declaration",
    "trait_declaration",
    // Members
    "method_declaration",
    "constructor_declaration",
    "field_declaration",
    "enum_constant",
    // Imports / package
    "import_declaration",
    "package_declaration",
    // Closures
    "closure",
    "lambda_expression",
    // Statements
    "variable_declaration",
    "assignment_expression",
    "expression_statement",
    "if_statement",
    "for_statement",
    "enhanced_for_statement",
    "while_statement",
    "do_while_statement",
    "switch_statement",
    "try_statement",
    "catch_clause",
    "finally_clause",
    "return_statement",
    "throw_statement",
    "assert_statement",
    "break_statement",
    "continue_statement",
    // Expressions
    "method_call",
    "method_invocation",
    "object_creation_expression",
    // Operator-bearing expressions: labelled by their operator token, which is kept as an
    // `operator` leaf child (see `node_to_cst`) so `6 + 2` -> `6 - 2` is not style-only.
    "binary_expression",
    "unary_expression",
    "update_expression",
    "range_expression",
    "operator",
    // Literals. tree-sitter-groovy emits the Java-style numeric kinds and bare
    // `true`/`false`; `integer_literal`/`boolean_literal` are kept for older grammars.
    "string_literal",
    "integer_literal",
    "decimal_integer_literal",
    "hex_integer_literal",
    "octal_integer_literal",
    "binary_integer_literal",
    "decimal_floating_point_literal",
    "hex_floating_point_literal",
    "character_literal",
    "boolean_literal",
    "true",
    "false",
    "null_literal",
    "identifier",
];

/// Expression kinds whose operator is an ANONYMOUS tree-sitter token. The shared SDK
/// converter keeps named children only, so without this the operator vanished from the
/// projection and from every structural hash (issue #4).
const OPERATOR_KINDS: &[&str] = &[
    "binary_expression",
    "unary_expression",
    "update_expression",
    "range_expression",
    "assignment_expression",
];

/// Anonymous tokens inside an operator-bearing expression that are layout, not operators.
const NON_OPERATOR_TOKENS: &[&str] = &["(", ")", "[", "]", "{", "}", ",", ";"];

fn is_operator_token(parent_kind: &str, child: &tree_sitter::Node<'_>, source: &[u8]) -> bool {
    if child.is_named() || !OPERATOR_KINDS.contains(&parent_kind) {
        return false;
    }
    let text = child.utf8_text(source).unwrap_or("");
    !text.trim().is_empty() && !NON_OPERATOR_TOKENS.contains(&text)
}

/// Tree-sitter -> CstNode, identical to the SDK's `ts_convert::node_to_cst` (named
/// children, 0-based rows, UTF-8 byte columns, text on leaves and literal kinds) except
/// that operator tokens of `OPERATOR_KINDS` survive as `operator` leaves in source order.
fn node_to_cst(node: tree_sitter::Node<'_>, source: &[u8]) -> CstNode {
    let kind = node.kind();
    let children: Vec<CstNode> = (0..node.child_count())
        .filter_map(|i| node.child(i))
        .filter_map(|child| {
            if child.is_named() {
                Some(node_to_cst(child, source))
            } else if is_operator_token(kind, &child, source) {
                Some(CstNode {
                    node_type: "operator".to_string(),
                    named: false,
                    text: Some(child.utf8_text(source).unwrap_or("").to_string()),
                    start_line: child.start_position().row as u32,
                    start_col: child.start_position().column as u32,
                    end_line: child.end_position().row as u32,
                    end_col: child.end_position().column as u32,
                    children: Vec::new(),
                })
            } else {
                None
            }
        })
        .collect();

    let keep_text =
        children.is_empty() || intentumdiff_plugin_sdk::ts_convert::cst_literal_kind(kind);
    let text = if keep_text {
        Some(
            node.utf8_text(source)
                .unwrap_or("")
                .chars()
                .take(4096)
                .collect(),
        )
    } else {
        None
    };

    CstNode {
        node_type: kind.to_string(),
        named: node.is_named(),
        text,
        start_line: node.start_position().row as u32,
        start_col: node.start_position().column as u32,
        end_line: node.end_position().row as u32,
        end_col: node.end_position().column as u32,
        children,
    }
}

/// The operator text of an operator-bearing expression (its first `operator` leaf).
fn operator_label(node: &CstNode) -> Option<String> {
    if !OPERATOR_KINDS.contains(&node.node_type.as_str()) {
        return None;
    }
    node.children
        .iter()
        .find(|c| c.node_type == "operator")
        .map(|c| c.text_or_empty().to_string())
}

fn is_semantic(node_type: &str) -> bool {
    SEMANTIC_TYPES.contains(&node_type)
}

fn label_for(node: &CstNode) -> String {
    if node.is_leaf() {
        return node.text_or_empty().to_string();
    }
    // Literal containers label with their captured source text (SDK-shared, issue #47).
    if let Some(label) = intentumdiff_plugin_sdk::ts_convert::literal_label(node) {
        return label;
    }
    if node.node_type != "assignment_expression" {
        if let Some(op) = operator_label(node) {
            return op;
        }
    }
    match node.node_type.as_str() {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "annotation_type_declaration"
        | "trait_declaration" => {
            for child in &node.children {
                if child.node_type == "identifier" || child.node_type == "type_identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "method_declaration" | "constructor_declaration" => {
            for child in &node.children {
                if child.node_type == "identifier" || child.node_type == "method_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "field_declaration" | "variable_declaration" => {
            for child in &node.children {
                if child.node_type == "variable_declarator" {
                    for gc in &child.children {
                        if gc.node_type == "identifier" {
                            return gc.text_or_empty().to_string();
                        }
                    }
                }
                if child.node_type == "identifier" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        "method_call" | "method_invocation" => {
            for child in &node.children {
                if child.node_type == "identifier" || child.node_type == "method_name" {
                    return child.text_or_empty().to_string();
                }
            }
        }
        _ => {}
    }
    for child in &node.children {
        if child.node_type == "identifier" {
            return child.text_or_empty().to_string();
        }
    }
    node.node_type.clone()
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "annotation_type_declaration"
            | "trait_declaration"
    )
}

fn is_method_like(node_type: &str) -> bool {
    matches!(node_type, "method_declaration" | "constructor_declaration")
}

fn convert(
    node: &CstNode,
    id_prefix: &str,
    parent_class: Option<&str>,
    memo: &mut std::collections::HashMap<usize, String>,
) -> Option<SemanticNode> {
    convert_semantic_classed(
        node,
        id_prefix,
        parent_class,
        memo,
        &|t| TRIVIA.contains(&t),
        &is_semantic,
        &is_class_like,
        &is_method_like,
        &label_for,
    )
}

use intentumdiff_plugin_sdk::ts_convert::convert_semantic_classed;

fn parse_source(source: &str) -> Result<CstNode, String> {
    let mut parser = tree_sitter::Parser::new();
    let lang = tree_sitter_groovy::LANGUAGE.into();
    parser
        .set_language(&lang)
        .map_err(|_| "Failed to load groovy grammar".to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "Parse failed".to_string())?;
    Ok(node_to_cst(tree.root_node(), source.as_bytes()))
}

fn process_impl(source: &str) -> String {
    let root: CstNode = match parse_source(source) {
        Ok(n) => n,
        Err(e) => return format!(r#"{{\"error\":\"{}\"}}"#, e),
    };
    let mut memo: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let sem = match convert(&root, "0", None, &mut memo) {
        Some(n) => n,
        None => return r#"{"error":"Empty semantic tree"}"#.to_string(),
    };
    match serde_json::to_string(&sem) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for GroovyParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "groovy".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".groovy") || lower.ends_with(".gradle") {
            return "groovy".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "def greet(name) {\n    println \"Hello, $name\"\n}\n\ndef add(a, b) {\n    return a + b\n}\n".to_string(),
            new: "def greet(String name) {\n    println \"Hello, ${name}!\"\n}\n\nint add(int x, int y) {\n    return x + y\n}\n\nint multiply(int x, int y) {\n    return x * y\n}\n".to_string(),
        }
    }
    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }
    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }
    fn language_ids() -> Vec<String> {
        vec!["groovy".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(GroovyParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentumdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!GroovyParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = GroovyParser::grammar_id();
        let ids = GroovyParser::language_ids();
        assert!(
            ids.contains(&gid),
            "language_ids {:?} must contain {:?}",
            ids,
            gid
        );
    }

    #[test]
    fn detect_language_known_ext() {
        let r = GroovyParser::detect_language("test.groovy".to_string(), "".to_string());
        assert_eq!(r.as_str(), "groovy");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r =
            GroovyParser::detect_language("test.xyz_notareal_ext_9z8y".to_string(), "".to_string());
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            GroovyParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = GroovyParser::example(GroovyParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(!out.contains("\"error\""), "{out}");
    }
    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }

    // --- Issue #4: numeric return values and arithmetic arguments -----------------------

    fn tree(src: &str) -> serde_json::Value {
        let out = process_impl(src);
        t::assert_valid_json(&out, "process");
        serde_json::from_str(&out).unwrap()
    }

    /// All (node_type, label, [start_line, start_col, end_line, end_col]) in pre-order.
    fn facts(v: &serde_json::Value, acc: &mut Vec<(String, String, [u64; 4])>) {
        let p = &v["position"];
        acc.push((
            v["node_type"].as_str().unwrap().to_string(),
            v["label"].as_str().unwrap().to_string(),
            [
                p["start_line"].as_u64().unwrap(),
                p["start_col"].as_u64().unwrap(),
                p["end_line"].as_u64().unwrap(),
                p["end_col"].as_u64().unwrap(),
            ],
        ));
        for c in v["children"].as_array().unwrap() {
            facts(c, acc);
        }
    }

    fn all_facts(src: &str) -> Vec<(String, String, [u64; 4])> {
        let mut acc = Vec::new();
        facts(&tree(src), &mut acc);
        acc
    }

    fn has(f: &[(String, String, [u64; 4])], ty: &str, label: &str, span: [u64; 4]) -> bool {
        f.iter()
            .any(|(t, l, s)| t == ty && l == label && *s == span)
    }

    const LITERAL_OLD: &str = "def answer() { return 1 }\nprintln answer()\n";
    const LITERAL_NEW: &str = "def answer() { return 2 }\nprintln answer()\n";
    const OPERATOR_OLD: &str = "println(6 + 2)\n";
    const OPERATOR_NEW: &str = "println(6 - 2)\n";

    #[test]
    fn return_value_literal_survives_projection() {
        let old = all_facts(LITERAL_OLD);
        let new = all_facts(LITERAL_NEW);
        assert!(
            has(&old, "decimal_integer_literal", "1", [0, 22, 0, 23]),
            "{old:?}"
        );
        assert!(
            has(&new, "decimal_integer_literal", "2", [0, 22, 0, 23]),
            "{new:?}"
        );
        assert_ne!(old, new);
        assert_ne!(
            tree(LITERAL_OLD)["structural_hash"],
            tree(LITERAL_NEW)["structural_hash"]
        );
    }

    #[test]
    fn arithmetic_operator_survives_projection() {
        let old = all_facts(OPERATOR_OLD);
        let new = all_facts(OPERATOR_NEW);
        assert!(
            has(&old, "binary_expression", "+", [0, 8, 0, 13]),
            "{old:?}"
        );
        assert!(has(&old, "operator", "+", [0, 10, 0, 11]), "{old:?}");
        assert!(
            has(&new, "binary_expression", "-", [0, 8, 0, 13]),
            "{new:?}"
        );
        assert!(has(&new, "operator", "-", [0, 10, 0, 11]), "{new:?}");
        for (src, f) in [(OPERATOR_OLD, &old), (OPERATOR_NEW, &new)] {
            assert!(
                has(f, "decimal_integer_literal", "6", [0, 8, 0, 9]),
                "{src}: {f:?}"
            );
            assert!(
                has(f, "decimal_integer_literal", "2", [0, 12, 0, 13]),
                "{src}: {f:?}"
            );
        }
        assert_ne!(
            tree(OPERATOR_OLD)["structural_hash"],
            tree(OPERATOR_NEW)["structural_hash"]
        );
    }

    #[test]
    fn operator_spacing_is_insignificant() {
        // Layout around the operator changes positions, never labels or hashes.
        let spaced = tree("println(6 + 2)\n");
        let tight = tree("println(6+2)\n");
        assert_eq!(spaced["structural_hash"], tight["structural_hash"]);
        let labels = |src: &str| {
            all_facts(src)
                .into_iter()
                .map(|(t, l, _)| (t, l))
                .collect::<Vec<_>>()
        };
        assert_eq!(labels("println(6 + 2)\n"), labels("println(6+2)\n"));
    }

    #[test]
    fn operator_and_literal_projection_is_deterministic() {
        for src in [LITERAL_OLD, OPERATOR_OLD, "x = -a++ + (1..<3).size()\n"] {
            assert_eq!(process_impl(src), process_impl(src));
        }
    }

    #[test]
    fn unary_and_update_operators_are_kept() {
        let f = all_facts("x = -a\ny++\n");
        assert!(
            f.iter()
                .any(|(t, l, _)| t == "unary_expression" && l == "-"),
            "{f:?}"
        );
        assert!(
            f.iter()
                .any(|(t, l, _)| t == "update_expression" && l == "++"),
            "{f:?}"
        );
        assert_ne!(
            tree("x = -a\n")["structural_hash"],
            tree("x = +a\n")["structural_hash"]
        );
    }

    #[test]
    fn operator_token_columns_are_utf8_bytes() {
        // "é" is two UTF-8 bytes, so the `+` sits at byte column 13, not char column 12.
        let f = all_facts("println(\"é\" + 1)\n");
        assert!(has(&f, "operator", "+", [0, 13, 0, 14]), "{f:?}");
    }
}
