use crate::shared::constants::{CHUNK_OVERLAP, CHUNK_WINDOW_SIZE};
use crate::shared::types::{ParsedSegment, SegmentRole};

/// Detects a language name from a file extension for unsupported tree-sitter languages.
fn language_from_extension(ext: &str) -> String {
    match ext {
        "md" | "markdown" => "markdown".into(),
        "txt" => "text".into(),
        "json" => "json".into(),
        "yaml" | "yml" => "yaml".into(),
        "toml" => "toml".into(),
        "xml" => "xml".into(),
        "html" | "htm" => "html".into(),
        "css" => "css".into(),
        "sql" => "sql".into(),
        "sh" | "bash" | "zsh" => "shell".into(),
        "rb" => "ruby".into(),
        "php" => "php".into(),
        "swift" => "swift".into(),
        "kt" | "kts" => "kotlin".into(),
        "r" | "R" => "r".into(),
        "scala" => "scala".into(),
        "ex" | "exs" => "elixir".into(),
        "erl" | "hrl" => "erlang".into(),
        "hs" => "haskell".into(),
        "lua" => "lua".into(),
        "pl" | "pm" => "perl".into(),
        "dart" => "dart".into(),
        "proto" => "protobuf".into(),
        "dockerfile" | "Dockerfile" => "dockerfile".into(),
        "makefile" | "Makefile" | "mk" => "makefile".into(),
        "cmake" => "cmake".into(),
        "tf" | "hcl" => "terraform".into(),
        _ => ext.to_lowercase(),
    }
}

/// Chunks file content into sliding-window segments for languages without tree-sitter support.
///
/// Returns `Vec<ParsedSegment>` with `block_type` set to `"chunk"` and line numbers tracking
/// each window position. The window advances by `window_size - overlap` lines on each step.
pub fn chunk_file(
    content: &str,
    file_extension: &str,
    window_size: usize,
    overlap: usize,
) -> Vec<ParsedSegment> {
    if content.is_empty() {
        return Vec::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let language = language_from_extension(file_extension);
    let effective_window = window_size.max(1);
    let effective_overlap = overlap.min(effective_window.saturating_sub(1));
    let stride = effective_window - effective_overlap;

    let mut segments = Vec::new();
    let mut start = 0;

    while start < lines.len() {
        let end = (start + effective_window).min(lines.len());
        let chunk_lines = &lines[start..end];
        let chunk_content = chunk_lines.join("\n");

        segments.push(ParsedSegment {
            content: chunk_content,
            block_type: "chunk".into(),
            line_start: start + 1,
            line_end: end,
            language: language.clone(),
            breadcrumb: None,
            complexity: 0,
            role: SegmentRole::Implementation,
            defined_symbols: Vec::new(),
            referenced_symbols: Vec::new(),
            called_symbols: Vec::new(),
        });

        if end >= lines.len() {
            break;
        }

        start += stride;
    }

    segments
}

/// Chunks file content using the default constants from `shared::constants`.
pub fn chunk_file_default(content: &str, file_extension: &str) -> Vec<ParsedSegment> {
    chunk_file(content, file_extension, CHUNK_WINDOW_SIZE, CHUNK_OVERLAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_returns_no_segments() {
        let result = chunk_file("", "md", 10, 2);
        assert!(result.is_empty());
    }

    #[test]
    fn single_line_file() {
        let result = chunk_file("hello world", "txt", 10, 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "hello world");
        assert_eq!(result[0].block_type, "chunk");
        assert_eq!(result[0].line_start, 1);
        assert_eq!(result[0].line_end, 1);
        assert_eq!(result[0].language, "text");
    }

    #[test]
    fn file_smaller_than_window() {
        let content = "line 1\nline 2\nline 3";
        let result = chunk_file(content, "md", 10, 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].line_start, 1);
        assert_eq!(result[0].line_end, 3);
        assert_eq!(result[0].content, content);
        assert_eq!(result[0].language, "markdown");
    }

    #[test]
    fn sliding_window_with_overlap() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let content = lines.join("\n");

        let result = chunk_file(&content, "yaml", 10, 3);

        assert_eq!(result.len(), 3);

        assert_eq!(result[0].line_start, 1);
        assert_eq!(result[0].line_end, 10);

        assert_eq!(result[1].line_start, 8);
        assert_eq!(result[1].line_end, 17);

        assert_eq!(result[2].line_start, 15);
        assert_eq!(result[2].line_end, 20);
    }

    #[test]
    fn exact_window_size_file() {
        let lines: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
        let content = lines.join("\n");

        let result = chunk_file(&content, "toml", 10, 2);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].line_start, 1);
        assert_eq!(result[0].line_end, 10);
    }

    #[test]
    fn no_overlap() {
        let lines: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let content = lines.join("\n");

        let result = chunk_file(&content, "sql", 10, 0);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].line_start, 1);
        assert_eq!(result[0].line_end, 10);
        assert_eq!(result[1].line_start, 11);
        assert_eq!(result[1].line_end, 20);
    }

    #[test]
    fn segments_have_correct_metadata() {
        let content = "a\nb\nc\nd\ne";
        let result = chunk_file(content, "json", 3, 1);

        for seg in &result {
            assert_eq!(seg.block_type, "chunk");
            assert_eq!(seg.language, "json");
            assert_eq!(seg.complexity, 0);
            assert_eq!(seg.role, SegmentRole::Implementation);
            assert!(seg.defined_symbols.is_empty());
            assert!(seg.referenced_symbols.is_empty());
            assert!(seg.breadcrumb.is_none());
        }
    }

    #[test]
    fn default_constants_used() {
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        let content = lines.join("\n");

        let result = chunk_file_default(&content, "md");
        assert!(!result.is_empty());
        assert_eq!(result[0].line_start, 1);
        assert_eq!(result[0].line_end, CHUNK_WINDOW_SIZE);
    }

    #[test]
    fn language_detection_from_extension() {
        assert_eq!(chunk_file("x", "sh", 10, 0)[0].language, "shell");
        assert_eq!(chunk_file("x", "yml", 10, 0)[0].language, "yaml");
        assert_eq!(chunk_file("x", "htm", 10, 0)[0].language, "html");
        assert_eq!(
            chunk_file("x", "unknown_ext", 10, 0)[0].language,
            "unknown_ext"
        );
    }
}
