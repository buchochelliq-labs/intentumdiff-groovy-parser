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

use crate::exports::intentumdiff::plugin::parser::ExamplePair;
use crate::exports::intentumdiff::plugin::parser::Guest;
use crate::exports::intentumdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentumdiff::plugin::parser::ParserMode;

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
    "string_literal",
    "integer_literal",
    "boolean_literal",
    "identifier",
];

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



use intentumdiff_plugin_sdk::ts_convert::{convert_semantic_classed, node_to_cst};

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
    use crate::exports::intentumdiff::plugin::parser::Guest;
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
}
