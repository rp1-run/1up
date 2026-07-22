use std::path::Path;

use tree_sitter::{Language, Node, Parser};

use crate::indexer::parser::SupportedLanguage;
use crate::shared::constants::{
    CONTEXT_FALLBACK_LINES, MAX_CONTEXT_EXPANSION_LINES, MAX_WHOLE_SCOPE_LINES,
};
use crate::shared::types::{ContextAccessScope, ContextResult};

pub struct ContextEngine;

impl ContextEngine {
    pub fn retrieve(
        file_path: &Path,
        target_line: usize,
        expansion: Option<usize>,
    ) -> anyhow::Result<ContextResult> {
        Self::retrieve_with_scope(
            file_path,
            target_line,
            expansion,
            ContextAccessScope::ProjectRoot,
        )
    }

    pub fn retrieve_with_scope(
        file_path: &Path,
        target_line: usize,
        expansion: Option<usize>,
        access_scope: ContextAccessScope,
    ) -> anyhow::Result<ContextResult> {
        let source = std::fs::read_to_string(file_path)?;
        let total_lines = source.lines().count();

        if target_line == 0 || target_line > total_lines {
            anyhow::bail!("line {target_line} is out of range (file has {total_lines} lines)");
        }

        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

        let language = SupportedLanguage::from_extension(ext);

        match language {
            Some(lang) => match find_enclosing_scope(&source, lang, target_line) {
                Some(scope) => Ok(ContextResult {
                    file_path: file_path.display().to_string(),
                    language: lang.name().to_string(),
                    content: scope.content,
                    line_start: scope.line_start,
                    line_end: scope.line_end,
                    scope_type: scope.scope_type,
                    access_scope: Some(access_scope),
                }),
                None => Ok(line_range_fallback(
                    &source,
                    file_path,
                    target_line,
                    total_lines,
                    expansion.unwrap_or(CONTEXT_FALLBACK_LINES),
                    lang.name(),
                    access_scope,
                )),
            },
            None => {
                let lang_name = if ext.is_empty() { "unknown" } else { ext };
                Ok(line_range_fallback(
                    &source,
                    file_path,
                    target_line,
                    total_lines,
                    expansion.unwrap_or(CONTEXT_FALLBACK_LINES),
                    lang_name,
                    access_scope,
                ))
            }
        }
    }

    /// Retrieve file-line context as a scope-size-aware [`ScopeWindow`].
    ///
    /// When the smallest enclosing scope spans `<= MAX_WHOLE_SCOPE_LINES` it is
    /// returned whole (`clipped = false`) regardless of `expansion`, preserving
    /// legacy whole-scope fidelity for the common small-scope case. Larger scopes
    /// are windowed to `target_line ± expansion.unwrap_or(CONTEXT_FALLBACK_LINES)`
    /// (clamped to `MAX_CONTEXT_EXPANSION_LINES` each side) intersected with the
    /// scope, with `clipped = true` and the full scope range surfaced so callers
    /// can build a recoverable truncation note. Files with no enclosing scope fall
    /// back to a bounded line window (`clipped = false`).
    pub fn retrieve_scope_window(
        file_path: &Path,
        target_line: usize,
        expansion: Option<usize>,
    ) -> anyhow::Result<ScopeWindow> {
        let source = std::fs::read_to_string(file_path)?;
        let total_lines = source.lines().count();

        if target_line == 0 || target_line > total_lines {
            anyhow::bail!("line {target_line} is out of range (file has {total_lines} lines)");
        }

        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language = SupportedLanguage::from_extension(ext);

        match language.and_then(|lang| find_enclosing_scope(&source, lang, target_line)) {
            Some(scope) => {
                let bounds = bound_scope_window(
                    (scope.line_start, scope.line_end),
                    target_line,
                    expansion,
                    MAX_WHOLE_SCOPE_LINES,
                    MAX_CONTEXT_EXPANSION_LINES,
                );

                let lines: Vec<&str> = source.lines().collect();
                let content = lines[bounds.line_start - 1..bounds.line_end].join("\n");

                if bounds.clipped {
                    tracing::debug!(
                        scope_name = scope.scope_name.as_deref().unwrap_or("<anonymous>"),
                        scope_type = scope.scope_type.as_str(),
                        scope_line_start = scope.line_start,
                        scope_line_end = scope.line_end,
                        window_line_start = bounds.line_start,
                        window_line_end = bounds.line_end,
                        omitted_above = bounds.line_start - scope.line_start,
                        omitted_below = scope.line_end - bounds.line_end,
                        "oneup_context windowed enclosing scope"
                    );
                }

                Ok(ScopeWindow {
                    content,
                    line_start: bounds.line_start,
                    line_end: bounds.line_end,
                    scope_line_start: scope.line_start,
                    scope_line_end: scope.line_end,
                    scope_type: scope.scope_type,
                    scope_name: scope.scope_name,
                    clipped: bounds.clipped,
                })
            }
            None => {
                let lang_name: String = match language {
                    Some(lang) => lang.name().to_string(),
                    None if ext.is_empty() => "unknown".to_string(),
                    None => ext.to_string(),
                };
                let fallback = line_range_fallback(
                    &source,
                    file_path,
                    target_line,
                    total_lines,
                    expansion.unwrap_or(CONTEXT_FALLBACK_LINES),
                    &lang_name,
                    ContextAccessScope::ProjectRoot,
                );
                Ok(ScopeWindow {
                    content: fallback.content,
                    line_start: fallback.line_start,
                    line_end: fallback.line_end,
                    scope_line_start: fallback.line_start,
                    scope_line_end: fallback.line_end,
                    scope_type: fallback.scope_type,
                    scope_name: None,
                    clipped: false,
                })
            }
        }
    }
}

/// Scope-size-aware context window returned by
/// [`ContextEngine::retrieve_scope_window`].
///
/// Carries the (possibly clipped) window content and range, the full enclosing
/// scope range, and enough metadata for the MCP layer to render a load-bearing
/// truncation note with a recovery call. `clipped` is `true` only when the
/// returned window is a strict subset of the enclosing scope.
pub struct ScopeWindow {
    pub content: String,
    pub line_start: usize,
    pub line_end: usize,
    pub scope_line_start: usize,
    pub scope_line_end: usize,
    pub scope_type: String,
    pub scope_name: Option<String>,
    pub clipped: bool,
}

/// Pure line-geometry result of [`bound_scope_window`].
struct WindowBounds {
    line_start: usize,
    line_end: usize,
    clipped: bool,
}

/// Compute the bounded line window for an enclosing scope (pure, 1-based inclusive).
///
/// Scopes spanning `<= whole_scope_threshold` lines are returned whole with
/// `clipped = false`, regardless of `expansion` — so a small explicit expansion
/// never shrinks a small scope. Larger scopes are windowed to
/// `target_line ± expansion.unwrap_or(CONTEXT_FALLBACK_LINES)` (each side clamped
/// to `ceiling`) intersected with the scope; `clipped` is `true` iff the window
/// is a strict subset of the scope. `target_line` is assumed to lie within
/// `scope_range`.
fn bound_scope_window(
    scope_range: (usize, usize),
    target_line: usize,
    expansion: Option<usize>,
    whole_scope_threshold: usize,
    ceiling: usize,
) -> WindowBounds {
    let (scope_start, scope_end) = scope_range;
    let scope_span = scope_end.saturating_sub(scope_start) + 1;

    if scope_span <= whole_scope_threshold {
        return WindowBounds {
            line_start: scope_start,
            line_end: scope_end,
            clipped: false,
        };
    }

    let reach = expansion.unwrap_or(CONTEXT_FALLBACK_LINES).min(ceiling);
    let line_start = target_line.saturating_sub(reach).max(scope_start);
    let line_end = target_line.saturating_add(reach).min(scope_end);
    let clipped = line_start > scope_start || line_end < scope_end;

    WindowBounds {
        line_start,
        line_end,
        clipped,
    }
}

struct ScopeHit {
    content: String,
    line_start: usize,
    line_end: usize,
    scope_type: String,
    scope_name: Option<String>,
}

const SCOPE_NODE_KINDS: &[&[&str]] = &[
    // Rust
    &[
        "function_item",
        "struct_item",
        "enum_item",
        "trait_item",
        "impl_item",
        "mod_item",
        "macro_definition",
    ],
    // Python
    &[
        "function_definition",
        "class_definition",
        "decorated_definition",
    ],
    // JavaScript / TypeScript
    &[
        "function_declaration",
        "class_declaration",
        "method_definition",
        "arrow_function",
        "export_statement",
    ],
    // Go
    &[
        "function_declaration",
        "method_declaration",
        "type_declaration",
    ],
    // Java
    &[
        "class_declaration",
        "interface_declaration",
        "enum_declaration",
        "method_declaration",
        "constructor_declaration",
    ],
    // C
    &["function_definition", "struct_specifier", "enum_specifier"],
    // C++
    &[
        "function_definition",
        "class_specifier",
        "struct_specifier",
        "namespace_definition",
    ],
    // Kotlin
    &[
        "function_declaration",
        "class_declaration",
        "object_declaration",
        "companion_object",
    ],
    // CSS
    &["rule_set", "media_statement", "keyframes_statement"],
    // HTML
    &["element", "script_element", "style_element"],
    // JSON
    &["object", "array"],
    // Bash
    &[
        "function_definition",
        "if_statement",
        "for_statement",
        "case_statement",
    ],
    // TOML
    &["table"],
    // YAML
    &["block_mapping_pair"],
    // Markdown
    &["section"],
];

fn scope_kinds_for(lang: SupportedLanguage) -> &'static [&'static str] {
    match lang {
        SupportedLanguage::Rust => SCOPE_NODE_KINDS[0],
        SupportedLanguage::Python => SCOPE_NODE_KINDS[1],
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => SCOPE_NODE_KINDS[2],
        SupportedLanguage::Go => SCOPE_NODE_KINDS[3],
        SupportedLanguage::Java => SCOPE_NODE_KINDS[4],
        SupportedLanguage::C => SCOPE_NODE_KINDS[5],
        SupportedLanguage::Cpp => SCOPE_NODE_KINDS[6],
        SupportedLanguage::Kotlin => SCOPE_NODE_KINDS[7],
        SupportedLanguage::Css => SCOPE_NODE_KINDS[8],
        SupportedLanguage::Html => SCOPE_NODE_KINDS[9],
        SupportedLanguage::Json => SCOPE_NODE_KINDS[10],
        SupportedLanguage::Bash => SCOPE_NODE_KINDS[11],
        SupportedLanguage::Toml => SCOPE_NODE_KINDS[12],
        SupportedLanguage::Yaml => SCOPE_NODE_KINDS[13],
        SupportedLanguage::Markdown => SCOPE_NODE_KINDS[14],
    }
}

fn find_enclosing_scope(
    source: &str,
    lang: SupportedLanguage,
    target_line: usize,
) -> Option<ScopeHit> {
    let ts_language = Language::new(lang.language_fn());
    let mut parser = Parser::new();
    parser.set_language(&ts_language).ok()?;

    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let source_bytes = source.as_bytes();

    let target_row = target_line - 1;
    let scope_kinds = scope_kinds_for(lang);

    let mut best: Option<Node> = None;

    find_smallest_enclosing(&root, target_row, scope_kinds, &mut best);

    best.map(|node| {
        let content = node.utf8_text(source_bytes).unwrap_or("").to_string();
        let line_start = node.start_position().row + 1;
        let line_end = node.end_position().row + 1;
        let scope_type = classify_scope_type(node.kind(), lang);
        let scope_name = node
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(source_bytes).ok())
            .map(|name| name.to_string());

        ScopeHit {
            content,
            line_start,
            line_end,
            scope_type,
            scope_name,
        }
    })
}

fn find_smallest_enclosing<'a>(
    node: &Node<'a>,
    target_row: usize,
    scope_kinds: &[&str],
    best: &mut Option<Node<'a>>,
) {
    let start = node.start_position().row;
    let end = node.end_position().row;

    if target_row < start || target_row > end {
        return;
    }

    if scope_kinds.contains(&node.kind()) {
        match best {
            Some(current) => {
                let current_span = current.end_position().row - current.start_position().row;
                let new_span = end - start;
                if new_span < current_span {
                    *best = Some(*node);
                }
            }
            None => {
                *best = Some(*node);
            }
        }
    }

    let child_count = node.child_count();
    for i in 0..child_count {
        if let Some(child) = node.child(i as u32) {
            find_smallest_enclosing(&child, target_row, scope_kinds, best);
        }
    }
}

fn classify_scope_type(kind: &str, lang: SupportedLanguage) -> String {
    match lang {
        SupportedLanguage::Rust => match kind {
            "function_item" => "function",
            "struct_item" => "struct",
            "enum_item" => "enum",
            "trait_item" => "trait",
            "impl_item" => "impl",
            "mod_item" => "module",
            "macro_definition" => "macro",
            _ => kind,
        },
        SupportedLanguage::Python => match kind {
            "function_definition" => "function",
            "class_definition" => "class",
            "decorated_definition" => "function",
            _ => kind,
        },
        SupportedLanguage::JavaScript | SupportedLanguage::TypeScript => match kind {
            "function_declaration" => "function",
            "class_declaration" => "class",
            "method_definition" => "method",
            "arrow_function" => "function",
            "export_statement" => "export",
            _ => kind,
        },
        SupportedLanguage::Go => match kind {
            "function_declaration" => "function",
            "method_declaration" => "method",
            "type_declaration" => "type",
            _ => kind,
        },
        SupportedLanguage::Java => match kind {
            "class_declaration" => "class",
            "interface_declaration" => "interface",
            "enum_declaration" => "enum",
            "method_declaration" => "method",
            "constructor_declaration" => "constructor",
            _ => kind,
        },
        SupportedLanguage::C => match kind {
            "function_definition" => "function",
            "struct_specifier" => "struct",
            "enum_specifier" => "enum",
            _ => kind,
        },
        SupportedLanguage::Cpp => match kind {
            "function_definition" => "function",
            "class_specifier" => "class",
            "struct_specifier" => "struct",
            "namespace_definition" => "namespace",
            _ => kind,
        },
        SupportedLanguage::Kotlin => match kind {
            "function_declaration" => "function",
            "class_declaration" => "class",
            "object_declaration" => "object",
            "companion_object" => "companion",
            _ => kind,
        },
        SupportedLanguage::Css => match kind {
            "rule_set" => "rule",
            "media_statement" => "media",
            "keyframes_statement" => "keyframes",
            _ => kind,
        },
        SupportedLanguage::Html => match kind {
            "element" | "script_element" | "style_element" => "element",
            _ => kind,
        },
        SupportedLanguage::Json => match kind {
            "object" => "object",
            "array" => "array",
            _ => kind,
        },
        SupportedLanguage::Bash => match kind {
            "function_definition" => "function",
            "if_statement" => "if",
            "for_statement" => "for",
            "case_statement" => "case",
            _ => kind,
        },
        SupportedLanguage::Toml => "table",
        SupportedLanguage::Yaml => "mapping",
        SupportedLanguage::Markdown => "section",
    }
    .to_string()
}

fn line_range_fallback(
    source: &str,
    file_path: &Path,
    target_line: usize,
    total_lines: usize,
    window: usize,
    language: &str,
    access_scope: ContextAccessScope,
) -> ContextResult {
    let window = window.min(MAX_CONTEXT_EXPANSION_LINES);
    let start = if target_line > window {
        target_line - window
    } else {
        1
    };
    let end = std::cmp::min(target_line + window, total_lines);

    let lines: Vec<&str> = source.lines().collect();
    let content = lines[start - 1..end].join("\n");

    ContextResult {
        file_path: file_path.display().to_string(),
        language: language.to_string(),
        content,
        line_start: start,
        line_end: end,
        scope_type: "lines".to_string(),
        access_scope: Some(access_scope),
    }
}

pub fn parse_location(location: &str) -> anyhow::Result<(String, usize)> {
    let parts: Vec<&str> = location.rsplitn(2, ':').collect();
    if parts.len() != 2 {
        anyhow::bail!(
            "invalid location format '{}': expected <file>:<line>",
            location
        );
    }
    let line: usize = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid line number: {}", parts[0]))?;
    let file = parts[1].to_string();
    Ok((file, line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_temp_file(content: &str, ext: &str) -> (NamedTempFile, std::path::PathBuf) {
        let mut f = tempfile::Builder::new()
            .suffix(&format!(".{ext}"))
            .tempfile()
            .unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        let path = f.path().to_path_buf();
        (f, path)
    }

    #[test]
    fn test_parse_location_valid() {
        let (file, line) = parse_location("src/main.rs:42").unwrap();
        assert_eq!(file, "src/main.rs");
        assert_eq!(line, 42);
    }

    #[test]
    fn test_parse_location_colon_in_path() {
        let (file, line) = parse_location("C:/Users/foo/bar.rs:10").unwrap();
        assert_eq!(file, "C:/Users/foo/bar.rs");
        assert_eq!(line, 10);
    }

    #[test]
    fn test_parse_location_invalid_no_colon() {
        assert!(parse_location("src/main.rs").is_err());
    }

    #[test]
    fn test_parse_location_invalid_line() {
        assert!(parse_location("src/main.rs:abc").is_err());
    }

    #[test]
    fn test_context_rust_function() {
        let source = r#"
fn helper() -> i32 {
    42
}

fn main() {
    let x = helper();
    println!("{}", x);
    let y = x + 1;
    let z = y * 2;
}

fn another() {
    todo!()
}
"#;
        let (_f, path) = write_temp_file(source, "rs");
        let result = ContextEngine::retrieve(&path, 8, None).unwrap();
        assert_eq!(result.scope_type, "function");
        assert_eq!(result.access_scope, Some(ContextAccessScope::ProjectRoot));
        assert!(result.content.contains("fn main()"));
        assert!(result.content.contains("println!"));
        assert_eq!(result.line_start, 6);
        assert_eq!(result.line_end, 11);
    }

    #[test]
    fn test_context_rust_impl_block() {
        let source = r#"
struct Foo;

impl Foo {
    fn bar(&self) -> i32 {
        42
    }

    fn baz(&self) {
        println!("baz");
    }
}
"#;
        let (_f, path) = write_temp_file(source, "rs");
        let result = ContextEngine::retrieve(&path, 6, None).unwrap();
        assert_eq!(result.scope_type, "function");
        assert!(result.content.contains("fn bar"));
    }

    #[test]
    fn test_context_python_function() {
        let source = r#"
def greet(name):
    message = f"Hello, {name}"
    print(message)
    return message

def farewell():
    pass
"#;
        let (_f, path) = write_temp_file(source, "py");
        let result = ContextEngine::retrieve(&path, 4, None).unwrap();
        assert_eq!(result.scope_type, "function");
        assert!(result.content.contains("def greet"));
    }

    #[test]
    fn test_context_python_class() {
        let source = r#"
class MyClass:
    def __init__(self):
        self.x = 10

    def method(self):
        return self.x
"#;
        let (_f, path) = write_temp_file(source, "py");
        let result = ContextEngine::retrieve(&path, 6, None).unwrap();
        assert_eq!(result.scope_type, "function");
        assert!(result.content.contains("def method"));
    }

    #[test]
    fn test_context_fallback_unsupported_language() {
        let source = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
        let (_f, path) = write_temp_file(source, "txt");
        let result = ContextEngine::retrieve(&path, 5, Some(2)).unwrap();
        assert_eq!(result.scope_type, "lines");
        assert_eq!(result.access_scope, Some(ContextAccessScope::ProjectRoot));
        assert_eq!(result.line_start, 3);
        assert_eq!(result.line_end, 7);
        assert_eq!(result.language, "txt");
    }

    #[test]
    fn test_context_retrieve_with_scope_marks_outside_root() {
        let source = r#"
fn leaked() {
    println!("outside");
}
"#;
        let (_f, path) = write_temp_file(source, "rs");
        let result =
            ContextEngine::retrieve_with_scope(&path, 2, None, ContextAccessScope::OutsideRoot)
                .unwrap();

        assert_eq!(result.scope_type, "function");
        assert_eq!(result.access_scope, Some(ContextAccessScope::OutsideRoot));
    }

    #[test]
    fn test_context_fallback_clamps_to_file_bounds() {
        let source = "line1\nline2\nline3\n";
        let (_f, path) = write_temp_file(source, "txt");
        let result = ContextEngine::retrieve(&path, 2, Some(50)).unwrap();
        assert_eq!(result.line_start, 1);
        assert_eq!(result.line_end, 3);
    }

    #[test]
    fn test_context_line_out_of_range() {
        let source = "line1\nline2\n";
        let (_f, path) = write_temp_file(source, "rs");
        let result = ContextEngine::retrieve(&path, 100, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_context_line_zero() {
        let source = "fn main() {}\n";
        let (_f, path) = write_temp_file(source, "rs");
        let result = ContextEngine::retrieve(&path, 0, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_context_fallback_when_no_enclosing_scope() {
        let source = r#"
use std::io;

fn main() {
    println!("hello");
}
"#;
        let (_f, path) = write_temp_file(source, "rs");
        let result = ContextEngine::retrieve(&path, 2, Some(1)).unwrap();
        assert_eq!(result.scope_type, "lines");
    }

    #[test]
    fn test_context_go_function() {
        let source = r#"package main

func main() {
	fmt.Println("hello")
	x := 42
}

func helper() int {
	return 1
}
"#;
        let (_f, path) = write_temp_file(source, "go");
        let result = ContextEngine::retrieve(&path, 4, None).unwrap();
        assert_eq!(result.scope_type, "function");
        assert!(result.content.contains("func main()"));
    }

    fn huge_rust_source(body_lines: usize) -> String {
        let mut s = String::from("fn huge_function() -> i32 {\n");
        for i in 0..body_lines {
            s.push_str(&format!("    let v{i} = {i};\n"));
        }
        s.push_str("    0\n}\n");
        s
    }

    #[test]
    fn test_bound_scope_window_whole_scope_at_threshold() {
        // A scope spanning exactly MAX_WHOLE_SCOPE_LINES (101) is returned whole,
        // even for a small explicit expansion.
        let bounds = bound_scope_window((1, 101), 50, Some(3), MAX_WHOLE_SCOPE_LINES, 500);
        assert_eq!(bounds.line_start, 1);
        assert_eq!(bounds.line_end, 101);
        assert!(!bounds.clipped);
    }

    #[test]
    fn test_bound_scope_window_clipped_just_over_threshold() {
        // One line larger than the whole-scope threshold flips to the window branch
        // and clips regardless of the target's position.
        let bounds = bound_scope_window((1, 102), 50, Some(3), MAX_WHOLE_SCOPE_LINES, 500);
        assert!(bounds.clipped);
        assert!(bounds.line_start > 1 || bounds.line_end < 102);
    }

    #[test]
    fn test_bound_scope_window_default_expansion() {
        // Default expansion (CONTEXT_FALLBACK_LINES = 50) yields target +/- 50.
        let bounds = bound_scope_window((1, 600), 300, None, MAX_WHOLE_SCOPE_LINES, 500);
        assert_eq!(bounds.line_start, 250);
        assert_eq!(bounds.line_end, 350);
        assert!(bounds.clipped);
    }

    #[test]
    fn test_bound_scope_window_explicit_expansion() {
        let bounds = bound_scope_window((1, 600), 300, Some(10), MAX_WHOLE_SCOPE_LINES, 500);
        assert_eq!(bounds.line_start, 290);
        assert_eq!(bounds.line_end, 310);
        assert!(bounds.clipped);
    }

    #[test]
    fn test_bound_scope_window_ceiling_clamp() {
        // A huge requested expansion is clamped to the ceiling each side.
        let bounds = bound_scope_window(
            (1, 2000),
            1000,
            Some(999),
            MAX_WHOLE_SCOPE_LINES,
            MAX_CONTEXT_EXPANSION_LINES,
        );
        assert_eq!(bounds.line_start, 1000 - MAX_CONTEXT_EXPANSION_LINES);
        assert_eq!(bounds.line_end, 1000 + MAX_CONTEXT_EXPANSION_LINES);
        assert!(bounds.clipped);
    }

    #[test]
    fn test_bound_scope_window_intersects_scope_edges() {
        // The window is intersected with the scope: it never extends past the scope
        // start, and clipping is reported on whichever side is a strict subset.
        let bounds = bound_scope_window((100, 300), 105, Some(50), MAX_WHOLE_SCOPE_LINES, 500);
        assert_eq!(bounds.line_start, 100);
        assert_eq!(bounds.line_end, 155);
        assert!(bounds.clipped);
    }

    #[test]
    fn test_retrieve_scope_window_clips_huge_function_default() {
        let source = huge_rust_source(150);
        let (_f, path) = write_temp_file(&source, "rs");
        let window = ContextEngine::retrieve_scope_window(&path, 75, None).unwrap();

        assert!(window.clipped);
        assert_eq!(window.scope_type, "function");
        assert_eq!(window.scope_name.as_deref(), Some("huge_function"));
        assert_eq!(window.scope_line_start, 1);
        assert_eq!(window.scope_line_end, 153);
        // Default window is target +/- CONTEXT_FALLBACK_LINES (101 lines wide).
        assert_eq!(window.line_start, 25);
        assert_eq!(window.line_end, 125);
        assert_eq!(window.content.lines().count(), 101);
        assert!(window.content.contains("let v73"));
    }

    #[test]
    fn test_retrieve_scope_window_explicit_expansion() {
        let source = huge_rust_source(150);
        let (_f, path) = write_temp_file(&source, "rs");
        let window = ContextEngine::retrieve_scope_window(&path, 75, Some(10)).unwrap();

        assert!(window.clipped);
        assert_eq!(window.line_start, 65);
        assert_eq!(window.line_end, 85);
        assert_eq!(window.content.lines().count(), 21);
    }

    #[test]
    fn test_retrieve_scope_window_ceiling_clamp() {
        let source = huge_rust_source(1200);
        let (_f, path) = write_temp_file(&source, "rs");
        let window = ContextEngine::retrieve_scope_window(&path, 600, Some(999)).unwrap();

        assert!(window.clipped);
        assert_eq!(window.line_start, 600 - MAX_CONTEXT_EXPANSION_LINES);
        assert_eq!(window.line_end, 600 + MAX_CONTEXT_EXPANSION_LINES);
    }

    #[test]
    fn test_retrieve_scope_window_small_scope_returned_whole() {
        // A function well under the whole-scope threshold is returned whole even
        // with a tiny explicit expansion, with no clipping.
        let source = huge_rust_source(5);
        let (_f, path) = write_temp_file(&source, "rs");
        let window = ContextEngine::retrieve_scope_window(&path, 4, Some(2)).unwrap();

        assert!(!window.clipped);
        assert_eq!(window.scope_name.as_deref(), Some("huge_function"));
        assert_eq!(window.line_start, window.scope_line_start);
        assert_eq!(window.line_end, window.scope_line_end);
        assert!(window.content.contains("fn huge_function"));
        assert!(window.content.contains("let v0"));
        assert!(window.content.contains("let v4"));
    }

    #[test]
    fn test_retrieve_scope_window_fallback_no_scope() {
        let source = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10\n";
        let (_f, path) = write_temp_file(source, "txt");
        let window = ContextEngine::retrieve_scope_window(&path, 5, Some(2)).unwrap();

        assert!(!window.clipped);
        assert_eq!(window.scope_type, "lines");
        assert_eq!(window.scope_name, None);
        assert_eq!(window.line_start, 3);
        assert_eq!(window.line_end, 7);
    }
}
