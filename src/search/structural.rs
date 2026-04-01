use std::path::Path;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};
use turso::Connection;

use crate::indexer::parser::SupportedLanguage;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::StructuralResult;
use crate::storage::segments;

pub struct StructuralSearchEngine<'a> {
    conn: Option<&'a Connection>,
    project_root: &'a Path,
}

impl<'a> StructuralSearchEngine<'a> {
    pub fn new(project_root: &'a Path, conn: Option<&'a Connection>) -> Self {
        Self { conn, project_root }
    }

    pub async fn search(
        &self,
        pattern: &str,
        language_filter: Option<&str>,
    ) -> Result<Vec<StructuralResult>, OneupError> {
        let languages = self.resolve_languages(language_filter);

        if languages.is_empty() {
            return Err(SearchError::InvalidQuery(format!(
                "no supported language matches filter: {}",
                language_filter.unwrap_or("(none)")
            ))
            .into());
        }

        let mut all_results = Vec::new();

        for lang in &languages {
            let query = match Query::new(&lang.language_fn().into(), pattern) {
                Ok(q) => q,
                Err(e) => {
                    tracing::debug!("query pattern not valid for {}: {e}", lang.name());
                    continue;
                }
            };

            let file_paths = self.get_file_paths(lang).await?;

            for file_path in &file_paths {
                let abs_path = self.project_root.join(file_path);
                let source = match std::fs::read_to_string(&abs_path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!("skipping {file_path}: {e}");
                        continue;
                    }
                };

                let results = self.run_query(lang, &query, &source, file_path)?;
                all_results.extend(results);
            }
        }

        Ok(all_results)
    }

    fn resolve_languages(&self, filter: Option<&str>) -> Vec<SupportedLanguage> {
        let all = [
            SupportedLanguage::Rust,
            SupportedLanguage::Python,
            SupportedLanguage::JavaScript,
            SupportedLanguage::TypeScript,
            SupportedLanguage::Go,
            SupportedLanguage::Java,
            SupportedLanguage::C,
            SupportedLanguage::Cpp,
        ];

        match filter {
            Some(name) => all.into_iter().filter(|l| l.name() == name).collect(),
            None => all.to_vec(),
        }
    }

    async fn get_file_paths(&self, lang: &SupportedLanguage) -> Result<Vec<String>, OneupError> {
        if let Some(conn) = self.conn {
            return segments::get_file_paths_by_language(conn, lang.name()).await;
        }

        let mut paths = Vec::new();
        let scanner_results = crate::indexer::scanner::scan_directory(self.project_root)?;
        for file in scanner_results {
            if SupportedLanguage::from_extension(&file.extension) == Some(*lang) {
                if let Ok(rel) = file.path.strip_prefix(self.project_root) {
                    paths.push(rel.to_string_lossy().to_string());
                }
            }
        }
        Ok(paths)
    }

    fn run_query(
        &self,
        lang: &SupportedLanguage,
        query: &Query,
        source: &str,
        file_path: &str,
    ) -> Result<Vec<StructuralResult>, OneupError> {
        let mut parser = Parser::new();
        parser
            .set_language(&lang.language_fn().into())
            .map_err(|e| SearchError::QueryFailed(format!("failed to set language: {e}")))?;

        let tree = parser
            .parse(source, None)
            .ok_or_else(|| SearchError::QueryFailed("tree-sitter parse returned None".into()))?;

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), source.as_bytes());

        let source_bytes = source.as_bytes();
        let mut results = Vec::new();
        let mut seen_ranges = Vec::new();

        while let Some(m) = matches.next() {
            for capture in m.captures {
                let node = capture.node;
                let start_byte = node.start_byte();
                let end_byte = node.end_byte();

                let range = (start_byte, end_byte);
                if seen_ranges.contains(&range) {
                    continue;
                }
                seen_ranges.push(range);

                let content =
                    std::str::from_utf8(&source_bytes[start_byte..end_byte]).unwrap_or("");

                let capture_name = query.capture_names()[capture.index as usize].to_string();
                let pattern_name = if capture_name.is_empty() {
                    None
                } else {
                    Some(capture_name)
                };

                results.push(StructuralResult {
                    file_path: file_path.to_string(),
                    language: lang.name().to_string(),
                    pattern_name,
                    content: content.to_string(),
                    line_start: node.start_position().row + 1,
                    line_end: node.end_position().row + 1,
                });
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn find_rust_functions() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            r#"fn hello() {
    println!("hello");
}

fn world() {
    println!("world");
}

struct Foo;
"#,
        )
        .unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let results = engine
            .search("(function_item name: (identifier) @name)", Some("rust"))
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "hello");
        assert_eq!(results[1].content, "world");
        assert_eq!(results[0].language, "rust");
    }

    #[tokio::test]
    async fn find_python_functions() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.py"),
            r#"def greet(name):
    print(f"Hello, {name}")

def farewell():
    print("Goodbye")

class Foo:
    pass
"#,
        )
        .unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let results = engine
            .search(
                "(function_definition name: (identifier) @name)",
                Some("python"),
            )
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "greet");
        assert_eq!(results[1].content, "farewell");
    }

    #[tokio::test]
    async fn find_if_statements_in_rust() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("logic.rs"),
            r#"fn check(x: i32) {
    if x > 0 {
        println!("positive");
    }
    if x < 0 {
        println!("negative");
    }
    let y = x + 1;
}
"#,
        )
        .unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let results = engine
            .search("(if_expression) @if_stmt", Some("rust"))
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results[0].content.contains("x > 0"));
        assert!(results[1].content.contains("x < 0"));
    }

    #[tokio::test]
    async fn invalid_query_for_language() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.py"), "x = 1\n").unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let results = engine
            .search("(function_item) @fn", Some("python"))
            .await
            .unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn unsupported_language_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let result = engine.search("(identifier) @name", Some("haskell")).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn deduplicates_overlapping_matches() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("dup.rs"), "fn foo() {}\n").unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let results = engine
            .search("(function_item) @fn", Some("rust"))
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("fn foo()"));
    }

    #[tokio::test]
    async fn multi_language_scan() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn rust_fn() {}\n").unwrap();
        fs::write(
            tmp.path().join("main.go"),
            "package main\nfunc go_fn() {}\n",
        )
        .unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let results = engine.search("(identifier) @name", None).await.unwrap();

        let contents: Vec<&str> = results.iter().map(|r| r.content.as_str()).collect();
        assert!(contents.contains(&"rust_fn"));
        assert!(contents.contains(&"go_fn"));
    }

    #[tokio::test]
    async fn captures_line_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("lines.rs"),
            "// comment\n// another\nfn target() {}\n",
        )
        .unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let results = engine
            .search("(function_item) @fn", Some("rust"))
            .await
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line_start, 3);
        assert_eq!(results[0].line_end, 3);
    }
}
