mod common;

use assert_cmd::Command;
use common::{HideModelGuard, MODEL_MUTEX};
use oneup::mcp::types::{
    RETAINED_PUBLIC_TOOLS, TOOL_CONTEXT, TOOL_GET, TOOL_IMPACT, TOOL_OVERVIEW, TOOL_SEARCH,
    TOOL_START, TOOL_STATUS, TOOL_STRUCTURAL, TOOL_SYMBOL,
};
use oneup::shared::constants::{SCHEMA_VERSION, SCOPE_TRUNCATION_REASON};
use oneup::storage::{
    db::Db,
    queries, schema,
    segments::{self, IndexedFileMeta, SegmentInsert},
};
use predicates::prelude::*;
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, net::UnixStream};

fn cmd() -> Command {
    Command::cargo_bin("1up").unwrap()
}

fn test_data_dir(home: &std::path::Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("Application Support").join("1up")
    }

    #[cfg(not(target_os = "macos"))]
    {
        home.join(".local").join("share").join("1up")
    }
}

fn cmd_with_home(home: &Path) -> Command {
    let mut command = cmd();
    command
        .env("HOME", home)
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("XDG_CONFIG_HOME", home.join(".config"));
    command
}

fn seed_model_download_failure(home: &Path) {
    seed_model_download_failure_at_app_root(&test_data_dir(home));
}

fn seed_model_download_failure_at_app_root(app_root: &Path) {
    let model_dir = app_root.join("models").join("all-MiniLM-L6-v2");
    fs::create_dir_all(&model_dir).unwrap();
    fs::write(model_dir.join(".download_failed"), "skip download in test").unwrap();
}

#[cfg(unix)]
fn write_fake_runner(path: &Path) {
    fs::write(
        path,
        r#"#!/bin/sh
set -eu
{
  printf 'cwd=%s\n' "$(pwd -P)"
  printf 'runner=%s\n' "${0##*/}"
  i=0
  for arg in "$@"; do
    printf 'arg[%s]=%s\n' "$i" "$arg"
    i=$((i + 1))
  done
} > "${ONEUP_FAKE_RUNNER_LOG:?}"
exit "${ONEUP_FAKE_RUNNER_STATUS:-0}"
"#,
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn write_framed_json(stream: &mut UnixStream, value: &serde_json::Value) {
    let payload = serde_json::to_vec(value).unwrap();
    let length = u32::try_from(payload.len()).unwrap().to_be_bytes();
    stream.write_all(&length).unwrap();
    stream.write_all(&payload).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
}

#[cfg(unix)]
fn read_framed_json(stream: &mut UnixStream) -> serde_json::Value {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice(&payload).unwrap()
}

struct McpTestClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    _state_home: Option<TempDir>,
}

impl McpTestClient {
    fn start(path: &Path) -> Self {
        Self::start_with_initialize_response(path).0
    }

    fn start_with_isolated_state(path: &Path) -> Self {
        Self::start_with_initialize_response_and_state(path, true).0
    }

    fn start_with_initialize_response(path: &Path) -> (Self, serde_json::Value) {
        Self::start_with_initialize_response_and_state(path, false)
    }

    fn start_with_initialize_response_and_state(
        path: &Path,
        isolate_state: bool,
    ) -> (Self, serde_json::Value) {
        let state_home = isolate_state.then(|| TempDir::new().unwrap());
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_1up"));
        command
            .args(["mcp", "--path", path.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(home) = &state_home {
            let home_path = home.path().canonicalize().unwrap();
            seed_model_download_failure(&home_path);
            seed_model_download_failure_at_app_root(&home_path.join("data").join("1up"));
            command
                .env("HOME", &home_path)
                .env("XDG_DATA_HOME", home_path.join("data"))
                .env("XDG_CONFIG_HOME", home_path.join("config"));
        }

        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            _state_home: state_home,
        };

        let initialize_response = client.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {
                    "name": "1up-test",
                    "version": "0"
                }
            }),
        );
        client.notify("notifications/initialized", serde_json::json!({}));
        (client, initialize_response)
    }

    fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let response = self.request(
            "tools/call",
            serde_json::json!({
                "name": name,
                "arguments": arguments
            }),
        );
        response["result"].clone()
    }

    fn list_tools(&mut self) -> serde_json::Value {
        self.request("tools/list", serde_json::json!({}))["result"].clone()
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.write(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));

        loop {
            let mut line = String::new();
            let bytes = self.stdout.read_line(&mut line).unwrap();
            assert!(bytes > 0, "MCP server closed stdout before response {id}");
            let response: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
            if response["id"].as_u64() == Some(id) {
                return response;
            }
        }
    }

    fn notify(&mut self, method: &str, params: serde_json::Value) {
        self.write(serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }));
    }

    fn write(&mut self, value: serde_json::Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }
}

fn mcp_structured(result: &serde_json::Value) -> &serde_json::Value {
    &result["structuredContent"]
}

fn wait_for_mcp_last_update_complete(client: &mut McpTestClient) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
        let envelope = mcp_structured(&result);
        let state = envelope["data"]["last_update_state"].as_str();
        if state == Some("complete") {
            return result;
        }
        if state == Some("failed") {
            panic!("daemon update failed; last status={result}");
        }
        if Instant::now() >= deadline {
            panic!("daemon did not reach last_update_state=complete; last status={result}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Waits until the MCP index reaches searchable readiness (`ready`/`degraded`
/// with stored segments) — the correct synchronization point for tests that
/// assert the index was *built and is searchable*.
///
/// Unlike [`wait_for_mcp_last_update_complete`], this does not wait on the
/// daemon's refresh bookkeeping (`last_update_state`). After a one-shot
/// `index_if_missing`, the daemon's own refresh can legitimately stay `pending`
/// (it defers to the competing one-shot rebuild and only resumes on the next
/// file event), so waiting on `last_update_state` there is a load-sensitive
/// flake even though the index is already complete and searchable.
fn wait_for_mcp_searchable_readiness(client: &mut McpTestClient) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
        let envelope = mcp_structured(&result);
        let status = envelope["status"].as_str();
        let segments = envelope["data"]["total_segments"].as_u64().unwrap_or(0);
        if matches!(status, Some("ready" | "degraded")) && segments > 0 {
            return result;
        }
        if status == Some("blocked") {
            panic!("indexing reported blocked; last status={result}");
        }
        if Instant::now() >= deadline {
            panic!("index did not reach searchable readiness; last status={result}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_mcp_text_matches_summary(result: &serde_json::Value) {
    assert_eq!(
        result["content"][0]["text"],
        result["structuredContent"]["summary"]
    );
}

fn assert_mcp_next_actions_are_canonical(envelope: &serde_json::Value) {
    let actions = envelope["next_actions"]
        .as_array()
        .expect("next_actions must be an array");
    assert!(
        !actions.is_empty(),
        "MCP tool envelopes should include a next action"
    );
    for action in actions {
        let tool = action["tool"]
            .as_str()
            .expect("next action tool must be a string");
        assert!(
            RETAINED_PUBLIC_TOOLS.contains(&tool),
            "next action should name retained oneup tools only: {action:?}"
        );
    }
}

fn assert_mcp_response_is_presentation_free(result: &serde_json::Value) {
    assert_mcp_text_matches_summary(result);
    assert!(
        result["structuredContent"]["data"].is_object(),
        "MCP structured content should carry object-shaped data: {result:?}"
    );
    assert_mcp_next_actions_are_canonical(mcp_structured(result));
    assert_value_strings_are_presentation_free("MCP response", result);
}

fn assert_value_strings_are_presentation_free(label: &str, value: &serde_json::Value) {
    match value {
        serde_json::Value::String(text) => assert_text_is_presentation_free(label, text),
        serde_json::Value::Array(items) => {
            for item in items {
                assert_value_strings_are_presentation_free(label, item);
            }
        }
        serde_json::Value::Object(entries) => {
            for (key, value) in entries {
                if key == "content" {
                    continue;
                }
                assert_value_strings_are_presentation_free(label, value);
            }
        }
        _ => {}
    }
}

fn assert_text_is_presentation_free(label: &str, text: &str) {
    assert!(
        !text
            .as_bytes()
            .windows(2)
            .any(|window| window == [0x1b, b'[']),
        "{label} should not include ANSI color/control sequences: {text:?}"
    );
    for ch in text.chars() {
        let codepoint = ch as u32;
        assert!(
            !(0x2500..=0x257f).contains(&codepoint),
            "{label} should not include box/table drawing characters: {text:?}"
        );
        assert!(
            !(0x2800..=0x28ff).contains(&codepoint),
            "{label} should not include spinner glyphs: {text:?}"
        );
    }
    assert!(
        !text.lines().any(|line| {
            let trimmed = line.trim();
            trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
        }),
        "{label} should not include terminal-oriented table rows: {text:?}"
    );
}

fn write_running_progress(project: &Path) {
    fs::create_dir_all(project.join(".1up")).unwrap();
    fs::write(
        project.join(".1up").join("index_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "state": "running",
            "phase": "scanning",
            "files_total": 1,
            "files_scanned": 0,
            "files_processed": 0,
            "files_indexed": 0,
            "files_skipped": 0,
            "files_deleted": 0,
            "segments_stored": 0,
            "embeddings_enabled": true,
            "message": "test indexing",
            "updated_at": "2026-04-26T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_running_progress_for_context(project: &Path, context_id: &str) {
    fs::create_dir_all(project.join(".1up")).unwrap();
    fs::write(
        project.join(".1up").join("index_status.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "state": "running",
            "phase": "scanning",
            "context_id": context_id,
            "source_root": project,
            "branch_name": "other",
            "branch_status": "named",
            "files_total": 1,
            "files_scanned": 0,
            "files_processed": 0,
            "files_indexed": 0,
            "files_skipped": 0,
            "files_deleted": 0,
            "segments_stored": 0,
            "embeddings_enabled": true,
            "message": "other context indexing",
            "updated_at": "2026-04-26T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(future)
}

fn seed_current_index_for_context(project: &Path, context_id: &str) {
    fs::create_dir_all(project.join(".1up")).unwrap();
    fs::write(
        project.join(".1up").join("project_id"),
        "context-count-project",
    )
    .unwrap();

    block_on(async {
        let db = Db::open_rw(&project.join(".1up").join("index.db"))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
        let segment = SegmentInsert {
            id: format!("{context_id}-segment"),
            file_path: "src/other.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "pub fn other_context_only() {}\n".to_string(),
            line_start: 1,
            line_end: 1,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "source".to_string(),
            defined_symbols: "[]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "other-context-hash".to_string(),
        };
        let meta = IndexedFileMeta {
            extension: "rs".to_string(),
            file_hash: segment.file_hash.clone(),
            file_size: segment.content.len() as i64,
            modified_ns: 1,
        };
        segments::replace_file_segments_for_context_tx_with_meta(
            &conn,
            context_id,
            "src/other.rs",
            &[segment],
            Some(&meta),
        )
        .await
        .unwrap();
    });
}

/// Creates a current-schema index with no rows at all: the ready-but-empty
/// state REQ-010 distinguishes from a missing/unready index.
fn seed_ready_empty_index(project: &Path) {
    fs::create_dir_all(project.join(".1up")).unwrap();
    fs::write(
        project.join(".1up").join("project_id"),
        "overview-empty-project",
    )
    .unwrap();

    block_on(async {
        let db = Db::open_rw(&project.join(".1up").join("index.db"))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();
    });
}

/// Inserts leak-sensitive rows for a foreign worktree context into an
/// existing current-schema index: a distinctive language, top-level module,
/// DEFINITION-role struct segment, and defined symbol that would surface in
/// digest statistics, modules, entry points, or symbols if context scoping
/// broke.
fn seed_foreign_context_overview_rows(project: &Path, context_id: &str) {
    block_on(async {
        let db = Db::open_rw(&project.join(".1up").join("index.db"))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let segment = SegmentInsert {
            id: format!("{context_id}-overview-leak"),
            file_path: "foreignctx/leak.go".to_string(),
            language: "go".to_string(),
            block_type: "struct".to_string(),
            content: "type ForeignLeakWidget struct {\n\tValue int\n}\n".to_string(),
            line_start: 1,
            line_end: 3,
            content_key: None,
            embedding_vec: None,
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"ForeignLeakWidget\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: "foreign-overview-hash".to_string(),
        };
        let meta = IndexedFileMeta {
            extension: "go".to_string(),
            file_hash: segment.file_hash.clone(),
            file_size: segment.content.len() as i64,
            modified_ns: 1,
        };
        segments::replace_file_segments_for_context_tx_with_meta(
            &conn,
            context_id,
            "foreignctx/leak.go",
            &[segment],
            Some(&meta),
        )
        .await
        .unwrap();
    });
}

/// The 384-dim vector JSON that a `SegmentInsert` carries as its pool "miss"
/// payload (`embedding_vec`), mirroring the indexer write contract. The fill
/// value only has to be deterministic; pooling here is driven by the explicit
/// `content_key`, the dedup primitive the production `embedding_content_key`
/// produces.
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

/// Re-index / replace-in-place refcount reconciliation (REQ-002 delta-on-re-embed).
/// Re-writing a file whose content_key is UNCHANGED nets zero (the replace path
/// decrements via the `segments_vector_ad` trigger, then re-increments). Re-writing
/// with a CHANGED content_key seeds the new key at 1 and decrements the superseded
/// key to 0 — that orphan is reclaimed later by `delete_context`'s sweep, not by the
/// per-file replace path (orphan reclamation is centralized in `delete_context`).
#[test]
fn pooled_reindex_reconciles_ref_counts() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".1up")).unwrap();
    let db_path = root.join(".1up").join("index.db");

    let seg = |file: &str, key: &str, vector: &str, line: i64| -> SegmentInsert {
        SegmentInsert {
            id: format!("rx-{file}-seg"),
            file_path: file.to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: format!("pub fn item_{line}() {{}}\n"),
            line_start: line,
            line_end: line,
            content_key: Some(key.to_string()),
            embedding_vec: Some(vector.to_string()),
            breadcrumb: None,
            complexity: 1,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"item\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            referenced_relations: "[]".to_string(),
            called_symbols: "[]".to_string(),
            called_relations: "[]".to_string(),
            file_hash: format!("rx-{file}-hash"),
        }
    };

    block_on(async {
        let db = Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        // Initial index: stable.rs (k-stable) + churn.rs (k-v1).
        write_pooled_segment(
            &conn,
            "rx",
            "stable.rs",
            seg("stable.rs", "k-stable", &pool_vector_json(0.30), 1),
        )
        .await;
        write_pooled_segment(
            &conn,
            "rx",
            "churn.rs",
            seg("churn.rs", "k-v1", &pool_vector_json(0.40), 2),
        )
        .await;
        assert_eq!(pool_ref_count(&conn, "k-stable").await, Some(1));
        assert_eq!(pool_ref_count(&conn, "k-v1").await, Some(1));
        assert_eq!(pool_row_count(&conn).await, 2);

        // Re-index stable.rs with the SAME content_key (unchanged content): net-zero.
        write_pooled_segment(
            &conn,
            "rx",
            "stable.rs",
            seg("stable.rs", "k-stable", &pool_vector_json(0.30), 1),
        )
        .await;
        assert_eq!(
            pool_ref_count(&conn, "k-stable").await,
            Some(1),
            "re-indexing unchanged content nets zero (decrement-then-reinsert keeps ref_count at 1)"
        );

        // Re-index churn.rs with a CHANGED content_key: new key seeded, old key decremented.
        write_pooled_segment(
            &conn,
            "rx",
            "churn.rs",
            seg("churn.rs", "k-v2", &pool_vector_json(0.50), 2),
        )
        .await;
        assert_eq!(
            pool_ref_count(&conn, "k-v2").await,
            Some(1),
            "changed content seeds the new content_key at ref_count 1"
        );
        assert_eq!(
            pool_ref_count(&conn, "k-v1").await,
            Some(0),
            "the superseded content_key is decremented to 0 (orphan reclaimed later by delete_context, not the replace path)"
        );
        assert_eq!(
            pool_ref_count(&conn, "k-stable").await,
            Some(1),
            "the unrelated file's content_key is untouched by churn.rs re-index"
        );
    });
}

/// Cross-context dedup (REQ-001) and delta-only embedding (REQ-002) through the
/// production per-file write path (`replace_file_segments_for_context_tx_with_meta`).
///
/// Cold-start: a fresh context with all-distinct content stores one pool row per
/// segment (REQ-002 no-regression — everything is embedded). A second context that
/// reuses one content's `content_key` and adds one new content grows the pool by
/// exactly one row — the shared content is reused, not re-stored, so only the delta
/// would reach the embedder (REQ-002). The shared embedding ends with `ref_count == 2`
/// (REQ-001: stored once, referenced by both contexts); each unique content keeps
/// `ref_count == 1`.
#[test]
fn pooled_index_dedups_shared_content_and_embeds_only_deltas() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".1up")).unwrap();
    let db_path = root.join(".1up").join("index.db");

    // Identical content across contexts => identical content_key + identical
    // bytes => one shared pool row. Distinct contents get distinct keys/bytes.
    let shared_key = "key-shared".to_string();
    let shared_vec = pool_vector_json(0.10);

    let pooled_segment =
        |context: &str, file: &str, key: &str, vector: &str, line: i64| -> SegmentInsert {
            SegmentInsert {
                id: format!("{context}-{file}-seg"),
                file_path: file.to_string(),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!("pub fn item_{line}() {{}}\n"),
                line_start: line,
                line_end: line,
                content_key: Some(key.to_string()),
                embedding_vec: Some(vector.to_string()),
                breadcrumb: None,
                complexity: 1,
                role: "DEFINITION".to_string(),
                defined_symbols: "[\"item\"]".to_string(),
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

        // Cold start (ctx-a): two files with distinct content -> two pool rows,
        // each referenced once. No prior overlap, so everything is "embedded".
        let unique_a = pool_vector_json(0.20);
        write_pooled_segment(
            &conn,
            "ctx-a",
            "shared.rs",
            pooled_segment("ctx-a", "shared.rs", &shared_key, &shared_vec, 1),
        )
        .await;
        write_pooled_segment(
            &conn,
            "ctx-a",
            "only_a.rs",
            pooled_segment("ctx-a", "only_a.rs", "key-only-a", &unique_a, 2),
        )
        .await;

        assert_eq!(
            pool_row_count(&conn).await,
            2,
            "cold-start context embeds every distinct content (one pool row each)"
        );
        assert_eq!(pool_ref_count(&conn, &shared_key).await, Some(1));
        assert_eq!(pool_ref_count(&conn, "key-only-a").await, Some(1));

        // Second context (ctx-b): reuses the shared content_key and adds one new
        // content. The pool grows by exactly one row -> the shared content was
        // reused (only the delta would be embedded), not re-stored.
        let unique_b = pool_vector_json(0.30);
        write_pooled_segment(
            &conn,
            "ctx-b",
            "shared.rs",
            pooled_segment("ctx-b", "shared.rs", &shared_key, &shared_vec, 1),
        )
        .await;
        write_pooled_segment(
            &conn,
            "ctx-b",
            "only_b.rs",
            pooled_segment("ctx-b", "only_b.rs", "key-only-b", &unique_b, 3),
        )
        .await;

        assert_eq!(
            pool_row_count(&conn).await,
            3,
            "overlapping context stores only its new content (shared content reused, not re-embedded)"
        );
        assert_eq!(
            pool_ref_count(&conn, &shared_key).await,
            Some(2),
            "the shared embedding is stored once and referenced by both contexts (REQ-001)"
        );
        assert_eq!(pool_ref_count(&conn, "key-only-a").await, Some(1));
        assert_eq!(pool_ref_count(&conn, "key-only-b").await, Some(1));

        // Every pool row's ref_count equals its live referencing-row count.
        let mut drift = conn
            .query(
                "SELECT COUNT(*) FROM embedding_pool AS p \
                 WHERE p.ref_count != (\
                    SELECT COUNT(*) FROM segment_vectors AS sv \
                    WHERE sv.content_key = p.content_key\
                 )",
                (),
            )
            .await
            .unwrap();
        let drifted: i64 = drift.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(
            drifted, 0,
            "no pool row's ref_count may drift from its references"
        );
    });
}

/// `delete_context` must remove every row for the target context — and only that
/// context. Guards the `1up gc --apply` deletion path: context scoping has to isolate
/// the prune so a live context is never collateral-damaged, and the pruned context
/// must also disappear from `list_worktree_contexts` (its `worktree_contexts` row).
///
/// Adapted to the shared-store model (REQ-004/005): both contexts share one pooled
/// embedding, so pruning one must leave the shared vector intact for the survivor
/// (reference-counted, not unconditional, deletion) rather than orphaning it.
#[test]
fn delete_context_removes_only_the_target_context() {
    use oneup::shared::types::{BranchStatus, WorktreeContext, WorktreeRole};

    fn context(root: &Path, id: &str) -> WorktreeContext {
        WorktreeContext {
            context_id: id.to_string(),
            state_root: root.to_path_buf(),
            source_root: root.to_path_buf(),
            main_worktree_root: root.to_path_buf(),
            worktree_role: WorktreeRole::Main,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some(id.to_string()),
            branch_ref: Some(format!("refs/heads/{id}")),
            head_oid: None,
            branch_status: BranchStatus::Named,
        }
    }

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".1up")).unwrap();
    let db_path = root.join(".1up").join("index.db");

    block_on(async {
        let db = Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        // Both contexts reference one shared pooled embedding (same content_key +
        // bytes), so deleting one must reference-count it down rather than orphan
        // the survivor's vector.
        let shared_key = "key-shared-del".to_string();
        let shared_vec = pool_vector_json(0.40);

        for id in ["ctx-keep", "ctx-prune"] {
            let file = format!("{id}/a.rs");
            let segment = SegmentInsert {
                id: format!("{id}-seg"),
                file_path: file.clone(),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!("pub fn {id}() {{}}\n"),
                line_start: 1,
                line_end: 1,
                content_key: Some(shared_key.clone()),
                embedding_vec: Some(shared_vec.clone()),
                breadcrumb: None,
                complexity: 1,
                role: "DEFINITION".to_string(),
                defined_symbols: format!("[\"{id}\"]"),
                referenced_symbols: "[]".to_string(),
                referenced_relations: "[]".to_string(),
                called_symbols: "[]".to_string(),
                called_relations: "[]".to_string(),
                file_hash: format!("{id}-hash"),
            };
            let meta = IndexedFileMeta {
                extension: "rs".to_string(),
                file_hash: segment.file_hash.clone(),
                file_size: segment.content.len() as i64,
                modified_ns: 1,
            };
            segments::replace_file_segments_for_context_tx_with_meta(
                &conn,
                id,
                &file,
                &[segment],
                Some(&meta),
            )
            .await
            .unwrap();
            segments::upsert_worktree_context(&conn, &context(&root, id), "proj")
                .await
                .unwrap();
        }

        // Both contexts are present before the prune.
        let listed = segments::list_worktree_contexts(&conn).await.unwrap();
        assert_eq!(listed.len(), 2, "both contexts recorded before prune");

        // The shared embedding is stored once and referenced by both contexts.
        assert_eq!(
            pool_row_count(&conn).await,
            1,
            "the shared content is pooled exactly once"
        );
        assert_eq!(pool_ref_count(&conn, &shared_key).await, Some(2));

        let counts = segments::delete_context(&conn, "ctx-prune").await.unwrap();
        assert_eq!(
            counts.segments, 1,
            "one segment removed for the pruned context"
        );
        assert_eq!(counts.indexed_files, 1, "one indexed file removed");

        // The pruned context is gone from every context-scoped surface...
        assert_eq!(
            segments::count_segments_for_context(&conn, "ctx-prune")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            segments::count_files_for_context(&conn, "ctx-prune")
                .await
                .unwrap(),
            0
        );
        // ...including its worktree_contexts registry row.
        let remaining = segments::list_worktree_contexts(&conn).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].context_id, "ctx-keep");

        // The untouched context keeps all of its rows.
        assert_eq!(
            segments::count_segments_for_context(&conn, "ctx-keep")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            segments::count_files_for_context(&conn, "ctx-keep")
                .await
                .unwrap(),
            1
        );

        // Reference-counted deletion (REQ-004/005): pruning ctx-prune decremented
        // the shared embedding to a single reference rather than deleting it, so
        // the survivor's vector is intact and still resolves through the pool.
        assert_eq!(
            pool_row_count(&conn).await,
            1,
            "a still-referenced shared embedding survives a context delete"
        );
        assert_eq!(
            pool_ref_count(&conn, &shared_key).await,
            Some(1),
            "the surviving context leaves ref_count == 1"
        );
        let mut resolves = conn
            .query(
                "SELECT COUNT(*) FROM segment_vectors AS sv \
                 JOIN embedding_pool AS p ON p.content_key = sv.content_key \
                 WHERE sv.segment_id = ?1",
                ["ctx-keep-seg"],
            )
            .await
            .unwrap();
        let keep_resolves: i64 = resolves.next().await.unwrap().unwrap().get(0).unwrap();
        assert_eq!(
            keep_resolves, 1,
            "the surviving context still resolves the shared embedding through the pool"
        );

        // Removing the last referencer (ctx-keep) drops ref_count to zero, so the
        // delete-at-zero sweep reclaims the now-orphaned pooled embedding (REQ-004).
        segments::delete_context(&conn, "ctx-keep").await.unwrap();
        assert_eq!(
            pool_row_count(&conn).await,
            0,
            "removing the last referencer frees the shared pooled embedding"
        );
    });
}

/// The daemon's startup auto-prune must delete exactly the contexts whose source
/// worktree directory is gone — selected against the real filesystem — and leave a
/// live context untouched. Guards the `worker::source_missing_context_ids` ->
/// `segments::delete_context` composition end to end: real-`exists` selection (not
/// a hardcoded id) drives which context's rows are removed, the live one's rows and
/// its `worktree_contexts` row survive, and no stale-branch snapshot of the live
/// worktree is ever in scope.
///
/// Adapted to the shared-store model (REQ-005): both contexts share one pooled
/// embedding, so the prune must reference-count it down and leave the live
/// context's vector intact rather than deleting it.
#[cfg(unix)]
#[test]
fn startup_prune_removes_only_source_missing_contexts() {
    use oneup::daemon::worker::source_missing_context_ids;
    use oneup::shared::types::{BranchStatus, WorktreeContext, WorktreeRole};

    fn context(state_root: &Path, source_root: &Path, id: &str) -> WorktreeContext {
        WorktreeContext {
            context_id: id.to_string(),
            state_root: state_root.to_path_buf(),
            source_root: source_root.to_path_buf(),
            main_worktree_root: state_root.to_path_buf(),
            worktree_role: WorktreeRole::Linked,
            git_dir: None,
            common_git_dir: None,
            branch_name: Some(id.to_string()),
            branch_ref: Some(format!("refs/heads/{id}")),
            head_oid: None,
            branch_status: BranchStatus::Named,
        }
    }

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".1up")).unwrap();
    let db_path = root.join(".1up").join("index.db");

    // One shared index with two linked-worktree contexts. `ctx-live`'s source
    // exists; `ctx-gone`'s source is created then removed, modelling a deleted
    // worktree whose context rows linger in the shared index.
    let live_source = root.join("live-worktree");
    let gone_source = root.join("gone-worktree");
    fs::create_dir_all(&live_source).unwrap();
    fs::create_dir_all(&gone_source).unwrap();
    fs::remove_dir_all(&gone_source).unwrap();
    assert!(live_source.exists());
    assert!(!gone_source.exists());

    block_on(async {
        let db = Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        schema::initialize(&conn).await.unwrap();

        // Both linked-worktree contexts reference one shared pooled embedding, so
        // the startup prune of the source-missing context must reference-count it
        // down rather than delete a vector the live context still uses.
        let shared_key = "key-shared-prune".to_string();
        let shared_vec = pool_vector_json(0.50);

        for (id, source_root) in [("ctx-live", &live_source), ("ctx-gone", &gone_source)] {
            let file = format!("{id}/a.rs");
            let segment = SegmentInsert {
                id: format!("{id}-seg"),
                file_path: file.clone(),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!("pub fn {id}() {{}}\n"),
                line_start: 1,
                line_end: 1,
                content_key: Some(shared_key.clone()),
                embedding_vec: Some(shared_vec.clone()),
                breadcrumb: None,
                complexity: 1,
                role: "DEFINITION".to_string(),
                defined_symbols: format!("[\"{id}\"]"),
                referenced_symbols: "[]".to_string(),
                referenced_relations: "[]".to_string(),
                called_symbols: "[]".to_string(),
                called_relations: "[]".to_string(),
                file_hash: format!("{id}-hash"),
            };
            let meta = IndexedFileMeta {
                extension: "rs".to_string(),
                file_hash: segment.file_hash.clone(),
                file_size: segment.content.len() as i64,
                modified_ns: 1,
            };
            segments::replace_file_segments_for_context_tx_with_meta(
                &conn,
                id,
                &file,
                &[segment],
                Some(&meta),
            )
            .await
            .unwrap();
            segments::upsert_worktree_context(&conn, &context(&root, source_root, id), "proj")
                .await
                .unwrap();
        }

        // Select against the real filesystem: only the gone worktree's context.
        let listed = segments::list_worktree_contexts(&conn).await.unwrap();
        let pruned = source_missing_context_ids(&listed, &|p: &Path| p.exists());
        assert_eq!(
            pruned,
            vec!["ctx-gone".to_string()],
            "only the source-missing context is selected; the live one is retained"
        );

        // The shared embedding is stored once and referenced by both contexts.
        assert_eq!(
            pool_row_count(&conn).await,
            1,
            "the shared content is pooled exactly once"
        );
        assert_eq!(pool_ref_count(&conn, &shared_key).await, Some(2));

        // Apply the prune exactly as the startup routine does.
        for context_id in &pruned {
            segments::delete_context(&conn, context_id).await.unwrap();
        }

        // The source-missing context is gone from every context-scoped surface...
        assert_eq!(
            segments::count_segments_for_context(&conn, "ctx-gone")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            segments::count_files_for_context(&conn, "ctx-gone")
                .await
                .unwrap(),
            0
        );
        // ...including its worktree_contexts registry row.
        let remaining = segments::list_worktree_contexts(&conn).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].context_id, "ctx-live");

        // The live context keeps all of its rows.
        assert_eq!(
            segments::count_segments_for_context(&conn, "ctx-live")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            segments::count_files_for_context(&conn, "ctx-live")
                .await
                .unwrap(),
            1
        );

        // The startup prune is reference-aware (REQ-005): removing the
        // source-missing context decremented the shared embedding to one
        // reference instead of deleting a vector the live context still uses.
        assert_eq!(
            pool_row_count(&conn).await,
            1,
            "the live context's shared embedding survives the startup prune"
        );
        assert_eq!(
            pool_ref_count(&conn, &shared_key).await,
            Some(1),
            "the surviving live context leaves ref_count == 1"
        );
    });
}

impl Drop for McpTestClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Killing the `mcp` child does not stop the separate daemon worker the
        // server may have spawned under this client's isolated HOME, which would
        // then be orphaned when the temp HOME is removed. Best-effort reap by
        // pid (panic-safe: runs during unwind and ignores every failure).
        #[cfg(unix)]
        if let Some(home) = &self._state_home {
            let pid_path = test_data_dir(home.path()).join("daemon.pid");
            if let Ok(raw) = fs::read_to_string(&pid_path) {
                if let Ok(pid) = raw.trim().parse::<i32>() {
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                }
            }
        }
    }
}

struct RestoreHiddenModelGuard {
    model_path: PathBuf,
    hidden_path: PathBuf,
    restored: bool,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl RestoreHiddenModelGuard {
    fn new() -> Self {
        let lock = MODEL_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let model_dir = dirs::data_dir()
            .unwrap()
            .join("1up")
            .join("models")
            .join("all-MiniLM-L6-v2");
        let model_path = model_dir.join("model.onnx");
        let hidden_path = model_dir.join("model.onnx.hidden_by_test");
        let restored = !model_path.exists() && hidden_path.exists();

        if restored {
            fs::rename(&hidden_path, &model_path).unwrap();
        }

        Self {
            model_path,
            hidden_path,
            restored,
            _lock: lock,
        }
    }
}

impl Drop for RestoreHiddenModelGuard {
    fn drop(&mut self) {
        if self.restored && self.model_path.exists() {
            let _ = fs::rename(&self.model_path, &self.hidden_path);
        }
    }
}

fn create_multi_lang_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();

    fs::write(
        tmp.path().join("main.rs"),
        r#"use std::io;

fn greet(name: &str) -> String {
    format!("Hello, {}", name)
}

struct Config {
    pub host: String,
    pub port: u16,
}

impl Config {
    fn new(host: String, port: u16) -> Self {
        Config { host, port }
    }

    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn main() {
    let cfg = Config::new("localhost".to_string(), 8080);
    println!("{}", greet(&cfg.host));
    println!("Listening on {}", cfg.address());
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("utils.py"),
        r#"import os
import json

def parse_config(path: str) -> dict:
    """Parse a JSON configuration file."""
    with open(path) as f:
        return json.load(f)

class Logger:
    def __init__(self, name: str):
        self.name = name
        self.entries = []

    def log(self, message: str):
        self.entries.append(message)
        print(f"[{self.name}] {message}")

    def flush(self):
        self.entries.clear()

def validate_input(data: str) -> bool:
    return len(data.strip()) > 0
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("handler.js"),
        r#"function handleRequest(req, res) {
    const method = req.method;
    if (method === "GET") {
        return handleGet(req, res);
    }
    return handlePost(req, res);
}

function handleGet(req, res) {
    res.status(200).json({ ok: true });
}

function handlePost(req, res) {
    const body = req.body;
    if (!body) {
        res.status(400).json({ error: "missing body" });
        return;
    }
    res.status(201).json({ created: true });
}

module.exports = { handleRequest, handleGet, handlePost };
"#,
    )
    .unwrap();

    tmp
}

fn init_and_index(dir: &TempDir) {
    cmd()
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd()
        .args(["index", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
}

fn init_and_index_fts_only(dir: &TempDir) -> HideModelGuard {
    let guard = HideModelGuard::new();

    cmd()
        .args(["init", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd()
        .args(["index", dir.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    guard
}

fn init_project(dir: &std::path::Path) {
    cmd()
        .args(["init", dir.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
}

fn run_index_json(dir: &std::path::Path, extra_args: &[&str]) -> serde_json::Value {
    let mut command = cmd();
    command.arg("index");
    for arg in extra_args {
        command.arg(arg);
    }
    command.arg(dir);
    command.arg("--format").arg("json");

    let output = command.output().unwrap();
    assert!(output.status.success());

    serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
}

// =============================================================================
// Lean row grammar helpers
// =============================================================================
//
// Hidden discovery commands emit this grammar by default. Retained human
// commands use it only through `--plain`, so helpers that parse these rows must
// opt in explicitly.

/// A discovery row produced by `search`, `symbol`, or `impact`:
/// `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>[  ~<channel>]`.
#[derive(Debug, Clone)]
struct LeanDiscoveryRow {
    score: u32,
    file_path: String,
    line_start: usize,
    line_end: usize,
    kind: String,
    breadcrumb: String,
    symbol: String,
    segment_id: String,
    channel: Option<char>,
}

fn parse_discovery_row(line: &str) -> LeanDiscoveryRow {
    // Fields are separated by two ASCII spaces (design D2). We split on the
    // fixed separator rather than on whitespace so that single spaces inside
    // e.g. breadcrumbs are not misread as a field break.
    let parts: Vec<&str> = line.split("  ").collect();
    assert!(
        parts.len() == 5 || parts.len() == 6,
        "expected 5 or 6 double-space-separated fields, got {} in line: {line:?}",
        parts.len()
    );

    let score: u32 = parts[0]
        .parse()
        .unwrap_or_else(|_| panic!("score field must be integer 0-100, got {:?}", parts[0]));
    assert!(
        score <= 100,
        "score must be in [0,100], got {score} in line: {line:?}"
    );

    let (file_path, line_span) = parts[1]
        .rsplit_once(':')
        .unwrap_or_else(|| panic!("expected <path>:<l1>-<l2>, got {:?}", parts[1]));
    let (l1_raw, l2_raw) = line_span
        .split_once('-')
        .unwrap_or_else(|| panic!("expected <l1>-<l2>, got {line_span:?}"));
    let line_start: usize = l1_raw.parse().expect("l1 is integer");
    let line_end: usize = l2_raw.parse().expect("l2 is integer");

    let (breadcrumb, symbol) = parts[3]
        .split_once("::")
        .unwrap_or_else(|| panic!("expected <breadcrumb>::<symbol>, got {:?}", parts[3]));

    let segment_token = parts[4];
    assert!(
        segment_token.starts_with(':'),
        "segment handle must start with ':', got {segment_token:?}"
    );
    let segment_id = segment_token.trim_start_matches(':').to_string();
    assert!(
        !segment_id.is_empty(),
        "segment id body must be non-empty in {line:?}"
    );

    let channel = if parts.len() == 6 {
        let suffix = parts[5];
        assert!(
            suffix == "~P" || suffix == "~C",
            "channel suffix must be ~P or ~C, got {suffix:?}"
        );
        Some(suffix.chars().nth(1).unwrap())
    } else {
        None
    };

    LeanDiscoveryRow {
        score,
        file_path: file_path.to_string(),
        line_start,
        line_end,
        kind: parts[2].to_string(),
        breadcrumb: breadcrumb.to_string(),
        symbol: symbol.to_string(),
        segment_id,
        channel,
    }
}

fn parse_discovery_rows(stdout: &str) -> Vec<LeanDiscoveryRow> {
    stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(parse_discovery_row)
        .collect()
}

fn run_core_cmd(args: &[&str]) -> (String, String, bool) {
    let output = cmd().args(args).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    (stdout, stderr, output.status.success())
}

fn run_core_cmd_with_home(home: &Path, args: &[&str]) -> (String, String, bool) {
    let output = cmd_with_home(home).args(args).output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    (stdout, stderr, output.status.success())
}

fn search_rows(dir: &std::path::Path, query: &str) -> Vec<LeanDiscoveryRow> {
    let (stdout, stderr, ok) = run_core_cmd(&["search", query, "--path", dir.to_str().unwrap()]);
    assert!(ok, "search failed: {stderr}");
    parse_discovery_rows(&stdout)
}

fn search_rows_with_limit(
    dir: &std::path::Path,
    query: &str,
    limit: usize,
) -> Vec<LeanDiscoveryRow> {
    let limit = limit.to_string();
    let (stdout, stderr, ok) = run_core_cmd(&[
        "search",
        query,
        "-n",
        &limit,
        "--path",
        dir.to_str().unwrap(),
    ]);
    assert!(ok, "search failed: {stderr}");
    parse_discovery_rows(&stdout)
}

fn search_rows_with_home(home: &Path, dir: &std::path::Path, query: &str) -> Vec<LeanDiscoveryRow> {
    let (stdout, stderr, ok) =
        run_core_cmd_with_home(home, &["search", query, "--path", dir.to_str().unwrap()]);
    assert!(ok, "search failed: {stderr}");
    parse_discovery_rows(&stdout)
}

fn git_output(repo: &std::path::Path, args: &[&str]) -> std::process::Output {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|err| panic!("git {args:?} failed to launch: {err}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git(repo: &std::path::Path, args: &[&str]) {
    git_output(repo, args);
}

fn create_branch_filtering_fixture() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let main_repo = root.join("main");
    let feature_worktree = root.join("feature-worktree");
    fs::create_dir_all(&main_repo).unwrap();

    git(&root, &["init", main_repo.to_str().unwrap()]);
    git(
        &main_repo,
        &["config", "user.email", "oneup-test@example.com"],
    );
    git(&main_repo, &["config", "user.name", "1up Test"]);
    fs::write(
        main_repo.join("shared.rs"),
        "pub fn shared_branch_acceptance_marker() -> &'static str { \"shared branch acceptance sentinel\" }\n",
    )
    .unwrap();
    git(&main_repo, &["add", "."]);
    git(&main_repo, &["commit", "-m", "shared"]);
    git(&main_repo, &["branch", "-M", "main"]);
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature-acceptance",
            feature_worktree.to_str().unwrap(),
            "HEAD",
        ],
    );

    fs::write(
        main_repo.join("main_only.rs"),
        "pub fn main_branch_acceptance_marker() -> &'static str { \"main branch only acceptance sentinel\" }\n",
    )
    .unwrap();
    git(&main_repo, &["add", "."]);
    git(&main_repo, &["commit", "-m", "main only"]);

    fs::write(
        feature_worktree.join("feature_only.rs"),
        "pub fn feature_branch_acceptance_marker() -> &'static str { \"feature branch only acceptance sentinel\" }\n",
    )
    .unwrap();

    (
        tmp,
        main_repo.canonicalize().unwrap(),
        feature_worktree.canonicalize().unwrap(),
    )
}

fn symbol_rows(dir: &std::path::Path, name: &str, extra: &[&str]) -> Vec<LeanDiscoveryRow> {
    let mut args: Vec<&str> = vec!["symbol", name, "--plain", "--path", dir.to_str().unwrap()];
    args.extend_from_slice(extra);
    let (stdout, _stderr, ok) = run_core_cmd(&args);
    assert!(ok, "symbol lookup failed");
    parse_discovery_rows(&stdout)
}

fn impact_output(dir: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let mut full: Vec<&str> = vec!["impact", "--plain"];
    full.extend_from_slice(args);
    full.extend_from_slice(&["--path", dir.to_str().unwrap()]);
    run_core_cmd(&full)
}

fn impact_rows(dir: &std::path::Path, args: &[&str]) -> Vec<LeanDiscoveryRow> {
    let (stdout, stderr, ok) = impact_output(dir, args);
    assert!(ok, "impact failed: {stderr}");
    parse_discovery_rows(
        &stdout
            .lines()
            .filter(|l| {
                !l.starts_with("hint") && !l.starts_with("refused") && !l.starts_with("empty")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn write_parallel_regression_fixture(dir: &std::path::Path) {
    fs::write(
        dir.join("changed.rs"),
        "pub fn alpha_symbol() -> &'static str {\n    \"alpha\"\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("stable.rs"),
        "pub fn stable_symbol() -> &'static str {\n    \"stable\"\n}\n",
    )
    .unwrap();
    fs::write(
        dir.join("removed.rs"),
        "pub fn removed_symbol() -> &'static str {\n    \"removed\"\n}\n",
    )
    .unwrap();
}

fn mutate_parallel_regression_fixture(dir: &std::path::Path) {
    fs::write(
        dir.join("changed.rs"),
        "pub fn beta_symbol() -> &'static str {\n    \"beta\"\n}\n",
    )
    .unwrap();
    fs::remove_file(dir.join("removed.rs")).unwrap();
    fs::write(
        dir.join("fresh.rs"),
        "pub fn fresh_symbol() -> &'static str {\n    \"fresh\"\n}\n",
    )
    .unwrap();
}

fn create_search_acceptance_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::create_dir_all(tmp.path().join("docs")).unwrap();
    fs::create_dir_all(tmp.path().join("proto")).unwrap();
    fs::create_dir_all(tmp.path().join("sql")).unwrap();
    fs::create_dir_all(tmp.path().join("benches")).unwrap();
    fs::create_dir_all(tmp.path().join("tests")).unwrap();

    fs::write(
        tmp.path().join("src").join("policy.rs"),
        r#"pub struct PolicyRuleValidator;

impl PolicyRuleValidator {
    pub fn validate(&self, policy: &str) -> bool {
        !policy.is_empty()
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("runner.rs"),
        r#"use crate::policy::PolicyRuleValidator;

pub fn run_validation(validator: &PolicyRuleValidator) -> bool {
    validator.validate("allow")
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("signatures.rs"),
        r#"// validate incoming request signatures
pub fn validate_incoming_request_signatures(secret: &str, header: &str) -> bool {
    !secret.is_empty() && header.contains(secret)
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("config").join("signatures.yaml"),
        r#"request_signing_secret: test-secret
description: request signing secret used for request validation
policy_rule_preview_enabled: true
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("docs").join("signatures.md"),
        r#"# Request signing documentation guide

Use config/signatures.yaml to set the request signing secret for local development.
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("proto").join("policy_rules.proto"),
        r#"syntax = "proto3";

message PolicyRulePreview {
  string id = 1;
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("sql").join("policy_rules.sql"),
        r#"CREATE TABLE policy_rules_preview (
    id TEXT PRIMARY KEY,
    validator_name TEXT NOT NULL
);
"#,
    )
    .unwrap();

    // Bench-named file vs real implementation: the descriptive bench name
    // carries both query terms, the engine carries them only inside one
    // CamelCase token.
    fs::write(
        tmp.path().join("src").join("horizon.rs"),
        r#"pub struct ImpactHorizonEngine {
    pub max_depth: usize,
}

impl ImpactHorizonEngine {
    pub fn expand_horizon(&self) -> usize {
        self.max_depth + 1
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("benches").join("horizon_bench.rs"),
        r#"pub fn bench_impact_horizon() -> usize {
    let engine_depth = 4;
    engine_depth + 1
}
"#,
    )
    .unwrap();

    // Descriptive test name vs implementation: the snake_case test name is a
    // sentence with near-perfect term coverage for the conceptual query.
    fs::write(
        tmp.path().join("src").join("blast_radius.rs"),
        r#"pub struct BlastRadiusReport {
    pub expansion_depth: usize,
}

/// Folds expansion results into trust buckets for the blast radius report.
pub fn aggregate_bucket(report: &BlastRadiusReport) -> usize {
    report.expansion_depth + 1
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("tests").join("integration_checks.rs"),
        r#"pub fn blast_radius_expansion_preserves_trust_buckets_and_followups() {
    assert!(true);
}
"#,
    )
    .unwrap();

    // In-file `#[cfg(test)] mod tests` vs implementation: the descriptive
    // test fn name carries near-perfect term coverage for the conceptual
    // query, lives in a src path, and is only classifiable through its
    // `tests` breadcrumb component.
    fs::write(
        tmp.path().join("src").join("refinement.rs"),
        r#"pub struct Margin {
    pub pool_size: usize,
}

/// Applies the refinement margin while shrinking the candidate pool.
pub fn shrink_pool(margin: &Margin) -> usize {
    margin.pool_size.saturating_sub(1)
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("pool_state.rs"),
        r#"pub fn pool_state_label() -> &'static str {
    "pool"
}

#[cfg(test)]
mod tests {
    #[test]
    fn refinement_margin_shrinks_candidate_pool_state() {
        assert_eq!(super::pool_state_label(), "pool");
    }
}
"#,
    )
    .unwrap();

    // Inflected query vs stem-named symbol: reachable only through
    // stem-prefix FTS variants (`composed` -> compos*).
    fs::write(
        tmp.path().join("src").join("embedding_pipeline.rs"),
        r#"/// Builds the embedding input for one segment before the indexer stores it.
pub fn compose_embedding_text(language: &str, crumb: &str) -> String {
    format!("{language} {crumb}")
}
"#,
    )
    .unwrap();

    // Heading-matched doc query: the README section competes with a code
    // file that partially matches, and its H1 carries inline HTML noise.
    fs::write(
        tmp.path().join("src").join("daemon_worker.rs"),
        r#"/// Runs the daemon worker loop with watch support.
pub fn spawn_daemon_worker() -> bool {
    true
}
"#,
    )
    .unwrap();

    // Content-only doc relevance: the "What To Expect" heading shares no
    // terms with the cadence query, so only its body can prove relevance.
    // The code competitors below give the query a populated result list
    // whose middle tier (docs-path text chunks) would bury a doubly
    // penalized doc section.
    fs::write(
        tmp.path().join("src").join("snapshot_refresh.rs"),
        r#"/// Plans the next snapshot refresh for a capture window.
pub fn snapshot_refresh_planner(window: usize) -> usize {
    window + 1
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cadence.rs"),
        r#"/// Computes the offline cadence window between captures.
pub fn offline_cadence_window(base: usize) -> usize {
    base * 2
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("snapshot_store.rs"),
        r#"/// Stores one offline snapshot per capture run.
pub fn offline_snapshot_store(slot: usize) -> usize {
    slot.max(1)
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("docs").join("runtime_notes.txt"),
        "runtime notes\nkeep one offline snapshot per machine\nreview the snapshot before rotation\n",
    )
    .unwrap();

    fs::write(
        tmp.path().join("docs").join("startup_notes.txt"),
        "startup notes\nrefresh happens on a steady cadence\nkeep the cadence aligned with capture\n",
    )
    .unwrap();

    fs::write(
        tmp.path().join("README.md"),
        r#"# <img src="assets/logo.png" alt="logo"> Acceptance Project

Intro paragraph.

## Start Here

Setup notes.

## What To Expect

Each offline snapshot gets a refresh on a fixed cadence.

Background sweeps stay quiet and never require manual
restarts between sessions.

## Windows daemon support

The daemon runs as a background service on Windows hosts.
"#,
    )
    .unwrap();

    tmp
}

/// Markdown structural-indexing fixture: a code definition
/// (`render_widget_panel` in `src/widget/core.rs`), a real code reference
/// (`src/widget/dashboard.rs`), and a structured guide
/// (`docs/widget_guide.md`) whose nested headings produce four doc sections
/// (`Widget Guide` 1-4, `Rendering` 5-12, `Theming` 13-14, `Accent Colors`
/// 15-17). Inline and fenced code mentions of `render_widget_panel` attach to
/// the `Rendering` and `Accent Colors` sections.
fn create_markdown_docs_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    fs::create_dir_all(tmp.path().join("src").join("widget")).unwrap();
    fs::create_dir_all(tmp.path().join("docs")).unwrap();

    fs::write(
        tmp.path().join("src").join("widget").join("core.rs"),
        r#"pub fn render_widget_panel() -> &'static str {
    "panel"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("widget").join("dashboard.rs"),
        r#"use crate::widget::core::render_widget_panel;

pub fn draw_dashboard() -> &'static str {
    render_widget_panel()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("docs").join("widget_guide.md"),
        r#"# Widget Guide

Orientation notes for the widget panel stack.

## Rendering

The dashboard surface calls `render_widget_panel` on every refresh tick.

```rust
let panel = render_widget_panel();
```

## Theming

### Accent Colors

Accent color guidance: tint palette swatches for the `render_widget_panel` chrome.
"#,
    )
    .unwrap();

    tmp
}

/// Writes a Rust source file at `repo/src/{name}.rs` whose single top-level
/// function's `function_item` spans exactly `span` lines (1-based inclusive:
/// signature + `span - 2` body statements + closing brace). This gives
/// scope-window tests a deterministic enclosing-scope size so the
/// whole-scope threshold (`MAX_WHOLE_SCOPE_LINES = 101`) can be exercised at
/// its boundary. Returns the repo-relative path for `oneup_context` calls.
fn write_scope_file(repo: &Path, name: &str, span: usize) -> String {
    assert!(
        span >= 3,
        "a scope needs a signature, a body line, and a brace"
    );
    let mut lines = Vec::with_capacity(span);
    lines.push(format!("pub fn {name}() {{"));
    for i in 0..(span - 2) {
        lines.push(format!("    let v{i} = {i};"));
    }
    lines.push("}".to_string());
    let rel = format!("src/{name}.rs");
    let path = repo.join(&rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    rel
}

/// Like [`write_scope_file`] but plants `sentinel` as a unique string literal
/// on the second-to-last body line (near the scope tail). A near-top windowed
/// read omits the tail, so only the truncation note's recovery call retrieves
/// the sentinel — the HYP-002 regression surface. Returns the repo-relative path.
fn write_scope_file_with_tail_sentinel(
    repo: &Path,
    name: &str,
    span: usize,
    sentinel: &str,
) -> String {
    assert!(
        span >= 5,
        "need room for a near-top target and a tail sentinel"
    );
    let body = span - 2;
    let mut lines = Vec::with_capacity(span);
    lines.push(format!("pub fn {name}() {{"));
    for i in 0..body {
        if i == body - 1 {
            lines.push(format!("    let sentinel = \"{sentinel}\";"));
        } else {
            lines.push(format!("    let v{i} = {i};"));
        }
    }
    lines.push("}".to_string());
    let rel = format!("src/{name}.rs");
    let path = repo.join(&rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    rel
}

fn create_ambiguous_handle_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).unwrap();

    for i in 0..20 {
        fs::write(
            src.join(format!("ambiguous_{i:02}.rs")),
            format!("pub fn ambiguous_collision_token_{i:02}() -> usize {{ {i} }}\n"),
        )
        .unwrap();
    }

    tmp
}

fn ambiguous_handle_prefix(handles: &[String]) -> String {
    for prefix_len in 1..=12 {
        let mut counts = std::collections::BTreeMap::new();
        for handle in handles {
            if handle.len() >= prefix_len {
                *counts
                    .entry(handle[..prefix_len].to_string())
                    .or_insert(0usize) += 1;
            }
        }
        if let Some((prefix, _)) = counts.into_iter().find(|(_, count)| *count > 1) {
            return prefix;
        }
    }

    panic!("expected at least one ambiguous handle prefix in {handles:?}");
}

fn create_impact_acceptance_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    for dir in [
        "src/admin",
        "src/app",
        "src/auth",
        "src/cache",
        "src/contracts",
        "src/ui",
        "tests",
    ] {
        fs::create_dir_all(tmp.path().join(dir)).unwrap();
    }

    fs::write(
        tmp.path().join("src").join("auth").join("runtime.rs"),
        r#"pub fn load_auth_config() -> &'static str {
    "auth"
}

pub fn parse_auth_config(raw: &str) -> bool {
    !raw.trim().is_empty()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("bootstrap.rs"),
        r#"use crate::auth::runtime::load_auth_config;

pub fn boot_auth() -> &'static str {
    load_auth_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("tests").join("auth_runtime_test.rs"),
        r#"use crate::auth::runtime::load_auth_config;

#[test]
fn loads_auth_runtime() {
    assert_eq!(load_auth_config(), "auth");
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "auth-scope"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path()
            .join("src")
            .join("auth")
            .join("config_builder.rs"),
        r#"use crate::auth::config::load_config;

pub fn build_auth_config() -> &'static str {
    load_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("reload.rs"),
        r#"pub fn reload_auth_config() -> &'static str {
    crate::auth::config::load_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path()
            .join("src")
            .join("contracts")
            .join("auth_store.ts"),
        r#"export interface BaseAuthStore {
    get(key: string): string | null;
}

export interface AuthStore extends BaseAuthStore {
    set(key: string, value: string): void;
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("auth").join("auth_store.ts"),
        r#"import type { AuthStore } from "../contracts/auth_store";

export class SqlAuthStore implements AuthStore {
    get(key: string): string | null {
        return key;
    }

    set(key: string, value: string): void {
        void value;
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path()
            .join("src")
            .join("contracts")
            .join("formatter.ts"),
        r#"export interface Formatter {
    format(value: string): string;
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("plain_formatter.ts"),
        r#"import type { Formatter } from "../contracts/formatter";

export class PlainFormatter implements Formatter {
    format(value: string): string {
        return value.trim();
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("render_search.ts"),
        r#"import type { Formatter } from "../contracts/formatter";

export function renderSearch(formatter: Formatter, value: string): string {
    return formatter.format(value);
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("render_status.ts"),
        r#"import type { Formatter } from "../contracts/formatter";

export function renderStatus(formatter: Formatter, value: string): string {
    return formatter.format(value);
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "cache"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("runtime.rs"),
        r#"pub fn warm_cache_key() -> &'static str {
    "cache"
}

pub fn normalize_cache_key(raw: &str) -> String {
    raw.trim().to_lowercase()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("priming.rs"),
        r#"use crate::cache::runtime::warm_cache_key;

pub fn prime_cache() -> &'static str {
    warm_cache_key()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("worker.rs"),
        r#"use crate::cache::runtime::{normalize_cache_key, warm_cache_key};

pub fn warm_cache_for_request(user_key: &str) -> String {
    let normalized = normalize_cache_key(user_key);
    if normalized.is_empty() {
        return warm_cache_key().to_string();
    }
    format!("{}:{}", warm_cache_key(), normalized)
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("cache").join("test_support.rs"),
        r#"mod cache_tests {
    use crate::cache::runtime::warm_cache_key;

    fn inline_warm_cache_test() {
        assert_eq!(warm_cache_key(), "cache");
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("ui").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "ui"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("admin").join("config.rs"),
        r#"pub fn load_config() -> &'static str {
    "admin"
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("src").join("app").join("bootstrap.rs"),
        r#"use crate::auth::config::load_config;

pub fn boot_global_config() -> &'static str {
    load_config()
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("tests").join("config_fixture.rs"),
        r#"pub fn load_config() -> &'static str {
    "tests"
}
"#,
    )
    .unwrap();

    tmp
}

/// Multi-directory overview fixture with fully controlled digest contents:
/// one qualifying type (`PolicyEngine` in `src`) referenced from two `app`
/// files plus a second language in `lib`, so statistics, top symbols, the
/// module map, the `app -> src` dependency edge, and entry points are all
/// exactly predictable.
fn create_overview_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();
    for dir in ["src", "app", "lib"] {
        fs::create_dir_all(tmp.path().join(dir)).unwrap();
    }

    fs::write(
        tmp.path().join("src").join("policy.rs"),
        r#"pub struct PolicyEngine {
    pub limit: u32,
}

impl PolicyEngine {
    pub fn allows(&self, value: u32) -> bool {
        value <= self.limit
    }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("app").join("main.rs"),
        r#"use crate::policy::PolicyEngine;

fn main() {
    let engine = PolicyEngine { limit: 8 };
    println!("{}", engine.allows(3));
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("app").join("wiring.rs"),
        r#"use crate::policy::PolicyEngine;

pub fn build_engine() -> PolicyEngine {
    PolicyEngine { limit: 16 }
}
"#,
    )
    .unwrap();

    fs::write(
        tmp.path().join("lib").join("util.py"),
        r#"def helper_limit() -> int:
    return 8
"#,
    )
    .unwrap();

    tmp
}

/// Cap-saturating overview fixture: 16 cross-referencing rust modules (every
/// struct referenced from two other modules' flow files) plus a mixed-language
/// extras module, so every digest section reaches its documented cap and the
/// serialized payload approaches its largest realistic size.
fn create_overview_budget_fixture() -> TempDir {
    let tmp = TempDir::new().unwrap();

    for index in 0..16usize {
        let module = tmp.path().join(format!("module{index:02}"));
        fs::create_dir_all(&module).unwrap();
        fs::write(
            module.join("types.rs"),
            format!("pub struct DigestWidget{index:02} {{\n    pub value: u64,\n}}\n"),
        )
        .unwrap();

        let first = (index + 1) % 16;
        let second = (index + 2) % 16;
        fs::write(
            module.join("flow.rs"),
            format!(
                "use crate::module{first:02}::types::DigestWidget{first:02};\n\
                 use crate::module{second:02}::types::DigestWidget{second:02};\n\n\
                 pub fn flow{index:02}(first: &DigestWidget{first:02}, second: &DigestWidget{second:02}) -> u64 {{\n    \
                 first.value + second.value\n}}\n"
            ),
        )
        .unwrap();
    }

    let extras = tmp.path().join("extras");
    fs::create_dir_all(&extras).unwrap();
    fs::write(
        extras.join("util.py"),
        "def budget_helper():\n    return 1\n",
    )
    .unwrap();
    fs::write(
        extras.join("notes.md"),
        "# Budget fixture\n\nOverview payload sizing notes.\n",
    )
    .unwrap();
    fs::write(extras.join("config.yaml"), "budget: true\nlimit: 8192\n").unwrap();
    fs::write(
        extras.join("schema.sql"),
        "CREATE TABLE budget (id TEXT PRIMARY KEY);\n",
    )
    .unwrap();

    tmp
}

// =============================================================================
// Indexing / storage integration
// =============================================================================

#[test]
fn index_multi_language_repository() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let db_path = tmp.path().join(".1up").join("index.db");
    assert!(
        db_path.exists(),
        "index.db should be created after indexing"
    );
}

// =============================================================================
// Lean row grammar — search / symbol / impact / get
// =============================================================================

#[test]
fn search_row_grammar() {
    // design §2.2: every search hit is one line of
    // `<score>  <path>:<l1>-<l2>  <kind>  <breadcrumb>::<symbol>  :<segment_id>`.
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "Config host port");
    assert!(
        !rows.is_empty(),
        "search for 'Config host port' should return rows"
    );
    for row in &rows {
        assert!(
            !row.file_path.is_empty() && !row.kind.is_empty(),
            "required fields must be populated: {row:?}"
        );
        assert!(
            row.line_end >= row.line_start,
            "l2 >= l1 invariant violated: {row:?}"
        );
        assert!(
            row.channel.is_none(),
            "search rows must not carry a channel suffix: {row:?}"
        );
        // `:<segment_id>` is 1 to 12 chars of lowercase hex (design D3).
        assert!(row.segment_id.len() <= 12);
        assert!(
            row.segment_id
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '_' || c.is_ascii_alphanumeric()),
            "segment id must be ascii alphanumeric hex-ish: {row:?}"
        );
    }
}

#[test]
fn search_default_limit_caps_results_at_three() {
    // design §3.4: `1up search <query>` defaults to -n=3. The fixture
    // produces more than three matches for "config", so we pin the cap.
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "config");
    assert!(
        rows.len() <= 3,
        "default limit is 3, got {} rows",
        rows.len()
    );
}

#[test]
fn search_lean_output_contains_no_segment_prefix_literal() {
    // design D-grammar: the `:<id>` trailing token replaces the old
    // `segment=<id>` metadata substring. Grep-style guard against regressions.
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) =
        run_core_cmd(&["search", "Config", "--path", tmp.path().to_str().unwrap()]);
    assert!(ok);
    assert!(
        !stdout.contains("segment="),
        "lean search output must not include `segment=`: {stdout}"
    );
}

#[test]
fn symbol_uses_same_row_grammar() {
    // Symbol rows reuse the discovery grammar with a `<reference_kind>:<kind>`
    // composite in the kind slot (design §2.2, §3.5).
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = symbol_rows(tmp.path(), "greet", &[]);
    assert!(!rows.is_empty(), "symbol 'greet' should resolve to a row");
    for row in &rows {
        assert!(
            row.kind.starts_with("def:") || row.kind.starts_with("usage:"),
            "symbol kind must be `def:<k>` or `usage:<k>`, got {:?}",
            row.kind
        );
        assert!(
            row.channel.is_none(),
            "symbol rows must not carry a channel suffix: {row:?}"
        );
        assert_eq!(row.score, 0, "symbol rows have no score; grammar fills 0");
    }
    assert!(
        rows.iter()
            .any(|r| r.symbol == "greet" || r.breadcrumb.contains("greet")),
        "greet should appear somewhere in symbol output: {rows:?}"
    );
}

#[test]
fn symbol_references_include_definitions_and_usages() {
    let tmp = create_search_acceptance_fixture();
    init_and_index(&tmp);

    let rows = symbol_rows(tmp.path(), "PolicyRuleValidator", &["--references"]);
    assert!(
        rows.iter()
            .any(|r| r.kind.starts_with("def:") && r.file_path == "src/policy.rs"),
        "definition row missing: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.kind.starts_with("usage:") && r.file_path == "src/runner.rs"),
        "usage row missing: {rows:?}"
    );
}

#[test]
fn symbol_handle_roundtrips_through_get_and_impact() {
    // The advertised flow is `symbol -> get -> impact --from-handle`. The
    // 12-char handle printed by `symbol` must resolve both through `get` (full
    // segment body) and through `impact --from-handle` (anchor expansion).
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = symbol_rows(tmp.path(), "greet", &[]);
    let row = rows
        .iter()
        .find(|r| r.kind.starts_with("def:"))
        .expect("expected a definition row for `greet`");
    let handle = row.segment_id.clone();
    assert_eq!(
        handle.len(),
        12,
        "symbol row must carry a 12-char lean handle, got {handle:?}"
    );

    let (get_out, get_err, get_ok) = run_core_cmd(&[
        "get",
        &handle,
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(get_ok, "get failed: {get_err}");
    assert!(
        get_out.starts_with("segment "),
        "get should resolve the handle and emit a segment record, got: {get_out}"
    );
    assert!(
        !get_out.starts_with("not_found"),
        "handle `{handle}` must not resolve to not_found: {get_out}"
    );

    let (impact_out, impact_err, impact_ok) = run_core_cmd(&[
        "impact",
        "--from-handle",
        &handle,
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(impact_ok, "impact failed: {impact_err}");
    assert!(
        !impact_out.contains("anchor_not_found"),
        "impact --from-handle must accept the 12-char handle; got: {impact_out}"
    );
    assert!(
        !impact_out.contains("anchor_ambiguous"),
        "impact --from-handle should uniquely resolve a 12-char handle for a definition; got: {impact_out}"
    );
}

#[test]
fn search_acceptance_queries_preserve_top_hit_for_priority_classes() {
    // Ranking stability: each acceptance query should keep the expected top
    // file across two consecutive runs (covers the "handoff does not perturb
    // search ranking" contract at the grammar layer).
    let tmp = create_search_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let cases = [
        (
            "request_signing_secret policy_rule_preview_enabled",
            "config/signatures.yaml",
        ),
        (
            "api documentation guide local development",
            "docs/signatures.md",
        ),
        ("validate incoming request signatures", "src/signatures.rs"),
        ("PolicyRulePreview", "proto/policy_rules.proto"),
        ("policy_rules_preview table", "sql/policy_rules.sql"),
        // Bench-named file must not outrank the implementation on a
        // conceptual natural-language query.
        ("impact horizon", "src/horizon.rs"),
        // Descriptive snake_case test name must not outrank the
        // implementation on a conceptual natural-language query.
        (
            "blast radius expansion trust buckets",
            "src/blast_radius.rs",
        ),
        // In-file `#[cfg(test)] mod tests` segments live in src paths, so
        // only their `tests` breadcrumb component can keep the descriptive
        // test fn from outranking the implementation.
        (
            "refinement margin shrinks candidate pool",
            "src/refinement.rs",
        ),
        // Inflected natural-language words must reach stem-named symbols.
        (
            "where are embeddings composed before indexing",
            "src/embedding_pipeline.rs",
        ),
    ];

    for (query, expected_top_path) in cases {
        let first = search_rows(tmp.path(), query);
        let second = search_rows(tmp.path(), query);

        assert!(
            !first.is_empty(),
            "query {query:?} should produce at least one row"
        );
        assert_eq!(
            first[0].file_path, expected_top_path,
            "query {query:?} returned an unexpected top hit"
        );

        let first_paths: Vec<_> = first.iter().take(3).map(|r| r.file_path.clone()).collect();
        let second_paths: Vec<_> = second.iter().take(3).map(|r| r.file_path.clone()).collect();
        assert_eq!(
            first_paths, second_paths,
            "query {query:?} should keep a stable top-3 result set"
        );
    }

    // Heading-matched doc query: the README doc_section must stay in the
    // top-3 despite the markdown and readme-path penalties, and its
    // breadcrumb must carry cleaned heading text without the H1's inline
    // HTML noise.
    let rows = search_rows(tmp.path(), "windows daemon support");
    let doc_hit = rows
        .iter()
        .take(3)
        .find(|row| row.kind == "doc_section")
        .unwrap_or_else(|| {
            panic!("heading-matched doc query should keep a doc_section in the top-3: {rows:?}")
        });
    assert_eq!(doc_hit.file_path, "README.md");
    assert_eq!(
        doc_hit.breadcrumb,
        "README > Acceptance Project > Windows daemon support"
    );

    // Content-matched doc query: the "What To Expect" heading shares no
    // query terms, so breadcrumb neutralization cannot fire; strong body
    // coverage plus non-stacking markdown/docs-path penalties must still
    // carry the section into the top-5 over the docs-path text chunks.
    let rows = search_rows_with_limit(tmp.path(), "offline snapshot refresh cadence", 10);
    let doc_hit = rows
        .iter()
        .take(5)
        .find(|row| row.kind == "doc_section")
        .unwrap_or_else(|| {
            panic!("content-matched doc query should put a doc_section in the top-5: {rows:?}")
        });
    assert_eq!(doc_hit.file_path, "README.md");
    assert_eq!(
        doc_hit.breadcrumb,
        "README > Acceptance Project > What To Expect"
    );
}

// =============================================================================
// Markdown structural indexing — doc sections, mentions, impact, schema gate
// =============================================================================

fn count_index_rows(project: &Path, sql: &str) -> i64 {
    let project = project.canonicalize().unwrap();
    block_on(async {
        let db = Db::open_ro(&project.join(".1up").join("index.db"))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query(sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row.get::<i64>(0).unwrap()
    })
}

#[test]
fn markdown_doc_topic_search_returns_heading_scoped_section_with_breadcrumb() {
    // REQ-005: a documentation-topic query returns the heading-scoped doc
    // section with its document-rooted breadcrumb. Runs FTS-only, which also
    // pins degraded-path discoverability of markdown doc segments.
    let tmp = create_markdown_docs_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "accent color tint guide documentation");
    assert!(!rows.is_empty(), "doc-topic query should return rows");

    let top = &rows[0];
    assert_eq!(top.file_path, "docs/widget_guide.md");
    assert_eq!(top.kind, "doc_section");
    assert_eq!(
        (top.line_start, top.line_end),
        (15, 17),
        "expected the `Accent Colors` section span: {top:?}"
    );
    assert_eq!(
        top.breadcrumb,
        "widget_guide > Widget Guide > Theming > Accent Colors"
    );
}

#[test]
fn markdown_symbol_references_include_doc_mentions() {
    // REQ-003: documentation mentions surface through the existing symbol
    // reference lookup with doc-section provenance, alongside (not replacing)
    // code usages.
    let tmp = create_markdown_docs_fixture();
    init_and_index(&tmp);

    let rows = symbol_rows(tmp.path(), "render_widget_panel", &["--references"]);
    assert!(
        rows.iter()
            .any(|r| r.kind.starts_with("def:") && r.file_path == "src/widget/core.rs"),
        "definition row missing: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.kind.starts_with("usage:") && r.file_path == "src/widget/dashboard.rs"),
        "code usage row missing: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.file_path == "docs/widget_guide.md"
            && r.kind == "usage:doc_section"
            && r.symbol == "render_widget_panel"
            && r.breadcrumb == "widget_guide > Widget Guide > Rendering"),
        "doc mention row with section breadcrumb missing: {rows:?}"
    );
}

#[test]
fn markdown_impact_excludes_doc_mentions_while_code_reference_promotes() {
    // REQ-004 at the integration level: indexing stores doc_mention relation
    // rows for the markdown sections, yet anchored impact never surfaces the
    // doc segments in either trust bucket while the real code reference still
    // promotes to primary.
    let tmp = create_markdown_docs_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let doc_mention_rows = count_index_rows(
        tmp.path(),
        "SELECT COUNT(*) FROM segment_relations r \
         JOIN segments s ON s.id = r.source_segment_id \
         WHERE r.edge_identity_kind = 'doc_mention' \
         AND s.file_path = 'docs/widget_guide.md'",
    );
    assert!(
        doc_mention_rows >= 2,
        "both mentioning sections should store doc_mention relation rows, got {doc_mention_rows}"
    );

    let rows = impact_rows(tmp.path(), &["--from-symbol", "render_widget_panel"]);
    assert!(
        rows.iter()
            .any(|r| r.channel == Some('P') && r.file_path == "src/widget/dashboard.rs"),
        "code reference should still promote to primary: {rows:?}"
    );
    assert!(
        rows.iter().all(|r| r.file_path != "docs/widget_guide.md"),
        "doc-mention edges must not appear in any impact bucket: {rows:?}"
    );
}

#[test]
fn markdown_doc_segments_receive_vector_rows_when_embeddings_enabled() {
    // REQ-005: doc sections participate in the embedding path like any other
    // structural segment. Vector coverage is asserted only when this run
    // actually embedded (the local model is a machine-level artifact);
    // FTS-only discoverability is covered by the doc-topic search test.
    let _model_guard = RestoreHiddenModelGuard::new();
    let tmp = create_markdown_docs_fixture();
    init_project(tmp.path());
    let payload = run_index_json(tmp.path(), &[]);

    if payload["progress"]["embeddings_enabled"] != true {
        eprintln!(
            "skipping vector-row assertions: embedding model unavailable in this environment"
        );
        return;
    }

    let doc_segments = count_index_rows(
        tmp.path(),
        "SELECT COUNT(*) FROM segments WHERE block_type = 'doc_section'",
    );
    let doc_vectors = count_index_rows(
        tmp.path(),
        "SELECT COUNT(*) FROM segments s \
         JOIN segment_vectors v ON v.segment_id = s.id \
         WHERE s.block_type = 'doc_section'",
    );
    assert!(
        doc_segments > 0,
        "fixture should produce doc_section segments"
    );
    assert_eq!(
        doc_vectors, doc_segments,
        "every doc_section segment should carry a vector row"
    );
}

#[test]
fn fresh_index_stores_vector_rows_for_source_segments_when_embeddings_enabled() {
    // Defect A regression: a fresh index run with a working embedder must
    // persist vector rows for source-code segments through the real CLI
    // pipeline path. Counts are read through libsql: stock SQLite tooling
    // satisfies COUNT(*) from the DiskANN expression-index btree, which
    // libsql leaves empty, and therefore under-reports stored vectors as 0.
    let _model_guard = RestoreHiddenModelGuard::new();
    let tmp = create_multi_lang_fixture();
    init_project(tmp.path());
    let payload = run_index_json(tmp.path(), &[]);

    if payload["progress"]["embeddings_enabled"] != true {
        eprintln!(
            "skipping vector-row assertions: embedding model unavailable in this environment"
        );
        return;
    }

    let vector_rows = count_index_rows(tmp.path(), "SELECT COUNT(*) FROM segment_vectors");
    let embeddable_segments = count_index_rows(
        tmp.path(),
        "SELECT COUNT(*) FROM segments WHERE NOT (block_type = 'chunk' AND language IN \
         ('json','yaml','toml','protobuf','terraform','sql','config','makefile','dockerfile'))",
    );

    assert!(
        vector_rows > 0,
        "a fresh index with a working embedder must store vector rows"
    );
    assert_eq!(
        vector_rows, embeddable_segments,
        "every embeddable segment should carry a vector row"
    );
    assert_eq!(
        payload["progress"]["vector_rows"].as_i64(),
        Some(vector_rows),
        "reported vector coverage must match the stored rows"
    );
    assert_eq!(
        payload["progress"]["embeddable_segments"].as_i64(),
        Some(embeddable_segments),
        "reported embeddable count must match the stored segments"
    );
}

#[test]
fn prior_schema_version_index_fails_closed_with_reindex_guidance() {
    // REQ-006: a fresh index at the current schema version serves reads;
    // downgrading the stored version to the immediate prior value (v15 at the
    // v16 bump) fails discovery closed with explicit reindex guidance.
    let tmp = create_markdown_docs_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "accent color tint guide documentation");
    assert!(
        !rows.is_empty(),
        "fresh current-version index should serve search"
    );

    let project_root = tmp.path().canonicalize().unwrap();
    block_on(async {
        let db = Db::open_rw(&project_root.join(".1up").join("index.db"))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            queries::UPSERT_META,
            ["schema_version", &(SCHEMA_VERSION - 1).to_string()],
        )
        .await
        .unwrap();
    });

    let (stdout, stderr, ok) = run_core_cmd(&[
        "search",
        "accent color tint guide documentation",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(
        !ok,
        "stale prior-version index must fail closed; stdout={stdout}"
    );
    assert!(
        stderr.contains(&format!(
            "found v{}, expected v{SCHEMA_VERSION}",
            SCHEMA_VERSION - 1
        )),
        "stale-schema error should name found/expected versions: {stderr}"
    );
    assert!(
        stderr.contains("1up reindex"),
        "stale-schema error should carry reindex guidance: {stderr}"
    );
}

// =============================================================================
// Impact lean envelope
// =============================================================================

fn impact_status_line(stdout: &str) -> Option<&str> {
    stdout.lines().find(|l| {
        let token = l.split("  ").next().unwrap_or("");
        matches!(token, "refused" | "empty" | "empty_scoped")
    })
}

#[test]
fn mcp_initialize_advertises_primary_code_search_instructions() {
    let tmp = TempDir::new().unwrap();
    let (_client, initialize_response) = McpTestClient::start_with_initialize_response(tmp.path());

    let result = &initialize_response["result"];
    let instructions = result["instructions"]
        .as_str()
        .expect("initialize should expose server instructions");
    assert!(
        instructions.contains("primary code-search interface"),
        "instructions should position 1up as the primary code-search path: {instructions}"
    );
    assert!(
        instructions.contains("oneup_search before raw grep, rg, find"),
        "instructions should guide agents to use MCP before broad raw search: {instructions}"
    );
    assert!(
        instructions.contains("Call oneup_overview first when starting work on an unfamiliar repository"),
        "instructions should make orientation-first discovery via oneup_overview discoverable: {instructions}"
    );
    assert!(
        instructions.contains("oneup_get"),
        "instructions should teach the search/get hydration flow: {instructions}"
    );
    assert!(
        instructions.contains("oneup_context"),
        "instructions should make file-line context retrieval discoverable: {instructions}"
    );
    assert!(
        instructions.contains("Use oneup_impact only for explicit blast-radius questions"),
        "instructions should keep impact out of the default core discovery loop: {instructions}"
    );
    assert!(
        instructions.contains(&tmp.path().display().to_string()),
        "instructions should include the configured repository: {instructions}"
    );
    assert_eq!(
        result["serverInfo"]["description"],
        "Primary local code search and discovery MCP server"
    );
}

#[cfg(unix)]
#[test]
fn mcp_rejects_second_instance_for_same_project_root() {
    let tmp = TempDir::new().unwrap();
    let _client = McpTestClient::start(tmp.path());

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_1up"))
        .args(["mcp", "--path", tmp.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "second MCP process should exit while first one holds the lock"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("already running"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mcp_tools_list_default_palette_and_schemas() {
    let tmp = TempDir::new().unwrap();
    let mut client = McpTestClient::start(tmp.path());

    let result = client.list_tools();
    let tools = result["tools"].as_array().unwrap();
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();

    let expected_names = [
        "oneup_status",
        "oneup_start",
        "oneup_search",
        "oneup_get",
        "oneup_symbol",
        "oneup_context",
        "oneup_impact",
        "oneup_structural",
        "oneup_overview",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(names, expected_names);
    assert_eq!(tools.len(), expected_names.len());
    assert!(!names.contains("oneup_prepare"));
    assert!(!names.contains("oneup_read"));

    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(name.starts_with("oneup_"));
        assert!(!name.starts_with("1up_"));
        assert!(
            description.starts_with("Check ")
                || description.starts_with("Prepare ")
                || description.starts_with("Search ")
                || description.starts_with("Hydrate ")
                || description.starts_with("Retrieve ")
                || description.starts_with("Find ")
                || description.starts_with("Explore ")
                || description.starts_with("Run "),
            "description should front-load tool selection guidance: {description}"
        );
        if name == TOOL_SEARCH {
            assert!(
                description.contains("primary discovery path")
                    && description.contains("before raw grep, rg, find"),
                "oneup_search description should be strong enough for tool adoption: {description}"
            );
        }
        if name == TOOL_GET {
            let input_schema = tool
                .get("inputSchema")
                .or_else(|| tool.get("input_schema"))
                .expect("oneup_get should expose an input schema");
            assert!(
                input_schema["properties"]["handles"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Durable result handles"),
                "handles schema should describe result-handle hydration: {input_schema:?}"
            );
        }
        if name == TOOL_CONTEXT {
            assert!(
                description.contains("file-line context") && description.contains("locations"),
                "oneup_context description should expose file-line context retrieval: {description}"
            );
            let input_schema = tool
                .get("inputSchema")
                .or_else(|| tool.get("input_schema"))
                .expect("oneup_context should expose an input schema");
            let locations_schema = &input_schema["properties"]["locations"];
            assert!(
                locations_schema["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("file-line context retrieval"),
                "locations schema should describe file-line context retrieval: {input_schema:?}"
            );
            let location_def = &input_schema["$defs"]["ReadLocationInput"];
            assert!(
                location_def["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("file-line location"),
                "location schema should define the context location shape: {input_schema:?}"
            );
            assert!(
                location_def["properties"]["line"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("1-based"),
                "location line schema should state 1-based input: {input_schema:?}"
            );
        }
        if name == TOOL_IMPACT {
            assert!(
                description.contains("explicit blast-radius questions"),
                "oneup_impact description should keep impact as an explicit non-core follow-up: {description}"
            );
            let input_schema = tool
                .get("inputSchema")
                .or_else(|| tool.get("input_schema"))
                .expect("oneup_impact should expose an input schema");
            assert!(
                input_schema["properties"]["handle"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Result handle"),
                "impact schema should expose public result-handle anchors: {input_schema:?}"
            );
        }
        if name == TOOL_STRUCTURAL {
            let input_schema = tool
                .get("inputSchema")
                .or_else(|| tool.get("input_schema"))
                .expect("oneup_structural should expose an input schema");
            assert!(
                input_schema["properties"]["pattern"]["description"]
                    .as_str()
                    .unwrap_or("")
                    .contains("Tree-sitter query pattern"),
                "structural schema should describe tree-sitter patterns: {input_schema:?}"
            );
        }
        assert!(
            tool.get("inputSchema").is_some() || tool.get("input_schema").is_some(),
            "tool should expose an input schema: {tool:?}"
        );

        let output_schema = tool
            .get("outputSchema")
            .or_else(|| tool.get("output_schema"))
            .expect("tool should expose an output schema");
        assert_eq!(
            output_schema["properties"]["data"]["type"],
            "object",
            "dynamic envelope data should use an object schema instead of boolean true for OpenCode compatibility: {output_schema:?}"
        );
        assert_eq!(
            output_schema["$defs"]["NextAction"]["properties"]["arguments"]["type"],
            "object",
            "dynamic next-action arguments should use an object schema instead of boolean true for OpenCode compatibility: {output_schema:?}"
        );
    }
}

#[test]
fn mcp_status_and_start_report_readiness_states_and_next_actions() {
    let missing = TempDir::new().unwrap();
    fs::create_dir_all(missing.path().join(".git")).unwrap();
    let mut missing_client = McpTestClient::start_with_isolated_state(missing.path());
    let missing_result = missing_client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let missing_envelope = mcp_structured(&missing_result);
    assert_mcp_response_is_presentation_free(&missing_result);
    assert!(
        missing_envelope["status"] == "missing" || missing_envelope["status"] == "indexing",
        "fresh MCP project should be missing or already indexing after daemon auto-start; got {missing_envelope:?}"
    );
    assert_eq!(missing_envelope["data"]["project_initialized"], true);
    if missing_envelope["status"] == "missing" {
        assert_eq!(missing_envelope["next_actions"][0]["tool"], TOOL_START);
        assert_eq!(
            missing_envelope["next_actions"][0]["arguments"]["mode"],
            "index_if_missing"
        );
    } else {
        assert_eq!(missing_envelope["next_actions"][0]["tool"], TOOL_STATUS);
    }
    assert_mcp_next_actions_are_canonical(missing_envelope);

    let indexing = TempDir::new().unwrap();
    write_running_progress(indexing.path());
    let mut indexing_client = McpTestClient::start_with_isolated_state(indexing.path());
    let indexing_result = indexing_client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let indexing_envelope = mcp_structured(&indexing_result);
    assert_mcp_response_is_presentation_free(&indexing_result);
    assert_eq!(indexing_envelope["status"], "indexing");
    assert_eq!(indexing_envelope["next_actions"][0]["tool"], TOOL_STATUS);

    let start_indexing =
        indexing_client.call_tool(TOOL_START, serde_json::json!({ "mode": "index_if_needed" }));
    let start_indexing_envelope = mcp_structured(&start_indexing);
    assert_mcp_response_is_presentation_free(&start_indexing);
    assert_eq!(start_indexing_envelope["status"], "indexing");
    assert_mcp_next_actions_are_canonical(start_indexing_envelope);

    let stale = TempDir::new().unwrap();
    fs::create_dir_all(stale.path().join(".1up")).unwrap();
    fs::write(stale.path().join(".1up").join("project_id"), "test-project").unwrap();
    fs::write(
        stale.path().join(".1up").join("index.db"),
        b"not a current schema",
    )
    .unwrap();
    let mut stale_client = McpTestClient::start_with_isolated_state(stale.path());
    let stale_result = stale_client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let stale_envelope = mcp_structured(&stale_result);
    assert_mcp_response_is_presentation_free(&stale_result);
    assert_eq!(stale_envelope["status"], "stale");
    assert_eq!(stale_envelope["next_actions"][0]["tool"], TOOL_START);
    assert_eq!(
        stale_envelope["next_actions"][0]["arguments"]["mode"],
        "reindex"
    );

    {
        let _model_guard = RestoreHiddenModelGuard::new();
        let ready = create_multi_lang_fixture();
        init_and_index(&ready);
        let mut ready_client = McpTestClient::start(ready.path());
        let ready_result = wait_for_mcp_last_update_complete(&mut ready_client);
        let ready_envelope = mcp_structured(&ready_result);
        assert_mcp_response_is_presentation_free(&ready_result);
        assert_eq!(ready_envelope["status"], "degraded");
        assert_eq!(ready_envelope["data"]["index_readable"], true);
        assert_eq!(ready_envelope["data"]["branch_status"], "unknown");
        assert_eq!(ready_envelope["data"]["last_update_state"], "complete");
        assert_eq!(ready_envelope["next_actions"][0]["tool"], "oneup_search");
    }

    let degraded = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&degraded);
    let mut degraded_client = McpTestClient::start(degraded.path());
    let degraded_result = degraded_client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let degraded_envelope = mcp_structured(&degraded_result);
    assert_mcp_response_is_presentation_free(&degraded_result);
    assert_eq!(degraded_envelope["status"], "degraded");
    assert_eq!(degraded_envelope["data"]["index_readable"], true);
    assert!(
        degraded_envelope["data"].get("drifted").is_none()
            && degraded_envelope["data"].get("indexed_at_head").is_none(),
        "a repository indexed without a known HEAD must not report drift fields: {degraded_envelope:?}"
    );
    assert!(
        degraded_envelope["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["tool"] == "oneup_search"),
        "degraded readiness should still allow search as a next action"
    );
}

fn git_head_oid(dir: &Path) -> String {
    let output = git_output(dir, &["rev-parse", "HEAD"]);
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn mcp_readiness_reports_pinned_detached_commit_as_ready() {
    let _model_guard = RestoreHiddenModelGuard::new();

    let repo = create_multi_lang_fixture();
    git(repo.path(), &["init"]);
    git(
        repo.path(),
        &["config", "user.email", "oneup-test@example.com"],
    );
    git(repo.path(), &["config", "user.name", "1up Test"]);
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "initial"]);
    let indexed_head = git_head_oid(repo.path());

    // Detach HEAD at the exact commit before indexing so the segments are stored
    // under the detached-commit context and readiness reflects a pinned checkout
    // whose working tree matches the indexed state.
    git(
        repo.path(),
        &["checkout", "--detach", indexed_head.as_str()],
    );

    init_and_index(&repo);

    // Address the repo per call via `path` from a neutral isolated-state server
    // so no daemon registers or reconciles the repo and the readiness
    // observation cannot race a refresh.
    let neutral = TempDir::new().unwrap();
    fs::create_dir_all(neutral.path().join(".git")).unwrap();
    let mut client = McpTestClient::start_with_isolated_state(neutral.path());
    let repo_path = repo.path().to_str().unwrap();

    let result = client.call_tool(TOOL_STATUS, serde_json::json!({ "path": repo_path }));
    let envelope = mcp_structured(&result);
    assert_mcp_response_is_presentation_free(&result);

    // The pinned-detached readiness contract (REQ-005): an exact detached commit
    // matching the indexed state is never downgraded for branch ambiguity. The
    // CI-faithful test HOME carries a model-download-failed marker, so the read
    // path reports `degraded` for unavailable embeddings (as the existing ready
    // fixture also does); that is orthogonal to the branch caveat. The behavior
    // under test is that no branch-ambiguity reason is attached, which is what
    // `apply_branch_readiness` now exempts. The pure `Ready` outcome is covered
    // by the direct `apply_branch_readiness` unit matrix.
    assert_eq!(envelope["data"]["branch_status"], "detached");
    assert_eq!(envelope["data"]["drifted"], false);
    let reason = envelope["data"]["reason"].as_str().unwrap_or_default();
    assert!(
        !reason.contains("branch context") && !reason.contains("not branch-filtered"),
        "a pinned detached commit must not carry a branch-ambiguity reason: {envelope:?}"
    );
    assert_mcp_next_actions_are_canonical(envelope);
}

#[test]
fn mcp_status_reports_head_drift_and_start_clears_it() {
    let repo = create_multi_lang_fixture();
    git(repo.path(), &["init"]);
    git(
        repo.path(),
        &["config", "user.email", "oneup-test@example.com"],
    );
    git(repo.path(), &["config", "user.name", "1up Test"]);
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "initial"]);
    let indexed_head = git_head_oid(repo.path());

    let _guard = init_and_index_fts_only(&repo);

    // The MCP server is configured for a neutral project and the fixture repo
    // is addressed per call via `path`, so no daemon ever registers or
    // reconciles the repo and the drift observations cannot race a refresh.
    let neutral = TempDir::new().unwrap();
    fs::create_dir_all(neutral.path().join(".git")).unwrap();
    let mut client = McpTestClient::start_with_isolated_state(neutral.path());
    let repo_path = repo.path().to_str().unwrap();

    let fresh_result = client.call_tool(TOOL_STATUS, serde_json::json!({ "path": repo_path }));
    let fresh = mcp_structured(&fresh_result);
    assert_mcp_response_is_presentation_free(&fresh_result);
    assert_eq!(fresh["data"]["index_readable"], true);
    assert_eq!(fresh["data"]["drifted"], false);
    assert_eq!(fresh["data"]["indexed_at_head"], indexed_head.as_str());
    assert_eq!(fresh["data"]["current_head"], indexed_head.as_str());
    assert!(
        !fresh["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["arguments"]["mode"] == "index_if_needed"),
        "non-drifted readiness must not suggest an index_if_needed start: {fresh:?}"
    );
    assert_mcp_next_actions_are_canonical(fresh);

    fs::write(
        repo.path().join("drift.rs"),
        "pub fn head_drift_marker() {}\n",
    )
    .unwrap();
    git(repo.path(), &["add", "."]);
    git(repo.path(), &["commit", "-m", "move head"]);
    let moved_head = git_head_oid(repo.path());
    assert_ne!(indexed_head, moved_head);

    let drifted_result = client.call_tool(TOOL_STATUS, serde_json::json!({ "path": repo_path }));
    let drifted = mcp_structured(&drifted_result);
    assert_mcp_response_is_presentation_free(&drifted_result);
    assert_eq!(drifted["data"]["drifted"], true);
    assert_eq!(drifted["data"]["indexed_at_head"], indexed_head.as_str());
    assert_eq!(drifted["data"]["current_head"], moved_head.as_str());
    let drift_actions = drifted["next_actions"].as_array().unwrap();
    assert!(
        drift_actions
            .iter()
            .any(|action| action["tool"] == TOOL_START
                && action["arguments"]["mode"] == "index_if_needed"),
        "drifted readiness must suggest oneup_start with index_if_needed: {drifted:?}"
    );
    assert!(
        drift_actions
            .iter()
            .any(|action| action["tool"] == TOOL_SEARCH),
        "the drift advisory must keep the existing readiness actions: {drifted:?}"
    );
    assert_mcp_next_actions_are_canonical(drifted);

    let start_result = client.call_tool(
        TOOL_START,
        serde_json::json!({ "path": repo_path, "mode": "index_if_needed" }),
    );
    let started = mcp_structured(&start_result);
    assert_mcp_response_is_presentation_free(&start_result);
    assert_eq!(started["data"]["drifted"], false);
    assert_eq!(started["data"]["indexed_at_head"], moved_head.as_str());
    assert_eq!(started["data"]["current_head"], moved_head.as_str());
    assert_mcp_next_actions_are_canonical(started);
}

#[test]
fn mcp_status_ignores_index_progress_from_other_context() {
    let project = TempDir::new().unwrap();
    fs::create_dir_all(project.path().join(".git")).unwrap();
    write_running_progress_for_context(project.path(), "other-context");

    let mut client = McpTestClient::start_with_isolated_state(project.path());
    let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert!(
        envelope["status"] == "missing" || envelope["status"] == "indexing",
        "fresh MCP project should be missing or already indexing after daemon auto-start; got {envelope:?}"
    );
    assert!(
        envelope["data"].get("index_progress").is_none(),
        "non-active context progress should not be exposed in readiness data: {envelope:?}"
    );
    assert!(
        matches!(
            envelope["data"]["last_update_state"].as_str(),
            Some("unknown" | "pending" | "running")
        ),
        "fresh MCP project should not report a completed foreign-context update: {envelope:?}"
    );
    if envelope["status"] == "missing" {
        assert_eq!(
            envelope["next_actions"][0]["arguments"]["mode"],
            "index_if_missing"
        );
        assert_eq!(envelope["next_actions"][0]["tool"], TOOL_START);
    } else {
        assert_eq!(envelope["next_actions"][0]["tool"], TOOL_STATUS);
    }
}

#[test]
fn mcp_status_ignores_index_rows_from_other_context() {
    let project = TempDir::new().unwrap();
    let project_root = project.path().canonicalize().unwrap();
    fs::create_dir_all(project_root.join(".git")).unwrap();
    seed_current_index_for_context(&project_root, "other-context");

    let mut client = McpTestClient::start_with_isolated_state(&project_root);
    let result = client.call_tool(TOOL_STATUS, serde_json::json!({}));
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert!(
        envelope["status"] == "missing" || envelope["status"] == "indexing",
        "fresh MCP project should be missing or already indexing after daemon auto-start; got {envelope:?}"
    );
    if envelope["data"]["index_readable"].as_bool() == Some(true) {
        assert_eq!(envelope["data"]["indexed_files"], 0);
        assert_eq!(envelope["data"]["total_segments"], 0);
    }
    if envelope["status"] == "missing" {
        assert_eq!(
            envelope["next_actions"][0]["arguments"]["mode"],
            "index_if_missing"
        );
        assert_eq!(envelope["next_actions"][0]["tool"], TOOL_START);
    } else {
        assert_eq!(envelope["next_actions"][0]["tool"], TOOL_STATUS);
    }
}

#[test]
fn mcp_get_ignores_handles_from_other_context() {
    let project = TempDir::new().unwrap();
    let project_root = project.path().canonicalize().unwrap();
    fs::create_dir_all(project_root.join(".git")).unwrap();
    seed_current_index_for_context(&project_root, "other-context");

    let mut client = McpTestClient::start_with_isolated_state(&project_root);
    let result = client.call_tool(
        TOOL_GET,
        serde_json::json!({ "handles": [":other-context-segment"] }),
    );

    assert_eq!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "empty");
    assert_eq!(envelope["data"]["records"][0]["status"], "not_found");
    assert_eq!(
        envelope["data"]["records"][0]["source"]["normalized"],
        "other-context-segment"
    );
    assert!(
        envelope["data"]["records"][0]["segment"].is_null(),
        "foreign-context segment should not be hydrated: {envelope:?}"
    );
    assert_eq!(envelope["next_actions"][0]["tool"], TOOL_SEARCH);

    // REQ-001 AC3: a mistyped handle whose only unique-prefix match lives in a
    // foreign context must never be recovered into the active context. The
    // seeded id is "other-context-segment"; a one-character typo shares a long
    // prefix but must still resolve to not_found with no recovery disclosure.
    let recovery = client.call_tool(
        TOOL_GET,
        serde_json::json!({ "handles": [":other-context-segmenX"] }),
    );
    assert_eq!(recovery["isError"], true);
    let recovery_envelope = mcp_structured(&recovery);
    assert_eq!(
        recovery_envelope["data"]["records"][0]["status"],
        "not_found"
    );
    assert!(
        recovery_envelope["data"]["records"][0]["recovered_from"].is_null(),
        "a foreign-context handle must never be recovered: {recovery_envelope:?}"
    );
    assert!(recovery_envelope["data"]["records"][0]["segment"].is_null());
}

#[test]
fn mcp_start_index_if_missing_builds_index_state_only() {
    let tmp = TempDir::new().unwrap();
    let git_dir = tmp.path().join(".git");
    fs::create_dir_all(git_dir.join("refs").join("heads")).unwrap();
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(tmp.path().join("src")).unwrap();

    let source_path = tmp.path().join("src").join("lib.rs");
    let source_content = "pub fn readiness_probe() -> &'static str {\n    \"ready\"\n}\n";
    fs::write(&source_path, source_content).unwrap();

    let mut client = McpTestClient::start_with_isolated_state(tmp.path());
    client.call_tool(
        TOOL_START,
        serde_json::json!({ "mode": "index_if_missing" }),
    );
    // This test asserts the index is built and searchable, not the daemon's
    // refresh bookkeeping — wait on searchable readiness so a legitimately
    // deferred (`pending`) daemon refresh after the one-shot index can't flake
    // it under CI load.
    let result = wait_for_mcp_searchable_readiness(&mut client);
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert!(
        matches!(envelope["status"].as_str(), Some("ready" | "degraded")),
        "explicit indexing should produce searchable readiness: {envelope:?}"
    );
    assert_eq!(envelope["data"]["index_present"], true);
    assert_eq!(envelope["data"]["index_readable"], true);
    assert!(
        envelope["data"]["total_segments"].as_u64().unwrap() > 0,
        "explicit MCP indexing should create searchable segment state"
    );
    assert_eq!(fs::read_to_string(source_path).unwrap(), source_content);
    assert!(tmp.path().join(".1up").join("index.db").exists());
    assert_mcp_next_actions_are_canonical(envelope);
}

#[test]
fn mcp_start_reports_blocked_when_indexing_cannot_auto_initialize() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("loose.rs"),
        "pub fn loose_directory() -> &'static str { \"not a repo\" }\n",
    )
    .unwrap();

    let mut client = McpTestClient::start(tmp.path());
    let result = client.call_tool(
        TOOL_START,
        serde_json::json!({ "mode": "index_if_missing" }),
    );
    let envelope = mcp_structured(&result);

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_eq!(envelope["status"], "blocked");
    assert_eq!(envelope["data"]["status"], "blocked");
    assert!(
        envelope["data"]["reason"]
            .as_str()
            .unwrap()
            .contains("automatic project creation requires an existing 1up project or a git root"),
        "blocked readiness should explain why MCP could not create index state: {envelope:?}"
    );
    assert!(!tmp.path().join(".1up").exists());
    assert_mcp_next_actions_are_canonical(envelope);
}

#[test]
fn mcp_core_discovery_loop_returns_structured_evidence() {
    let tmp = create_search_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let status = wait_for_mcp_last_update_complete(&mut client);
    assert_ne!(status["isError"], true);
    assert_mcp_response_is_presentation_free(&status);
    let status_envelope = mcp_structured(&status);
    assert!(
        matches!(
            status_envelope["status"].as_str(),
            Some("ready" | "degraded")
        ),
        "status should be ready or degraded depending on local model cache: {status_envelope:?}"
    );
    assert_eq!(status_envelope["data"]["index_readable"], true);
    assert!(
        status_envelope["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| action["tool"] == TOOL_SEARCH),
        "ready or degraded status should lead agents to search: {status_envelope:?}"
    );

    let search = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({ "query": "PolicyRuleValidator", "limit": 3 }),
    );
    assert_ne!(search["isError"], true);
    assert_mcp_response_is_presentation_free(&search);
    assert_mcp_text_matches_summary(&search);
    let search_text = search["content"][0]["text"].as_str().unwrap();
    assert!(
        search_text.contains("src/policy.rs:"),
        "oneup_search text should include ranked rows, not only a count summary: {search_text}"
    );
    assert!(
        search_text
            .lines()
            .any(|line| line.contains("PolicyRuleValidator")
                && line.contains(":")
                && line.contains("  ")),
        "oneup_search text should include a CLI-like row with symbol and handle: {search_text}"
    );
    let search_envelope = mcp_structured(&search);
    assert!(
        matches!(search_envelope["status"].as_str(), Some("ok" | "degraded")),
        "search should be ok or degraded depending on local model cache: {search_envelope:?}"
    );
    assert_mcp_next_actions_are_canonical(search_envelope);
    let search_actions = search_envelope["next_actions"].as_array().unwrap();
    assert!(
        search_actions
            .iter()
            .any(|action| action["tool"] == TOOL_GET && action["arguments"]["handles"].is_array()),
        "search hits should offer handle hydration: {search_actions:?}"
    );
    assert!(
        search_actions
            .iter()
            .any(|action| action["tool"] == TOOL_CONTEXT
                && action["arguments"]["locations"].is_array()),
        "search hits should offer file-line context retrieval: {search_actions:?}"
    );
    assert!(
        search_actions
            .iter()
            .any(|action| action["tool"] == TOOL_SYMBOL),
        "search hits with symbol hints should offer symbol verification: {search_actions:?}"
    );
    assert!(
        search_actions
            .iter()
            .all(|action| action["tool"] != "oneup_impact"),
        "impact should not be a primary search next action: {search_actions:?}"
    );

    let hit = &search_envelope["data"]["results"][0];
    let handle = hit["handle"].as_str().unwrap();
    assert!(!handle.is_empty());
    assert_eq!(hit["path"], "src/policy.rs");
    assert!(!hit["kind"].as_str().unwrap().is_empty());
    assert!(hit["score"].as_u64().unwrap() <= 100);
    assert!(hit["line_end"].as_u64().unwrap() >= hit["line_start"].as_u64().unwrap());

    let read_handle = client.call_tool(
        TOOL_GET,
        serde_json::json!({ "handles": [format!(":{handle}")] }),
    );
    assert_mcp_response_is_presentation_free(&read_handle);
    assert_mcp_text_matches_summary(&read_handle);
    let read_handle_text = read_handle["content"][0]["text"].as_str().unwrap();
    assert!(
        read_handle_text.contains("segment "),
        "oneup_get text should include hydrated segment records, not only a status summary: {read_handle_text}"
    );
    assert!(
        !read_handle_text.contains("PolicyRuleValidator"),
        "oneup_get text must be content-free (REQ-001): source appears only in structured data, never mirrored into the summary: {read_handle_text}"
    );
    let read_handle_envelope = mcp_structured(&read_handle);
    assert_eq!(read_handle_envelope["status"], "ok");
    assert_eq!(
        read_handle_envelope["data"]["records"][0]["segment"]["path"],
        "src/policy.rs"
    );
    assert!(
        read_handle_envelope["data"]["records"][0]["segment"]["content"]
            .as_str()
            .unwrap()
            .contains("PolicyRuleValidator")
    );
    assert_mcp_next_actions_are_canonical(read_handle_envelope);
    let read_handle_actions = read_handle_envelope["next_actions"].as_array().unwrap();
    assert!(
        read_handle_actions
            .iter()
            .any(|action| action["tool"] == TOOL_CONTEXT
                && action["arguments"]["locations"].is_array()),
        "hydrated segments should offer surrounding file-line context: {read_handle_actions:?}"
    );
    assert!(
        read_handle_actions
            .iter()
            .any(|action| action["tool"] == TOOL_SYMBOL),
        "hydrated defining segments should offer symbol verification: {read_handle_actions:?}"
    );
    assert!(
        read_handle_actions
            .iter()
            .all(|action| action["tool"] != "oneup_impact"),
        "impact should not be a primary get next action: {read_handle_actions:?}"
    );

    let read_location = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "locations": [{ "path": "src/policy.rs", "line": 4, "expansion": 2 }]
        }),
    );
    assert_mcp_response_is_presentation_free(&read_location);
    let read_location_text = read_location["content"][0]["text"].as_str().unwrap();
    assert!(
        read_location_text.contains("src/policy.rs:"),
        "oneup_context text should include the content-free location line: {read_location_text}"
    );
    assert!(
        !read_location_text.contains("validate(&self"),
        "oneup_context text must be content-free (REQ-001): source context appears only in structured data, never mirrored into the summary: {read_location_text}"
    );
    let read_location_envelope = mcp_structured(&read_location);
    assert_eq!(read_location_envelope["status"], "ok");
    assert_eq!(
        read_location_envelope["data"]["records"][0]["context"]["path"],
        "src/policy.rs"
    );
    assert!(
        read_location_envelope["data"]["records"][0]["context"]["line_start"]
            .as_u64()
            .unwrap()
            <= 4
    );
    assert!(
        read_location_envelope["data"]["records"][0]["context"]["content"]
            .as_str()
            .unwrap()
            .contains("validate(&self"),
        "context source content must remain in structured data exactly once: {read_location_envelope:?}"
    );

    let symbol = client.call_tool(
        TOOL_SYMBOL,
        serde_json::json!({ "name": "PolicyRuleValidator", "include": "both" }),
    );
    assert_mcp_response_is_presentation_free(&symbol);
    assert_mcp_text_matches_summary(&symbol);
    let symbol_envelope = mcp_structured(&symbol);
    assert_eq!(symbol_envelope["status"], "ok");
    assert!(symbol_envelope["data"]["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["path"] == "src/policy.rs"
            && !record["handle"].as_str().unwrap().is_empty()));
    assert!(symbol_envelope["data"]["references"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["path"] == "src/runner.rs"
            && !record["handle"].as_str().unwrap().is_empty()));
    assert_mcp_next_actions_are_canonical(symbol_envelope);
    let symbol_actions = symbol_envelope["next_actions"].as_array().unwrap();
    assert!(
        symbol_actions
            .iter()
            .any(|action| action["tool"] == TOOL_GET && action["arguments"]["handles"].is_array()),
        "symbol results should offer handle hydration: {symbol_actions:?}"
    );
    assert!(
        symbol_actions
            .iter()
            .any(|action| action["tool"] == TOOL_CONTEXT
                && action["arguments"]["locations"].is_array()),
        "symbol results should offer file-line context retrieval: {symbol_actions:?}"
    );
    assert!(
        symbol_actions
            .iter()
            .all(|action| action["tool"] != "oneup_impact"),
        "impact should not be a primary symbol next action: {symbol_actions:?}"
    );
}

/// REQ-001 (exactly-once serialization): the authoritative source of a
/// hydrated segment lives only in `structuredContent.data.records[].segment.content`;
/// it is never mirrored into the text summary. Serializing the whole
/// `CallToolResult` and counting a content-only sentinel proves the mirror is
/// gone — the token occurs exactly once. Currently red before the summary flip.
#[test]
fn mcp_get_serializes_segment_source_exactly_once() {
    let tmp = TempDir::new().unwrap();
    let sentinel = "ZZ_ONCE_SENTINEL_5e91";
    write_scope_file_with_tail_sentinel(tmp.path(), "once_marker", 6, sentinel);
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());
    wait_for_mcp_searchable_readiness(&mut client);

    let search = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({ "query": "once_marker", "limit": 3 }),
    );
    let handle = mcp_structured(&search)["data"]["results"][0]["handle"]
        .as_str()
        .expect("search should return a hydratable handle")
        .to_string();

    let get = client.call_tool(
        TOOL_GET,
        serde_json::json!({ "handles": [format!(":{handle}")] }),
    );
    assert_ne!(get["isError"], true);

    // The source lives in structured content...
    let content = mcp_structured(&get)["data"]["records"][0]["segment"]["content"]
        .as_str()
        .expect("hydrated segment carries source content");
    assert!(
        content.contains(sentinel),
        "structured content should carry the source: {content}"
    );
    // ...and not in the text summary.
    let text = get["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains(sentinel),
        "text summary must stay content-free (REQ-001): {text}"
    );

    // Across the entire serialized CallToolResult the source appears exactly once.
    let serialized = serde_json::to_string(&get).unwrap();
    assert_eq!(
        serialized.matches(sentinel).count(),
        1,
        "source must be serialized exactly once, not mirrored into text: {serialized}"
    );
}

/// REQ-002 (bounded, invariant summary): the text summary is a constant
/// per-record grammar independent of record size. A tiny scope and an unclipped
/// ~120-line scope must yield summaries of near-identical length (differing only
/// by line-number digits), both well within budget, while the large record's
/// structured content dwarfs its summary — proving the bulk moved out of text.
/// Modeled on `mcp_overview_data_payload_stays_within_documented_budget`.
#[test]
fn mcp_read_summary_size_is_bounded_and_invariant_across_record_sizes() {
    let tmp = TempDir::new().unwrap();
    write_scope_file(tmp.path(), "tiny_scope", 6);
    write_scope_file(tmp.path(), "big_scope", 120);
    let mut client = McpTestClient::start(tmp.path());

    let small = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "locations": [{ "path": "src/tiny_scope.rs", "line": 3, "expansion": 2 }]
        }),
    );
    // A large explicit expansion returns the whole 120-line scope unclipped, so
    // this is a genuinely large-content record.
    let large = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "locations": [{ "path": "src/big_scope.rs", "line": 60, "expansion": 500 }]
        }),
    );
    assert_ne!(small["isError"], true);
    assert_ne!(large["isError"], true);

    let small_summary = small["content"][0]["text"].as_str().unwrap();
    let large_summary = large["content"][0]["text"].as_str().unwrap();

    const SUMMARY_BUDGET: usize = 256;
    assert!(
        large_summary.len() <= SUMMARY_BUDGET,
        "large-record summary must stay within budget; got {} bytes:\n{large_summary}",
        large_summary.len()
    );
    assert!(
        large_summary.len().abs_diff(small_summary.len()) <= 16,
        "summary length must be invariant to record size (small={}, large={})",
        small_summary.len(),
        large_summary.len()
    );

    // The large record's structured source content dwarfs its summary: the
    // compaction moved the bulk out of the text block (REQ-001/REQ-002).
    let large_content = mcp_structured(&large)["data"]["records"][0]["context"]["content"]
        .as_str()
        .unwrap();
    assert!(
        large_content.len() > large_summary.len() * 4,
        "structured content ({}) should dwarf the summary ({})",
        large_content.len(),
        large_summary.len()
    );
    assert!(
        mcp_structured(&large)["data"]["records"][0]["context"]["truncation"].is_null(),
        "the whole scope returned unclipped, so no truncation note: {large}"
    );
}

/// REQ-003/REQ-004 (whole-scope threshold, end-to-end): a scope of exactly
/// `MAX_WHOLE_SCOPE_LINES` (101) lines returns whole with no truncation note,
/// while a 102-line scope windowed near its middle carries a load-bearing note
/// stating the full scope range. Asserts the ==101 whole / ==102 clipped
/// boundary end-to-end through `oneup_context`.
#[test]
fn mcp_context_truncation_note_tracks_whole_scope_threshold() {
    let tmp = TempDir::new().unwrap();
    write_scope_file(tmp.path(), "at_threshold", 101);
    write_scope_file(tmp.path(), "over_threshold", 102);
    let mut client = McpTestClient::start(tmp.path());

    let whole = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "locations": [{ "path": "src/at_threshold.rs", "line": 50, "expansion": 2 }]
        }),
    );
    assert_ne!(whole["isError"], true);
    assert!(
        mcp_structured(&whole)["data"]["records"][0]["context"]["truncation"].is_null(),
        "a scope of exactly MAX_WHOLE_SCOPE_LINES must return whole (no truncation): {whole}"
    );

    let clipped = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "locations": [{ "path": "src/over_threshold.rs", "line": 50, "expansion": 2 }]
        }),
    );
    assert_ne!(clipped["isError"], true);
    let note = &mcp_structured(&clipped)["data"]["records"][0]["context"]["truncation"];
    assert_eq!(
        note["reason"].as_str(),
        Some(SCOPE_TRUNCATION_REASON),
        "a scope one line over threshold must window with a load-bearing note: {clipped}"
    );
    assert_eq!(note["full_line_start"].as_u64(), Some(1));
    assert_eq!(note["full_line_end"].as_u64(), Some(102));
}

/// REQ-004 recovery round-trip (REQUIRED, HYP-002 regression): a near-top
/// windowed read of a large scope omits the tail sentinel; deserializing the
/// truncation note's recovery call and re-issuing it verbatim retrieves the
/// omitted remainder, so the union of both responses covers the full enclosing
/// scope and surfaces the sentinel. Guards against the fixed-window miss
/// observed at manager.ts:546-561.
#[test]
fn mcp_context_truncation_recovery_round_trips_to_full_scope() {
    let tmp = TempDir::new().unwrap();
    let sentinel = "ZZ_TAIL_SENTINEL_7b2c";
    let rel = write_scope_file_with_tail_sentinel(tmp.path(), "recovery_scope", 120, sentinel);
    let mut client = McpTestClient::start(tmp.path());

    // Near the scope top, with a tiny expansion: the tail (and its sentinel) is
    // omitted from the first window.
    let first = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({ "locations": [{ "path": rel, "line": 2, "expansion": 3 }] }),
    );
    assert_ne!(first["isError"], true);
    let first_ctx = mcp_structured(&first)["data"]["records"][0]["context"].clone();
    let first_content = first_ctx["content"].as_str().unwrap();
    assert!(
        !first_content.contains(sentinel),
        "the near-top window must omit the tail sentinel: {first_content}"
    );

    let note = first_ctx["truncation"].clone();
    assert_eq!(note["reason"].as_str(), Some(SCOPE_TRUNCATION_REASON));
    let scope_start = note["full_line_start"].as_u64().unwrap();
    let scope_end = note["full_line_end"].as_u64().unwrap();
    assert_eq!((scope_start, scope_end), (1, 120));

    // The recovery call is prepended as the first envelope next_action, naming
    // the clipped scope and omitted counts, and carries the note's arguments
    // verbatim (REQ-004).
    let first_actions = mcp_structured(&first)["next_actions"].as_array().unwrap();
    assert_eq!(first_actions[0]["tool"].as_str(), Some(TOOL_CONTEXT));
    assert_eq!(first_actions[0]["arguments"], note["recovery"]["arguments"]);
    let reason = first_actions[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("recovery_scope") && reason.contains("115"),
        "recovery next_action must name the clipped scope and omitted line counts: {reason}"
    );

    // Re-issue the note's exact recovery call verbatim.
    let recovery = note["recovery"].clone();
    assert_eq!(recovery["tool"].as_str(), Some(TOOL_CONTEXT));
    let recovered = client.call_tool(TOOL_CONTEXT, recovery["arguments"].clone());
    assert_ne!(recovered["isError"], true);
    let recovered_ctx = mcp_structured(&recovered)["data"]["records"][0]["context"].clone();
    let recovered_content = recovered_ctx["content"].as_str().unwrap();

    // The recovered content surfaces the omitted sentinel...
    assert!(
        recovered_content.contains(sentinel),
        "recovery call must retrieve the omitted tail sentinel: {recovered_content}"
    );
    // ...and the union of both windows covers the full enclosing scope.
    let union_start = first_ctx["line_start"]
        .as_u64()
        .unwrap()
        .min(recovered_ctx["line_start"].as_u64().unwrap());
    let union_end = first_ctx["line_end"]
        .as_u64()
        .unwrap()
        .max(recovered_ctx["line_end"].as_u64().unwrap());
    assert!(
        union_start <= scope_start && union_end >= scope_end,
        "union [{union_start},{union_end}] must cover the full enclosing scope [{scope_start},{scope_end}]"
    );
}

#[test]
fn mcp_context_returns_context_and_structured_location_failures() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("src").join("policy.rs"),
        "pub fn policy_context() -> bool {\n    true\n}\n",
    )
    .unwrap();
    let outside = tmp.path().join("outside.rs");
    fs::write(&outside, "pub fn outside_context() {}\n").unwrap();

    let mut client = McpTestClient::start(&repo);
    let result = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "locations": [
                { "path": "src/policy.rs", "line": 2, "expansion": 1 },
                { "path": "src/policy.rs", "line": 0 },
                { "path": "../outside.rs", "line": 1 },
                { "path": outside.to_str().unwrap(), "line": 1 },
                { "path": "src/missing.rs", "line": 1 }
            ]
        }),
    );

    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_mcp_text_matches_summary(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "partial");
    assert!(
        envelope["summary"]
            .as_str()
            .unwrap()
            .contains("file-line context"),
        "read-location summaries should identify context retrieval: {envelope:?}"
    );

    let records = envelope["data"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 5);
    assert_eq!(records[0]["status"], "found");
    assert_eq!(records[0]["source"]["kind"], "location");
    assert_eq!(records[0]["context"]["path"], "src/policy.rs");
    assert!(records[0]["context"]["content"]
        .as_str()
        .unwrap()
        .contains("policy_context"));

    assert_eq!(records[1]["status"], "rejected");
    assert!(records[1]["message"].as_str().unwrap().contains("1-based"));
    assert_eq!(records[2]["status"], "rejected");
    assert!(records[2]["message"]
        .as_str()
        .unwrap()
        .contains("outside the configured repository"));
    assert_eq!(records[3]["status"], "rejected");
    assert!(records[3]["message"]
        .as_str()
        .unwrap()
        .contains("outside the configured repository"));
    assert_eq!(records[4]["status"], "error");
    assert!(records[4]["message"].is_string());
    assert_mcp_next_actions_are_canonical(envelope);

    let failed = client.call_tool(
        TOOL_CONTEXT,
        serde_json::json!({
            "locations": [
                { "path": "src/policy.rs", "line": 0 },
                { "path": "../outside.rs", "line": 1 }
            ]
        }),
    );
    assert_eq!(failed["isError"], true);
    assert_mcp_response_is_presentation_free(&failed);
    let failed_envelope = mcp_structured(&failed);
    assert_eq!(failed_envelope["status"], "empty");
    assert_eq!(failed_envelope["data"]["records"][0]["status"], "rejected");
    assert_eq!(failed_envelope["next_actions"][0]["tool"], TOOL_SEARCH);
}

#[test]
fn mcp_get_handles_return_structured_not_found_and_ambiguous_records() {
    let tmp = create_ambiguous_handle_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let search = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({ "query": "ambiguous_collision_token", "limit": 32 }),
    );
    assert_ne!(search["isError"], true);
    assert_mcp_response_is_presentation_free(&search);
    let handles = mcp_structured(&search)["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["handle"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert!(
        handles.len() >= 17,
        "fixture should produce enough hits to force a prefix collision: {handles:?}"
    );
    let ambiguous_prefix = ambiguous_handle_prefix(&handles);

    let result = client.call_tool(
        TOOL_GET,
        serde_json::json!({ "handles": [ambiguous_prefix, ":does-not-exist"] }),
    );
    assert_eq!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "empty");

    let records = envelope["data"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["status"], "ambiguous");
    assert_eq!(records[0]["source"]["kind"], "handle");
    assert!(
        records[0]["matching_handles"].as_array().unwrap().len() > 1,
        "ambiguous handle records should include disambiguation candidates: {records:?}"
    );
    assert_eq!(records[1]["status"], "not_found");
    assert_eq!(records[1]["source"]["normalized"], "does-not-exist");

    // REQ-002: the ambiguous record's disambiguation next-action prefills
    // oneup_get with the real candidate ids (never placeholders) ahead of the
    // generic search fallback, so an agent can pick one unambiguous handle.
    let next_actions = envelope["next_actions"].as_array().unwrap();
    assert_eq!(next_actions[0]["tool"], TOOL_GET);
    let prefilled = next_actions[0]["arguments"]["handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|handle| handle.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let candidates = records[0]["matching_handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|handle| handle.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        prefilled, candidates,
        "disambiguation action must prefill the exact ambiguous candidates"
    );
    assert!(
        prefilled.iter().all(|handle| !handle.is_empty()),
        "disambiguation handles must be real values, never placeholders: {prefilled:?}"
    );
    assert!(
        next_actions
            .iter()
            .any(|action| action["tool"] == TOOL_SEARCH),
        "the generic search fallback must remain available: {next_actions:?}"
    );
    assert_mcp_next_actions_are_canonical(envelope);
}

#[test]
fn mcp_structural_returns_matches_and_explicit_diagnostics() {
    let tmp = create_search_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let matched = client.call_tool(
        TOOL_STRUCTURAL,
        serde_json::json!({
            "pattern": "(struct_item name: (type_identifier) @name)",
            "language": "rust"
        }),
    );
    assert_ne!(matched["isError"], true);
    assert_mcp_response_is_presentation_free(&matched);
    let matched_envelope = mcp_structured(&matched);
    assert_eq!(matched_envelope["status"], "ok");
    assert!(matched_envelope["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["file_path"] == "src/policy.rs"
            && record["content"] == "PolicyRuleValidator"));
    assert_eq!(matched_envelope["next_actions"][0]["tool"], TOOL_CONTEXT);

    let unsupported = client.call_tool(
        TOOL_STRUCTURAL,
        serde_json::json!({ "pattern": "(identifier) @name", "language": "haskell" }),
    );
    assert_eq!(unsupported["isError"], true);
    assert_mcp_response_is_presentation_free(&unsupported);
    let unsupported_envelope = mcp_structured(&unsupported);
    assert_eq!(unsupported_envelope["status"], "error");
    assert_eq!(
        unsupported_envelope["data"]["diagnostics"][0]["kind"],
        "unsupported_language"
    );
    assert!(unsupported_envelope["data"]["supported_languages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|language| language == "rust"));

    let invalid = client.call_tool(
        TOOL_STRUCTURAL,
        serde_json::json!({ "pattern": "(function_item) @fn", "language": "python" }),
    );
    assert_eq!(invalid["isError"], true);
    assert_mcp_response_is_presentation_free(&invalid);
    assert_eq!(
        mcp_structured(&invalid)["data"]["diagnostics"][0]["kind"],
        "invalid_pattern"
    );

    let blank = client.call_tool(
        TOOL_STRUCTURAL,
        serde_json::json!({ "pattern": "   ", "language": "rust" }),
    );
    assert_eq!(blank["isError"], true);
    assert_mcp_response_is_presentation_free(&blank);
    let blank_envelope = mcp_structured(&blank);
    assert_eq!(blank_envelope["status"], "error");
    assert_eq!(
        blank_envelope["data"]["diagnostics"][0]["kind"],
        "invalid_pattern"
    );
    assert!(blank_envelope["data"]["results"]
        .as_array()
        .unwrap()
        .is_empty());

    let empty = client.call_tool(
        TOOL_STRUCTURAL,
        serde_json::json!({ "pattern": "(enum_item) @enum", "language": "rust" }),
    );
    assert_ne!(empty["isError"], true);
    assert_mcp_response_is_presentation_free(&empty);
    let empty_envelope = mcp_structured(&empty);
    assert_eq!(empty_envelope["status"], "empty");
    assert!(empty_envelope["data"]["results"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn mcp_overview_returns_orientation_digest_sections() {
    let tmp = create_overview_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());
    wait_for_mcp_last_update_complete(&mut client);

    let result = client.call_tool(TOOL_OVERVIEW, serde_json::json!({}));
    assert_ne!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "ok");

    let data = &envelope["data"];
    for section in [
        "stats",
        "top_symbols",
        "modules",
        "module_dependencies",
        "entry_points",
    ] {
        assert!(
            data.get(section).is_some(),
            "one overview call should return the {section} section: {data:?}"
        );
    }

    let stats = &data["stats"];
    assert_eq!(stats["indexed_files"], 4);
    let total_segments = stats["total_segments"].as_u64().unwrap();
    assert!(total_segments > 0);
    let languages = stats["languages"].as_array().unwrap();
    let language_names = languages
        .iter()
        .map(|language| language["language"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(language_names, BTreeSet::from(["python", "rust"]));
    assert_eq!(
        languages
            .iter()
            .map(|language| language["files"].as_u64().unwrap())
            .sum::<u64>(),
        4,
        "per-language file counts should partition the indexed files: {languages:?}"
    );
    assert_eq!(
        languages
            .iter()
            .map(|language| language["segments"].as_u64().unwrap())
            .sum::<u64>(),
        total_segments,
        "per-language segment counts should partition the indexed segments: {languages:?}"
    );

    let top_symbols = data["top_symbols"].as_array().unwrap();
    assert_eq!(
        top_symbols.len(),
        1,
        "only PolicyEngine has a qualifying type definition: {top_symbols:?}"
    );
    let top = &top_symbols[0];
    assert_eq!(top["name"], "PolicyEngine");
    assert_eq!(top["path"], "src/policy.rs");
    assert!(
        top["referencing_files"].as_u64().unwrap() >= 2,
        "both app files reference PolicyEngine: {top:?}"
    );
    assert_eq!(top["definition_count"], 1);
    assert!(top["line_start"].as_u64().unwrap() >= 1);
    assert!(top["line_end"].as_u64().unwrap() >= top["line_start"].as_u64().unwrap());

    let modules = data["modules"].as_array().unwrap();
    let module_names = modules
        .iter()
        .map(|module| module["module"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(module_names, BTreeSet::from(["app", "lib", "src"]));
    assert!(
        modules
            .iter()
            .all(|module| module["segments"].as_u64().unwrap() > 0),
        "every listed module should report its segment count: {modules:?}"
    );

    let dependencies = data["module_dependencies"].as_array().unwrap();
    assert_eq!(
        dependencies
            .iter()
            .map(|edge| (
                edge["source"].as_str().unwrap(),
                edge["target"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![("app", "src")],
        "the only cross-module reference flow is app -> src: {dependencies:?}"
    );
    assert_eq!(
        dependencies[0]["count"], 2,
        "both app files form distinct (file, symbol) dependency pairs: {dependencies:?}"
    );

    let entry_points = data["entry_points"].as_array().unwrap();
    assert!(
        !entry_points.is_empty(),
        "the PolicyEngine definition should surface as an entry point"
    );
    for entry in entry_points {
        assert!(
            matches!(
                entry["role"].as_str().unwrap(),
                "DEFINITION" | "ORCHESTRATION"
            ),
            "entry points should come from the existing role classification: {entry:?}"
        );
        assert!(!entry["handle"].as_str().unwrap().is_empty());
        assert!(!entry["path"].as_str().unwrap().is_empty());
    }
    assert!(
        entry_points
            .iter()
            .any(|entry| entry["path"] == "src/policy.rs"),
        "shallow definition segments should be listed: {entry_points:?}"
    );

    let summary = envelope["summary"].as_str().unwrap();
    assert!(
        summary.contains("Indexed 4 file(s)"),
        "summary should state headline statistics: {summary}"
    );
    assert!(
        summary.contains("PolicyEngine"),
        "summary should name the most-referenced type: {summary}"
    );

    let actions = envelope["next_actions"].as_array().unwrap();
    let symbol_action = actions
        .iter()
        .find(|action| action["tool"] == TOOL_SYMBOL)
        .expect("non-empty digest should suggest oneup_symbol on the top type");
    assert_eq!(symbol_action["arguments"]["name"], "PolicyEngine");
    let search_action = actions
        .iter()
        .find(|action| action["tool"] == TOOL_SEARCH)
        .expect("non-empty digest should suggest oneup_search on the densest module");
    let densest_module = modules[0]["module"].as_str().unwrap();
    assert_eq!(
        search_action["arguments"]["query"],
        format!("{densest_module} module responsibilities")
    );

    let handle = top["handle"].as_str().unwrap();
    let hydrated = client.call_tool(
        TOOL_GET,
        serde_json::json!({ "handles": [format!(":{handle}")] }),
    );
    assert_ne!(hydrated["isError"], true);
    let hydrated_envelope = mcp_structured(&hydrated);
    assert_eq!(hydrated_envelope["data"]["records"][0]["status"], "found");
    assert_eq!(
        hydrated_envelope["data"]["records"][0]["segment"]["path"], "src/policy.rs",
        "top-symbol handles should hydrate through oneup_get: {hydrated_envelope:?}"
    );
}

#[test]
fn mcp_overview_repeated_calls_return_byte_identical_payloads() {
    let tmp = create_overview_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());
    wait_for_mcp_last_update_complete(&mut client);

    let first = client.call_tool(TOOL_OVERVIEW, serde_json::json!({}));
    let second = client.call_tool(TOOL_OVERVIEW, serde_json::json!({}));

    assert_ne!(first["isError"], true);
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap(),
        "repeated overview calls on an unchanged index should be byte-identical"
    );
}

#[test]
fn mcp_overview_data_payload_stays_within_documented_budget() {
    let tmp = create_overview_budget_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());
    wait_for_mcp_last_update_complete(&mut client);

    let result = client.call_tool(TOOL_OVERVIEW, serde_json::json!({}));
    assert_ne!(result["isError"], true);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "ok");

    let data = &envelope["data"];
    assert_eq!(data["top_symbols"].as_array().unwrap().len(), 10);
    assert_eq!(data["modules"].as_array().unwrap().len(), 12);
    assert_eq!(data["module_dependencies"].as_array().unwrap().len(), 15);
    assert_eq!(data["entry_points"].as_array().unwrap().len(), 8);
    assert!(data["stats"]["languages"].as_array().unwrap().len() <= 10);

    let serialized = serde_json::to_string(data).unwrap();
    assert!(
        serialized.len() <= 8192,
        "digest data should stay within the documented payload budget with every section cap saturated; got {} bytes",
        serialized.len()
    );
}

#[test]
fn mcp_overview_ignores_rows_from_other_context() {
    let tmp = create_overview_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let project_root = tmp.path().canonicalize().unwrap();
    seed_foreign_context_overview_rows(&project_root, "other-context");

    let mut client = McpTestClient::start(&project_root);
    wait_for_mcp_last_update_complete(&mut client);
    let result = client.call_tool(TOOL_OVERVIEW, serde_json::json!({}));

    assert_ne!(result["isError"], true);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "ok");
    let data = &envelope["data"];

    assert_eq!(
        data["stats"]["indexed_files"], 4,
        "foreign-context files should not inflate active-context statistics: {data:?}"
    );
    assert!(
        data["stats"]["languages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|language| language["language"] != "go"),
        "foreign-context language should not leak into statistics: {data:?}"
    );
    assert!(
        data["modules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|module| module["module"] != "foreignctx"),
        "foreign-context module should not leak into the module map: {data:?}"
    );
    assert!(
        data["top_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .all(|symbol| symbol["name"] != "ForeignLeakWidget"),
        "foreign-context symbols should not leak into the ranking: {data:?}"
    );
    assert!(
        data["entry_points"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| !entry["path"].as_str().unwrap().starts_with("foreignctx/")),
        "foreign-context segments should not leak into entry points: {data:?}"
    );
}

#[test]
fn mcp_overview_empty_index_returns_zeroed_digest() {
    let project = TempDir::new().unwrap();
    let project_root = project.path().canonicalize().unwrap();
    seed_ready_empty_index(&project_root);

    let mut client = McpTestClient::start_with_isolated_state(&project_root);
    let result = client.call_tool(TOOL_OVERVIEW, serde_json::json!({}));

    assert_ne!(
        result["isError"], true,
        "a ready-but-empty index is a valid state, not an error: {result:?}"
    );
    assert_mcp_response_is_presentation_free(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "empty");

    let data = &envelope["data"];
    assert_eq!(data["status"], "empty");
    assert_eq!(data["stats"]["indexed_files"], 0);
    assert_eq!(data["stats"]["total_segments"], 0);
    assert!(data["stats"]["languages"].as_array().unwrap().is_empty());
    for section in [
        "top_symbols",
        "modules",
        "module_dependencies",
        "entry_points",
    ] {
        assert!(
            data[section].as_array().unwrap().is_empty(),
            "empty digest should keep the {section} section present but empty: {data:?}"
        );
    }
    assert_eq!(envelope["next_actions"][0]["tool"], TOOL_STATUS);
}

#[test]
fn mcp_overview_unready_index_returns_readiness_error() {
    let configured = TempDir::new().unwrap();
    let unready = TempDir::new().unwrap();
    fs::create_dir_all(unready.path().join(".git")).unwrap();
    fs::write(unready.path().join("lib.rs"), "pub fn unready_probe() {}\n").unwrap();

    let mut client = McpTestClient::start_with_isolated_state(configured.path());
    // The MCP daemon auto-start can pre-initialize an empty current-schema
    // index for the configured repository, so the unready probe must target
    // a separate repository the daemon has never touched.
    let result = client.call_tool(
        TOOL_OVERVIEW,
        serde_json::json!({ "path": unready.path().to_str().unwrap() }),
    );

    assert_eq!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    let envelope = mcp_structured(&result);
    assert_eq!(envelope["status"], "error");
    assert!(
        envelope["summary"]
            .as_str()
            .unwrap()
            .contains("no current index"),
        "unready index should produce the standard readiness-style error: {envelope:?}"
    );
    assert_eq!(envelope["next_actions"][0]["tool"], TOOL_STATUS);
}

#[test]
fn mcp_impact_preserves_trust_buckets_and_followups() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let expanded = client.call_tool(
        TOOL_IMPACT,
        serde_json::json!({ "symbol": "warm_cache_key" }),
    );
    assert_mcp_text_matches_summary(&expanded);
    let expanded_envelope = mcp_structured(&expanded);
    assert_eq!(expanded_envelope["status"], "expanded");
    assert!(
        !expanded_envelope["data"]["results"]
            .as_array()
            .unwrap()
            .is_empty(),
        "file impact should include primary likely-impact results"
    );
    assert!(
        expanded_envelope["data"]["contextual_results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["file_path"] == "src/cache/test_support.rs"),
        "impact output should preserve contextual lower-confidence guidance"
    );
    assert_eq!(expanded_envelope["next_actions"][0]["tool"], TOOL_GET);

    let search = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({ "query": "load auth config", "limit": 5 }),
    );
    let handle = mcp_structured(&search)["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["path"] == "src/auth/runtime.rs")
        .and_then(|record| record["handle"].as_str())
        .expect("search should return a runtime handle");
    let handle_impact = client.call_tool(
        TOOL_IMPACT,
        serde_json::json!({ "handle": format!(":{handle}") }),
    );
    assert_ne!(handle_impact["isError"], true);
    assert_mcp_response_is_presentation_free(&handle_impact);
    let handle_envelope = mcp_structured(&handle_impact);
    assert!(
        matches!(
            handle_envelope["status"].as_str().unwrap(),
            "expanded" | "expanded_scoped" | "empty" | "empty_scoped"
        ),
        "public handle anchor should resolve to advisory impact output: {handle_envelope:?}"
    );
    assert_mcp_next_actions_are_canonical(handle_envelope);

    let empty = client.call_tool(
        TOOL_IMPACT,
        serde_json::json!({ "file": "src/admin/config.rs" }),
    );
    let empty_envelope = mcp_structured(&empty);
    assert!(
        matches!(
            empty_envelope["status"].as_str().unwrap(),
            "empty" | "empty_scoped"
        ),
        "expected explicit empty impact status, got {empty_envelope:?}"
    );
    assert_mcp_next_actions_are_canonical(empty_envelope);
}

#[test]
fn mcp_impact_refusal_sets_is_error() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let result = client.call_tool(
        TOOL_IMPACT,
        serde_json::json!({ "handle": "does-not-exist" }),
    );

    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["status"], "refused");
    assert_eq!(result["structuredContent"]["data"]["status"], "refused");
    assert!(result["structuredContent"]["next_actions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|action| action["tool"].as_str().unwrap().starts_with("oneup_")));
}

#[test]
fn mcp_get_all_failed_handles_sets_is_error() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);
    let mut client = McpTestClient::start(tmp.path());

    let result = client.call_tool(
        TOOL_GET,
        serde_json::json!({ "handles": [":does-not-exist"] }),
    );

    assert_eq!(result["isError"], true);
    assert_mcp_response_is_presentation_free(&result);
    assert_eq!(result["structuredContent"]["status"], "empty");
    assert_eq!(
        result["structuredContent"]["data"]["records"][0]["status"],
        "not_found"
    );
    assert_eq!(
        result["structuredContent"]["next_actions"][0]["tool"],
        "oneup_search"
    );
}

#[test]
fn impact_rows_carry_channel_suffix() {
    // design §2.2, D5: every impact row ends with ` ~P` or ` ~C`.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, stderr, ok) = impact_output(tmp.path(), &["--from-file", "src/auth/runtime.rs"]);
    assert!(ok, "impact failed: {stderr}");

    // status_line should be absent on expanded envelopes; every non-empty
    // stdout line must end with the channel suffix.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.ends_with("  ~P") || line.ends_with("  ~C"),
            "every impact row must end with ~P or ~C, got {line:?}"
        );
    }

    let rows = parse_discovery_rows(&stdout);
    assert!(!rows.is_empty());
    // At least one primary; bootstrap is the known call site.
    assert!(rows.iter().any(|r| r.channel == Some('P')));
    assert!(
        rows.iter()
            .any(|r| r.channel == Some('P') && r.file_path == "src/auth/bootstrap.rs"),
        "expected bootstrap primary row in: {rows:?}"
    );
}

#[test]
fn impact_primary_precedes_contextual() {
    // All primary (~P) rows must appear before any contextual (~C) row so an
    // agent can split the stream by channel without re-sorting.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(tmp.path(), &["--from-symbol", "warm_cache_key"]);
    assert!(ok);

    let rows = parse_discovery_rows(&stdout);
    let first_contextual = rows.iter().position(|r| r.channel == Some('C'));
    if let Some(idx) = first_contextual {
        assert!(
            rows[..idx].iter().all(|r| r.channel == Some('P')),
            "primary rows must precede contextual rows"
        );
    }
}

#[test]
fn impact_file_anchor_surfaces_bootstrap_primary() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-file", "src/auth/runtime.rs"]);
    assert!(rows
        .iter()
        .any(|r| r.channel == Some('P') && r.file_path == "src/auth/bootstrap.rs"));
}

#[test]
fn impact_file_line_anchor_resolves_requested_line() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-file", "src/auth/runtime.rs:1"]);
    assert!(rows
        .iter()
        .any(|r| r.file_path == "src/auth/bootstrap.rs" && r.channel == Some('P')));
}

#[test]
fn impact_symbol_anchor_expands_with_resolved_seed() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "load_auth_config"]);
    assert!(rows
        .iter()
        .any(|r| r.file_path == "src/auth/bootstrap.rs" && r.channel == Some('P')));
}

#[test]
fn impact_symbol_anchor_scope_narrows_ambiguous_matches() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(
        tmp.path(),
        &["--from-symbol", "load_config", "--scope", "src/auth"],
    );
    assert!(!rows.is_empty());
    // top primary comes from the scoped subtree
    let top_primary = rows
        .iter()
        .find(|r| r.channel == Some('P'))
        .expect("at least one primary row");
    assert_eq!(top_primary.file_path, "src/auth/reload.rs");
}

#[test]
fn impact_symbol_anchor_qualified_relation_promotes_matching_definition() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "reload_auth_config"]);
    assert!(
        rows.iter()
            .any(|r| r.channel == Some('P') && r.file_path == "src/auth/config.rs"),
        "config.rs should appear as primary: {rows:?}"
    );
}

#[test]
fn impact_symbol_anchor_interface_implementor_surfaces_primary() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "AuthStore"]);
    assert!(rows.iter().any(|r| r.channel == Some('P')
        && r.file_path == "src/auth/auth_store.ts"
        && r.kind == "class"
        && r.symbol == "SqlAuthStore"));
}

#[test]
fn impact_symbol_anchor_formatter_implementor_stays_primary_under_reference_pressure() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "Formatter"]);
    let primaries: Vec<_> = rows.iter().filter(|r| r.channel == Some('P')).collect();
    assert!(!primaries.is_empty());
    assert_eq!(primaries[0].file_path, "src/ui/plain_formatter.ts");

    // Same path should not also appear in the contextual bucket.
    let contextual_has_plain = rows
        .iter()
        .any(|r| r.channel == Some('C') && r.file_path == "src/ui/plain_formatter.ts");
    assert!(
        !contextual_has_plain,
        "primary implementor should not also be duplicated as contextual"
    );
}

#[test]
fn impact_symbol_anchor_ambiguous_helper_emits_context_only_hint() {
    // Lean renderer collapses `empty` envelopes to a status line plus a hint
    // line (design §3.6); no discovery rows follow. The hint's `context_only`
    // code signals that contextual guidance exists without embedding the rows
    // directly on the wire.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(tmp.path(), &["--from-symbol", "boot_global_config"]);
    assert!(ok);

    assert_eq!(
        stdout.lines().next().unwrap_or(""),
        "empty",
        "expected bare `empty` status, got: {stdout}"
    );
    let hint_line = stdout
        .lines()
        .find(|l| l.starts_with("hint"))
        .expect("empty envelope should carry a hint line");
    assert!(hint_line.contains("context_only"));
    // No discovery rows: every remaining line is either the status or hint.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.starts_with("empty") || line.starts_with("hint"),
            "unexpected discovery row in empty envelope: {line:?}"
        );
    }
}

#[test]
fn impact_symbol_anchor_prefers_stronger_primary_over_wrapper() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "warm_cache_key"]);
    let primaries: Vec<_> = rows.iter().filter(|r| r.channel == Some('P')).collect();
    assert!(!primaries.is_empty());
    assert_eq!(primaries[0].file_path, "src/cache/worker.rs");

    if let Some(wrapper_idx) = primaries
        .iter()
        .position(|r| r.file_path == "src/cache/priming.rs")
    {
        assert!(wrapper_idx > 0, "wrapper should never outrank worker");
    }
}

#[test]
fn impact_symbol_anchor_inline_test_context_stays_contextual() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = impact_rows(tmp.path(), &["--from-symbol", "warm_cache_key"]);

    assert!(rows
        .iter()
        .filter(|r| r.channel == Some('P'))
        .all(|r| r.file_path != "src/cache/test_support.rs"));
    assert!(rows
        .iter()
        .any(|r| r.channel == Some('C') && r.file_path == "src/cache/test_support.rs"));
}

#[test]
fn impact_file_anchor_limit_caps_total_rows() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(
        tmp.path(),
        &["--from-file", "src/auth/runtime.rs", "--limit", "1"],
    );
    assert!(ok);

    let rows = parse_discovery_rows(&stdout);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, Some('P'));
}

#[test]
fn impact_file_anchor_scope_refuses_out_of_scope_seed() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(
        tmp.path(),
        &["--from-file", "src/auth/runtime.rs", "--scope", "src/cache"],
    );
    assert!(ok);

    let first_line = stdout.lines().next().unwrap_or("");
    assert!(
        first_line.starts_with("refused"),
        "expected refused line, got {first_line:?}"
    );
    assert!(first_line.contains("anchor_out_of_scope"));
    // Any hint line should point at alignment guidance.
    assert!(stdout
        .lines()
        .any(|l| l.starts_with("hint") && l.contains("align_anchor_and_scope")));
}

#[test]
fn impact_symbol_anchor_refuses_broad_requests_with_hint() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = impact_output(tmp.path(), &["--from-symbol", "load_config"]);
    assert!(ok);

    assert!(stdout.lines().next().unwrap_or("").starts_with("refused"));
    assert!(stdout.contains("symbol_too_broad"));
    assert!(stdout.lines().any(|l| l.starts_with("hint")
        && l.contains("narrow_with_scope")
        && l.contains("--scope")));
}

#[test]
fn impact_file_line_anchor_returns_empty_with_hint() {
    // Lean renderer: `empty` envelopes emit the status label plus a hint line
    // (no `~C` rows on the wire — see design §3.6).
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) =
        impact_output(tmp.path(), &["--from-file", "src/auth/runtime.rs:5"]);
    assert!(ok);

    assert_eq!(
        impact_status_line(&stdout).unwrap_or(""),
        "empty",
        "expected bare `empty` status, got {stdout:?}"
    );
    let hint = stdout
        .lines()
        .find(|l| l.starts_with("hint"))
        .expect("empty envelope should carry a hint line");
    assert!(hint.contains("context_only"));
}

#[test]
fn impact_scoped_file_line_anchor_returns_empty_scoped_with_hint() {
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let args = &[
        "--from-file",
        "src/auth/runtime.rs:5",
        "--scope",
        "src/auth",
    ];
    let (stdout, _stderr, ok) = impact_output(tmp.path(), args);
    assert!(ok);

    assert_eq!(
        stdout.lines().next().unwrap_or(""),
        "empty_scoped",
        "expected bare `empty_scoped` status line, got: {stdout}"
    );
    let hint = stdout
        .lines()
        .find(|l| l.starts_with("hint"))
        .expect("empty_scoped envelope should carry a hint line");
    assert!(hint.contains("context_only"));
    // Scope echoed on the hint line via `scope=<s>` per design §3.6.
    assert!(
        hint.contains("scope=src/auth"),
        "hint should echo the requested scope, got: {hint}"
    );
    // No discovery rows in empty_scoped envelopes.
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        assert!(
            line.starts_with("empty") || line.starts_with("hint"),
            "unexpected discovery row in empty_scoped envelope: {line:?}"
        );
    }
}

// =============================================================================
// Search -> get round-trip
// =============================================================================

fn parse_get_record_header(line: &str) -> Option<&str> {
    line.strip_prefix("segment ")
}

fn parse_get_records(stdout: &str) -> Vec<(String, Option<String>)> {
    // Parse lean `get` output: each record is `segment <id>\n<tab-metadata>\n\n<body>\n\n---\n`
    // or `not_found\t<raw>\n---\n`. Returns (id_or_raw, Some(content_string)) for
    // resolved records and (raw, None) for not_found.
    let lines: Vec<&str> = stdout.lines().collect();
    let mut records = Vec::new();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        if let Some(rest) = line.strip_prefix("not_found\t") {
            // Skip this line and the following `---` sentinel if present.
            idx += 1;
            if idx < lines.len() && lines[idx] == "---" {
                idx += 1;
            }
            records.push((rest.to_string(), None));
        } else if let Some(id) = parse_get_record_header(line) {
            // Advance past the header line.
            idx += 1;
            // Consume the tab-delimited metadata line.
            if idx < lines.len() {
                idx += 1;
            }
            // Consume the blank line separating metadata from body.
            if idx < lines.len() && lines[idx].is_empty() {
                idx += 1;
            }
            // Accumulate body lines until the `---` sentinel is reached; the
            // last blank line before `---` is considered the record terminator.
            let mut body = String::new();
            while idx < lines.len() && lines[idx] != "---" {
                let body_line = lines[idx];
                if body_line.is_empty() && idx + 1 < lines.len() && lines[idx + 1] == "---" {
                    idx += 1;
                    break;
                }
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(body_line);
                idx += 1;
            }
            // Consume the `---` sentinel if still pointing at it.
            if idx < lines.len() && lines[idx] == "---" {
                idx += 1;
            }
            records.push((id.to_string(), Some(body)));
        } else {
            idx += 1;
        }
    }
    records
}

#[test]
fn get_returns_body_for_known_handle() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = search_rows(tmp.path(), "Config host port");
    assert!(!rows.is_empty());
    let handle = rows[0].segment_id.clone();

    let (stdout, stderr, ok) = run_core_cmd(&[
        "get",
        &handle,
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok, "get failed: {stderr}");

    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 1);
    let (returned_id, body) = &records[0];
    assert!(
        returned_id.starts_with(&handle[..handle.len().min(returned_id.len())])
            || handle.starts_with(returned_id),
        "get header `segment {returned_id}` should correspond to queried handle {handle}"
    );
    assert!(body.as_ref().is_some_and(|b| !b.is_empty()));
}

#[test]
fn get_tolerates_leading_colon_handle() {
    // The lean row grammar emits `:<id>` as the trailing token; agents should
    // be able to paste that directly into `1up get`.
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = search_rows(tmp.path(), "Config");
    let handle_with_colon = format!(":{}", rows[0].segment_id);

    let (stdout, stderr, ok) = run_core_cmd(&[
        "get",
        &handle_with_colon,
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok, "get failed: {stderr}");
    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 1);
    assert!(
        records[0].1.is_some(),
        "leading-colon handle should resolve"
    );
}

#[test]
fn get_reports_not_found_for_unknown_handle() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "get",
        "ffffffffffff",
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    // `get` does not fail on an unresolved handle; it emits `not_found\t<raw>`.
    assert!(ok);
    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, "ffffffffffff");
    assert!(records[0].1.is_none());
}

#[test]
fn get_preserves_order_across_handles() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let rows = search_rows(tmp.path(), "Config");
    assert!(rows.len() >= 2, "need at least two hits for ordering test");
    let first = rows[0].segment_id.clone();
    let second = rows[1].segment_id.clone();

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "get",
        &first,
        "ffffffffffff",
        &second,
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);

    let records = parse_get_records(&stdout);
    assert_eq!(records.len(), 3);
    assert!(records[0].1.is_some());
    assert_eq!(records[1].0, "ffffffffffff");
    assert!(records[1].1.is_none());
    assert!(records[2].1.is_some());
}

// =============================================================================
// Search handle handoff: search -> impact --from-handle preserves ranking
// =============================================================================

#[test]
fn search_segment_id_round_trips_into_impact_from_segment() {
    // The lean row grammar emits a 12-char display handle (`:<prefix>`). `get`
    // resolves that prefix back to the full segment id, which is what
    // `impact --from-handle` expects for its exact-anchor lookup. This pins
    // the discovery -> hydrate -> impact follow-up chain at the row-grammar
    // layer.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let rows = search_rows(tmp.path(), "load auth config");
    let seed = rows
        .iter()
        .find(|r| r.file_path == "src/auth/runtime.rs")
        .expect("search should return the runtime definition segment");
    let handle_prefix = seed.segment_id.clone();

    let (get_stdout, _stderr, ok) = run_core_cmd(&[
        "get",
        &handle_prefix,
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let records = parse_get_records(&get_stdout);
    let full_segment_id = records
        .iter()
        .find_map(|(id, body)| body.as_ref().map(|_| id.clone()))
        .expect("get should resolve the prefix to a full segment id");
    assert!(full_segment_id.starts_with(&handle_prefix));

    let impact = impact_rows(tmp.path(), &["--from-handle", &full_segment_id]);
    assert!(!impact.is_empty());
    assert!(impact
        .iter()
        .any(|r| r.channel == Some('P') && r.file_path == "src/auth/bootstrap.rs"));
    // Seeds never appear in their own primary results.
    assert!(impact
        .iter()
        .all(|r| !full_segment_id.starts_with(&r.segment_id)));
}

#[test]
fn search_segment_id_handoff_keeps_search_top_hits_stable() {
    // The hand-off from `search` to `impact --from-handle` must not perturb
    // subsequent search ranking.
    let tmp = create_impact_acceptance_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let before = search_rows(tmp.path(), "load auth config");
    assert!(!before.is_empty());

    let segment_id = before[0].segment_id.clone();
    let _ = impact_rows(tmp.path(), &["--from-handle", &segment_id]);
    let after = search_rows(tmp.path(), "load auth config");

    let before_ranked: Vec<_> = before
        .iter()
        .take(5)
        .map(|r| (r.file_path.clone(), r.line_start, r.kind.clone()))
        .collect();
    let after_ranked: Vec<_> = after
        .iter()
        .take(5)
        .map(|r| (r.file_path.clone(), r.line_start, r.kind.clone()))
        .collect();
    assert_eq!(before_ranked, after_ranked);
}

// =============================================================================
// Context lean shape
// =============================================================================

#[test]
fn context_retrieval_returns_enclosing_scope() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        "main.rs:4",
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);

    // Header line: `<path>:<l1>-<l2>  context  <scope_type>`.
    let header = stdout.lines().next().unwrap_or("");
    let parts: Vec<&str> = header.split("  ").collect();
    assert_eq!(parts.len(), 3, "context header shape: {header:?}");
    assert!(
        parts[0].ends_with("main.rs:3-5") || parts[0].contains("main.rs:"),
        "context path/lines token shape: {:?}",
        parts[0]
    );
    assert_eq!(parts[1], "context");
    assert_eq!(parts[2], "function");

    // The enclosing body should quote `fn greet`.
    assert!(stdout.contains("fn greet"));
}

#[test]
fn context_retrieval_python_scope() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        "utils.py:6",
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    let header = stdout.lines().next().unwrap_or("");
    assert!(header.contains("  context  function"));
    assert!(stdout.contains("parse_config"));
}

#[test]
fn context_rejects_outside_root_by_default() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let outside_file = tmp.path().join("outside.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &outside_file,
        "fn leaked() {\n    println!(\"outside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", outside_file.display());

    cmd()
        .args([
            "context",
            &location,
            "--path",
            project_root.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--allow-outside-root"));
}

#[test]
fn context_rejects_absolute_in_root_path_by_default() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let in_root_file = project_root.join("in_root.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &in_root_file,
        "fn internal() {\n    println!(\"inside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", in_root_file.display());

    cmd()
        .args([
            "context",
            &location,
            "--path",
            project_root.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "absolute context paths are disabled by default",
        ))
        .stderr(predicate::str::contains("--allow-outside-root"));
}

#[test]
fn context_allows_absolute_in_root_path_with_explicit_override() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let in_root_file = project_root.join("in_root.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &in_root_file,
        "fn internal() {\n    println!(\"inside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", in_root_file.display());

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        &location,
        "--plain",
        "--path",
        project_root.to_str().unwrap(),
        "--allow-outside-root",
    ]);
    assert!(ok);
    let header = stdout.lines().next().unwrap_or("");
    assert!(header.contains("  context  function"));
    assert!(stdout.contains("fn internal"));
}

#[test]
fn context_allows_outside_root_with_explicit_override() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().join("project");
    let outside_file = tmp.path().join("outside.rs");
    fs::create_dir_all(&project_root).unwrap();
    fs::write(
        &outside_file,
        "fn leaked() {\n    println!(\"outside\");\n}\n",
    )
    .unwrap();
    let location = format!("{}:1", outside_file.display());

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "context",
        &location,
        "--plain",
        "--path",
        project_root.to_str().unwrap(),
        "--allow-outside-root",
    ]);
    assert!(ok);
    let header = stdout.lines().next().unwrap_or("");
    assert!(header.contains("  context  function"));
    assert!(stdout.contains("fn leaked"));
}

// =============================================================================
// Incremental indexing
// =============================================================================

#[test]
fn incremental_indexing_detects_changes() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let before = symbol_rows(tmp.path(), "greet", &[]);
    assert!(!before.is_empty());

    fs::write(
        tmp.path().join("main.rs"),
        r#"fn welcome(name: &str) -> String {
    format!("Welcome, {}", name)
}

fn main() {
    println!("{}", welcome("world"));
}
"#,
    )
    .unwrap();

    cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let after_greet = symbol_rows(tmp.path(), "greet", &[]);
    assert!(
        after_greet.is_empty(),
        "greet should no longer exist after re-index"
    );

    let after_welcome = symbol_rows(tmp.path(), "welcome", &[]);
    assert!(
        !after_welcome.is_empty(),
        "welcome should exist after re-index"
    );
    assert!(after_welcome
        .iter()
        .any(|r| r.symbol == "welcome" || r.breadcrumb.contains("welcome")));
}

#[test]
fn default_parallel_index_matches_jobs_one_for_incremental_cleanup() {
    let _guard = HideModelGuard::new();
    let default_repo = TempDir::new().unwrap();
    let serial_repo = TempDir::new().unwrap();

    write_parallel_regression_fixture(default_repo.path());
    write_parallel_regression_fixture(serial_repo.path());

    init_project(default_repo.path());
    init_project(serial_repo.path());

    let initial_default = run_index_json(default_repo.path(), &[]);
    let initial_serial = run_index_json(serial_repo.path(), &["--jobs", "1"]);
    assert!(
        initial_default["progress"]["files_indexed"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        initial_default["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        initial_serial["progress"]["files_indexed"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(
        initial_serial["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert_eq!(
        initial_default["progress"]["files_indexed"],
        initial_serial["progress"]["files_indexed"]
    );

    mutate_parallel_regression_fixture(default_repo.path());
    mutate_parallel_regression_fixture(serial_repo.path());

    let rerun_default = run_index_json(default_repo.path(), &[]);
    let rerun_serial = run_index_json(serial_repo.path(), &["--jobs", "1"]);
    assert!(rerun_default["progress"]["files_indexed"].as_u64().unwrap() > 0);
    assert!(
        rerun_default["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(rerun_serial["progress"]["files_indexed"].as_u64().unwrap() > 0);
    assert!(
        rerun_serial["progress"]["segments_stored"]
            .as_u64()
            .unwrap()
            > 0
    );

    for field in ["files_indexed", "files_skipped", "files_deleted"] {
        assert_eq!(
            rerun_default["progress"][field], rerun_serial["progress"][field],
            "mismatched {field} after incremental re-index"
        );
    }

    assert_eq!(rerun_default["progress"]["files_indexed"], 2);
    assert_eq!(rerun_default["progress"]["files_skipped"], 1);
    assert_eq!(rerun_default["progress"]["files_deleted"], 1);

    assert!(symbol_rows(default_repo.path(), "removed_symbol", &[]).is_empty());
    assert!(symbol_rows(serial_repo.path(), "removed_symbol", &[]).is_empty());
    assert_eq!(
        symbol_rows(default_repo.path(), "beta_symbol", &[]).len(),
        1
    );
    assert_eq!(symbol_rows(serial_repo.path(), "beta_symbol", &[]).len(), 1);
    assert_eq!(
        symbol_rows(default_repo.path(), "fresh_symbol", &[]).len(),
        1
    );
    assert_eq!(
        symbol_rows(serial_repo.path(), "fresh_symbol", &[]).len(),
        1
    );
    assert_eq!(
        symbol_rows(default_repo.path(), "stable_symbol", &[]).len(),
        1
    );
    assert_eq!(
        symbol_rows(serial_repo.path(), "stable_symbol", &[]).len(),
        1
    );
}

// =============================================================================
// Daemon lifecycle + PID
// =============================================================================

#[cfg(unix)]
#[test]
fn daemon_pid_file_lifecycle() {
    let tmp = TempDir::new().unwrap();
    let pid_path = tmp.path().join("test_daemon.pid");

    assert!(!pid_path.exists());

    let pid = std::process::id();
    fs::write(&pid_path, pid.to_string()).unwrap();
    assert!(pid_path.exists());

    let read_pid: u32 = fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(read_pid, pid);

    fs::remove_file(&pid_path).unwrap();
    assert!(!pid_path.exists());
}

#[cfg(unix)]
#[test]
fn daemon_stale_pid_detection() {
    let tmp = TempDir::new().unwrap();
    let pid_path = tmp.path().join("stale_daemon.pid");

    fs::write(&pid_path, "99999").unwrap();
    assert!(pid_path.exists());

    let content = fs::read_to_string(&pid_path).unwrap();
    let stale_pid: u32 = content.trim().parse().unwrap();

    let is_alive = unsafe {
        libc::kill(stale_pid as i32, 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    };
    assert!(
        !is_alive,
        "PID 99999 should not be a live process in test environment"
    );

    fs::remove_file(&pid_path).unwrap();
    assert!(!pid_path.exists(), "stale PID file should be cleaned up");
}

// =============================================================================
// add-mcp wrapper delegation
// =============================================================================

#[cfg(unix)]
#[test]
fn add_mcp_prefers_bunx_then_npx() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo");
    let log_path = tmp.path().join("runner.log");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("bunx"));
    write_fake_runner(&fake_bin.join("npx"));

    cmd()
        .env("PATH", &fake_bin)
        .env("ONEUP_FAKE_RUNNER_LOG", &log_path)
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--agent",
            "codex",
            "--yes",
        ])
        .assert()
        .success();

    let log = fs::read_to_string(log_path).unwrap();
    let canonical_repo = repo.canonicalize().unwrap();
    assert!(
        log.contains(&format!("cwd={}", canonical_repo.display())),
        "unexpected runner cwd: {log}"
    );
    assert!(log.contains("runner=bunx"), "unexpected runner log: {log}");
    assert!(log.contains("arg[0]=add-mcp"), "unexpected argv: {log}");
    assert!(log.contains("arg[1]=1up mcp"), "unexpected argv: {log}");
}

#[cfg(unix)]
#[test]
fn add_mcp_builds_oneup_server_command() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo with spaces");
    let log_path = tmp.path().join("runner.log");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("npx"));

    cmd()
        .env("PATH", &fake_bin)
        .env("ONEUP_FAKE_RUNNER_LOG", &log_path)
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--runner",
            "npx",
            "--agent",
            "codex",
            "--agent",
            "cursor",
            "--global",
            "--yes",
        ])
        .assert()
        .success();

    let canonical_repo = repo.canonicalize().unwrap();
    let expected_source = format!("arg[1]=1up mcp --path '{}'", canonical_repo.display());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("runner=npx"), "unexpected runner log: {log}");
    assert!(log.contains("arg[0]=add-mcp"), "unexpected argv: {log}");
    assert!(log.contains(&expected_source), "unexpected argv: {log}");
    assert!(log.contains("arg[2]=--name"), "unexpected argv: {log}");
    assert!(log.contains("arg[3]=oneup"), "unexpected argv: {log}");
    assert!(log.contains("arg[4]=--agent"), "unexpected argv: {log}");
    assert!(log.contains("arg[5]=codex"), "unexpected argv: {log}");
    assert!(log.contains("arg[6]=--agent"), "unexpected argv: {log}");
    assert!(log.contains("arg[7]=cursor"), "unexpected argv: {log}");
    assert!(log.contains("arg[8]=--global"), "unexpected argv: {log}");
    assert!(log.contains("arg[9]=--yes"), "unexpected argv: {log}");
}

#[cfg(unix)]
#[test]
fn add_mcp_runner_override_requires_available_runner() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("bunx"));

    cmd()
        .env("PATH", &fake_bin)
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--runner",
            "npx",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("Manual MCP setup fallback")
                .and(predicate::str::contains(
                    "selected runner `npx` was not found on PATH",
                ))
                .and(predicate::str::contains("call `oneup_status`")),
        );
}

#[cfg(unix)]
#[test]
fn add_mcp_fallback_has_manual_snippets() {
    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("bin");
    let repo = tmp.path().join("repo");
    let log_path = tmp.path().join("runner.log");
    fs::create_dir_all(&fake_bin).unwrap();
    fs::create_dir_all(&repo).unwrap();
    write_fake_runner(&fake_bin.join("npx"));

    cmd()
        .env("PATH", &fake_bin)
        .env("ONEUP_FAKE_RUNNER_LOG", &log_path)
        .env("ONEUP_FAKE_RUNNER_STATUS", "17")
        .args([
            "add-mcp",
            "--path",
            repo.to_str().unwrap(),
            "--runner",
            "npx",
        ])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("Manual MCP setup fallback")
                .and(predicate::str::contains("Project/workspace MCP JSON"))
                .and(predicate::str::contains("Project/workspace Codex TOML"))
                .and(predicate::str::contains("server identity `oneup`"))
                .and(predicate::str::contains("[\"mcp\"]"))
                .and(predicate::str::contains("[\"mcp\", \"--path\""))
                .and(predicate::str::contains("call `oneup_status`"))
                .and(predicate::str::contains("add-mcp exited with exit code 17")),
        );
}

#[test]
fn add_mcp_does_not_add_config_writer_artifacts() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for forbidden_path in [
        "src/cli/mcp_install.rs",
        "src/mcp/install",
        "src/mcp/codex.rs",
        "src/mcp/claude_code.rs",
        "src/mcp/cursor.rs",
        "src/mcp/vscode.rs",
    ] {
        assert!(
            !root.join(forbidden_path).exists(),
            "unexpected native MCP installer or host adapter artifact: {forbidden_path}"
        );
    }

    let wrapper = fs::read_to_string(root.join("src/cli/add_mcp.rs")).unwrap();
    let production_wrapper = wrapper.split("#[cfg(test)]").next().unwrap();
    for forbidden_snippet in [
        "mcp_install",
        "mcp-install",
        "std::fs::write",
        "fs::write",
        "File::create",
        "serde_json::to_writer",
        "toml::to_string",
    ] {
        assert!(
            !production_wrapper.contains(forbidden_snippet),
            "add-mcp wrapper should not include native config mutation snippet {forbidden_snippet:?}"
        );
    }
}

// =============================================================================
// End-to-end workflow + maintenance command JSON surface
// =============================================================================

#[test]
fn cli_init_then_index_then_search_workflow() {
    let tmp = create_multi_lang_fixture();
    let _guard = HideModelGuard::new();

    cmd()
        .args(["init", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Initialized"));

    let id_path = tmp.path().join(".1up").join("project_id");
    assert!(id_path.exists());

    cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Indexed"));

    // Search now renders lean rows; just assert it succeeds.
    cmd()
        .args(["search", "logger", "--path", tmp.path().to_str().unwrap()])
        .assert()
        .success();
}

#[test]
fn index_json_output_includes_progress_summary() {
    let tmp = create_multi_lang_fixture();
    let _guard = HideModelGuard::new();

    cmd()
        .args(["init", tmp.path().to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let output = cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let progress = &payload["progress"];

    assert!(payload["message"].as_str().unwrap().contains("Indexed"));
    assert_eq!(progress["state"], "complete");
    assert_eq!(progress["phase"], "complete");
    assert!(progress["files_scanned"].as_u64().unwrap() > 0);
    assert!(progress["segments_stored"].as_u64().unwrap() > 0);
    assert_eq!(progress["embeddings_enabled"], false);
    assert!(progress["updated_at"].as_str().is_some());
}

#[test]
fn status_json_reports_noop_index_progress() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let second_index = cmd()
        .args(["index", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();
    assert!(second_index.status.success());

    let output = cmd()
        .args(["status", tmp.path().to_str().unwrap(), "--format", "json"])
        .output()
        .unwrap();

    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).unwrap();
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let progress = &payload["index_progress"];

    assert_eq!(progress["state"], "complete");
    assert_eq!(progress["phase"], "complete");
    assert_eq!(progress["files_indexed"], 0);
    assert_eq!(progress["segments_stored"], 0);
    assert!(progress["files_skipped"].as_u64().unwrap() > 0);
    assert_eq!(progress["files_total"], progress["files_scanned"]);
    assert_eq!(progress["embeddings_enabled"], false);
    assert!(payload["indexed_files"].as_u64().unwrap() > 0);
}

// =============================================================================
// Daemon IPC: lean SearchResult round-trip
// =============================================================================

#[cfg(unix)]
#[test]
fn daemon_response_carries_lean_results() {
    // The CLI should deserialize the lean `SearchResult` shape sent back by the
    // daemon (framed JSON over Unix socket) and re-render it through the lean
    // row grammar on stdout.
    let home = tempfile::Builder::new()
        .prefix("1up-home-")
        .tempdir_in("/tmp")
        .unwrap();
    let project = TempDir::new().unwrap();
    let socket_path = test_data_dir(home.path()).join("daemon.sock");
    let expected_root = project.path().canonicalize().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let server_socket_path = socket_path.clone();
    let server_expected_root = expected_root.clone();
    let server = std::thread::spawn(move || {
        use std::os::unix::net::UnixListener;

        if let Some(parent) = server_socket_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let _ = fs::remove_file(&server_socket_path);

        let listener = UnixListener::bind(&server_socket_path).unwrap();
        ready_tx.send(()).unwrap();
        let (mut stream, _) = listener.accept().unwrap();

        let payload = read_framed_json(&mut stream);
        assert_eq!(
            payload["project_root"].as_str().unwrap(),
            server_expected_root.to_str().unwrap()
        );
        assert_eq!(payload["query"], "test");
        assert_eq!(payload["limit"], 3);

        // Lean SearchResult: segment_id required, score u32 integer, no
        // complexity/role/referenced_symbols/called_symbols fields.
        let response = serde_json::json!({
            "status": "results",
            "results": [
                {
                    "segment_id": "daemonseg000",
                    "file_path": "src/daemon.rs",
                    "language": "rust",
                    "block_type": "function",
                    "content": "fn daemon_search() {}",
                    "score": 87,
                    "line_number": 3,
                    "line_end": 5
                }
            ]
        });
        write_framed_json(&mut stream, &response);
    });

    ready_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();

    let output = cmd()
        .env("HOME", home.path())
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .args(["search", "test", "--path", project.path().to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let rows = parse_discovery_rows(&stdout);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].score, 87);
    assert_eq!(rows[0].file_path, "src/daemon.rs");
    assert_eq!(rows[0].line_start, 3);
    assert_eq!(rows[0].line_end, 5);
    assert_eq!(rows[0].segment_id, "daemonseg000");

    server.join().unwrap();
}

// =============================================================================
// Flag rejection on core commands
// =============================================================================

#[test]
fn search_rejects_format_flag() {
    // core commands reject all presentation flags at clap parse time.
    for flag_pair in [["-f", "human"], ["--format", "json"], ["-f", "plain"]] {
        cmd()
            .args(["search", "needle", flag_pair[0], flag_pair[1]])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

#[test]
fn core_commands_reject_legacy_flags() {
    // no core command should quietly accept `--full`, `--human`, or
    // `--verbose-fields` either.
    for bad_flag in ["--full", "--human", "--verbose-fields"] {
        cmd()
            .args(["search", "needle", bad_flag])
            .assert()
            .failure();
    }
}

#[test]
fn cli_search_without_index_requires_reindex() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args(["search", "test", "--path", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1up reindex"));
}

#[test]
fn cli_symbol_without_index_requires_reindex() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args(["symbol", "test", "--path", tmp.path().to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("1up reindex"));
}

#[test]
fn cli_context_nonexistent_file_fails() {
    let tmp = TempDir::new().unwrap();

    cmd()
        .args([
            "context",
            "nonexistent.rs:1",
            "--path",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn cli_search_empty_results_emits_nothing() {
    let tmp = create_multi_lang_fixture();
    let _guard = init_and_index_fts_only(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "search",
        "zznonexistentqueryzz",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    assert!(
        stdout.lines().filter(|l| !l.is_empty()).count() == 0,
        "empty search should emit zero rows, got: {stdout:?}"
    );
}

#[test]
fn cli_symbol_empty_results_emits_nothing_on_stdout() {
    let tmp = create_multi_lang_fixture();
    init_and_index(&tmp);

    let (stdout, _stderr, ok) = run_core_cmd(&[
        "symbol",
        "zznonexistentsymbolzz",
        "--plain",
        "--path",
        tmp.path().to_str().unwrap(),
    ]);
    assert!(ok);
    assert!(
        stdout.lines().filter(|l| !l.is_empty()).count() == 0,
        "empty symbol lookup should emit zero rows, got: {stdout:?}"
    );
}

#[test]
fn cli_worktree_resolves_to_main_repo_index() {
    // Isolated fake HOME so this test gets its OWN daemon instead of sharing the
    // process-wide real-HOME daemon with every other bare-`cmd()` test. Under the
    // `security-check` job those tests hammer a single shared daemon at once, and
    // this test's post-`reindex` search would race that daemon's background
    // atomic-swap schema-init window — exceeding the CLI read path's ~450ms
    // tolerating-init budget — and flake with "index schema is missing or
    // unreadable". A private HOME removes the cross-test contention; the seed marker
    // keeps indexing FTS-only. Mirrors `branch_context_search_excludes_other_worktree_only_content`.
    let home = TempDir::new().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    seed_model_download_failure(&canonical_home);

    let tmp = TempDir::new().unwrap();
    let tmp_root = tmp.path().canonicalize().unwrap();
    let main_repo = tmp_root.join("main");
    fs::create_dir_all(&main_repo).unwrap();

    std::process::Command::new("git")
        .args(["init", main_repo.to_str().unwrap()])
        .output()
        .expect("git init failed");

    fs::write(
        main_repo.join("hello.rs"),
        "fn greet() -> &'static str { \"hello\" }\n",
    )
    .unwrap();

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(&main_repo)
        .output()
        .expect("git add failed");

    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(&main_repo)
        .output()
        .expect("git commit failed");

    cmd_with_home(&canonical_home)
        .args(["init", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    cmd_with_home(&canonical_home)
        .args(["index", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let worktree_path = tmp_root.join("wt-feature");
    std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
            "-b",
            "feature-branch",
        ])
        .current_dir(&main_repo)
        .output()
        .expect("git worktree add failed");

    assert!(worktree_path.join(".git").is_file());

    let status_output = cmd_with_home(&canonical_home)
        .args([
            "status",
            worktree_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .unwrap();

    assert!(
        status_output.status.success(),
        "status from worktree failed: {}",
        String::from_utf8_lossy(&status_output.stderr)
    );

    let status_json: serde_json::Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status_json["project_initialized"], true);

    // Core command from a worktree renders lean rows and should succeed against
    // the main repo's index.
    cmd_with_home(&canonical_home)
        .args(["search", "greet", "--path", worktree_path.to_str().unwrap()])
        .assert()
        .success();

    // Write a worktree-only file and re-index from the worktree; the indexer
    // scans the worktree's files, not the main repo's.
    fs::write(
        worktree_path.join("worktree_only.rs"),
        "fn worktree_exclusive() -> bool { true }\n",
    )
    .unwrap();

    cmd_with_home(&canonical_home)
        .args([
            "reindex",
            worktree_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success();

    let rows = search_rows_with_home(&canonical_home, &worktree_path, "worktree_exclusive");
    assert!(
        !rows.is_empty(),
        "worktree-only symbol should appear after reindex from worktree"
    );
}

#[test]
fn branch_context_search_excludes_other_worktree_only_content() {
    let home = TempDir::new().unwrap();
    let canonical_home = home.path().canonicalize().unwrap();
    seed_model_download_failure(&canonical_home);
    let (_tmp, main_repo, feature_worktree) = create_branch_filtering_fixture();

    cmd_with_home(&canonical_home)
        .args(["init", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
    cmd_with_home(&canonical_home)
        .args(["index", main_repo.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
    cmd_with_home(&canonical_home)
        .args([
            "index",
            feature_worktree.to_str().unwrap(),
            "--format",
            "json",
        ])
        .assert()
        .success();

    let main_rows = search_rows_with_home(
        &canonical_home,
        &main_repo,
        "main branch only acceptance sentinel",
    );
    assert!(
        main_rows
            .iter()
            .any(|row| row.file_path.ends_with("main_only.rs")),
        "main branch should find main-only content: {main_rows:?}"
    );
    assert!(
        !search_rows_with_home(
            &canonical_home,
            &main_repo,
            "feature branch only acceptance sentinel",
        )
        .iter()
        .any(|row| row.file_path.ends_with("feature_only.rs")),
        "main branch search must not return feature-only file rows"
    );

    let feature_rows = search_rows_with_home(
        &canonical_home,
        &feature_worktree,
        "feature branch only acceptance sentinel",
    );
    assert!(
        feature_rows
            .iter()
            .any(|row| row.file_path.ends_with("feature_only.rs")),
        "feature branch should find feature-only content: {feature_rows:?}"
    );
    assert!(
        !search_rows_with_home(
            &canonical_home,
            &feature_worktree,
            "main branch only acceptance sentinel",
        )
        .iter()
        .any(|row| row.file_path.ends_with("main_only.rs")),
        "feature branch search must not return main-only file rows"
    );

    assert!(
        search_rows_with_home(
            &canonical_home,
            &main_repo,
            "shared branch acceptance sentinel"
        )
        .iter()
        .any(|row| row.file_path.ends_with("shared.rs")),
        "main branch should keep shared content discoverable"
    );
    assert!(
        search_rows_with_home(
            &canonical_home,
            &feature_worktree,
            "shared branch acceptance sentinel",
        )
        .iter()
        .any(|row| row.file_path.ends_with("shared.rs")),
        "feature branch should keep shared content discoverable"
    );
}

/// REQ-003: the `state_root`-keyed single-writer rebuild lock must serialize
/// concurrent rebuilds — exactly one process performs the rebuild while a second
/// concurrent attempt defers (never starting a competing destructive rebuild) —
/// and must release on drop so a later rebuild can proceed.
#[cfg(unix)]
#[test]
fn rebuild_lock_serializes_concurrent_rebuilds_to_a_single_writer() {
    use oneup::daemon::lifecycle::{acquire_rebuild_lock, try_acquire_rebuild_lock};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Arc;

    let tmp = TempDir::new().unwrap();
    let state_root = tmp.path().canonicalize().unwrap();
    let rebuilds = Arc::new(AtomicUsize::new(0));

    let (winner_holds_lock_tx, winner_holds_lock_rx) = mpsc::channel::<()>();
    let (loser_done_tx, loser_done_rx) = mpsc::channel::<()>();

    let winner = {
        let state_root = state_root.clone();
        let rebuilds = Arc::clone(&rebuilds);
        thread::spawn(move || {
            let _lock =
                acquire_rebuild_lock(&state_root).expect("winner acquires the rebuild lock");
            // Model "this process performs the rebuild" while holding the lock.
            rebuilds.fetch_add(1, Ordering::SeqCst);
            winner_holds_lock_tx.send(()).unwrap();
            // Hold the lock until the loser has attempted its concurrent rebuild.
            loser_done_rx.recv().unwrap();
            // `_lock` drops here, releasing the flock for any later acquirer.
        })
    };

    // The loser attempts a concurrent rebuild only once the winner provably
    // holds the lock, so the contention window is deterministic.
    winner_holds_lock_rx.recv().unwrap();
    let loser_guard =
        try_acquire_rebuild_lock(&state_root).expect("loser's lock attempt must not error");
    if loser_guard.is_some() {
        // A competing rebuild would have started here — it must not.
        rebuilds.fetch_add(1, Ordering::SeqCst);
    }
    assert!(
        loser_guard.is_none(),
        "a second concurrent rebuild must defer while the single-writer lock is held"
    );
    loser_done_tx.send(()).unwrap();
    winner.join().unwrap();

    assert_eq!(
        rebuilds.load(Ordering::SeqCst),
        1,
        "exactly one process performs the rebuild under contention"
    );

    // The lock released on drop: a fresh rebuild can now acquire it.
    drop(acquire_rebuild_lock(&state_root).expect("rebuild lock re-acquires after release"));
}

/// Cancelling an in-flight indexing pass *after a non-zero prefix has already
/// committed* must leave the on-disk libSQL index consistent (stopped at a
/// committed batch boundary, not torn mid-write) AND resumable: the committed
/// prefix survives, a freshly-opened DB handle still validates via
/// `ensure_current` and reads, and a subsequent uncancelled pass completes the
/// remainder to the full file count.
///
/// This is the design's headline T7 "resume-don't-drop" guarantee (REQ-002):
/// the prior cancellation tests all cancel *before* the pass (a 0->N from-scratch
/// resume), so the resume-from-a-committed-prefix path was unguarded. The
/// mid-pass cancellation here is DETERMINISTIC, not timing-based: a libSQL
/// update hook on the pipeline's own connection fires on the first committed
/// row write and cancels the token, so the very next safe-point check (loop top
/// / pre-flush — never mid-flush) surfaces `Cancelled` with at least one batch
/// already durably committed. No wall-clock sleep or race is involved.
///
/// The timing claim (SIGTERM interrupts a real daemon pass within the ~3s bound)
/// was validated separately by the HYP-001/HYP-003 daemon CODE_EXPERIMENT.
#[cfg(unix)]
#[test]
fn cancelled_mid_pass_keeps_committed_prefix_reopens_and_resumes() {
    use libsql::Op;
    use oneup::indexer::pipeline;
    use oneup::shared::types::{
        BranchStatus, IndexingConfig, RunScope, WorktreeContext, WorktreeRole,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    const FILES: usize = 12;
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for i in 0..FILES {
        fs::write(
            root.join(format!("mod_{i}.rs")),
            format!("pub fn item_{i}() -> usize {{ {i} }}\n"),
        )
        .unwrap();
    }

    let db_path = root.join(".1up").join("index.db");
    let context = WorktreeContext {
        context_id: "ctx-cancel-midpass".to_string(),
        state_root: root.clone(),
        source_root: root.clone(),
        main_worktree_root: root.clone(),
        worktree_role: WorktreeRole::Main,
        git_dir: None,
        common_git_dir: None,
        branch_name: Some("main".to_string()),
        branch_ref: Some("refs/heads/main".to_string()),
        head_oid: Some("0".repeat(40)),
        branch_status: BranchStatus::Named,
    };
    // Determinism: jobs = 1 + write_batch_files = 1. With a single parse worker,
    // exactly one file is ever in flight, so the dispatch loop processes files in
    // submission order and each `flush_reorder_buffer` call can only ever drain
    // ONE buffered file (one committed batch). The update hook below cancels the
    // token on that first committed insert, so the very next safe-point check
    // (loop top, before dispatching file 1 — never mid-flush) observes the cancel
    // and returns, leaving a committed prefix of exactly 1. This removes the
    // scheduling race that a multi-worker config has, where all files can land in
    // the reorder buffer and a single flush commits them all before the next
    // safe-point is reached. (Real-world cancel granularity is fine regardless —
    // validated separately by HYP-003: files parse over time and flush
    // incrementally, giving 17-53ms interruption. This knob is test-only.)
    let config =
        IndexingConfig::with_glob_config(1, 1, 1, Vec::new(), Vec::new(), Vec::new()).unwrap();

    block_on(async {
        let db = Db::open_rw(&db_path).await.unwrap();
        schema::initialize(&db.connect_tuned().await.unwrap())
            .await
            .unwrap();

        // The token starts LIVE. An update hook on the pipeline's own connection
        // cancels it on the first committed row insert — deterministically
        // landing the cancellation *after* at least one batch has committed.
        let token = CancellationToken::new();
        let conn = db.connect_tuned().await.unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        {
            let token = token.clone();
            let fired = Arc::clone(&fired);
            conn.add_update_hook(Box::new(move |op, _db, _table, _rowid| {
                if op == Op::Insert && !fired.swap(true, Ordering::SeqCst) {
                    // First committed write: request cancellation. The in-flight
                    // batch still commits; the next loop-top / pre-flush check
                    // (never mid-flush) returns Cancelled.
                    token.cancel();
                }
            }))
            .expect("registering the update hook must succeed");
        }

        let result = pipeline::run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            Some(&root),
            &token,
        )
        .await;
        assert!(
            matches!(
                result,
                Err(oneup::shared::errors::OneupError::Indexing(
                    oneup::shared::errors::IndexingError::Cancelled
                ))
            ),
            "the mid-pass cancelled pass must surface the Cancelled outcome, got: {result:?}"
        );
        assert!(
            fired.load(Ordering::SeqCst),
            "the update hook must have fired (a write must have been committed before cancellation)"
        );
    });

    // Reopen a FRESH handle on the same on-disk DB: it must validate cleanly,
    // be readable, and expose a STRICTLY-PARTIAL committed prefix (0 < n < FILES)
    // — proof the pass stopped at a committed batch boundary mid-way, not at 0
    // (pre-cancel) and not at FILES (no cancellation). With jobs=1 +
    // write_batch=1 the depth-1 store double-buffer bounds the prefix to at
    // most 3: when the first batch's committed write fires the cancel, one
    // batch may already be executing in the store task and one more may be
    // queued in the depth-1 channel (both drain at a committed boundary)
    // before the embed loop observes the cancel safe-point. Asserting 1..=3
    // still catches any regression that drains unboundedly past a cancel.
    block_on(async {
        let db = Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect_tuned().await.unwrap();
        schema::ensure_current(&conn, &schema::SchemaContext::unspecified())
            .await
            .expect("a cancelled pass must leave the reopened index schema-valid");
        let prefix = segments::count_files_for_context(&conn, "ctx-cancel-midpass")
            .await
            .expect("reading the reopened index must succeed") as usize;
        assert!(
            prefix > 0 && prefix < FILES,
            "a mid-pass cancellation must leave a non-zero, incomplete committed prefix; \
             got {prefix} of {FILES}"
        );
        assert!(
            (1..=3).contains(&prefix),
            "with jobs=1 + write_batch=1 and a depth-1 store double-buffer the cancel \
             must land within the pipeline window (triggering commit + in-flight + \
             queued = at most three committed batches); got {prefix} of {FILES}"
        );

        // A subsequent uncancelled pass resumes the remainder against the
        // consistent-but-incomplete index and completes to the full count.
        let live = CancellationToken::new();
        let stats = pipeline::run_with_context_scope_setup_and_progress_root(
            &conn,
            &context,
            None,
            &RunScope::Full,
            &config,
            None,
            false,
            None,
            None,
            Some(&root),
            &live,
        )
        .await
        .expect("a normal pass after cancellation must complete");
        assert_eq!(
            stats.files_skipped + stats.files_indexed,
            FILES,
            "the resumed pass must account for every source file"
        );
        assert_eq!(
            segments::count_files_for_context(&conn, "ctx-cancel-midpass")
                .await
                .unwrap() as usize,
            FILES,
            "every file must be committed after the resumed pass"
        );
    });
}

// =============================================================================
// Non-destructive background index rebuild (build-aside + atomic switch-over)
//
// These are the end-to-end guards the design's Validation Plan defers to T7. The
// decisive in-process primitive guards already live inline:
//   - all-or-nothing swap / new-generation / sidecar retirement (HYP-001):
//     `src/storage/swap.rs` (`swap_is_all_or_nothing_under_concurrent_readers`,
//     `swap_replaces_index_with_new_generation_and_leaves_no_sidecars`),
//   - aborted-rebuild-leaves-prior-index-intact: `src/storage/swap.rs`
//     (`aborted_staging_rebuild_leaves_prior_index_intact_and_no_orphan`,
//     `swap_leaves_prior_index_unchanged_when_staging_missing`),
//   - daemon stale-handle reopen after a swap (HYP-002): `src/daemon/worker.rs`
//     (`reopen_adopts_swapped_index_so_writes_land_in_the_new_inode`),
//   - stale/embeddings reason combination + scoping: `src/shared/types.rs` and
//     `src/mcp/ops.rs` detector tests.
// The tests below exercise the assembled behavior through the real CLI reindex,
// the real MCP search path, and the CLI render seam.
// =============================================================================

/// Open `index.db` read-only, gate it through `ensure_current`, and return the
/// segment count — the "is this a complete, valid index?" probe a real reader
/// runs. Retries on a transient lock the same way the production read paths do
/// (`retry_on_db_lock`), so a momentary checkpoint/rename lock is absorbed rather
/// than misread as a torn index; a genuinely absent/partial index can never occur
/// because the switch-over is a single atomic rename.
async fn count_segments_ro(db_path: &Path) -> Result<i64, String> {
    let mut last = String::new();
    for _ in 0..50 {
        match count_segments_ro_once(db_path).await {
            Ok(count) => return Ok(count),
            Err(err) => {
                last = err;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
    Err(last)
}

async fn count_segments_ro_once(db_path: &Path) -> Result<i64, String> {
    let ro = Db::open_ro(db_path).await.map_err(|e| e.to_string())?;
    let conn = ro.connect().map_err(|e| e.to_string())?;
    schema::ensure_current(&conn, &schema::SchemaContext::unspecified())
        .await
        .map_err(|e| e.to_string())?;
    let mut rows = conn
        .query(queries::COUNT_SEGMENTS, ())
        .await
        .map_err(|e| e.to_string())?;
    let row = rows
        .next()
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "count query returned no row".to_string())?;
    row.get::<i64>(0).map_err(|e| e.to_string())
}

/// Whether the served index holds a segment for `repo_relative_path` (the new
/// generation's distinguishing content). The path is a test-controlled literal.
async fn served_index_has_file(db_path: &Path, repo_relative_path: &str) -> bool {
    let ro = Db::open_ro(db_path).await.unwrap();
    let conn = ro.connect().unwrap();
    schema::ensure_current(&conn, &schema::SchemaContext::unspecified())
        .await
        .unwrap();
    let sql = format!("SELECT COUNT(*) FROM segments WHERE file_path = '{repo_relative_path}'");
    let mut rows = conn.query(sql.as_str(), ()).await.unwrap();
    let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
    count > 0
}

/// REQ-001 AC4 + AC2 (integration): a *real* `1up reindex` builds the refreshed
/// index aside and switches it over in a single atomic rename. A reader
/// repeatedly inspecting the served index throughout the reindex only ever
/// observes a full valid index (never absent/empty/partial); the switch installs
/// a *new inode* (build-aside-then-rename, not edit-in-place, mirroring the binary
/// replacement discipline); and a fresh read afterwards is the new generation.
///
/// The new inode is also the HYP-002 substrate: any handle opened before the swap
/// is left on the orphaned old inode — the exact reason the daemon must reopen
/// after a swap. The daemon's reopen response is unit-covered in
/// `src/daemon/worker.rs`; here we prove the cross-process switch installs the new
/// inode under a live concurrent reader.
#[cfg(unix)]
#[test]
fn reindex_switch_over_is_all_or_nothing_with_atomic_inode_replacement() {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let _guard = HideModelGuard::new();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let db_path = root.join(".1up").join("index.db");

    // Seed a prior served index (FTS-only) holding only the "alpha" generation.
    fs::write(
        root.join("alpha.rs"),
        "pub fn alpha_generation_marker() -> u32 { 1 }\n",
    )
    .unwrap();
    cmd()
        .args(["init", root.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
    cmd()
        .args(["index", root.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();

    let prior_inode = fs::metadata(&db_path).unwrap().ino();
    let prior_count = block_on(count_segments_ro(&db_path)).expect("prior index is a full index");
    assert!(
        prior_count > 0,
        "the prior index must hold the alpha generation"
    );
    assert!(
        !block_on(served_index_has_file(&db_path, "beta.rs")),
        "the prior generation must not yet contain beta.rs"
    );

    // Introduce the new "beta" generation, then sample the served index from a
    // background reader straddling the real reindex.
    fs::write(
        root.join("beta.rs"),
        "pub fn beta_generation_marker() -> u32 { 2 }\n",
    )
    .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let db_path = db_path.clone();
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let mut samples = Vec::new();
            while !stop.load(Ordering::Acquire) {
                samples.push(rt.block_on(count_segments_ro(&db_path)));
                thread::sleep(Duration::from_millis(1));
            }
            // A few more samples after the reindex returns so some land on the
            // settled new inode.
            for _ in 0..5 {
                samples.push(rt.block_on(count_segments_ro(&db_path)));
            }
            samples
        })
    };

    cmd()
        .args(["reindex", root.to_str().unwrap(), "--format", "json"])
        .assert()
        .success();
    stop.store(true, Ordering::Release);
    let samples = sampler.join().unwrap();

    // REQ-001 AC4: every inspection during the reindex saw a complete, valid index
    // — never absent, empty, or partial.
    assert!(!samples.is_empty(), "the sampler must observe the index");
    for sample in &samples {
        let count = sample
            .as_ref()
            .unwrap_or_else(|err| panic!("every sample must be a full valid index: {err}"));
        assert!(
            *count > 0,
            "a sample observed an empty index ({count} rows) mid-reindex"
        );
    }

    // REQ-001 AC2: the switch-over is a build-aside atomic rename, so the served
    // file is a *new inode* (an in-place rebuild would keep the same inode).
    let new_inode = fs::metadata(&db_path).unwrap().ino();
    assert_ne!(
        prior_inode, new_inode,
        "the atomic switch-over must replace index.db with a freshly-built inode"
    );

    // The fresh read is unambiguously the new generation (REQ-001 AC4 / HYP-001
    // post-swap-is-new-generation), proving the rename flipped readers over.
    assert!(
        block_on(served_index_has_file(&db_path, "beta.rs")),
        "the served index after reindex must be the new generation (beta.rs present)"
    );
}

/// REQ-002 AC1 + REQ-003 AC2/AC4 (integration, MCP surface): while a rebuild is in
/// progress and a usable prior index exists, MCP `oneup_search` keeps returning the
/// prior index's results (stale-but-available) and folds the stale notice into
/// `degraded_reason` — combined with the pre-existing embeddings-unavailable reason,
/// neither dropping the other. Rebuild-in-progress is modelled by holding the
/// single-writer rebuild lock from the test process; the MCP server (a separate
/// process) detects it via the out-of-process `try_acquire_rebuild_lock` probe, so
/// the signal is deterministic and independent of daemon refresh-state timing.
#[cfg(unix)]
#[test]
fn mcp_search_during_rebuild_serves_prior_results_with_stale_degraded_reason() {
    use oneup::daemon::lifecycle::acquire_rebuild_lock;
    use oneup::shared::constants::{NO_INDEXED_EMBEDDINGS_REASON, STALE_REBUILD_REASON};

    let project = TempDir::new().unwrap();
    let root = project.path().canonicalize().unwrap();
    // A git repo structure so `index_if_missing` can auto-initialize and index
    // (loose files without a repo are blocked — see
    // `mcp_start_reports_blocked_when_indexing_cannot_auto_initialize`).
    fs::create_dir_all(root.join(".git").join("refs").join("heads")).unwrap();
    fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("lib.rs"),
        "pub fn rebuild_stale_marker_fn() -> &'static str {\n    \"ready\"\n}\n",
    )
    .unwrap();

    // Build a real FTS-only index through the MCP start flow (isolated state seeds
    // a model-download failure, so the index carries no embeddings).
    let mut client = McpTestClient::start_with_isolated_state(&root);
    client.call_tool(
        TOOL_START,
        serde_json::json!({ "mode": "index_if_missing" }),
    );
    wait_for_mcp_searchable_readiness(&mut client);

    // Model "a rebuild is in progress": hold the single-writer rebuild lock.
    let lock = acquire_rebuild_lock(&root).expect("test holds the rebuild lock");

    let result = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({ "query": "rebuild_stale_marker_fn" }),
    );
    let envelope = mcp_structured(&result);

    // Stale-but-available: the prior index is still served (REQ-002 AC1).
    let hits = envelope["data"]["results"]
        .as_array()
        .expect("search payload carries a results array");
    assert!(
        !hits.is_empty(),
        "a rebuild-in-progress search must still serve prior results: {result}"
    );

    // The notice rides only in `degraded_reason` and flips status to degraded
    // (REQ-003 AC2 — reuse the existing reason channel, no parallel field).
    assert_eq!(
        envelope["status"].as_str(),
        Some("degraded"),
        "a stale-but-available search reports degraded status: {result}"
    );
    let reason = envelope["data"]["degraded_reason"]
        .as_str()
        .expect("degraded_reason is set while a rebuild is in progress");
    assert!(
        reason.contains(STALE_REBUILD_REASON),
        "degraded_reason must carry the stale fragment: {reason}"
    );
    // REQ-003 AC4: the pre-existing embeddings-unavailable reason coexists with the
    // stale fragment (combined via `combine_degraded_reasons`), neither dropped.
    assert!(
        reason.contains(NO_INDEXED_EMBEDDINGS_REASON),
        "stale and embeddings reasons must coexist in degraded_reason: {reason}"
    );

    // The result rows themselves are clean lean data — the notice never pollutes
    // them (it lives only in the dedicated `degraded_reason` field, asserted above).
    for hit in hits {
        assert!(
            !serde_json::to_string(hit).unwrap().contains("rebuilding"),
            "the stale notice must not leak into a result row: {hit}"
        );
    }

    drop(lock);
}

/// REQ-002 AC2 (integration, MCP surface): the stale-but-available plumbing must
/// not fabricate results on the cold-start path. With a rebuild in progress
/// (rebuild lock held) and *no* prior index, MCP `oneup_search` surfaces a
/// not-ready state with no result rows, never a synthesized hit. The lock is held
/// before the server starts so the auto-started daemon also defers indexing,
/// keeping the cold-start state deterministic.
#[cfg(unix)]
#[test]
fn mcp_search_cold_start_during_rebuild_does_not_fabricate_results() {
    use oneup::daemon::lifecycle::acquire_rebuild_lock;

    let project = TempDir::new().unwrap();
    let root = project.path().canonicalize().unwrap();

    // Hold the rebuild lock first: the daemon the MCP server best-effort starts
    // then defers its own indexing, so no index is ever built (true cold start).
    let _lock = acquire_rebuild_lock(&root).expect("test holds the rebuild lock");

    let mut client = McpTestClient::start_with_isolated_state(&root);
    let result = client.call_tool(
        TOOL_SEARCH,
        serde_json::json!({ "query": "cold_start_marker_fn" }),
    );
    let envelope = mcp_structured(&result);

    let result_rows = envelope["data"]["results"]
        .as_array()
        .map(|rows| rows.len())
        .unwrap_or(0);
    assert_eq!(
        result_rows, 0,
        "cold-start search during a rebuild must not fabricate results: {result}"
    );
    assert_ne!(
        envelope["status"].as_str(),
        Some("ok"),
        "cold-start search during a rebuild must not report a successful result set: {result}"
    );
}

/// REQ-003 AC1 + AC3 (integration, CLI render seam): `1up search` keeps the
/// machine-readable result stream (stdout) byte-for-byte identical whether or not
/// a stale-rebuild notice is present, and emits the notice only on the warning
/// channel (stderr) and only while a rebuild is in progress. A one-shot fake daemon
/// supplies the framed `SearchResponse` (mirroring the daemon's IPC contract) so the
/// production CLI render path (`serve_daemon_results`) is exercised deterministically.
#[cfg(unix)]
#[test]
fn cli_search_keeps_stale_rebuild_notice_off_stdout() {
    use oneup::shared::constants::STALE_REBUILD_REASON;
    use std::os::unix::net::UnixListener;

    // Run `1up search` against a fake daemon that replies once with a fixed lean
    // result set and an optional `degraded_reason`. Returns (stdout, stderr).
    fn search_against_fake_daemon(degraded_reason: Option<&str>) -> (Vec<u8>, String) {
        let home = tempfile::Builder::new()
            .prefix("1up-home-")
            .tempdir_in("/tmp")
            .unwrap();
        // No project_id, so the CLI does not auto-start a real daemon and only the
        // fake socket answers the search.
        let project = TempDir::new().unwrap();
        let socket_path = test_data_dir(home.path()).join("daemon.sock");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let server_socket_path = socket_path.clone();
        let reason = degraded_reason.map(str::to_string);
        let server = thread::spawn(move || {
            if let Some(parent) = server_socket_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let _ = fs::remove_file(&server_socket_path);
            let listener = UnixListener::bind(&server_socket_path).unwrap();
            ready_tx.send(()).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let _request = read_framed_json(&mut stream);

            let mut response = serde_json::json!({
                "status": "results",
                "results": [{
                    "segment_id": "servedseg000",
                    "file_path": "src/lib.rs",
                    "language": "rust",
                    "block_type": "function",
                    "content": "fn served() {}",
                    "score": 42,
                    "line_number": 1,
                    "line_end": 2
                }]
            });
            if let Some(reason) = reason {
                response["degraded_reason"] = serde_json::Value::String(reason);
            }
            write_framed_json(&mut stream, &response);
        });

        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let output = cmd()
            .env("HOME", home.path())
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .args([
                "search",
                "served",
                "--path",
                project.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        server.join().unwrap();
        assert!(
            output.status.success(),
            "search against the fake daemon failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        (output.stdout, String::from_utf8(output.stderr).unwrap())
    }

    let (stdout_stale, stderr_stale) = search_against_fake_daemon(Some(STALE_REBUILD_REASON));
    let (stdout_fresh, stderr_fresh) = search_against_fake_daemon(None);

    // REQ-003 AC1: the stale notice never alters the machine-readable result
    // stream — stdout is byte-identical with and without it.
    assert_eq!(
        stdout_stale, stdout_fresh,
        "the stale notice must not change stdout"
    );
    assert!(
        !stdout_stale.is_empty(),
        "the served result row must be written to stdout"
    );
    let stdout_text = String::from_utf8(stdout_stale).unwrap();
    assert!(
        !stdout_text.contains("rebuilding") && !stdout_text.contains(STALE_REBUILD_REASON),
        "the stale notice must not appear on stdout: {stdout_text}"
    );

    // The notice appears on stderr only, and only when a rebuild is in progress
    // (REQ-003 AC1 warning-channel + AC3 scoping).
    assert!(
        stderr_stale.contains(STALE_REBUILD_REASON),
        "the stale notice must appear on stderr during a rebuild: {stderr_stale}"
    );
    assert!(
        !stderr_fresh.contains("rebuilding") && !stderr_fresh.contains("stale"),
        "no stale notice may appear on stderr when no rebuild is in progress: {stderr_fresh}"
    );
}
