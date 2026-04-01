use criterion::{criterion_group, criterion_main, Criterion};

fn setup_db_and_index() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();

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

    std::fs::write(tmp.path().join("main.rs"), rust_source).unwrap();
    std::fs::write(tmp.path().join("processor.py"), python_source).unwrap();
    std::fs::write(tmp.path().join("handler.js"), js_source).unwrap();
    std::fs::create_dir_all(tmp.path().join(".1up")).unwrap();
    std::fs::write(
        tmp.path().join(".1up").join("project_id"),
        "bench-project-id",
    )
    .unwrap();

    let db_path = tmp.path().join(".1up").join("index.db");

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let db = oneup::storage::db::Db::open_rw(&db_path).await.unwrap();
        let conn = db.connect().unwrap();
        oneup::storage::schema::initialize(&conn).await.unwrap();
        oneup::indexer::pipeline::run(&conn, tmp.path(), None)
            .await
            .unwrap();
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

criterion_group!(benches, bench_symbol_lookup, bench_fts_search);
criterion_main!(benches);
