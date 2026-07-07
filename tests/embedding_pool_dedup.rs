use oneup::storage::{
    db::Db,
    schema,
    segments::{self, IndexedFileMeta, SegmentInsert},
};
use std::fs;
use tempfile::TempDir;

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

/// Generate a mock embedding vector in JSON format (384-dimensional all-MiniLM vector).
fn pool_vector_json(fill: f32) -> String {
    serde_json::to_string(&vec![fill; 384]).unwrap()
}

/// Total rows in the content-addressed `embedding_pool` — the count of distinct
/// stored embeddings across every context.
async fn pool_row_count(conn: &libsql::Connection) -> i64 {
    let mut rows = conn
        .query("SELECT COUNT(*) FROM embedding_pool", ())
        .await
        .unwrap();
    rows.next().await.unwrap().unwrap().get(0).unwrap()
}

/// The reference count recorded for a pooled embedding, or `None` if no pool row
/// exists for `content_key` (i.e. it was reclaimed by the delete-at-zero sweep).
async fn pool_ref_count(conn: &libsql::Connection, content_key: &str) -> Option<i64> {
    let mut rows = conn
        .query(
            "SELECT ref_count FROM embedding_pool WHERE content_key = ?1",
            [content_key],
        )
        .await
        .unwrap();
    rows.next().await.unwrap().map(|row| row.get(0).unwrap())
}

/// Write a single pooled segment through the production per-file transaction
/// (`replace_file_segments_for_context_tx_with_meta`) — the path the indexer
/// uses — so the pool upsert, the `segment_vectors` reference, and `ref_count`
/// seeding all run exactly as in production.
async fn write_pooled_segment(
    conn: &libsql::Connection,
    context: &str,
    file: &str,
    seg: SegmentInsert,
) {
    let meta = IndexedFileMeta {
        extension: "rs".to_string(),
        file_hash: seg.file_hash.clone(),
        file_size: seg.content.len() as i64,
        modified_ns: 1,
    };
    segments::replace_file_segments_for_context_tx_with_meta(
        conn,
        context,
        file,
        &[seg],
        Some(&meta),
    )
    .await
    .unwrap();
}

/// Regression test: identical file stems in different scope cones share one embedding pool row.
///
/// This test pins the embed-input path-invariance behavior documented in
/// `compose_embedding_text`: the embedding input uses only the file stem (e.g., "utils"
/// from "services/auth/utils.rs"), not the full repository-relative path. This enables
/// cross-cone embedding reuse — identical content in two different scope cones
/// (e.g., `services/auth/utils.rs` and `services/web/utils.rs`) produces identical
/// embedding inputs and shares one pooled embedding row (REQ-012).
///
/// This test prevents future "fixes" from silently breaking cross-cone dedup by
/// changing the embed input to use full paths. It runs in the default test suite.
#[test]
fn embedding_pool_deduplication_cross_cone_share() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".1up")).unwrap();
    let db_path = root.join(".1up").join("index.db");

    // Identical content (same source code) in two different scope cones with
    // the same file stem. Because compose_embedding_text uses only the file stem,
    // not the full path, both files produce identical embed inputs and share one
    // pooled embedding row.
    let shared_content_key = "key-cross-cone".to_string();
    let shared_vector = pool_vector_json(0.50);
    let shared_code = "pub fn helper() { println!(\"shared\"); }\n";

    let pooled_segment = |context: &str, file: &str, key: &str, vector: &str| -> SegmentInsert {
        SegmentInsert {
            id: format!("{context}-{file}-seg"),
            file_path: file.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: shared_code.to_string(),
            line_start: 1,
            line_end: 1,
            content_key: Some(key.to_string()),
            embedding_vec: Some(vector.to_string()),
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"helper\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("{context}-{file}-hash"),
        }
    };

    block_on(async {
        let db = Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        // First cone (services/auth): index utils.rs with shared content.
        write_pooled_segment(
            &conn,
            "ctx-auth",
            "services/auth/utils.rs",
            pooled_segment(
                "ctx-auth",
                "services/auth/utils.rs",
                &shared_content_key,
                &shared_vector,
            ),
        )
        .await;

        assert_eq!(
            pool_row_count(&conn).await,
            1,
            "first cone creates one pool row for the shared content"
        );
        assert_eq!(
            pool_ref_count(&conn, &shared_content_key).await,
            Some(1),
            "shared content referenced once (ctx-auth only)"
        );

        // Second cone (services/web): index utils.rs with identical content.
        // Despite the full path being different (services/web vs services/auth),
        // the file stem is identical ("utils"), so compose_embedding_text produces
        // the same embed input, resulting in the same content_key.
        // The pool should NOT grow; instead, the existing row's ref_count increments.
        write_pooled_segment(
            &conn,
            "ctx-web",
            "services/web/utils.rs",
            pooled_segment(
                "ctx-web",
                "services/web/utils.rs",
                &shared_content_key,
                &shared_vector,
            ),
        )
        .await;

        assert_eq!(
            pool_row_count(&conn).await,
            1,
            "second cone with same file stem reuses the existing pool row (no new row created)"
        );
        assert_eq!(
            pool_ref_count(&conn, &shared_content_key).await,
            Some(2),
            "shared embedding is now referenced by both cones (ctx-auth and ctx-web)"
        );
    });
}
