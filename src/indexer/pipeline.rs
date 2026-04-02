use std::collections::HashSet;
use std::path::Path;

use nanospinner::{Spinner, SpinnerHandle};
use sha2::{Digest, Sha256};
use tracing::{debug, info};
use turso::Connection;

fn spin(msg: impl Into<String>) -> SpinnerHandle {
    use std::io::IsTerminal;
    Spinner::with_writer_tty(msg, std::io::stderr(), std::io::stderr().is_terminal()).start()
}

use crate::indexer::chunker;
use crate::indexer::embedder::Embedder;
use crate::indexer::parser;
use crate::indexer::scanner;
use crate::shared::errors::OneupError;
use crate::shared::types::ParsedSegment;
use crate::storage::segments::{self, SegmentInsert};

fn compute_file_hash(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let hash = hasher.finalize();
    hash.iter().map(|b| format!("{:02x}", b)).collect()
}

fn generate_segment_id(file_path: &str, line_start: usize, line_end: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}", file_path, line_start, line_end).as_bytes());
    let hash = hasher.finalize();
    hash.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>()[..16]
        .to_string()
}

fn f32_to_json_array(vec: &[f32]) -> String {
    let parts: Vec<String> = vec.iter().map(|v| format!("{v}")).collect();
    format!("[{}]", parts.join(","))
}

fn f32_to_q8_json_array(vec: &[f32]) -> String {
    let max_abs = vec
        .iter()
        .map(|v| v.abs())
        .fold(0.0f32, f32::max)
        .max(1e-10);

    let scale = 127.0 / max_abs;
    let parts: Vec<String> = vec.iter().map(|v| format!("{}", (v * scale) as i8 as u8)).collect();
    format!("[{}]", parts.join(","))
}

/// Statistics returned after a pipeline run.
#[derive(Debug, Clone, Default)]
pub struct PipelineStats {
    pub files_scanned: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_deleted: usize,
    pub segments_stored: usize,
    pub embeddings_generated: bool,
}

/// Run the indexing pipeline on a project root directory.
///
/// Scans for source files, computes SHA-256 hashes for incremental detection,
/// parses/chunks files, generates embeddings, and stores segments in the database.
/// Deleted files have their segments removed.
pub async fn run(
    conn: &Connection,
    project_root: &Path,
    embedder: Option<&mut Embedder>,
) -> Result<PipelineStats, OneupError> {
    let mut stats = PipelineStats::default();

    let has_embedder = embedder.is_some();
    stats.embeddings_generated = has_embedder;

    if !has_embedder {
        info!("embedding model not available: indexing without embeddings (semantic search will be degraded, FTS-only mode active)");
    }

    let scan_spinner = spin("Scanning files");

    let scanned = scanner::scan_directory(project_root)?;
    stats.files_scanned = scanned.len();

    scan_spinner.update(&format!("Scanning {} files", scanned.len()));

    let indexed_paths: HashSet<String> = segments::get_all_file_paths(conn)
        .await?
        .into_iter()
        .collect();

    let project_root_str = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    let scanned_relative: Vec<(String, &scanner::ScannedFile)> = scanned
        .iter()
        .map(|f| {
            let canonical = f.path.canonicalize().unwrap_or_else(|_| f.path.clone());
            let relative = canonical
                .strip_prefix(&project_root_str)
                .unwrap_or(&canonical)
                .to_string_lossy()
                .to_string();
            (relative, f)
        })
        .collect();

    let scanned_paths: HashSet<String> = scanned_relative
        .iter()
        .map(|(rel, _)| rel.clone())
        .collect();

    let deleted_paths: Vec<String> = indexed_paths.difference(&scanned_paths).cloned().collect();

    for path in &deleted_paths {
        segments::delete_segments_by_file(conn, path).await?;
        debug!("removed segments for deleted file: {path}");
        stats.files_deleted += 1;
    }

    scan_spinner.update(&format!("Parsing {} files", scanned_relative.len()));

    struct PendingFile {
        relative_path: String,
        file_hash: String,
        segments: Vec<ParsedSegment>,
    }

    let mut pending: Vec<PendingFile> = Vec::new();
    let mut unsupported_extensions: HashSet<String> = HashSet::new();
    let total_files = scanned_relative.len();

    for (file_idx, (relative_path, scanned_file)) in scanned_relative.iter().enumerate() {
        if file_idx % 100 == 0 {
            scan_spinner.update(&format!("Parsing files ({}/{})", file_idx, total_files));
        }
        let content = match std::fs::read_to_string(&scanned_file.path) {
            Ok(c) => c,
            Err(e) => {
                info!(
                    "skipping unreadable file {}: {e}",
                    scanned_file.path.display()
                );
                stats.files_skipped += 1;
                continue;
            }
        };

        let file_hash = compute_file_hash(content.as_bytes());

        if let Some(stored_hash) = segments::get_file_hash(conn, relative_path).await? {
            if stored_hash == file_hash {
                stats.files_skipped += 1;
                continue;
            }
        }

        let parsed_segments = if parser::use_structural_parser(&scanned_file.extension) {
            match parser::parse_file(&content, &scanned_file.extension) {
                Ok(segs) => segs,
                Err(e) => {
                    info!(
                        "tree-sitter parse failed for {}, falling back to text chunker: {e}",
                        relative_path
                    );
                    chunker::chunk_file_default(&content, &scanned_file.extension)
                }
            }
        } else if parser::is_language_supported(&scanned_file.extension) {
            // Recognized language but better served by text chunking (YAML, TOML, etc.)
            chunker::chunk_file_default(&content, &scanned_file.extension)
        } else {
            // Unknown file type — skip entirely. A secondary search (grep)
            // will cover these files without needing them in the FTS index.
            unsupported_extensions.insert(scanned_file.extension.clone());
            stats.files_skipped += 1;
            continue;
        };

        if parsed_segments.is_empty() {
            stats.files_skipped += 1;
            continue;
        }

        pending.push(PendingFile {
            relative_path: relative_path.clone(),
            file_hash,
            segments: parsed_segments,
        });
    }

    if !unsupported_extensions.is_empty() {
        let mut exts: Vec<&str> = unsupported_extensions.iter().map(|s| s.as_str()).collect();
        exts.sort();
        debug!(
            "skipped unsupported file types: .{}",
            exts.join(", .")
        );
    }

    let total_segments: usize = pending.iter().map(|pf| pf.segments.len()).sum();
    scan_spinner.success_with(&format!(
        "Scanned {} files, {} to index ({} segments)",
        scanned_relative.len(),
        pending.len(),
        total_segments,
    ));

    // Drop FTS index before bulk inserts — FTS maintenance during INSERT is
    // ~160x slower than rebuilding the index once afterwards.
    conn.execute_batch("DROP INDEX IF EXISTS idx_segments_fts")
        .await
        .map_err(|e| {
            crate::shared::errors::StorageError::Query(format!("drop FTS index: {e}"))
        })?;

    let store_label = if has_embedder {
        "Embedding & storing segments"
    } else {
        "Storing segments"
    };
    let store_spinner = spin(store_label);

    if let Some(emb) = embedder {
        for (file_idx, pf) in pending.iter().enumerate() {
            let texts: Vec<&str> = pf.segments.iter().map(|s| s.content.as_str()).collect();
            let embeddings = emb.embed_batch(&texts)?;

            segments::delete_segments_by_file(conn, &pf.relative_path).await?;

            for (i, seg) in pf.segments.iter().enumerate() {
                let embedding = &embeddings[i];
                let embedding_json = f32_to_json_array(embedding);
                let embedding_q8_json = f32_to_q8_json_array(embedding);

                let insert = SegmentInsert {
                    id: generate_segment_id(&pf.relative_path, seg.line_start, seg.line_end),
                    file_path: pf.relative_path.clone(),
                    language: seg.language.clone(),
                    block_type: seg.block_type.clone(),
                    content: seg.content.clone(),
                    line_start: seg.line_start as i64,
                    line_end: seg.line_end as i64,
                    embedding: Some(embedding_json),
                    embedding_q8: Some(embedding_q8_json),
                    breadcrumb: seg.breadcrumb.clone(),
                    complexity: seg.complexity as i64,
                    role: format!("{:?}", seg.role).to_uppercase(),
                    defined_symbols: serde_json::to_string(&seg.defined_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    referenced_symbols: serde_json::to_string(&seg.referenced_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    called_symbols: serde_json::to_string(&seg.called_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    file_hash: pf.file_hash.clone(),
                };

                segments::upsert_segment(conn, &insert).await?;
                stats.segments_stored += 1;
            }

            stats.files_indexed += 1;
            store_spinner.update(&format!(
                "Embedding & storing segments ({}/{})",
                file_idx + 1,
                pending.len()
            ));
        }
    } else {
        for (file_idx, pf) in pending.iter().enumerate() {
            segments::delete_segments_by_file(conn, &pf.relative_path).await?;

            for seg in &pf.segments {
                let insert = SegmentInsert {
                    id: generate_segment_id(&pf.relative_path, seg.line_start, seg.line_end),
                    file_path: pf.relative_path.clone(),
                    language: seg.language.clone(),
                    block_type: seg.block_type.clone(),
                    content: seg.content.clone(),
                    line_start: seg.line_start as i64,
                    line_end: seg.line_end as i64,
                    embedding: None,
                    embedding_q8: None,
                    breadcrumb: seg.breadcrumb.clone(),
                    complexity: seg.complexity as i64,
                    role: format!("{:?}", seg.role).to_uppercase(),
                    defined_symbols: serde_json::to_string(&seg.defined_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    referenced_symbols: serde_json::to_string(&seg.referenced_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    called_symbols: serde_json::to_string(&seg.called_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    file_hash: pf.file_hash.clone(),
                };

                segments::upsert_segment(conn, &insert).await?;
                stats.segments_stored += 1;
            }

            stats.files_indexed += 1;
            if file_idx % 50 == 0 {
                store_spinner.update(&format!(
                    "Storing segments ({}/{})",
                    file_idx + 1,
                    pending.len()
                ));
            }
        }
    }

    store_spinner.update("Building search index");

    // Rebuild FTS index after bulk inserts.
    conn.execute_batch(crate::storage::queries::CREATE_FTS_INDEX)
        .await
        .map_err(|e| {
            crate::shared::errors::StorageError::Query(format!("rebuild FTS index: {e}"))
        })?;

    store_spinner.success_with(&format!(
        "Stored {} segments across {} files",
        stats.segments_stored, stats.files_indexed,
    ));

    info!(
        "pipeline complete: {} scanned, {} indexed, {} skipped, {} deleted, {} segments",
        stats.files_scanned,
        stats.files_indexed,
        stats.files_skipped,
        stats.files_deleted,
        stats.segments_stored,
    );

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{db::Db, schema};
    use std::fs;

    async fn setup() -> (Db, Connection) {
        let db = Db::open_memory().await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        (db, conn)
    }

    #[tokio::test]
    async fn index_temp_directory_without_embedder() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            "fn hello() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("notes.md"),
            "# Notes\n\nSome content here.\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_scanned, 2);
        assert!(stats.files_indexed > 0);
        assert_eq!(stats.files_deleted, 0);
        assert!(!stats.embeddings_generated);
        assert!(stats.segments_stored > 0);

        let count = segments::count_segments(&conn).await.unwrap();
        assert!(count > 0);
    }

    #[tokio::test]
    async fn incremental_indexing_skips_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            "fn hello() {\n    println!(\"hello\");\n}\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;

        let stats1 = run(&conn, tmp.path(), None).await.unwrap();
        assert!(stats1.files_indexed > 0);

        let stats2 = run(&conn, tmp.path(), None).await.unwrap();
        assert_eq!(stats2.files_indexed, 0);
        assert_eq!(stats2.files_skipped, 1);
    }

    #[tokio::test]
    async fn incremental_indexing_reindexes_changed() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.rs"), "fn hello() {}\n").unwrap();

        let (_db, conn) = setup().await;

        let stats1 = run(&conn, tmp.path(), None).await.unwrap();
        assert!(stats1.files_indexed > 0);

        fs::write(tmp.path().join("main.rs"), "fn hello() {}\nfn world() {}\n").unwrap();

        let stats2 = run(&conn, tmp.path(), None).await.unwrap();
        assert!(stats2.files_indexed > 0);
    }

    #[tokio::test]
    async fn deleted_files_removed_from_index() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn b() {}\n").unwrap();

        let (_db, conn) = setup().await;

        run(&conn, tmp.path(), None).await.unwrap();
        let paths1 = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths1.len(), 2);

        fs::remove_file(tmp.path().join("b.rs")).unwrap();

        let stats = run(&conn, tmp.path(), None).await.unwrap();
        assert_eq!(stats.files_deleted, 1);

        let paths2 = segments::get_all_file_paths(&conn).await.unwrap();
        assert_eq!(paths2.len(), 1);
    }

    #[tokio::test]
    async fn empty_directory_produces_no_segments() {
        let tmp = tempfile::tempdir().unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_scanned, 0);
        assert_eq!(stats.files_indexed, 0);
        assert_eq!(stats.segments_stored, 0);
    }

    #[tokio::test]
    async fn routes_supported_language_to_parser() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("lib.rs"),
            "pub struct Foo {\n    pub x: i32,\n}\n\nimpl Foo {\n    pub fn new() -> Self {\n        Self { x: 0 }\n    }\n}\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.segments_stored > 0);

        let segs = segments::get_segments_by_file(&conn, "lib.rs")
            .await
            .unwrap();
        let has_struct = segs.iter().any(|s| s.block_type == "struct");
        assert!(has_struct, "parser should extract struct segments");
    }

    #[tokio::test]
    async fn indexes_text_documents_via_chunking() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        fs::write(tmp.path().join("readme.txt"), lines.join("\n")).unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.segments_stored > 0);

        let segs = segments::get_segments_by_file(&conn, "readme.txt")
            .await
            .unwrap();
        assert!(segs.iter().all(|s| s.block_type == "chunk"));
    }

    #[tokio::test]
    async fn segment_ids_are_deterministic() {
        let id1 = generate_segment_id("src/main.rs", 1, 10);
        let id2 = generate_segment_id("src/main.rs", 1, 10);
        let id3 = generate_segment_id("src/main.rs", 1, 11);
        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[tokio::test]
    async fn file_hash_computation_is_consistent() {
        let hash1 = compute_file_hash(b"hello world");
        let hash2 = compute_file_hash(b"hello world");
        let hash3 = compute_file_hash(b"hello world!");
        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
    }

    #[tokio::test]
    async fn skips_unknown_file_types() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=30)
            .map(|i| format!("config_line_{i} = value"))
            .collect();
        fs::write(tmp.path().join("config.ini"), lines.join("\n")).unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 0);
        assert_eq!(stats.files_skipped, 1);
    }

    #[tokio::test]
    async fn pipeline_without_embedder_stores_no_embeddings() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("main.rs"),
            "fn hello() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert!(!stats.embeddings_generated);
        assert!(stats.files_indexed > 0);
    }

    #[tokio::test]
    async fn mixed_code_docs_and_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn foo() {}\n").unwrap();
        fs::write(tmp.path().join("notes.txt"), "some notes\n").unwrap();
        fs::write(tmp.path().join("config.ini"), "key=val\n").unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 2, "rs + txt should be indexed");
        assert_eq!(stats.files_skipped, 1, "ini should be skipped");

        let rs_segs = segments::get_segments_by_file(&conn, "lib.rs")
            .await
            .unwrap();
        assert!(
            rs_segs.iter().any(|s| s.block_type != "chunk"),
            "rust files should produce structural segments"
        );

        let txt_segs = segments::get_segments_by_file(&conn, "notes.txt")
            .await
            .unwrap();
        assert!(
            txt_segs.iter().all(|s| s.block_type == "chunk"),
            "txt files should produce chunk segments"
        );
    }

}
