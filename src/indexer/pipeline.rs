use std::collections::HashSet;
use std::path::Path;

use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};
use turso::Connection;

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

    let scanned = scanner::scan_directory(project_root)?;
    stats.files_scanned = scanned.len();

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

    let pb = ProgressBar::new(scanned_relative.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{msg} [{bar:40}] {pos}/{len} files ({eta})")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=> "),
    );
    pb.set_message("Indexing");

    let has_embedder = embedder.is_some();
    stats.embeddings_generated = has_embedder;

    if !has_embedder {
        warn!("embedding model not available: indexing without embeddings (semantic search will be degraded, FTS-only mode active)");
    }

    struct PendingFile {
        relative_path: String,
        file_hash: String,
        segments: Vec<ParsedSegment>,
    }

    let mut pending: Vec<PendingFile> = Vec::new();
    let mut unsupported_extensions: HashSet<String> = HashSet::new();

    for (relative_path, scanned_file) in &scanned_relative {
        let content = match std::fs::read_to_string(&scanned_file.path) {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "skipping unreadable file {}: {e}",
                    scanned_file.path.display()
                );
                pb.inc(1);
                stats.files_skipped += 1;
                continue;
            }
        };

        let file_hash = compute_file_hash(content.as_bytes());

        if let Some(stored_hash) = segments::get_file_hash(conn, relative_path).await? {
            if stored_hash == file_hash {
                pb.inc(1);
                stats.files_skipped += 1;
                continue;
            }
        }

        let parsed_segments = if parser::is_language_supported(&scanned_file.extension) {
            match parser::parse_file(&content, &scanned_file.extension) {
                Ok(segs) => segs,
                Err(e) => {
                    warn!(
                        "tree-sitter parse failed for {}, falling back to text chunker: {e}",
                        relative_path
                    );
                    chunker::chunk_file_default(&content, &scanned_file.extension)
                }
            }
        } else {
            unsupported_extensions.insert(scanned_file.extension.clone());
            debug!(
                "no tree-sitter grammar for .{} files, using text chunker: {}",
                scanned_file.extension, relative_path
            );
            chunker::chunk_file_default(&content, &scanned_file.extension)
        };

        if parsed_segments.is_empty() {
            pb.inc(1);
            stats.files_skipped += 1;
            continue;
        }

        pending.push(PendingFile {
            relative_path: relative_path.clone(),
            file_hash,
            segments: parsed_segments,
        });

        pb.inc(1);
    }

    if !unsupported_extensions.is_empty() {
        let mut exts: Vec<&str> = unsupported_extensions.iter().map(|s| s.as_str()).collect();
        exts.sort();
        warn!(
            "no tree-sitter grammar available for file types: .{}; these files were indexed with text chunking only (symbol and structural search will not cover them)",
            exts.join(", .")
        );
    }

    if let Some(emb) = embedder {
        for pf in &pending {
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
                    complexity: seg.complexity as i64,
                    role: format!("{:?}", seg.role).to_uppercase(),
                    defined_symbols: serde_json::to_string(&seg.defined_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    referenced_symbols: serde_json::to_string(&seg.referenced_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    file_hash: pf.file_hash.clone(),
                };

                segments::upsert_segment(conn, &insert).await?;
                stats.segments_stored += 1;
            }

            stats.files_indexed += 1;
        }
    } else {
        for pf in &pending {
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
                    complexity: seg.complexity as i64,
                    role: format!("{:?}", seg.role).to_uppercase(),
                    defined_symbols: serde_json::to_string(&seg.defined_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    referenced_symbols: serde_json::to_string(&seg.referenced_symbols)
                        .unwrap_or_else(|_| "[]".into()),
                    file_hash: pf.file_hash.clone(),
                };

                segments::upsert_segment(conn, &insert).await?;
                stats.segments_stored += 1;
            }

            stats.files_indexed += 1;
        }
    }

    pb.finish_with_message("Indexing complete");

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
    async fn routes_unsupported_language_to_chunker() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=100).map(|i| format!("line {i}")).collect();
        fs::write(tmp.path().join("data.yaml"), lines.join("\n")).unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.segments_stored > 0);

        let segs = segments::get_segments_by_file(&conn, "data.yaml")
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
    async fn unsupported_language_falls_back_to_chunker_with_segments() {
        let tmp = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (1..=30)
            .map(|i| format!("config_line_{i} = value"))
            .collect();
        fs::write(tmp.path().join("config.toml"), lines.join("\n")).unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.segments_stored > 0);

        let segs = segments::get_segments_by_file(&conn, "config.toml")
            .await
            .unwrap();
        assert!(
            segs.iter().all(|s| s.block_type == "chunk"),
            "unsupported language should produce chunk segments"
        );
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
    async fn mixed_supported_and_unsupported_languages() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("lib.rs"), "pub fn foo() {}\n").unwrap();
        let lines: Vec<String> = (1..=30).map(|i| format!("key{i}: val")).collect();
        fs::write(tmp.path().join("data.yaml"), lines.join("\n")).unwrap();

        let (_db, conn) = setup().await;
        let stats = run(&conn, tmp.path(), None).await.unwrap();

        assert_eq!(stats.files_indexed, 2);

        let rs_segs = segments::get_segments_by_file(&conn, "lib.rs")
            .await
            .unwrap();
        assert!(
            rs_segs.iter().any(|s| s.block_type != "chunk"),
            "rust files should produce non-chunk segments"
        );

        let yaml_segs = segments::get_segments_by_file(&conn, "data.yaml")
            .await
            .unwrap();
        assert!(
            yaml_segs.iter().all(|s| s.block_type == "chunk"),
            "yaml files should produce only chunk segments"
        );
    }
}
