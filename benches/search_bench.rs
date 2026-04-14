use criterion::{criterion_group, criterion_main, Criterion};
use oneup::search::impact::{ImpactAnchor, ImpactHorizonEngine, ImpactRequest, ImpactStatus};
use oneup::search::intent::detect_intent;
use oneup::search::ranking::rank_candidates;
use oneup::search::retrieval::{RetrievalBackend, RetrievalMode};
use oneup::storage::segments::{self, SegmentInsert};

// macOS tempdirs are often exposed through the symlinked `/var` alias.
fn canonical_temp_root(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    tmp.path().canonicalize().unwrap()
}

fn setup_db_and_index() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let temp_root = canonical_temp_root(&tmp);

    let rust_source = r#"
use std::io;
use std::collections::HashMap;

fn process_data(input: &str) -> String {
    input.trim().to_uppercase()
}

fn validate_input(data: &str) -> bool {
    !data.is_empty() && data.len() < 1024
}

struct Config {
    pub host: String,
    pub port: u16,
    pub workers: usize,
}

impl Config {
    fn new(host: String, port: u16) -> Self {
        Config { host, port, workers: 4 }
    }

    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn is_valid(&self) -> bool {
        !self.host.is_empty() && self.port > 0
    }
}

fn handle_request(method: &str, path: &str) -> (u16, String) {
    match method {
        "GET" => (200, format!("OK: {}", path)),
        "POST" => (201, "Created".to_string()),
        _ => (405, "Method Not Allowed".to_string()),
    }
}

fn main() {
    let cfg = Config::new("localhost".to_string(), 8080);
    if cfg.is_valid() {
        println!("Listening on {}", cfg.address());
    }
    let result = process_data("  hello world  ");
    println!("{}", result);
}
"#;

    let python_source = r#"
import json
import os

class DataProcessor:
    def __init__(self, name: str):
        self.name = name
        self.results = []

    def process(self, data: str) -> str:
        result = data.strip().upper()
        self.results.append(result)
        return result

    def summary(self) -> dict:
        return {"name": self.name, "count": len(self.results)}

def parse_config(path: str) -> dict:
    with open(path) as f:
        return json.load(f)

def validate_email(email: str) -> bool:
    return "@" in email and "." in email

def format_output(data: dict) -> str:
    return json.dumps(data, indent=2)
"#;

    let js_source = r#"
function handleRequest(req, res) {
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

function validateInput(data) {
    return data && data.length > 0;
}

module.exports = { handleRequest, handleGet, handlePost, validateInput };
"#;

    let yaml_source = r#"
request_signing_secret: test-secret
description: request signing secret used for request validation
policy_rule_preview_enabled: true
"#;

    let proto_source = r#"
syntax = "proto3";

message PolicyRulePreview {
    string id = 1;
}
"#;

    let sql_source = r#"
CREATE TABLE policy_rules_preview (
    id TEXT PRIMARY KEY,
    validator_name TEXT NOT NULL
);
"#;

    std::fs::write(temp_root.join("main.rs"), rust_source).unwrap();
    std::fs::write(temp_root.join("processor.py"), python_source).unwrap();
    std::fs::write(temp_root.join("handler.js"), js_source).unwrap();
    std::fs::create_dir_all(temp_root.join("config")).unwrap();
    std::fs::create_dir_all(temp_root.join("proto")).unwrap();
    std::fs::create_dir_all(temp_root.join("sql")).unwrap();
    std::fs::write(
        temp_root.join("config").join("signatures.yaml"),
        yaml_source,
    )
    .unwrap();
    std::fs::write(
        temp_root.join("proto").join("policy_rules.proto"),
        proto_source,
    )
    .unwrap();
    std::fs::write(temp_root.join("sql").join("policy_rules.sql"), sql_source).unwrap();
    std::fs::create_dir_all(temp_root.join(".1up")).unwrap();
    std::fs::write(
        temp_root.join(".1up").join("project_id"),
        "bench-project-id",
    )
    .unwrap();

    let db_path = temp_root.join(".1up").join("index.db");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let db = oneup::storage::db::Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        oneup::storage::schema::initialize(&conn).await.unwrap();
        oneup::indexer::pipeline::run(&conn, &temp_root, None)
            .await
            .unwrap();
    });

    (tmp, db_path)
}

fn embedding_with(values: &[(usize, f32)]) -> Vec<f32> {
    let mut embedding = vec![0.0; 384];
    for (idx, value) in values {
        embedding[*idx] = *value;
    }
    embedding
}

fn setup_retrieval_db() -> (tempfile::TempDir, std::path::PathBuf, Vec<f32>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let temp_root = canonical_temp_root(&tmp);
    std::fs::create_dir_all(temp_root.join(".1up")).unwrap();

    let db_path = temp_root.join(".1up").join("index.db");
    let query = "request auth token validation".to_string();
    let query_embedding = embedding_with(&[(0, 1.0), (1, 0.8)]);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let db = oneup::storage::db::Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        oneup::storage::schema::initialize(&conn).await.unwrap();

        for idx in 0..40 {
            let insert = SegmentInsert {
                id: format!("auth-{idx}"),
                file_path: format!("src/auth_{idx}.rs"),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!(
                    "fn validate_token_{idx}(token: &str) -> bool {{\n    let request_context = \"request auth token validation middleware\";\n    let session_state = \"auth token validator\";\n    !token.is_empty() && !request_context.is_empty() && !session_state.is_empty()\n}}\n"
                ),
                line_start: 1,
                line_end: 5,
                embedding_vec: Some(
                    serde_json::to_string(&embedding_with(&[
                        (0, 0.95),
                        (1, 0.78),
                        (2, idx as f32 / 100.0),
                    ]))
                    .unwrap(),
                ),
                breadcrumb: Some("auth".to_string()),
                complexity: 2,
                role: "IMPLEMENTATION".to_string(),
                defined_symbols: format!("[\"validate_token_{idx}\"]"),
                referenced_symbols: "[\"request\",\"token\"]".to_string(),
                called_symbols: "[\"validate\"]".to_string(),
                file_hash: format!("hash-auth-{idx}"),
            };
            segments::upsert_segment(&conn, &insert).await.unwrap();
        }

        for idx in 0..40 {
            let insert = SegmentInsert {
                id: format!("config-{idx}"),
                file_path: format!("src/config_{idx}.rs"),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!(
                    "fn load_config_{idx}() -> &'static str {{\n    let host = \"config loading host port settings\";\n    let file = \"config path\";\n    if host.is_empty() {{\n        return file;\n    }}\n    host\n}}\n"
                ),
                line_start: 1,
                line_end: 8,
                embedding_vec: Some(
                    serde_json::to_string(&embedding_with(&[
                        (2, 0.92),
                        (3, 0.81),
                        (0, idx as f32 / 200.0),
                    ]))
                    .unwrap(),
                ),
                breadcrumb: Some("config".to_string()),
                complexity: 2,
                role: "DEFINITION".to_string(),
                defined_symbols: format!("[\"load_config_{idx}\"]"),
                referenced_symbols: "[\"host\",\"port\"]".to_string(),
                called_symbols: "[]".to_string(),
                file_hash: format!("hash-config-{idx}"),
            };
            segments::upsert_segment(&conn, &insert).await.unwrap();
        }

        for idx in 0..40 {
            let insert = SegmentInsert {
                id: format!("billing-{idx}"),
                file_path: format!("src/billing_{idx}.rs"),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!(
                    "fn invoice_total_{idx}() -> i64 {{\n    let ledger = \"billing invoice total payment\";\n    let taxes = 12;\n    let subtotal = 120;\n    let _ = ledger;\n    subtotal + taxes\n}}\n"
                ),
                line_start: 1,
                line_end: 7,
                embedding_vec: Some(
                    serde_json::to_string(&embedding_with(&[
                        (4, 0.93),
                        (5, 0.79),
                        (1, idx as f32 / 200.0),
                    ]))
                    .unwrap(),
                ),
                breadcrumb: Some("billing".to_string()),
                complexity: 2,
                role: "IMPLEMENTATION".to_string(),
                defined_symbols: format!("[\"invoice_total_{idx}\"]"),
                referenced_symbols: "[\"invoice\"]".to_string(),
                called_symbols: "[\"total\"]".to_string(),
                file_hash: format!("hash-billing-{idx}"),
            };
            segments::upsert_segment(&conn, &insert).await.unwrap();
        }
    });

    (tmp, db_path, query_embedding, query)
}

fn setup_impact_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let temp_root = canonical_temp_root(&tmp);
    std::fs::create_dir_all(temp_root.join(".1up")).unwrap();

    let db_path = temp_root.join(".1up").join("index.db");
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let db = oneup::storage::db::Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        oneup::storage::schema::initialize(&conn).await.unwrap();

        let anchor = SegmentInsert {
            id: "auth-runtime-anchor".to_string(),
            file_path: "src/auth/runtime.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "pub fn load_auth_config() -> &'static str {\n    \"auth\"\n}\n".to_string(),
            line_start: 1,
            line_end: 3,
            embedding_vec: None,
            breadcrumb: Some("auth".to_string()),
            complexity: 2,
            role: "DEFINITION".to_string(),
            defined_symbols: "[\"load_auth_config\"]".to_string(),
            referenced_symbols: "[]".to_string(),
            called_symbols: "[]".to_string(),
            file_hash: "hash-auth-runtime-anchor".to_string(),
        };
        segments::upsert_segment(&conn, &anchor).await.unwrap();

        let sibling = SegmentInsert {
            id: "auth-runtime-parse".to_string(),
            file_path: "src/auth/runtime.rs".to_string(),
            language: "rust".to_string(),
            block_type: "function".to_string(),
            content: "pub fn parse_auth_config(raw: &str) -> bool {\n    !raw.trim().is_empty()\n}\n"
                .to_string(),
            line_start: 5,
            line_end: 7,
            embedding_vec: None,
            breadcrumb: Some("auth".to_string()),
            complexity: 3,
            role: "IMPLEMENTATION".to_string(),
            defined_symbols: "[\"parse_auth_config\"]".to_string(),
            referenced_symbols: "[\"raw\"]".to_string(),
            called_symbols: "[]".to_string(),
            file_hash: "hash-auth-runtime-parse".to_string(),
        };
        segments::upsert_segment(&conn, &sibling).await.unwrap();

        for idx in 0..24 {
            let caller = SegmentInsert {
                id: format!("auth-caller-{idx}"),
                file_path: format!("src/auth/service_{idx}.rs"),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!(
                    "pub fn boot_auth_{idx}() -> &'static str {{\n    load_auth_config()\n}}\n"
                ),
                line_start: 1,
                line_end: 3,
                embedding_vec: None,
                breadcrumb: Some("auth".to_string()),
                complexity: 2,
                role: "ORCHESTRATION".to_string(),
                defined_symbols: format!("[\"boot_auth_{idx}\"]"),
                referenced_symbols: "[]".to_string(),
                called_symbols: "[\"load_auth_config\"]".to_string(),
                file_hash: format!("hash-auth-caller-{idx}"),
            };
            segments::upsert_segment(&conn, &caller).await.unwrap();
        }

        for idx in 0..12 {
            let test_segment = SegmentInsert {
                id: format!("auth-test-{idx}"),
                file_path: format!("tests/auth/runtime_test_{idx}.rs"),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!(
                    "#[test]\nfn runtime_test_{idx}() {{\n    assert_eq!(load_auth_config(), \"auth\");\n}}\n"
                ),
                line_start: 1,
                line_end: 4,
                embedding_vec: None,
                breadcrumb: Some("tests".to_string()),
                complexity: 1,
                role: "DEFINITION".to_string(),
                defined_symbols: format!("[\"runtime_test_{idx}\"]"),
                referenced_symbols: "[\"load_auth_config\"]".to_string(),
                called_symbols: "[]".to_string(),
                file_hash: format!("hash-auth-test-{idx}"),
            };
            segments::upsert_segment(&conn, &test_segment).await.unwrap();
        }

        for (idx, file_path) in [
            "src/auth/config.rs",
            "src/cache/config.rs",
            "src/ui/config.rs",
            "tests/config_fixture.rs",
        ]
        .iter()
        .enumerate()
        {
            let broad_definition = SegmentInsert {
                id: format!("broad-config-{idx}"),
                file_path: (*file_path).to_string(),
                language: "rust".to_string(),
                block_type: "function".to_string(),
                content: format!(
                    "pub fn load_config() -> &'static str {{\n    \"{}\"\n}}\n",
                    idx
                ),
                line_start: 1,
                line_end: 3,
                embedding_vec: None,
                breadcrumb: Some("config".to_string()),
                complexity: 1,
                role: "DEFINITION".to_string(),
                defined_symbols: "[\"load_config\"]".to_string(),
                referenced_symbols: "[]".to_string(),
                called_symbols: "[]".to_string(),
                file_hash: format!("hash-broad-config-{idx}"),
            };
            segments::upsert_segment(&conn, &broad_definition)
                .await
                .unwrap();
        }
    });

    (tmp, db_path)
}

fn bench_symbol_lookup(c: &mut Criterion) {
    let (_tmp, db_path) = setup_db_and_index();

    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("symbol_lookup_exact", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::SymbolSearchEngine::new(&conn);
                let results = engine.find_definitions("Config").await.unwrap();
                assert!(!results.is_empty());
            });
        });
    });

    c.bench_function("symbol_lookup_partial", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::SymbolSearchEngine::new(&conn);
                let _results = engine.find_definitions("handle").await.unwrap();
            });
        });
    });

    c.bench_function("symbol_references", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::SymbolSearchEngine::new(&conn);
                let _results = engine.find_references("Config").await.unwrap();
            });
        });
    });
}

fn bench_fts_search(c: &mut Criterion) {
    let (_tmp, db_path) = setup_db_and_index();

    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("fts_search_single_term", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::HybridSearchEngine::new(&conn, None);
                let results = engine.fts_only_search("config", 20).await.unwrap();
                assert!(!results.is_empty());
            });
        });
    });

    c.bench_function("fts_search_multi_term", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::HybridSearchEngine::new(&conn, None);
                let _results = engine
                    .fts_only_search("handle request validation", 20)
                    .await
                    .unwrap();
            });
        });
    });

    c.bench_function("fts_search_no_results", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::HybridSearchEngine::new(&conn, None);
                let results = engine.fts_only_search("zznonexistentzz", 20).await.unwrap();
                assert!(results.is_empty());
            });
        });
    });
}

fn bench_chunked_content_search(c: &mut Criterion) {
    let (_tmp, db_path) = setup_db_and_index();
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("fts_search_chunked_config_query", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::HybridSearchEngine::new(&conn, None);
                let results = engine
                    .fts_only_search("request_signing_secret policy_rule_preview_enabled", 20)
                    .await
                    .unwrap();
                assert!(!results.is_empty());
                assert_eq!(results[0].file_path, "config/signatures.yaml");
            });
        });
    });

    c.bench_function("fts_search_chunked_proto_query", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let mut engine = oneup::search::HybridSearchEngine::new(&conn, None);
                let results = engine.search("PolicyRulePreview", 20).await.unwrap();
                assert!(!results.is_empty());
                assert_eq!(results[0].file_path, "proto/policy_rules.proto");
            });
        });
    });

    c.bench_function("fts_search_chunked_sql_query", |b| {
        b.iter(|| {
            rt.block_on(async {
                let db = oneup::storage::db::Db::open_ro(&db_path).await.unwrap();
                let conn = db.connect().unwrap();
                let engine = oneup::search::HybridSearchEngine::new(&conn, None);
                let results = engine
                    .fts_only_search("policy_rules_preview table", 20)
                    .await
                    .unwrap();
                assert!(!results.is_empty());
                assert_eq!(results[0].file_path, "sql/policy_rules.sql");
            });
        });
    });
}

fn bench_retrieval_backend(c: &mut Criterion) {
    let (_tmp, db_path, query_embedding, query) = setup_retrieval_db();
    let intent = detect_intent(&query);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = rt
        .block_on(async { oneup::storage::db::Db::open_ro(&db_path).await })
        .unwrap();
    let conn = db.connect().unwrap();

    c.bench_function("retrieval_sql_vector_v2_candidates", |b| {
        b.iter(|| {
            rt.block_on(async {
                let backend = RetrievalBackend::select(&conn, Some(&query_embedding))
                    .await
                    .unwrap();
                let candidates = backend
                    .search(&query, Some(&query_embedding))
                    .await
                    .unwrap();
                assert_eq!(backend.mode(), RetrievalMode::SqlVectorV2);
                assert!(!candidates.vector_results.is_empty());
                assert!(!candidates.fts_results.is_empty());
            });
        });
    });

    c.bench_function("hybrid_sql_vector_v2_fusion", |b| {
        b.iter(|| {
            rt.block_on(async {
                let backend = RetrievalBackend::select(&conn, Some(&query_embedding))
                    .await
                    .unwrap();
                let candidates = backend
                    .search(&query, Some(&query_embedding))
                    .await
                    .unwrap();
                let ranked = rank_candidates(
                    candidates.vector_results,
                    candidates.fts_results,
                    Vec::new(),
                    &query,
                    intent,
                    10,
                );
                assert_eq!(backend.mode(), RetrievalMode::SqlVectorV2);
                assert!(!ranked.is_empty());
                assert!(ranked[0].candidate.file_path.starts_with("src/auth_"));
            });
        });
    });
}

fn bench_impact_horizon(c: &mut Criterion) {
    let (_tmp, db_path) = setup_impact_db();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let db = rt
        .block_on(async { oneup::storage::db::Db::open_ro(&db_path).await })
        .unwrap();
    let conn = db.connect().unwrap();

    let file_request = ImpactRequest {
        anchor: ImpactAnchor::File {
            path: "src/auth/runtime.rs".to_string(),
            line: None,
        },
        scope: None,
        depth: 2,
        limit: 20,
    };
    let refused_request = ImpactRequest {
        anchor: ImpactAnchor::Symbol {
            name: "load_config".to_string(),
        },
        scope: None,
        depth: 2,
        limit: 20,
    };
    let empty_request = ImpactRequest {
        anchor: ImpactAnchor::Segment {
            id: "auth-runtime-parse".to_string(),
        },
        scope: None,
        depth: 2,
        limit: 20,
    };
    let empty_scoped_request = ImpactRequest {
        anchor: ImpactAnchor::Symbol {
            name: "parse_auth_config".to_string(),
        },
        scope: Some("src/auth".to_string()),
        depth: 2,
        limit: 20,
    };

    c.bench_function("impact_file_anchor_primary", |b| {
        b.iter(|| {
            rt.block_on(async {
                let engine = ImpactHorizonEngine::new(&conn);
                let result = engine.explore(file_request.clone()).await.unwrap();
                assert_eq!(result.status, ImpactStatus::Expanded);
                assert!(!result.results.is_empty());
            });
        });
    });

    c.bench_function("impact_symbol_anchor_refused", |b| {
        b.iter(|| {
            rt.block_on(async {
                let engine = ImpactHorizonEngine::new(&conn);
                let result = engine.explore(refused_request.clone()).await.unwrap();
                assert_eq!(result.status, ImpactStatus::Refused);
                assert!(result.results.is_empty());
            });
        });
    });

    c.bench_function("impact_segment_anchor_empty", |b| {
        b.iter(|| {
            rt.block_on(async {
                let engine = ImpactHorizonEngine::new(&conn);
                let result = engine.explore(empty_request.clone()).await.unwrap();
                assert_eq!(result.status, ImpactStatus::Empty);
                assert!(result.results.is_empty());
            });
        });
    });

    c.bench_function("impact_symbol_anchor_empty_scoped", |b| {
        b.iter(|| {
            rt.block_on(async {
                let engine = ImpactHorizonEngine::new(&conn);
                let result = engine.explore(empty_scoped_request.clone()).await.unwrap();
                assert_eq!(result.status, ImpactStatus::EmptyScoped);
                assert!(result.results.is_empty());
            });
        });
    });
}

criterion_group!(
    benches,
    bench_symbol_lookup,
    bench_fts_search,
    bench_chunked_content_search,
    bench_retrieval_backend,
    bench_impact_horizon
);
criterion_main!(benches);
