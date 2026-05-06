use std::path::Path;

use libsql::Connection;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::indexer::parser::SupportedLanguage;
use crate::shared::errors::{OneupError, SearchError};
use crate::shared::types::{
    StructuralDiagnostic, StructuralDiagnosticKind, StructuralResult, StructuralSearchReport,
    StructuralSearchStatus,
};
use crate::storage::segments;

pub struct StructuralSearchEngine<'a> {
    conn: Option<&'a Connection>,
    context_id: Option<&'a str>,
    project_root: &'a Path,
}

impl<'a> StructuralSearchEngine<'a> {
    pub fn new(project_root: &'a Path, conn: Option<&'a Connection>) -> Self {
        Self {
            conn,
            context_id: None,
            project_root,
        }
    }

    pub fn new_scoped(project_root: &'a Path, conn: &'a Connection, context_id: &'a str) -> Self {
        Self {
            conn: Some(conn),
            context_id: Some(context_id),
            project_root,
        }
    }

    pub async fn search(
        &self,
        pattern: &str,
        language_filter: Option<&str>,
    ) -> Result<Vec<StructuralResult>, OneupError> {
        let report = self.search_report(pattern, language_filter).await?;

        if report.status == StructuralSearchStatus::Error {
            return Err(SearchError::InvalidQuery(format!(
                "structural search failed: {}",
                report_error_message(&report)
            ))
            .into());
        }

        Ok(report.results)
    }

    pub async fn search_report(
        &self,
        pattern: &str,
        language_filter: Option<&str>,
    ) -> Result<StructuralSearchReport, OneupError> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Ok(self.error_report(vec![StructuralDiagnostic {
                kind: StructuralDiagnosticKind::InvalidPattern,
                message: "structural pattern cannot be empty".to_string(),
                language: None,
            }]));
        }

        let languages = match self.resolve_languages(language_filter) {
            Ok(languages) => languages,
            Err(report) => return Ok(report),
        };

        let mut language_paths = Vec::with_capacity(languages.len());
        let mut has_candidate_paths = false;
        for lang in languages {
            let file_paths = self.get_file_paths(&lang).await?;
            has_candidate_paths |= !file_paths.is_empty();
            language_paths.push((lang, file_paths));
        }

        let compile_without_candidates = language_filter.is_some() || !has_candidate_paths;
        let mut all_results = Vec::new();
        let mut diagnostics = Vec::new();
        let mut compiled_any = false;

        for (lang, file_paths) in language_paths {
            if !compile_without_candidates && file_paths.is_empty() {
                continue;
            }

            let query = match Query::new(&lang.language_fn().into(), pattern) {
                Ok(query) => {
                    compiled_any = true;
                    query
                }
                Err(e) => {
                    diagnostics.push(StructuralDiagnostic {
                        kind: StructuralDiagnosticKind::InvalidPattern,
                        message: format!("pattern is not valid for {}: {e}", lang.name()),
                        language: Some(lang.name().to_string()),
                    });
                    continue;
                }
            };

            for file_path in &file_paths {
                let abs_path = self.project_root.join(file_path);
                let source = match std::fs::read_to_string(&abs_path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::debug!("skipping {file_path}: {e}");
                        continue;
                    }
                };

                let results = self.run_query(&lang, &query, &source, file_path)?;
                all_results.extend(results);
            }
        }

        let status = if !compiled_any {
            StructuralSearchStatus::Error
        } else if all_results.is_empty() {
            StructuralSearchStatus::Empty
        } else {
            StructuralSearchStatus::Ok
        };

        Ok(StructuralSearchReport {
            status,
            results: all_results,
            diagnostics,
            supported_languages: Self::supported_language_names(),
        })
    }

    pub fn supported_language_names() -> Vec<String> {
        Self::supported_languages()
            .into_iter()
            .map(|lang| lang.name().to_string())
            .collect()
    }

    fn supported_languages() -> Vec<SupportedLanguage> {
        [
            SupportedLanguage::Rust,
            SupportedLanguage::Python,
            SupportedLanguage::JavaScript,
            SupportedLanguage::TypeScript,
            SupportedLanguage::Go,
            SupportedLanguage::Java,
            SupportedLanguage::C,
            SupportedLanguage::Cpp,
            SupportedLanguage::Kotlin,
            SupportedLanguage::Css,
            SupportedLanguage::Html,
            SupportedLanguage::Json,
            SupportedLanguage::Bash,
            SupportedLanguage::Toml,
            SupportedLanguage::Yaml,
            SupportedLanguage::Markdown,
        ]
        .into_iter()
        .filter(|lang| lang.has_structural_segments())
        .collect()
    }

    fn resolve_languages(
        &self,
        filter: Option<&str>,
    ) -> Result<Vec<SupportedLanguage>, StructuralSearchReport> {
        let all = Self::supported_languages();

        match filter.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => {
                let matches = all
                    .into_iter()
                    .filter(|lang| lang.name() == name)
                    .collect::<Vec<_>>();
                if matches.is_empty() {
                    return Err(self.error_report(vec![StructuralDiagnostic {
                        kind: StructuralDiagnosticKind::UnsupportedLanguage,
                        message: format!("unsupported structural language filter: {name}"),
                        language: Some(name.to_string()),
                    }]));
                }
                Ok(matches)
            }
            None => Ok(all),
        }
    }

    async fn get_file_paths(&self, lang: &SupportedLanguage) -> Result<Vec<String>, OneupError> {
        if let Some(conn) = self.conn {
            if let Some(context_id) = self.context_id {
                return segments::get_file_paths_by_language_for_context(
                    conn,
                    context_id,
                    lang.name(),
                )
                .await;
            }
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

    fn error_report(&self, diagnostics: Vec<StructuralDiagnostic>) -> StructuralSearchReport {
        StructuralSearchReport {
            status: StructuralSearchStatus::Error,
            results: Vec::new(),
            diagnostics,
            supported_languages: Self::supported_language_names(),
        }
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

fn report_error_message(report: &StructuralSearchReport) -> String {
    let diagnostics = report
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();

    if diagnostics.is_empty() {
        return "no selected language could compile the pattern".to_string();
    }

    let mut message = diagnostics.join("; ");
    if report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.kind == StructuralDiagnosticKind::UnsupportedLanguage)
    {
        message.push_str(&format!(
            "; supported languages: {}",
            report.supported_languages.join(", ")
        ));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{
        db::Db,
        schema,
        segments::{self, SegmentInsert},
    };
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
        let report = engine
            .search_report("(function_item) @fn", Some("python"))
            .await
            .unwrap();

        assert_eq!(report.status, StructuralSearchStatus::Error);
        assert_eq!(
            report.diagnostics[0].kind,
            StructuralDiagnosticKind::InvalidPattern
        );
        assert!(engine
            .search("(function_item) @fn", Some("python"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn unsupported_language_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let report = engine
            .search_report("(identifier) @name", Some("haskell"))
            .await
            .unwrap();

        assert_eq!(report.status, StructuralSearchStatus::Error);
        assert_eq!(
            report.diagnostics[0].kind,
            StructuralDiagnosticKind::UnsupportedLanguage
        );
        assert!(report.supported_languages.contains(&"rust".to_string()));
        assert!(engine
            .search("(identifier) @name", Some("haskell"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn valid_query_without_matches_returns_empty_report() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("types.rs"), "struct Foo;\n").unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let report = engine
            .search_report("(function_item) @fn", Some("rust"))
            .await
            .unwrap();

        assert_eq!(report.status, StructuralSearchStatus::Empty);
        assert!(report.results.is_empty());
        assert!(report.diagnostics.is_empty());
    }

    #[tokio::test]
    async fn multi_language_query_errors_when_no_language_can_compile() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn rust_fn() {}\n").unwrap();
        fs::write(tmp.path().join("main.py"), "def python_fn():\n    pass\n").unwrap();

        let engine = StructuralSearchEngine::new(tmp.path(), None);
        let report = engine
            .search_report("(definitely_not_a_node) @bad", None)
            .await
            .unwrap();

        assert_eq!(report.status, StructuralSearchStatus::Error);
        assert!(report.results.is_empty());
        assert_eq!(report.diagnostics.len(), 2);
        assert!(report
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.kind == StructuralDiagnosticKind::InvalidPattern));
    }

    #[tokio::test]
    async fn scoped_index_search_only_reads_context_paths() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/active.rs"), "fn active() {}\n").unwrap();
        fs::write(tmp.path().join("src/other.rs"), "fn other() {}\n").unwrap();

        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        let active = test_segment("active", "src/active.rs", "rust");
        let other = test_segment("other", "src/other.rs", "rust");
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-active",
            "src/active.rs",
            &[active],
        )
        .await
        .unwrap();
        segments::replace_file_segments_for_context_tx(
            &conn,
            "ctx-other",
            "src/other.rs",
            &[other],
        )
        .await
        .unwrap();

        let engine = StructuralSearchEngine::new_scoped(tmp.path(), &conn, "ctx-active");
        let report = engine
            .search_report("(function_item name: (identifier) @name)", Some("rust"))
            .await
            .unwrap();

        assert_eq!(report.status, StructuralSearchStatus::Ok);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].content, "active");
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

    fn test_segment(id: &str, file_path: &str, language: &str) -> SegmentInsert {
        SegmentInsert {
            id: id.to_string(),
            file_path: file_path.to_string(),
            language: language.to_string(),
            block_type: "function".to_string(),
            content: format!("fn {id}() {{}}"),
            line_start: 1,
            line_end: 1,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: format!("[\"{id}\"]"),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("hash-{id}"),
        }
    }
}
