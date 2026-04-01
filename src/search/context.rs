use std::path::Path;

use tree_sitter::{Language, Node, Parser};

use crate::indexer::parser::SupportedLanguage;
use crate::shared::constants::CONTEXT_FALLBACK_LINES;
use crate::shared::types::ContextResult;

pub struct ContextEngine;

impl ContextEngine {
    pub fn retrieve(
        file_path: &Path,
        target_line: usize,
        expansion: Option<usize>,
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
                }),
                None => Ok(line_range_fallback(
                    &source,
                    file_path,
                    target_line,
                    total_lines,
                    expansion.unwrap_or(CONTEXT_FALLBACK_LINES),
                    lang.name(),
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
                ))
            }
        }
    }
}

struct ScopeHit {
    content: String,
    line_start: usize,
    line_end: usize,
    scope_type: String,
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

        ScopeHit {
            content,
            line_start,
            line_end,
            scope_type,
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
) -> ContextResult {
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
        assert_eq!(result.line_start, 3);
        assert_eq!(result.line_end, 7);
        assert_eq!(result.language, "txt");
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
}
