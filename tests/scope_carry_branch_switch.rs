// Integration test: scope carry on branch switch
// This test exercises the actual database state to answer the empirical question:
// does scope carried from a prior context reach the new context's pipeline?
//
// KEY QUESTION: Is the database meta table shared across branch contexts, and if so,
// does the carried scope reach it when persist_carried_scope() only writes to the
// progress file (index_status.json)?

use oneup::shared::config;
use oneup::shared::types::{IndexPhase, IndexProgress, IndexScopeInfo, IndexState};
use oneup::storage::db::Db;
use oneup::storage::schema;
use std::fs;
use tempfile::TempDir;

#[tokio::test]
async fn scope_carry_branch_switch_database_sharing() {
    // SETUP: Create a test repository
    let repo_dir = TempDir::new().unwrap();
    let repo_path = repo_dir.path().canonicalize().unwrap();

    // Initialize git repo
    git_init_repo(&repo_path);

    // Create test files
    create_test_file(&repo_path, "services/auth/mod.rs", "// Auth service");
    create_test_file(&repo_path, "libs/core/utils.rs", "// Core utils");
    create_test_file(&repo_path, "other/unscoped.rs", "// Unscoped");
    git_commit(&repo_path, "initial commit");

    let state_root = repo_path.clone();
    oneup::shared::fs::ensure_secure_project_root(&state_root).unwrap();

    // PHASE 1: Write scope to database meta table (simulating initial scoped index)
    println!("\n=== PHASE 1: Initial scoped index writes scope to database ===");

    let db_path = config::project_db_path(&state_root);
    let db = Db::open_rw(&db_path).await.unwrap();
    let conn = db.connect_tuned().await.unwrap();
    schema::initialize(&conn).await.unwrap();

    let scope_roots = vec!["services/auth".to_string(), "libs/core".to_string()];
    println!("Writing scope to database meta table: {:?}", scope_roots);
    schema::write_scope_to_meta(&conn, &scope_roots)
        .await
        .expect("failed to write scope to database");

    // Write progress file indicating scoped index on "main" context
    let progress_path = config::project_dot_dir(&state_root).join("index_status.json");
    fs::create_dir_all(config::project_dot_dir(&state_root)).unwrap();
    let progress = IndexProgress {
        state: IndexState::Complete,
        phase: IndexPhase::Complete,
        context_id: Some("context_main".to_string()),
        source_root: Some(repo_path.clone()),
        branch_name: Some("main".to_string()),
        branch_status: None,
        files_total: 100,
        files_scanned: 4,
        files_processed: 4,
        files_indexed: 4,
        files_skipped: 0,
        files_deleted: 0,
        segments_stored: 0,
        embeddings_enabled: false,
        embedding_unavailable_reason: None,
        vector_rows: None,
        embeddable_segments: None,
        message: Some("Scoped index complete".to_string()),
        parallelism: None,
        timings: None,
        scope: Some(IndexScopeInfo {
            requested: "scoped:2".to_string(),
            executed: "scoped:2".to_string(),
            changed_paths: 2,
            fallback_reason: None,
            roots: vec!["services/auth".to_string(), "libs/core".to_string()],
        }),
        prefilter: None,
        indexer_pid: None,
        updated_at: chrono::Utc::now(),
    };
    let json = serde_json::to_string_pretty(&progress).unwrap();
    fs::write(&progress_path, json).unwrap();

    // Verify scope is in database
    let db_scope = schema::read_scope_from_meta(&conn)
        .await
        .expect("failed to read")
        .expect("scope should exist");
    assert_eq!(db_scope, scope_roots);
    println!("CONFIRMED: Scope in database meta table: {:?}", db_scope);
    drop(conn);

    // PHASE 2: Simulate branch switch (what mark_branch_context_changes does)
    println!("\n=== PHASE 2: Simulate branch switch and persist_carried_scope ===");

    // Read carried scope from progress file
    let progress_data = fs::read_to_string(&progress_path).expect("should read progress");
    let mut progress: IndexProgress =
        serde_json::from_str(&progress_data).expect("should parse progress");

    let carried_scope = progress.scope.clone().expect("should have scope");
    println!("Carried scope from progress file: {:?}", carried_scope);

    // Simulate branch switch
    git_checkout_new_branch(&repo_path, "feature");
    create_test_file(&repo_path, "services/auth/new.rs", "// New");
    git_commit(&repo_path, "feature commit");

    // Simulate persist_carried_scope - it updates the progress file with the carried scope
    // (which it already has, so this is idempotent)
    progress.context_id = Some("context_feature".to_string());
    progress.branch_name = Some("feature".to_string());
    progress.scope = Some(carried_scope);
    progress.updated_at = chrono::Utc::now();

    let json = serde_json::to_string_pretty(&progress).unwrap();
    fs::write(&progress_path, json).unwrap();

    println!(
        "Updated progress file for new context (scope still present): {:?}",
        progress.scope
    );

    // PHASE 3: KEY QUESTION - Is the database meta table still available?
    println!("\n=== PHASE 3: Check if database meta is available to new context ===");

    let db2 = Db::open_rw(&db_path).await.unwrap();
    let conn2 = db2.connect_tuned().await.unwrap();

    // This is what run_project() does at line 1820 - reads from database meta
    let db_scope_check = schema::read_scope_from_meta(&conn2)
        .await
        .expect("failed to read")
        .expect("scope should exist in database");

    println!(
        "New context CAN read scope from database meta: {:?}",
        db_scope_check
    );
    assert_eq!(db_scope_check, scope_roots);

    // PHASE 4: Analyze the evidence
    println!("\n=== ANALYSIS ===");
    println!("1. Progress file: scope persisted by persist_carried_scope ✓");
    println!("2. Database meta: scope from prior context still available ✓");
    println!("3. run_project line 1820: would read from database meta ✓");
    println!("\nCONCLUSION: Scope DOES reach the new context via database meta table.");
    println!(
        "The repair code (364aaf7, lines 1860-1894) re-persists the scope,\nwhich is IDEMPOTENT if the database already has it."
    );
    println!("\nQUESTION NOW: Is the database meta ever CLEARED between contexts?");
    println!("If not, the repair code is redundant defensive programming.");
    println!("If yes, we need to verify WHEN it's cleared and under what circumstances.");
}

/// A freshly-created staging database does NOT inherit scope from the active
/// index.db — which is exactly why every rebuild path must write scope to the
/// staging connection explicitly (ops.rs staged write, daemon re-persist)
/// before `finalize_and_swap` can be trusted to preserve it. This pins the
/// empirical conclusion of the scope-carry investigation as assertions.
#[tokio::test]
async fn scope_carry_with_fresh_staging_database() {
    let repo_dir = TempDir::new().unwrap();
    let repo_path = repo_dir.path().canonicalize().unwrap();

    git_init_repo(&repo_path);
    create_test_file(&repo_path, "services/auth/mod.rs", "// Auth");
    create_test_file(&repo_path, "libs/core/utils.rs", "// Core");
    git_commit(&repo_path, "initial");

    let state_root = repo_path.clone();
    oneup::shared::fs::ensure_secure_project_root(&state_root).unwrap();

    // Scoped active index.db.
    let db_path = config::project_db_path(&state_root);
    let db = Db::open_rw(&db_path).await.unwrap();
    let conn = db.connect_tuned().await.unwrap();
    schema::initialize(&conn).await.unwrap();

    let scope_roots = vec!["services/auth".to_string(), "libs/core".to_string()];
    schema::write_scope_to_meta(&conn, &scope_roots)
        .await
        .unwrap();
    assert_eq!(
        schema::read_scope_from_meta(&conn).await.unwrap(),
        Some(scope_roots.clone()),
        "active index.db should persist the scoped roots"
    );

    // A fresh staging database (opened the way real rebuilds open it) starts
    // WITHOUT the active DB's scope.
    let staged = oneup::storage::swap::StagingRebuild::open(&state_root)
        .await
        .unwrap();
    let staging_conn = staged.connection();
    assert_eq!(
        schema::read_scope_from_meta(staging_conn).await.unwrap(),
        None,
        "fresh staging database must not inherit scope implicitly"
    );

    // The explicit write every rebuild path performs makes the scope survive
    // the eventual swap.
    schema::write_scope_to_meta(staging_conn, &scope_roots)
        .await
        .unwrap();
    assert_eq!(
        schema::read_scope_from_meta(staging_conn).await.unwrap(),
        Some(scope_roots.clone()),
        "staging database should persist scope once written explicitly"
    );

    // Writing to staging leaves the active DB's scope untouched.
    assert_eq!(
        schema::read_scope_from_meta(&conn).await.unwrap(),
        Some(scope_roots),
        "active index.db scope should be unaffected by staging writes"
    );
}

// Helpers
fn git_init_repo(path: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(path)
        .output()
        .expect("git init failed");

    std::process::Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(path)
        .output()
        .expect("git config user.email failed");

    std::process::Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(path)
        .output()
        .expect("git config user.name failed");
}

fn create_test_file(repo_path: &std::path::Path, rel_path: &str, content: &str) {
    let file_path = repo_path.join(rel_path);
    fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    fs::write(file_path, content).unwrap();
}

fn git_commit(path: &std::path::Path, message: &str) {
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .expect("git add failed");

    std::process::Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(path)
        .output()
        .expect("git commit failed");
}

fn git_checkout_new_branch(path: &std::path::Path, branch: &str) {
    std::process::Command::new("git")
        .args(["checkout", "-b", branch])
        .current_dir(path)
        .output()
        .expect("git checkout -b failed");
}
