use tree_sitter::{Language, Node, Parser};
use tree_sitter_language::LanguageFn;

use crate::shared::errors::ParserError;
use crate::shared::symbols::{
    normalize_edge_identity_kind, EDGE_IDENTITY_BARE_IDENTIFIER, EDGE_IDENTITY_CONSTRUCTOR_LIKE,
    EDGE_IDENTITY_MACRO_LIKE, EDGE_IDENTITY_MEMBER_ACCESS, EDGE_IDENTITY_METHOD_RECEIVER,
    EDGE_IDENTITY_QUALIFIED_PATH,
};
use crate::shared::types::{ParsedSegment, SegmentRole};

/// Supported language identifiers and their tree-sitter grammars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedLanguage {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    C,
    Cpp,
    Kotlin,
    Css,
    Html,
    Json,
    Bash,
    Toml,
    Yaml,
    Markdown,
}

impl SupportedLanguage {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" | "tsx" => Some(Self::TypeScript),
            "go" => Some(Self::Go),
            "java" => Some(Self::Java),
            "c" | "h" => Some(Self::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            "kt" | "kts" => Some(Self::Kotlin),
            "css" => Some(Self::Css),
            "html" | "htm" => Some(Self::Html),
            "json" => Some(Self::Json),
            "sh" | "bash" | "zsh" => Some(Self::Bash),
            "toml" => Some(Self::Toml),
            "yaml" | "yml" => Some(Self::Yaml),
            "md" | "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Go => "go",
            Self::Java => "java",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Kotlin => "kotlin",
            Self::Css => "css",
            Self::Html => "html",
            Self::Json => "json",
            Self::Bash => "bash",
            Self::Toml => "toml",
            Self::Yaml => "yaml",
            Self::Markdown => "markdown",
        }
    }

    pub fn language_fn(&self) -> LanguageFn {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE,
            Self::Python => tree_sitter_python::LANGUAGE,
            Self::JavaScript => tree_sitter_javascript::LANGUAGE,
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
            Self::Go => tree_sitter_go::LANGUAGE,
            Self::Java => tree_sitter_java::LANGUAGE,
            Self::C => tree_sitter_c::LANGUAGE,
            Self::Cpp => tree_sitter_cpp::LANGUAGE,
            Self::Kotlin => tree_sitter_kotlin_ng::LANGUAGE,
            Self::Css => tree_sitter_css::LANGUAGE,
            Self::Html => tree_sitter_html::LANGUAGE,
            Self::Json => tree_sitter_json::LANGUAGE,
            Self::Bash => tree_sitter_bash::LANGUAGE,
            Self::Toml => tree_sitter_toml_ng::LANGUAGE,
            Self::Yaml => tree_sitter_yaml::LANGUAGE,
            Self::Markdown => tree_sitter_md::LANGUAGE,
        }
    }

    fn top_level_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "function_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "impl_item",
                "type_item",
                "const_item",
                "static_item",
                "mod_item",
                "use_declaration",
                "macro_definition",
            ],
            Self::Python => &[
                "function_definition",
                "class_definition",
                "decorated_definition",
                "import_statement",
                "import_from_statement",
            ],
            Self::JavaScript | Self::TypeScript => &[
                "function_declaration",
                "class_declaration",
                "export_statement",
                "import_statement",
                "lexical_declaration",
                "variable_declaration",
                "interface_declaration",
                "type_alias_declaration",
                "enum_declaration",
            ],
            Self::Go => &[
                "function_declaration",
                "method_declaration",
                "type_declaration",
                "import_declaration",
                "const_declaration",
                "var_declaration",
            ],
            Self::Java => &[
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
                "import_declaration",
                "method_declaration",
                "annotation_type_declaration",
            ],
            Self::C => &[
                "function_definition",
                "struct_specifier",
                "enum_specifier",
                "type_definition",
                "declaration",
                "preproc_include",
                "preproc_def",
            ],
            Self::Cpp => &[
                "function_definition",
                "class_specifier",
                "struct_specifier",
                "enum_specifier",
                "namespace_definition",
                "template_declaration",
                "type_definition",
                "declaration",
                "preproc_include",
                "preproc_def",
            ],
            Self::Kotlin => &[
                "function_declaration",
                "class_declaration",
                "object_declaration",
                "property_declaration",
                "import_header",
                "type_alias",
            ],
            Self::Css => &[
                "rule_set",
                "media_statement",
                "import_statement",
                "keyframes_statement",
                "supports_statement",
                "charset_statement",
                "namespace_statement",
                "at_rule",
            ],
            Self::Html => &["element", "script_element", "style_element", "doctype"],
            Self::Json => &["object", "array"],
            Self::Bash => &[
                "function_definition",
                "command",
                "if_statement",
                "for_statement",
                "while_statement",
                "case_statement",
                "pipeline",
                "variable_assignment",
            ],
            Self::Toml => &["table", "pair"],
            Self::Yaml => &["block_mapping_pair", "block_sequence"],
            Self::Markdown => &["section", "fenced_code_block", "html_block", "list"],
        }
    }

    fn container_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["impl_item", "trait_item", "mod_item"],
            Self::Python => &["class_definition"],
            Self::JavaScript | Self::TypeScript => &["class_declaration"],
            Self::Go => &[],
            Self::Java => &[
                "class_declaration",
                "interface_declaration",
                "enum_declaration",
            ],
            Self::C => &[],
            Self::Cpp => &[
                "class_specifier",
                "struct_specifier",
                "namespace_definition",
            ],
            Self::Kotlin => &[
                "class_declaration",
                "object_declaration",
                "companion_object",
            ],
            Self::Css => &["media_statement", "supports_statement"],
            Self::Html => &["element"],
            Self::Json => &[],
            Self::Bash => &[],
            Self::Toml => &[],
            Self::Yaml => &[],
            Self::Markdown => &["section"],
        }
    }

    fn nested_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["function_item", "const_item", "type_item"],
            Self::Python => &["function_definition", "decorated_definition"],
            Self::JavaScript | Self::TypeScript => &[
                "method_definition",
                "public_field_definition",
                "field_definition",
            ],
            Self::Go => &[],
            Self::Java => &[
                "method_declaration",
                "constructor_declaration",
                "field_declaration",
            ],
            Self::C => &[],
            Self::Cpp => &["function_definition", "field_declaration", "declaration"],
            Self::Kotlin => &[
                "function_declaration",
                "property_declaration",
                "companion_object",
            ],
            Self::Css => &["rule_set"],
            Self::Html => &[],
            Self::Json => &[],
            Self::Bash => &["function_definition", "command", "variable_assignment"],
            Self::Toml => &["pair"],
            Self::Yaml => &["block_mapping_pair"],
            Self::Markdown => &[],
        }
    }

    fn import_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["use_declaration"],
            Self::Python => &["import_statement", "import_from_statement"],
            Self::JavaScript | Self::TypeScript => &["import_statement"],
            Self::Go => &["import_declaration"],
            Self::Java => &["import_declaration"],
            Self::C | Self::Cpp => &["preproc_include"],
            Self::Kotlin => &["import_header"],
            Self::Css => &["import_statement"],
            Self::Html => &[],
            Self::Json => &[],
            Self::Bash => &[],
            Self::Toml => &[],
            Self::Yaml => &[],
            Self::Markdown => &[],
        }
    }

    fn comment_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["line_comment", "block_comment"],
            Self::Python => &["comment"],
            Self::JavaScript | Self::TypeScript => &["comment"],
            Self::Go => &["comment"],
            Self::Java => &["line_comment", "block_comment"],
            Self::C | Self::Cpp => &["comment"],
            Self::Kotlin => &["line_comment", "multiline_comment"],
            Self::Css => &["comment"],
            Self::Html => &["comment"],
            Self::Json => &[],
            Self::Bash => &["comment"],
            Self::Toml => &["comment"],
            Self::Yaml => &["comment"],
            Self::Markdown => &[],
        }
    }

    fn control_flow_kinds(&self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "if_expression",
                "match_expression",
                "for_expression",
                "while_expression",
                "loop_expression",
                "closure_expression",
            ],
            Self::Python => &[
                "if_statement",
                "for_statement",
                "while_statement",
                "try_statement",
                "with_statement",
                "match_statement",
                "lambda",
            ],
            Self::JavaScript | Self::TypeScript => &[
                "if_statement",
                "for_statement",
                "for_in_statement",
                "while_statement",
                "do_statement",
                "switch_statement",
                "try_statement",
                "ternary_expression",
                "arrow_function",
            ],
            Self::Go => &[
                "if_statement",
                "for_statement",
                "switch_statement",
                "select_statement",
                "type_switch_statement",
                "func_literal",
            ],
            Self::Java => &[
                "if_statement",
                "for_statement",
                "enhanced_for_statement",
                "while_statement",
                "do_statement",
                "switch_expression",
                "try_statement",
                "ternary_expression",
                "lambda_expression",
            ],
            Self::C | Self::Cpp => &[
                "if_statement",
                "for_statement",
                "while_statement",
                "do_statement",
                "switch_statement",
                "conditional_expression",
            ],
            Self::Kotlin => &[
                "if_expression",
                "when_expression",
                "for_statement",
                "while_statement",
                "do_while_statement",
                "try_expression",
                "lambda_literal",
            ],
            Self::Css | Self::Html | Self::Json | Self::Toml | Self::Yaml | Self::Markdown => &[],
            Self::Bash => &[
                "if_statement",
                "for_statement",
                "while_statement",
                "case_statement",
            ],
        }
    }

    /// Whether this language has meaningful structural segments (functions,
    /// classes, rules) that benefit from tree-sitter segmentation. Data/config
    /// formats like JSON, YAML, TOML produce too many fine-grained segments
    /// and are better served by the text chunker.
    pub fn has_structural_segments(&self) -> bool {
        match self {
            Self::Rust
            | Self::Python
            | Self::JavaScript
            | Self::TypeScript
            | Self::Go
            | Self::Java
            | Self::C
            | Self::Cpp
            | Self::Kotlin
            | Self::Css
            | Self::Bash => true,
            Self::Html | Self::Json | Self::Toml | Self::Yaml | Self::Markdown => false,
        }
    }
}

/// Parse a source file and extract segments using tree-sitter.
///
/// Returns `Vec<ParsedSegment>` with one segment per top-level construct and
/// one per nested method/function inside container types.
pub fn parse_file(source: &str, language: &str) -> Result<Vec<ParsedSegment>, ParserError> {
    let lang = SupportedLanguage::from_extension(language)
        .or(match language {
            "rust" => Some(SupportedLanguage::Rust),
            "python" => Some(SupportedLanguage::Python),
            "javascript" => Some(SupportedLanguage::JavaScript),
            "typescript" => Some(SupportedLanguage::TypeScript),
            "go" => Some(SupportedLanguage::Go),
            "java" => Some(SupportedLanguage::Java),
            "c" => Some(SupportedLanguage::C),
            "cpp" => Some(SupportedLanguage::Cpp),
            "kotlin" => Some(SupportedLanguage::Kotlin),
            "css" => Some(SupportedLanguage::Css),
            "html" => Some(SupportedLanguage::Html),
            "json" => Some(SupportedLanguage::Json),
            "bash" | "shell" => Some(SupportedLanguage::Bash),
            "toml" => Some(SupportedLanguage::Toml),
            "yaml" => Some(SupportedLanguage::Yaml),
            "markdown" => Some(SupportedLanguage::Markdown),
            _ => None,
        })
        .ok_or_else(|| ParserError::UnsupportedLanguage(language.to_string()))?;

    let ts_language = Language::new(lang.language_fn());
    let mut parser = Parser::new();
    parser
        .set_language(&ts_language)
        .map_err(|e| ParserError::ParseFailed(format!("failed to set language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| ParserError::ParseFailed("tree-sitter parse returned None".into()))?;

    let root = tree.root_node();
    let source_bytes = source.as_bytes();
    let mut segments = Vec::new();

    let top_level = lang.top_level_kinds();
    let containers = lang.container_kinds();
    let comment_kinds = lang.comment_kinds();

    let mut i = 0;
    let child_count = root.named_child_count();

    while i < child_count {
        let node = root.named_child(i as u32).unwrap();
        let kind = node.kind();

        if comment_kinds.contains(&kind) {
            i += 1;
            continue;
        }

        let leading_comments = collect_leading_comments(&root, i, comment_kinds, source_bytes);

        if top_level.contains(&kind) {
            if containers.contains(&kind) {
                let container_segment =
                    extract_segment(&node, source_bytes, lang, &leading_comments, None);
                segments.push(container_segment);

                extract_nested(
                    &node,
                    source_bytes,
                    lang,
                    &node_name(&node, source_bytes, lang),
                    &mut segments,
                );
            } else if kind == "decorated_definition" {
                if let Some(inner) = find_decorated_inner(&node) {
                    let segment =
                        extract_segment(&node, source_bytes, lang, &leading_comments, None);
                    if containers.contains(&inner.kind()) {
                        segments.push(segment);
                        extract_nested(
                            &inner,
                            source_bytes,
                            lang,
                            &node_name(&inner, source_bytes, lang),
                            &mut segments,
                        );
                    } else {
                        segments.push(segment);
                    }
                } else {
                    let segment =
                        extract_segment(&node, source_bytes, lang, &leading_comments, None);
                    segments.push(segment);
                }
            } else if kind == "export_statement" {
                if let Some(decl) = node.child_by_field_name("declaration") {
                    let segment =
                        extract_segment(&node, source_bytes, lang, &leading_comments, None);
                    if containers.contains(&decl.kind()) {
                        segments.push(segment);
                        extract_nested(
                            &decl,
                            source_bytes,
                            lang,
                            &node_name(&decl, source_bytes, lang),
                            &mut segments,
                        );
                    } else {
                        segments.push(segment);
                    }
                } else {
                    let segment =
                        extract_segment(&node, source_bytes, lang, &leading_comments, None);
                    segments.push(segment);
                }
            } else {
                let segment = extract_segment(&node, source_bytes, lang, &leading_comments, None);
                segments.push(segment);
            }
        }

        i += 1;
    }

    Ok(segments)
}

/// Check if a language/extension is supported for indexing.
/// This covers both tree-sitter parsed languages and text document types
/// that are indexed via FTS text chunking.
pub fn is_language_supported(language: &str) -> bool {
    SupportedLanguage::from_extension(language).is_some()
        || is_text_document(language)
        || matches!(
            language,
            "rust"
                | "python"
                | "javascript"
                | "typescript"
                | "go"
                | "java"
                | "c"
                | "cpp"
                | "kotlin"
                | "css"
                | "html"
                | "bash"
                | "shell"
                | "toml"
                | "yaml"
                | "markdown"
        )
}

/// Text document types that are indexed via FTS text chunking.
/// These don't have tree-sitter grammars but contain valuable searchable text.
fn is_text_document(ext: &str) -> bool {
    matches!(
        ext,
        "txt"
            | "rst"
            | "adoc"
            | "asciidoc"
            | "tex"
            | "org"
            | "proto"
            | "properties"
            | "conf"
            | "ini"
            | "tf"
            | "hcl"
            | "sql"
            | "sq"
            | "sqm"
            | "dockerfile"
            | "makefile"
            | "justfile"
    )
}

/// Check if a language benefits from tree-sitter structural segmentation.
/// Data/config/markup languages are recognized but better served by the text
/// chunker to avoid segment explosion and slow indexing.
pub fn use_structural_parser(language: &str) -> bool {
    match SupportedLanguage::from_extension(language) {
        Some(lang) => lang.has_structural_segments(),
        None => matches!(
            language,
            "rust"
                | "python"
                | "javascript"
                | "typescript"
                | "go"
                | "java"
                | "c"
                | "cpp"
                | "kotlin"
                | "css"
                | "bash"
                | "shell"
        ),
    }
}

fn collect_leading_comments<'a>(
    root: &Node<'a>,
    current_index: usize,
    comment_kinds: &[&str],
    source: &[u8],
) -> String {
    let mut comments = Vec::new();
    let mut j = current_index;
    while j > 0 {
        j -= 1;
        if let Some(prev) = root.named_child(j as u32) {
            if comment_kinds.contains(&prev.kind()) {
                if let Ok(text) = prev.utf8_text(source) {
                    comments.push(text.to_string());
                }
            } else {
                break;
            }
        } else {
            break;
        }
    }
    comments.reverse();
    comments.join("\n")
}

fn extract_segment(
    node: &Node,
    source: &[u8],
    lang: SupportedLanguage,
    leading_comments: &str,
    breadcrumb: Option<&str>,
) -> ParsedSegment {
    let content = node.utf8_text(source).unwrap_or("");
    let full_content = if leading_comments.is_empty() {
        content.to_string()
    } else {
        format!("{leading_comments}\n{content}")
    };

    let line_start = if leading_comments.is_empty() {
        node.start_position().row + 1
    } else {
        let comment_lines = leading_comments.matches('\n').count();
        let node_line = node.start_position().row + 1;
        node_line.saturating_sub(comment_lines + 1)
    };
    let line_end = node.end_position().row + 1;

    let block_type = classify_block_type(node, lang);
    let role = classify_role(node, lang);
    let complexity = compute_complexity(node, lang);
    let defined_symbols = collect_defined_symbols(node, source, lang);
    let referenced_symbols = collect_referenced_symbols(node, source, lang);
    let called_symbols = collect_called_symbols(node, source, lang);

    ParsedSegment {
        content: full_content,
        block_type,
        line_start,
        line_end,
        language: lang.name().to_string(),
        breadcrumb: breadcrumb.map(|s| s.to_string()),
        complexity,
        role,
        defined_symbols,
        referenced_symbols,
        called_symbols,
    }
}

fn extract_nested(
    container: &Node,
    source: &[u8],
    lang: SupportedLanguage,
    parent_name: &str,
    segments: &mut Vec<ParsedSegment>,
) {
    let nested_kinds = lang.nested_kinds();
    let comment_kinds = lang.comment_kinds();

    let body = find_body_node(container, lang);
    let search_node = body.as_ref().unwrap_or(container);

    let child_count = search_node.named_child_count();
    for i in 0..child_count {
        let child = search_node.named_child(i as u32).unwrap();
        let kind = child.kind();

        if kind == "decorated_definition" {
            if let Some(inner) = find_decorated_inner(&child) {
                if nested_kinds.contains(&inner.kind()) {
                    let comments = collect_leading_comments(search_node, i, comment_kinds, source);
                    let segment =
                        extract_segment(&child, source, lang, &comments, Some(parent_name));
                    segments.push(segment);
                }
            }
            continue;
        }

        if nested_kinds.contains(&kind) {
            let comments = collect_leading_comments(search_node, i, comment_kinds, source);
            let segment = extract_segment(&child, source, lang, &comments, Some(parent_name));
            segments.push(segment);
        }
    }
}

fn find_body_node<'a>(node: &Node<'a>, lang: SupportedLanguage) -> Option<Node<'a>> {
    match lang {
        SupportedLanguage::Rust => node.child_by_field_name("body"),
        SupportedLanguage::Python => node.child_by_field_name("body"),
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => {
            node.child_by_field_name("body")
        }
        SupportedLanguage::Java => node.child_by_field_name("body"),
        SupportedLanguage::Go => None,
        SupportedLanguage::C | SupportedLanguage::Cpp => node.child_by_field_name("body"),
        SupportedLanguage::Kotlin => node.child_by_field_name("body"),
        SupportedLanguage::Bash => node.child_by_field_name("body"),
        SupportedLanguage::Css
        | SupportedLanguage::Html
        | SupportedLanguage::Json
        | SupportedLanguage::Toml
        | SupportedLanguage::Yaml
        | SupportedLanguage::Markdown => None,
    }
}

fn find_decorated_inner<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    node.child_by_field_name("definition")
}

fn node_name(node: &Node, source: &[u8], lang: SupportedLanguage) -> String {
    let name_field = match lang {
        SupportedLanguage::Rust => {
            let kind = node.kind();
            if kind == "impl_item" {
                return impl_name(node, source);
            }
            "name"
        }
        SupportedLanguage::Go => {
            if node.kind() == "type_declaration" {
                if let Some(spec) = node.named_child(0) {
                    if let Some(n) = spec.child_by_field_name("name") {
                        return n.utf8_text(source).unwrap_or("unknown").to_string();
                    }
                }
                return "unknown".to_string();
            }
            "name"
        }
        _ => "name",
    };

    node.child_by_field_name(name_field)
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("unknown")
        .to_string()
}

fn impl_name(node: &Node, source: &[u8]) -> String {
    let type_node = node.child_by_field_name("type");
    let trait_node = node.child_by_field_name("trait");

    match (trait_node, type_node) {
        (Some(tr), Some(ty)) => {
            let trait_name = tr.utf8_text(source).unwrap_or("?");
            let type_name = ty.utf8_text(source).unwrap_or("?");
            format!("{trait_name} for {type_name}")
        }
        (None, Some(ty)) => ty.utf8_text(source).unwrap_or("unknown").to_string(),
        _ => "unknown".to_string(),
    }
}

fn classify_block_type(node: &Node, lang: SupportedLanguage) -> String {
    let kind = node.kind();
    match lang {
        SupportedLanguage::Rust => match kind {
            "function_item" => "function",
            "struct_item" => "struct",
            "enum_item" => "enum",
            "trait_item" => "trait",
            "impl_item" => "impl",
            "type_item" => "type",
            "const_item" => "const",
            "static_item" => "static",
            "mod_item" => "module",
            "use_declaration" => "import",
            "macro_definition" => "macro",
            _ => kind,
        },
        SupportedLanguage::Python => match kind {
            "function_definition" => "function",
            "class_definition" => "class",
            "decorated_definition" => classify_decorated_block_type(node),
            "import_statement" | "import_from_statement" => "import",
            _ => kind,
        },
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => match kind {
            "function_declaration" => "function",
            "class_declaration" => "class",
            "method_definition" => "function",
            "export_statement" => classify_export_block_type(node),
            "import_statement" => "import",
            "lexical_declaration" | "variable_declaration" => "variable",
            "interface_declaration" => "interface",
            "type_alias_declaration" => "type",
            "enum_declaration" => "enum",
            _ => kind,
        },
        SupportedLanguage::Go => match kind {
            "function_declaration" => "function",
            "method_declaration" => "function",
            "type_declaration" => "type",
            "import_declaration" => "import",
            "const_declaration" => "const",
            "var_declaration" => "variable",
            _ => kind,
        },
        SupportedLanguage::Java => match kind {
            "class_declaration" => "class",
            "interface_declaration" => "interface",
            "enum_declaration" => "enum",
            "method_declaration" | "constructor_declaration" => "function",
            "import_declaration" => "import",
            "annotation_type_declaration" => "annotation",
            _ => kind,
        },
        SupportedLanguage::C => match kind {
            "function_definition" => "function",
            "struct_specifier" => "struct",
            "enum_specifier" => "enum",
            "type_definition" => "type",
            "declaration" => "variable",
            "preproc_include" => "import",
            "preproc_def" => "macro",
            _ => kind,
        },
        SupportedLanguage::Cpp => match kind {
            "function_definition" => "function",
            "class_specifier" => "class",
            "struct_specifier" => "struct",
            "enum_specifier" => "enum",
            "namespace_definition" => "namespace",
            "template_declaration" => "template",
            "type_definition" => "type",
            "declaration" => "variable",
            "preproc_include" => "import",
            "preproc_def" => "macro",
            _ => kind,
        },
        SupportedLanguage::Kotlin => match kind {
            "function_declaration" => "function",
            "class_declaration" => "class",
            "object_declaration" => "class",
            "property_declaration" => "variable",
            "import_header" => "import",
            "type_alias" => "type",
            "companion_object" => "class",
            _ => kind,
        },
        SupportedLanguage::Css => match kind {
            "rule_set" => "rule",
            "media_statement" => "media",
            "import_statement" => "import",
            "keyframes_statement" => "keyframes",
            "supports_statement" => "supports",
            "charset_statement" => "charset",
            "namespace_statement" => "namespace",
            "at_rule" => "at_rule",
            _ => kind,
        },
        SupportedLanguage::Html => match kind {
            "element" | "script_element" | "style_element" => "element",
            "doctype" => "doctype",
            _ => kind,
        },
        SupportedLanguage::Json => match kind {
            "object" => "object",
            "array" => "array",
            _ => kind,
        },
        SupportedLanguage::Bash => match kind {
            "function_definition" => "function",
            "command" => "command",
            "if_statement" => "if",
            "for_statement" => "for",
            "while_statement" => "while",
            "case_statement" => "case",
            "pipeline" => "pipeline",
            "variable_assignment" => "variable",
            _ => kind,
        },
        SupportedLanguage::Toml => match kind {
            "table" => "table",
            "pair" => "pair",
            _ => kind,
        },
        SupportedLanguage::Yaml => match kind {
            "block_mapping_pair" => "mapping",
            "block_sequence" => "sequence",
            _ => kind,
        },
        SupportedLanguage::Markdown => match kind {
            "section" => "section",
            "fenced_code_block" => "code_block",
            "html_block" => "html_block",
            "list" => "list",
            _ => kind,
        },
    }
    .to_string()
}

fn classify_decorated_block_type(node: &Node) -> &'static str {
    if let Some(inner) = find_decorated_inner(node) {
        match inner.kind() {
            "function_definition" => "function",
            "class_definition" => "class",
            _ => "decorated",
        }
    } else {
        "decorated"
    }
}

fn classify_export_block_type(node: &Node) -> &'static str {
    if let Some(decl) = node.child_by_field_name("declaration") {
        match decl.kind() {
            "function_declaration" => "function",
            "class_declaration" => "class",
            "lexical_declaration" | "variable_declaration" => "variable",
            "interface_declaration" => "interface",
            "type_alias_declaration" => "type",
            "enum_declaration" => "enum",
            _ => "export",
        }
    } else {
        "export"
    }
}

fn classify_role(node: &Node, lang: SupportedLanguage) -> SegmentRole {
    let kind = node.kind();

    if lang.import_kinds().contains(&kind) {
        return SegmentRole::Import;
    }

    match lang {
        SupportedLanguage::Rust => match kind {
            "function_item" => SegmentRole::Implementation,
            "struct_item" | "enum_item" | "trait_item" | "type_item" => SegmentRole::Definition,
            "impl_item" => SegmentRole::Implementation,
            "mod_item" => SegmentRole::Orchestration,
            "macro_definition" => SegmentRole::Definition,
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::Python => match kind {
            "function_definition" => SegmentRole::Implementation,
            "class_definition" => SegmentRole::Definition,
            "decorated_definition" => {
                if let Some(inner) = find_decorated_inner(node) {
                    match inner.kind() {
                        "function_definition" => SegmentRole::Implementation,
                        "class_definition" => SegmentRole::Definition,
                        _ => SegmentRole::Definition,
                    }
                } else {
                    SegmentRole::Definition
                }
            }
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => match kind {
            "function_declaration" | "method_definition" => SegmentRole::Implementation,
            "class_declaration" => SegmentRole::Definition,
            "export_statement" => {
                if let Some(decl) = node.child_by_field_name("declaration") {
                    match decl.kind() {
                        "function_declaration" => SegmentRole::Implementation,
                        "class_declaration" => SegmentRole::Definition,
                        _ => SegmentRole::Definition,
                    }
                } else {
                    SegmentRole::Orchestration
                }
            }
            "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
                SegmentRole::Definition
            }
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::Go => match kind {
            "function_declaration" | "method_declaration" => SegmentRole::Implementation,
            "type_declaration" => SegmentRole::Definition,
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::Java => match kind {
            "class_declaration" | "interface_declaration" | "enum_declaration" => {
                SegmentRole::Definition
            }
            "method_declaration" | "constructor_declaration" => SegmentRole::Implementation,
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::C | SupportedLanguage::Cpp => match kind {
            "function_definition" => SegmentRole::Implementation,
            "struct_specifier" | "class_specifier" | "enum_specifier" | "type_definition" => {
                SegmentRole::Definition
            }
            "namespace_definition" => SegmentRole::Orchestration,
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::Kotlin => match kind {
            "function_declaration" => SegmentRole::Implementation,
            "class_declaration" | "object_declaration" | "type_alias" => SegmentRole::Definition,
            "property_declaration" => SegmentRole::Definition,
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::Css => match kind {
            "rule_set" => SegmentRole::Definition,
            "import_statement" => SegmentRole::Import,
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::Html => SegmentRole::Definition,
        SupportedLanguage::Json | SupportedLanguage::Toml | SupportedLanguage::Yaml => {
            SegmentRole::Definition
        }
        SupportedLanguage::Bash => match kind {
            "function_definition" => SegmentRole::Implementation,
            "command" | "pipeline" => SegmentRole::Orchestration,
            _ => SegmentRole::Definition,
        },
        SupportedLanguage::Markdown => SegmentRole::Docs,
    }
}

fn compute_complexity(node: &Node, lang: SupportedLanguage) -> u32 {
    let cf_kinds = lang.control_flow_kinds();
    let mut max_depth = 0u32;
    walk_complexity(node, cf_kinds, 0, &mut max_depth);
    max_depth
}

fn walk_complexity(node: &Node, cf_kinds: &[&str], current_depth: u32, max_depth: &mut u32) {
    let kind = node.kind();
    let depth = if cf_kinds.contains(&kind) {
        current_depth + 1
    } else {
        current_depth
    };

    if depth > *max_depth {
        *max_depth = depth;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_complexity(&child, cf_kinds, depth, max_depth);
    }
}

fn collect_defined_symbols(node: &Node, source: &[u8], lang: SupportedLanguage) -> Vec<String> {
    let mut symbols = Vec::new();
    collect_defined_symbols_inner(node, source, lang, &mut symbols);
    symbols
}

fn collect_defined_symbols_inner(
    node: &Node,
    source: &[u8],
    lang: SupportedLanguage,
    symbols: &mut Vec<String>,
) {
    let kind = node.kind();

    match lang {
        SupportedLanguage::Rust => match kind {
            "function_item" | "struct_item" | "enum_item" | "trait_item" | "type_item"
            | "const_item" | "static_item" | "macro_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "impl_item" => {
                if let Some(ty) = node.child_by_field_name("type") {
                    if let Ok(text) = ty.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            _ => {}
        },
        SupportedLanguage::Python => match kind {
            "function_definition" | "class_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "decorated_definition" => {
                if let Some(inner) = find_decorated_inner(node) {
                    collect_defined_symbols_inner(&inner, source, lang, symbols);
                }
            }
            _ => {}
        },
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => match kind {
            "function_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "type_alias_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "method_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "export_statement" => {
                if let Some(decl) = node.child_by_field_name("declaration") {
                    collect_defined_symbols_inner(&decl, source, lang, symbols);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                collect_variable_names(node, source, symbols);
            }
            _ => {}
        },
        SupportedLanguage::Go => match kind {
            "function_declaration" | "method_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "type_declaration" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "type_spec" {
                        if let Some(name) = child.child_by_field_name("name") {
                            if let Ok(text) = name.utf8_text(source) {
                                symbols.push(text.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        },
        SupportedLanguage::Java => match kind {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "method_declaration"
            | "constructor_declaration"
            | "annotation_type_declaration" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            _ => {}
        },
        SupportedLanguage::C | SupportedLanguage::Cpp => match kind {
            "function_definition" => {
                if let Some(declarator) = node.child_by_field_name("declarator") {
                    extract_c_declarator_name(&declarator, source, symbols);
                }
            }
            "struct_specifier" | "class_specifier" | "enum_specifier" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "type_definition" => {
                if let Some(declarator) = node.child_by_field_name("declarator") {
                    if let Ok(text) = declarator.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "namespace_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            _ => {}
        },
        SupportedLanguage::Kotlin => match kind {
            "function_declaration" | "class_declaration" | "object_declaration" | "type_alias" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "property_declaration" => {
                // Kotlin properties use variable_declaration children
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "variable_declaration" {
                        if let Some(name) = child.child_by_field_name("name") {
                            if let Ok(text) = name.utf8_text(source) {
                                symbols.push(text.to_string());
                            }
                        }
                    }
                }
            }
            _ => {}
        },
        SupportedLanguage::Css => {
            if kind == "rule_set" {
                if let Some(sel) = node.child_by_field_name("selectors") {
                    if let Ok(text) = sel.utf8_text(source) {
                        symbols.push(text.trim().to_string());
                    }
                }
            }
        }
        SupportedLanguage::Html => {
            if kind == "element" || kind == "script_element" || kind == "style_element" {
                if let Some(tag) = node.child(0) {
                    if let Some(name) = tag.child_by_field_name("name") {
                        if let Ok(text) = name.utf8_text(source) {
                            symbols.push(text.to_string());
                        }
                    }
                }
            }
        }
        SupportedLanguage::Json => {}
        SupportedLanguage::Bash => match kind {
            "function_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            "variable_assignment" => {
                if let Some(name) = node.child_by_field_name("name") {
                    if let Ok(text) = name.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
            _ => {}
        },
        SupportedLanguage::Toml => match kind {
            "table" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "bare_key" || child.kind() == "quoted_key" {
                        if let Ok(text) = child.utf8_text(source) {
                            symbols.push(text.to_string());
                            break;
                        }
                    }
                }
            }
            "pair" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "bare_key" || child.kind() == "quoted_key" {
                        if let Ok(text) = child.utf8_text(source) {
                            symbols.push(text.to_string());
                            break;
                        }
                    }
                }
            }
            _ => {}
        },
        SupportedLanguage::Yaml => {
            if kind == "block_mapping_pair" {
                if let Some(key) = node.child_by_field_name("key") {
                    if let Ok(text) = key.utf8_text(source) {
                        symbols.push(text.to_string());
                    }
                }
            }
        }
        SupportedLanguage::Markdown => {}
    }
}

fn extract_c_declarator_name(node: &Node, source: &[u8], symbols: &mut Vec<String>) {
    match node.kind() {
        "identifier" => {
            if let Ok(text) = node.utf8_text(source) {
                symbols.push(text.to_string());
            }
        }
        "function_declarator" | "pointer_declarator" | "parenthesized_declarator" => {
            if let Some(inner) = node.child_by_field_name("declarator") {
                extract_c_declarator_name(&inner, source, symbols);
            }
        }
        "qualified_identifier" => {
            if let Ok(text) = node.utf8_text(source) {
                symbols.push(text.to_string());
            }
        }
        _ => {
            if let Ok(text) = node.utf8_text(source) {
                symbols.push(text.to_string());
            }
        }
    }
}

fn collect_variable_names(node: &Node, source: &[u8], symbols: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            if let Some(name) = child.child_by_field_name("name") {
                if let Ok(text) = name.utf8_text(source) {
                    symbols.push(text.to_string());
                }
            }
        }
    }
}

fn collect_referenced_symbols(node: &Node, source: &[u8], lang: SupportedLanguage) -> Vec<String> {
    let mut refs = collect_referenced_relations(node, source, lang)
        .into_iter()
        .map(|relation| relation.symbol)
        .collect::<Vec<_>>();
    refs.dedup();
    refs
}

fn collect_called_symbols(node: &Node, source: &[u8], lang: SupportedLanguage) -> Vec<String> {
    let mut calls = Vec::new();
    for relation in collect_called_relations(node, source, lang) {
        if !calls.contains(&relation.symbol) {
            calls.push(relation.symbol);
        }
    }
    calls
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExtractedRelation {
    symbol: String,
    edge_identity_kind: String,
}

impl ExtractedRelation {
    fn new(symbol: String, edge_identity_kind: &str) -> Self {
        Self {
            symbol,
            edge_identity_kind: normalize_edge_identity_kind(edge_identity_kind),
        }
    }
}

fn collect_referenced_relations(
    node: &Node,
    source: &[u8],
    lang: SupportedLanguage,
) -> Vec<ExtractedRelation> {
    let mut refs = Vec::new();
    let defined = collect_defined_symbols(node, source, lang);
    walk_references(node, source, lang, &defined, &mut refs);
    refs.sort_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then(left.edge_identity_kind.cmp(&right.edge_identity_kind))
    });
    refs.dedup();
    refs
}

fn collect_called_relations(
    node: &Node,
    source: &[u8],
    lang: SupportedLanguage,
) -> Vec<ExtractedRelation> {
    let mut calls = Vec::new();
    walk_called_symbols(node, source, lang, &mut calls);
    calls
}

fn walk_references(
    node: &Node,
    source: &[u8],
    lang: SupportedLanguage,
    defined: &[String],
    refs: &mut Vec<ExtractedRelation>,
) {
    let kind = node.kind();

    let is_reference = match lang {
        SupportedLanguage::Rust => {
            matches!(kind, "identifier" | "type_identifier" | "field_identifier")
        }
        SupportedLanguage::Python => kind == "identifier",
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => {
            matches!(
                kind,
                "identifier" | "type_identifier" | "property_identifier"
            )
        }
        SupportedLanguage::Go => kind == "identifier" || kind == "type_identifier",
        SupportedLanguage::Java => kind == "identifier" || kind == "type_identifier",
        SupportedLanguage::C | SupportedLanguage::Cpp => {
            matches!(kind, "identifier" | "type_identifier" | "field_identifier")
        }
        SupportedLanguage::Kotlin => {
            matches!(kind, "simple_identifier" | "type_identifier")
        }
        SupportedLanguage::Bash => kind == "variable_name" || kind == "command_name",
        SupportedLanguage::Css
        | SupportedLanguage::Html
        | SupportedLanguage::Json
        | SupportedLanguage::Toml
        | SupportedLanguage::Yaml
        | SupportedLanguage::Markdown => false,
    };

    if is_reference {
        if let Ok(text) = node.utf8_text(source) {
            let name = text.to_string();
            if !defined.contains(&name) && !is_keyword(lang, &name) && !is_builtin_type(lang, &name)
            {
                refs.push(ExtractedRelation::new(
                    name,
                    &reference_edge_identity_kind(node, source),
                ));
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_references(&child, source, lang, defined, refs);
    }
}

fn walk_called_symbols(
    node: &Node,
    source: &[u8],
    lang: SupportedLanguage,
    calls: &mut Vec<ExtractedRelation>,
) {
    if let Some(call_target) = extract_call_relation(node, source, lang) {
        if !call_target.symbol.is_empty() && !calls.contains(&call_target) {
            calls.push(call_target);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_called_symbols(&child, source, lang, calls);
    }
}

fn extract_call_relation(
    node: &Node,
    source: &[u8],
    lang: SupportedLanguage,
) -> Option<ExtractedRelation> {
    match lang {
        SupportedLanguage::Rust => match node.kind() {
            "call_expression" | "method_call_expression" | "macro_invocation" => {
                extract_rust_call_relation(node, source)
            }
            _ => None,
        },
        SupportedLanguage::Python => match node.kind() {
            "call" => extract_relation_from_fields(node, source, &["function"], None),
            _ => None,
        },
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => match node.kind() {
            "call_expression" => extract_relation_from_fields(node, source, &["function"], None),
            "new_expression" => extract_relation_from_fields(
                node,
                source,
                &["constructor", "function"],
                Some(EDGE_IDENTITY_CONSTRUCTOR_LIKE),
            ),
            _ => None,
        },
        SupportedLanguage::Go => match node.kind() {
            "call_expression" => extract_relation_from_fields(node, source, &["function"], None),
            _ => None,
        },
        SupportedLanguage::Java => match node.kind() {
            "method_invocation" => {
                let name = extract_child_text(node, source, &["name"])?;
                if let Some(object) = extract_child_text(node, source, &["object"]) {
                    Some(ExtractedRelation::new(
                        format!("{object}.{name}"),
                        EDGE_IDENTITY_METHOD_RECEIVER,
                    ))
                } else {
                    Some(ExtractedRelation::new(name, EDGE_IDENTITY_BARE_IDENTIFIER))
                }
            }
            "object_creation_expression" => extract_relation_from_fields(
                node,
                source,
                &["type"],
                Some(EDGE_IDENTITY_CONSTRUCTOR_LIKE),
            ),
            _ => None,
        },
        SupportedLanguage::C | SupportedLanguage::Cpp => match node.kind() {
            "call_expression" => extract_relation_from_fields(node, source, &["function"], None),
            _ => None,
        },
        SupportedLanguage::Kotlin => match node.kind() {
            "call_expression" => extract_relation_from_fields(node, source, &["function"], None),
            _ => None,
        },
        SupportedLanguage::Bash => match node.kind() {
            "command" => extract_relation_from_fields(node, source, &["name"], None),
            _ => None,
        },
        SupportedLanguage::Css
        | SupportedLanguage::Html
        | SupportedLanguage::Json
        | SupportedLanguage::Toml
        | SupportedLanguage::Yaml
        | SupportedLanguage::Markdown => None,
    }
}

fn extract_relation_from_fields(
    node: &Node,
    source: &[u8],
    fields: &[&str],
    edge_identity_kind: Option<&str>,
) -> Option<ExtractedRelation> {
    for field in fields {
        if let Some(child) = node.child_by_field_name(field) {
            if let Ok(text) = child.utf8_text(source) {
                let trimmed = sanitize_call_target(text);
                if !trimmed.is_empty() {
                    let edge_identity_kind = edge_identity_kind
                        .map(str::to_string)
                        .unwrap_or_else(|| relation_edge_identity_kind(node, &child, source));
                    return Some(ExtractedRelation::new(trimmed, &edge_identity_kind));
                }
            }
        }
    }

    None
}

fn extract_child_text(node: &Node, source: &[u8], fields: &[&str]) -> Option<String> {
    for field in fields {
        if let Some(child) = node.child_by_field_name(field) {
            if let Ok(text) = child.utf8_text(source) {
                let trimmed = sanitize_call_target(text);
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }

    None
}

fn extract_last_named_text(node: &Node, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let mut last_text = None;

    for child in node.named_children(&mut cursor) {
        if let Ok(text) = child.utf8_text(source) {
            let trimmed = sanitize_call_target(text);
            if !trimmed.is_empty() {
                last_text = Some(trimmed);
            }
        }
    }

    last_text
}

fn extract_rust_call_relation(node: &Node, source: &[u8]) -> Option<ExtractedRelation> {
    match node.kind() {
        "call_expression" => {
            let function = node.child_by_field_name("function")?;
            let symbol = render_rust_callable(&function, source)?;
            Some(ExtractedRelation::new(
                symbol,
                &relation_edge_identity_kind(node, &function, source),
            ))
        }
        "method_call_expression" => {
            let receiver = node
                .child_by_field_name("receiver")
                .and_then(|receiver| render_rust_callable(&receiver, source))?;
            let method = extract_child_text(node, source, &["method", "name"])
                .or_else(|| extract_last_named_text(node, source))?;
            Some(ExtractedRelation::new(
                format!("{receiver}.{method}"),
                EDGE_IDENTITY_METHOD_RECEIVER,
            ))
        }
        "macro_invocation" => extract_child_text(node, source, &["macro", "name"])
            .map(|symbol| ExtractedRelation::new(symbol, EDGE_IDENTITY_MACRO_LIKE)),
        _ => None,
    }
}

fn render_rust_callable(node: &Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" | "scoped_identifier" => node
            .utf8_text(source)
            .ok()
            .map(sanitize_call_target)
            .filter(|text| !text.is_empty()),
        "field_expression" => {
            let value = node
                .child_by_field_name("value")
                .and_then(|value| render_rust_callable(&value, source))
                .or_else(|| {
                    let mut cursor = node.walk();
                    let first_child = node
                        .named_children(&mut cursor)
                        .next()
                        .and_then(|child| render_rust_callable(&child, source));
                    first_child
                })?;
            let field = node
                .child_by_field_name("field")
                .and_then(|field| field.utf8_text(source).ok())
                .map(sanitize_call_target)
                .filter(|text| !text.is_empty())
                .or_else(|| extract_last_named_text(node, source))?;
            Some(format!("{value}.{field}"))
        }
        "call_expression" => node
            .child_by_field_name("function")
            .and_then(|function| render_rust_callable(&function, source)),
        "method_call_expression" => {
            extract_rust_call_relation(node, source).map(|relation| relation.symbol)
        }
        _ => node
            .utf8_text(source)
            .ok()
            .map(sanitize_call_target)
            .filter(|text| !text.is_empty()),
    }
}

fn reference_edge_identity_kind(node: &Node, source: &[u8]) -> String {
    let mut current = node.parent();
    while let Some(candidate) = current {
        if let Some(edge_identity_kind) = call_context_edge_identity_kind(&candidate, source) {
            return edge_identity_kind;
        }
        current = candidate.parent();
    }

    let mut current = node.parent();
    while let Some(candidate) = current {
        if let Some(edge_identity_kind) = explicit_edge_identity_kind(candidate.kind()) {
            return normalize_edge_identity_kind(edge_identity_kind);
        }
        current = candidate.parent();
    }

    edge_identity_kind_for_node(node, source)
}

fn relation_edge_identity_kind(node: &Node, target: &Node, source: &[u8]) -> String {
    call_context_edge_identity_kind(node, source)
        .unwrap_or_else(|| edge_identity_kind_for_node(target, source))
}

fn call_context_edge_identity_kind(node: &Node, source: &[u8]) -> Option<String> {
    let target = match node.kind() {
        "call_expression" | "call" => node.child_by_field_name("function"),
        "method_call_expression" | "method_invocation" => {
            return Some(normalize_edge_identity_kind(EDGE_IDENTITY_METHOD_RECEIVER));
        }
        _ => None,
    }?;

    let edge_identity_kind = edge_identity_kind_for_node(&target, source);
    Some(if edge_identity_kind == EDGE_IDENTITY_MEMBER_ACCESS {
        normalize_edge_identity_kind(EDGE_IDENTITY_METHOD_RECEIVER)
    } else {
        edge_identity_kind
    })
}

fn edge_identity_kind_for_node(node: &Node, source: &[u8]) -> String {
    if let Some(edge_identity_kind) = explicit_edge_identity_kind(node.kind()) {
        return normalize_edge_identity_kind(edge_identity_kind);
    }

    let text = node.utf8_text(source).unwrap_or("");
    if text.contains("::") {
        return normalize_edge_identity_kind(EDGE_IDENTITY_QUALIFIED_PATH);
    }
    if (text.contains('.') && !text.contains('/')) || text.contains("->") {
        return normalize_edge_identity_kind(EDGE_IDENTITY_MEMBER_ACCESS);
    }

    normalize_edge_identity_kind(EDGE_IDENTITY_BARE_IDENTIFIER)
}

fn explicit_edge_identity_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "scoped_identifier"
        | "qualified_identifier"
        | "qualified_name"
        | "namespace_identifier"
        | "path_expression" => Some(EDGE_IDENTITY_QUALIFIED_PATH),
        "field_expression"
        | "member_expression"
        | "field_access"
        | "navigation_expression"
        | "attribute" => Some(EDGE_IDENTITY_MEMBER_ACCESS),
        "method_call_expression" | "method_invocation" => Some(EDGE_IDENTITY_METHOD_RECEIVER),
        "new_expression" | "object_creation_expression" => Some(EDGE_IDENTITY_CONSTRUCTOR_LIKE),
        "macro_invocation" => Some(EDGE_IDENTITY_MACRO_LIKE),
        _ => None,
    }
}

fn sanitize_call_target(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" .", ".")
        .replace(". ", ".")
        .replace(" ::", "::")
        .replace(":: ", "::")
        .replace("( ", "(")
        .replace(" )", ")")
}

fn is_keyword(lang: SupportedLanguage, name: &str) -> bool {
    match lang {
        SupportedLanguage::Rust => matches!(
            name,
            "self"
                | "Self"
                | "super"
                | "crate"
                | "pub"
                | "fn"
                | "let"
                | "mut"
                | "const"
                | "static"
                | "if"
                | "else"
                | "match"
                | "for"
                | "while"
                | "loop"
                | "return"
                | "break"
                | "continue"
                | "struct"
                | "enum"
                | "trait"
                | "impl"
                | "type"
                | "where"
                | "use"
                | "mod"
                | "as"
                | "in"
                | "ref"
                | "true"
                | "false"
                | "async"
                | "await"
                | "move"
                | "dyn"
                | "unsafe"
        ),
        SupportedLanguage::Python => matches!(
            name,
            "self"
                | "cls"
                | "None"
                | "True"
                | "False"
                | "def"
                | "class"
                | "if"
                | "else"
                | "elif"
                | "for"
                | "while"
                | "return"
                | "pass"
                | "break"
                | "continue"
                | "import"
                | "from"
                | "as"
                | "try"
                | "except"
                | "finally"
                | "raise"
                | "with"
                | "yield"
                | "lambda"
                | "and"
                | "or"
                | "not"
                | "in"
                | "is"
                | "global"
                | "nonlocal"
                | "assert"
                | "del"
        ),
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => matches!(
            name,
            "this"
                | "super"
                | "undefined"
                | "null"
                | "true"
                | "false"
                | "var"
                | "let"
                | "const"
                | "function"
                | "class"
                | "if"
                | "else"
                | "for"
                | "while"
                | "do"
                | "switch"
                | "case"
                | "break"
                | "continue"
                | "return"
                | "throw"
                | "try"
                | "catch"
                | "finally"
                | "new"
                | "delete"
                | "typeof"
                | "instanceof"
                | "void"
                | "in"
                | "of"
                | "import"
                | "export"
                | "default"
                | "async"
                | "await"
                | "yield"
        ),
        SupportedLanguage::Go => matches!(
            name,
            "nil"
                | "true"
                | "false"
                | "iota"
                | "func"
                | "var"
                | "const"
                | "type"
                | "struct"
                | "interface"
                | "map"
                | "chan"
                | "if"
                | "else"
                | "for"
                | "range"
                | "switch"
                | "case"
                | "default"
                | "return"
                | "break"
                | "continue"
                | "goto"
                | "go"
                | "defer"
                | "select"
                | "package"
                | "import"
                | "fallthrough"
        ),
        SupportedLanguage::Java => matches!(
            name,
            "this"
                | "super"
                | "null"
                | "true"
                | "false"
                | "void"
                | "class"
                | "interface"
                | "enum"
                | "extends"
                | "implements"
                | "if"
                | "else"
                | "for"
                | "while"
                | "do"
                | "switch"
                | "case"
                | "break"
                | "continue"
                | "return"
                | "throw"
                | "try"
                | "catch"
                | "finally"
                | "new"
                | "instanceof"
                | "import"
                | "package"
                | "public"
                | "private"
                | "protected"
                | "static"
                | "final"
                | "abstract"
                | "synchronized"
                | "volatile"
                | "transient"
                | "native"
        ),
        SupportedLanguage::C | SupportedLanguage::Cpp => matches!(
            name,
            "NULL"
                | "void"
                | "if"
                | "else"
                | "for"
                | "while"
                | "do"
                | "switch"
                | "case"
                | "break"
                | "continue"
                | "return"
                | "sizeof"
                | "typedef"
                | "struct"
                | "enum"
                | "union"
                | "static"
                | "extern"
                | "const"
                | "volatile"
                | "register"
                | "auto"
                | "goto"
                | "default"
                | "inline"
                | "this"
                | "class"
                | "namespace"
                | "template"
                | "virtual"
                | "override"
                | "public"
                | "private"
                | "protected"
                | "new"
                | "delete"
                | "throw"
                | "try"
                | "catch"
                | "true"
                | "false"
                | "nullptr"
        ),
        SupportedLanguage::Kotlin => matches!(
            name,
            "this"
                | "super"
                | "null"
                | "true"
                | "false"
                | "fun"
                | "val"
                | "var"
                | "class"
                | "object"
                | "interface"
                | "if"
                | "else"
                | "when"
                | "for"
                | "while"
                | "do"
                | "return"
                | "break"
                | "continue"
                | "throw"
                | "try"
                | "catch"
                | "finally"
                | "import"
                | "package"
                | "is"
                | "as"
                | "in"
                | "typealias"
                | "companion"
                | "data"
                | "sealed"
                | "enum"
                | "abstract"
                | "open"
                | "override"
                | "private"
                | "protected"
                | "public"
                | "internal"
                | "suspend"
                | "inline"
                | "operator"
                | "infix"
                | "lateinit"
                | "by"
                | "it"
        ),
        SupportedLanguage::Bash => matches!(
            name,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "for"
                | "while"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "in"
                | "function"
                | "return"
                | "exit"
                | "local"
                | "export"
                | "readonly"
                | "declare"
                | "unset"
                | "shift"
                | "true"
                | "false"
        ),
        SupportedLanguage::Css
        | SupportedLanguage::Html
        | SupportedLanguage::Json
        | SupportedLanguage::Toml
        | SupportedLanguage::Yaml
        | SupportedLanguage::Markdown => false,
    }
}

fn is_builtin_type(lang: SupportedLanguage, name: &str) -> bool {
    match lang {
        SupportedLanguage::Rust => matches!(
            name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "f32"
                | "f64"
                | "bool"
                | "char"
                | "str"
                | "String"
                | "Vec"
                | "Option"
                | "Result"
                | "Box"
                | "Rc"
                | "Arc"
        ),
        SupportedLanguage::Python => matches!(
            name,
            "int"
                | "float"
                | "str"
                | "bool"
                | "list"
                | "dict"
                | "set"
                | "tuple"
                | "bytes"
                | "type"
                | "object"
                | "range"
                | "print"
                | "len"
                | "enumerate"
                | "zip"
                | "map"
                | "filter"
                | "sorted"
                | "reversed"
                | "any"
                | "all"
                | "sum"
                | "min"
                | "max"
                | "abs"
                | "round"
                | "isinstance"
                | "issubclass"
                | "hasattr"
                | "getattr"
                | "setattr"
                | "super"
                | "property"
                | "staticmethod"
                | "classmethod"
                | "Exception"
        ),
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => matches!(
            name,
            "string"
                | "number"
                | "boolean"
                | "object"
                | "symbol"
                | "bigint"
                | "any"
                | "void"
                | "never"
                | "unknown"
                | "undefined"
                | "null"
                | "Array"
                | "Object"
                | "String"
                | "Number"
                | "Boolean"
                | "Map"
                | "Set"
                | "Promise"
                | "Date"
                | "RegExp"
                | "Error"
                | "console"
                | "Math"
                | "JSON"
                | "parseInt"
                | "parseFloat"
                | "isNaN"
                | "isFinite"
                | "Infinity"
                | "NaN"
                | "globalThis"
                | "window"
                | "document"
                | "setTimeout"
                | "setInterval"
                | "clearTimeout"
                | "clearInterval"
                | "require"
                | "module"
                | "exports"
                | "process"
        ),
        SupportedLanguage::Go => matches!(
            name,
            "int"
                | "int8"
                | "int16"
                | "int32"
                | "int64"
                | "uint"
                | "uint8"
                | "uint16"
                | "uint32"
                | "uint64"
                | "uintptr"
                | "float32"
                | "float64"
                | "complex64"
                | "complex128"
                | "bool"
                | "string"
                | "byte"
                | "rune"
                | "error"
                | "any"
                | "comparable"
                | "make"
                | "len"
                | "cap"
                | "append"
                | "copy"
                | "close"
                | "delete"
                | "new"
                | "panic"
                | "recover"
                | "print"
                | "println"
                | "fmt"
        ),
        SupportedLanguage::Java => matches!(
            name,
            "int"
                | "long"
                | "short"
                | "byte"
                | "float"
                | "double"
                | "boolean"
                | "char"
                | "String"
                | "Integer"
                | "Long"
                | "Short"
                | "Byte"
                | "Float"
                | "Double"
                | "Boolean"
                | "Character"
                | "Object"
                | "System"
                | "Math"
                | "Override"
                | "Deprecated"
                | "SuppressWarnings"
                | "FunctionalInterface"
        ),
        SupportedLanguage::C | SupportedLanguage::Cpp => matches!(
            name,
            "int"
                | "long"
                | "short"
                | "char"
                | "float"
                | "double"
                | "unsigned"
                | "signed"
                | "size_t"
                | "ssize_t"
                | "ptrdiff_t"
                | "int8_t"
                | "int16_t"
                | "int32_t"
                | "int64_t"
                | "uint8_t"
                | "uint16_t"
                | "uint32_t"
                | "uint64_t"
                | "bool"
                | "string"
                | "vector"
                | "map"
                | "set"
                | "list"
                | "deque"
                | "queue"
                | "stack"
                | "pair"
                | "tuple"
                | "shared_ptr"
                | "unique_ptr"
                | "weak_ptr"
                | "optional"
                | "variant"
                | "any"
                | "cout"
                | "cerr"
                | "cin"
                | "endl"
                | "std"
        ),
        SupportedLanguage::Kotlin => matches!(
            name,
            "Int"
                | "Long"
                | "Short"
                | "Byte"
                | "Float"
                | "Double"
                | "Boolean"
                | "Char"
                | "String"
                | "Unit"
                | "Nothing"
                | "Any"
                | "Array"
                | "List"
                | "Map"
                | "Set"
                | "MutableList"
                | "MutableMap"
                | "MutableSet"
                | "Pair"
                | "Triple"
                | "Sequence"
                | "Comparable"
                | "Iterable"
                | "Collection"
                | "println"
                | "print"
                | "require"
                | "check"
                | "error"
                | "TODO"
        ),
        SupportedLanguage::Css
        | SupportedLanguage::Html
        | SupportedLanguage::Json
        | SupportedLanguage::Bash
        | SupportedLanguage::Toml
        | SupportedLanguage::Yaml
        | SupportedLanguage::Markdown => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::{Language, Parser as TsParser, Tree};

    fn parse_tree(source: &str, lang: SupportedLanguage) -> Tree {
        let ts_language = Language::new(lang.language_fn());
        let mut parser = TsParser::new();
        parser.set_language(&ts_language).unwrap();
        parser.parse(source, None).unwrap()
    }

    fn find_first_named_node<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(found) = find_first_named_node(child, kind) {
                return Some(found);
            }
        }

        None
    }

    #[test]
    fn test_parse_rust_functions_and_structs() {
        let source = r#"
/// A point in 2D space.
struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    fn distance(&self, other: &Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}

fn main() {
    let p1 = Point::new(1.0, 2.0);
    let p2 = Point::new(4.0, 6.0);
    println!("{}", p1.distance(&p2));
}
"#;
        let segments = parse_file(source, "rust").unwrap();

        let struct_seg = segments.iter().find(|s| s.block_type == "struct").unwrap();
        assert!(struct_seg.defined_symbols.contains(&"Point".to_string()));
        assert_eq!(struct_seg.role, SegmentRole::Definition);

        let impl_seg = segments.iter().find(|s| s.block_type == "impl").unwrap();
        assert!(impl_seg.defined_symbols.contains(&"Point".to_string()));
        assert_eq!(impl_seg.role, SegmentRole::Implementation);

        let nested_fns: Vec<_> = segments
            .iter()
            .filter(|s| s.breadcrumb.is_some() && s.block_type == "function")
            .collect();
        assert_eq!(nested_fns.len(), 2);
        assert!(nested_fns
            .iter()
            .any(|s| s.defined_symbols.contains(&"new".to_string())));
        assert!(nested_fns
            .iter()
            .any(|s| s.defined_symbols.contains(&"distance".to_string())));

        let main_seg = segments
            .iter()
            .find(|s| s.block_type == "function" && s.breadcrumb.is_none())
            .unwrap();
        assert!(main_seg.defined_symbols.contains(&"main".to_string()));
        assert_eq!(main_seg.role, SegmentRole::Implementation);
    }

    #[test]
    fn test_parse_rust_complexity() {
        let source = r#"
fn complex(x: i32) -> i32 {
    if x > 0 {
        for i in 0..x {
            if i % 2 == 0 {
                match i {
                    0 => return 0,
                    _ => continue,
                }
            }
        }
    }
    x
}
"#;
        let segments = parse_file(source, "rust").unwrap();
        let func = segments
            .iter()
            .find(|s| s.block_type == "function")
            .unwrap();
        assert!(
            func.complexity >= 3,
            "expected complexity >= 3, got {}",
            func.complexity
        );
    }

    #[test]
    fn test_parse_python_class_and_methods() {
        let source = r#"
import os
from pathlib import Path

class Calculator:
    """A simple calculator."""

    def __init__(self, value=0):
        self.value = value

    def add(self, x):
        self.value += x
        return self

    def multiply(self, x):
        if x == 0:
            return self
        self.value *= x
        return self
"#;
        let segments = parse_file(source, "python").unwrap();

        let imports: Vec<_> = segments
            .iter()
            .filter(|s| s.block_type == "import")
            .collect();
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].role, SegmentRole::Import);

        let class_seg = segments.iter().find(|s| s.block_type == "class").unwrap();
        assert!(class_seg
            .defined_symbols
            .contains(&"Calculator".to_string()));
        assert_eq!(class_seg.role, SegmentRole::Definition);

        let methods: Vec<_> = segments
            .iter()
            .filter(|s| s.breadcrumb.as_deref() == Some("Calculator"))
            .collect();
        assert_eq!(methods.len(), 3);
    }

    #[test]
    fn test_parse_typescript_exports() {
        let source = r#"
import { useState } from 'react';

export function greet(name: string): string {
    return `Hello, ${name}!`;
}

export class Greeter {
    name: string;

    constructor(name: string) {
        this.name = name;
    }

    greet(): string {
        return `Hello, ${this.name}!`;
    }
}

export interface Config {
    debug: boolean;
    port: number;
}

export type Status = 'active' | 'inactive';
"#;
        let segments = parse_file(source, "typescript").unwrap();

        let import_seg = segments.iter().find(|s| s.block_type == "import").unwrap();
        assert_eq!(import_seg.role, SegmentRole::Import);

        let func_seg = segments
            .iter()
            .find(|s| {
                s.block_type == "function"
                    && s.defined_symbols.contains(&"greet".to_string())
                    && s.breadcrumb.is_none()
            })
            .unwrap();
        assert_eq!(func_seg.role, SegmentRole::Implementation);

        let class_seg = segments.iter().find(|s| s.block_type == "class").unwrap();
        assert!(class_seg.defined_symbols.contains(&"Greeter".to_string()));

        let iface_seg = segments
            .iter()
            .find(|s| s.block_type == "interface")
            .unwrap();
        assert!(iface_seg.defined_symbols.contains(&"Config".to_string()));

        let type_seg = segments.iter().find(|s| s.block_type == "type").unwrap();
        assert!(type_seg.defined_symbols.contains(&"Status".to_string()));
    }

    #[test]
    fn test_parse_go_functions_and_types() {
        let source = r#"
package main

import "fmt"

type Point struct {
	X float64
	Y float64
}

func NewPoint(x, y float64) Point {
	return Point{X: x, Y: y}
}

func (p Point) Distance(other Point) float64 {
	dx := p.X - other.X
	dy := p.Y - other.Y
	return dx*dx + dy*dy
}
"#;
        let segments = parse_file(source, "go").unwrap();

        let type_seg = segments.iter().find(|s| s.block_type == "type").unwrap();
        assert!(type_seg.defined_symbols.contains(&"Point".to_string()));

        let funcs: Vec<_> = segments
            .iter()
            .filter(|s| s.block_type == "function")
            .collect();
        assert_eq!(funcs.len(), 2);
        assert!(funcs
            .iter()
            .any(|f| f.defined_symbols.contains(&"NewPoint".to_string())));
        assert!(funcs
            .iter()
            .any(|f| f.defined_symbols.contains(&"Distance".to_string())));
    }

    #[test]
    fn test_parse_java_class() {
        let source = r#"
import java.util.List;

public class Calculator {
    private int value;

    public Calculator(int initial) {
        this.value = initial;
    }

    public int add(int x) {
        this.value += x;
        return this.value;
    }
}
"#;
        let segments = parse_file(source, "java").unwrap();

        let import_seg = segments.iter().find(|s| s.block_type == "import").unwrap();
        assert_eq!(import_seg.role, SegmentRole::Import);

        let class_seg = segments.iter().find(|s| s.block_type == "class").unwrap();
        assert!(class_seg
            .defined_symbols
            .contains(&"Calculator".to_string()));

        let methods: Vec<_> = segments
            .iter()
            .filter(|s| s.breadcrumb.as_deref() == Some("Calculator"))
            .collect();
        assert!(methods.len() >= 2);
    }

    #[test]
    fn test_parse_c_functions() {
        let source = r#"
#include <stdio.h>

struct Point {
    double x;
    double y;
};

double distance(struct Point a, struct Point b) {
    double dx = a.x - b.x;
    double dy = a.y - b.y;
    return dx * dx + dy * dy;
}

int main() {
    struct Point p1 = {1.0, 2.0};
    struct Point p2 = {4.0, 6.0};
    printf("%f\n", distance(p1, p2));
    return 0;
}
"#;
        let segments = parse_file(source, "c").unwrap();

        let include_seg = segments.iter().find(|s| s.block_type == "import").unwrap();
        assert_eq!(include_seg.role, SegmentRole::Import);

        let struct_seg = segments.iter().find(|s| s.block_type == "struct").unwrap();
        assert!(struct_seg.defined_symbols.contains(&"Point".to_string()));

        let funcs: Vec<_> = segments
            .iter()
            .filter(|s| s.block_type == "function")
            .collect();
        assert_eq!(funcs.len(), 2);
        assert!(funcs
            .iter()
            .any(|f| f.defined_symbols.contains(&"distance".to_string())));
        assert!(funcs
            .iter()
            .any(|f| f.defined_symbols.contains(&"main".to_string())));
    }

    #[test]
    fn test_language_from_extension() {
        assert_eq!(
            SupportedLanguage::from_extension("rs"),
            Some(SupportedLanguage::Rust)
        );
        assert_eq!(
            SupportedLanguage::from_extension("py"),
            Some(SupportedLanguage::Python)
        );
        assert_eq!(
            SupportedLanguage::from_extension("ts"),
            Some(SupportedLanguage::TypeScript)
        );
        assert_eq!(
            SupportedLanguage::from_extension("tsx"),
            Some(SupportedLanguage::TypeScript)
        );
        assert_eq!(SupportedLanguage::from_extension("xyz"), None);
    }

    #[test]
    fn test_text_document_extensions_use_chunking() {
        for ext in [
            "proto",
            "properties",
            "conf",
            "ini",
            "tf",
            "hcl",
            "sql",
            "sq",
            "sqm",
            "dockerfile",
            "makefile",
            "justfile",
        ] {
            assert!(is_language_supported(ext), "{ext} should be supported");
            assert!(
                !use_structural_parser(ext),
                "{ext} should use text chunking"
            );
        }
    }

    #[test]
    fn test_unsupported_language_error() {
        let result = parse_file("some code", "brainfuck");
        assert!(result.is_err());
        match result.unwrap_err() {
            ParserError::UnsupportedLanguage(lang) => assert_eq!(lang, "brainfuck"),
            other => panic!("expected UnsupportedLanguage, got {other:?}"),
        }
    }

    #[test]
    fn test_referenced_symbols() {
        let source = r#"
fn process(data: Vec<Item>) -> Result<Output, Error> {
    let config = Config::load();
    let processor = Processor::new(config);
    processor.run(data)
}
"#;
        let segments = parse_file(source, "rust").unwrap();
        let func = segments
            .iter()
            .find(|s| s.block_type == "function")
            .unwrap();
        assert!(func.referenced_symbols.contains(&"Item".to_string()));
        assert!(func.referenced_symbols.contains(&"Output".to_string()));
        assert!(func.referenced_symbols.contains(&"Error".to_string()));
        assert!(func.referenced_symbols.contains(&"Config".to_string()));
        assert!(func.referenced_symbols.contains(&"Processor".to_string()));
        assert!(!func.referenced_symbols.contains(&"process".to_string()));
    }

    #[test]
    fn test_called_relations_assign_edge_identity_kinds() {
        let source = r#"
fn process(config: Config, worker: Worker) {
    load_config();
    crate::auth::config::load_config();
    worker.run();
    tracing::info!("ready");
}
"#;
        let tree = parse_tree(source, SupportedLanguage::Rust);
        let function = find_first_named_node(tree.root_node(), "function_item").unwrap();
        let relations =
            collect_called_relations(&function, source.as_bytes(), SupportedLanguage::Rust);

        assert!(relations.contains(&ExtractedRelation::new(
            "load_config".to_string(),
            EDGE_IDENTITY_BARE_IDENTIFIER,
        )));
        assert!(relations.contains(&ExtractedRelation::new(
            "crate::auth::config::load_config".to_string(),
            EDGE_IDENTITY_QUALIFIED_PATH,
        )));
        assert!(relations.contains(&ExtractedRelation::new(
            "worker.run".to_string(),
            EDGE_IDENTITY_METHOD_RECEIVER,
        )));
        assert!(relations.contains(&ExtractedRelation::new(
            "tracing::info".to_string(),
            EDGE_IDENTITY_MACRO_LIKE,
        )));
    }

    #[test]
    fn test_constructor_and_member_call_relations_use_normalized_edge_identity() {
        let source = r#"
function build(service) {
    new WidgetFactory();
    service.client.fetch();
    run();
}
"#;
        let tree = parse_tree(source, SupportedLanguage::JavaScript);
        let function = find_first_named_node(tree.root_node(), "function_declaration").unwrap();
        let relations =
            collect_called_relations(&function, source.as_bytes(), SupportedLanguage::JavaScript);

        assert!(relations.contains(&ExtractedRelation::new(
            "WidgetFactory".to_string(),
            EDGE_IDENTITY_CONSTRUCTOR_LIKE,
        )));
        assert!(relations.contains(&ExtractedRelation::new(
            "service.client.fetch".to_string(),
            EDGE_IDENTITY_METHOD_RECEIVER,
        )));
        assert!(relations.contains(&ExtractedRelation::new(
            "run".to_string(),
            EDGE_IDENTITY_BARE_IDENTIFIER,
        )));
    }

    #[test]
    fn test_referenced_relations_assign_edge_identity_kinds() {
        let source = r#"
fn process(worker: Worker) {
    let loader = crate::auth::Config::load();
    worker.run();
}
"#;
        let tree = parse_tree(source, SupportedLanguage::Rust);
        let function = find_first_named_node(tree.root_node(), "function_item").unwrap();
        let relations =
            collect_referenced_relations(&function, source.as_bytes(), SupportedLanguage::Rust);

        assert!(relations.contains(&ExtractedRelation::new(
            "Config".to_string(),
            EDGE_IDENTITY_QUALIFIED_PATH,
        )));
        assert!(relations.contains(&ExtractedRelation::new(
            "worker".to_string(),
            EDGE_IDENTITY_METHOD_RECEIVER,
        )));
        assert!(relations.contains(&ExtractedRelation::new(
            "run".to_string(),
            EDGE_IDENTITY_METHOD_RECEIVER,
        )));
    }

    #[test]
    fn test_leading_comment_collection() {
        let source = r#"
/// Adds two numbers together.
/// Returns the sum.
fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;
        let segments = parse_file(source, "rust").unwrap();
        let func = segments
            .iter()
            .find(|s| s.block_type == "function")
            .unwrap();
        assert!(func.content.contains("Adds two numbers together"));
        assert!(func.content.contains("Returns the sum"));
    }

    #[test]
    fn test_role_classification() {
        let source = r#"
use std::io;

struct Config {
    debug: bool,
}

trait Validator {
    fn validate(&self) -> bool;
}

impl Validator for Config {
    fn validate(&self) -> bool {
        true
    }
}

fn main() {
    let c = Config { debug: true };
    c.validate();
}
"#;
        let segments = parse_file(source, "rust").unwrap();

        let import = segments.iter().find(|s| s.block_type == "import").unwrap();
        assert_eq!(import.role, SegmentRole::Import);

        let struct_seg = segments.iter().find(|s| s.block_type == "struct").unwrap();
        assert_eq!(struct_seg.role, SegmentRole::Definition);

        let trait_seg = segments.iter().find(|s| s.block_type == "trait").unwrap();
        assert_eq!(trait_seg.role, SegmentRole::Definition);

        let impl_seg = segments.iter().find(|s| s.block_type == "impl").unwrap();
        assert_eq!(impl_seg.role, SegmentRole::Implementation);
    }
}
