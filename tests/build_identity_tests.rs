//! Regression tests for the build-identity stamp's invalidation triggers.
//!
//! The authority gate (`daemon_response_is_authoritative`) trusts a daemon only
//! on an exact `ONEUP_BUILD_IDENTITY` match, so the stamp must refresh for
//! every rebuild that can consume changed tracked source. Rebuilding this
//! crate twice per scenario would cost minutes, so each test instead stamps a
//! miniature dependency-free probe crate with this repository's *actual*
//! `build.rs` inside a throwaway git repo, builds it with the real cargo
//! invalidation machinery, and reads the identity the produced binary prints.
//!
//! Covered regressions:
//! - a bare unstaged edit to a tracked file must refresh the dirty digest even
//!   though it touches no git metadata (HEAD / index / refs);
//! - with packed-only refs (a linked worktree cut after `git pack-refs`), a
//!   commit that *creates* the previously absent loose branch ref must still
//!   retrigger the stamp.

use std::path::Path;
use std::process::Command;

/// This repository's build script, embedded verbatim so the probe crate
/// exercises the exact production logic.
const BUILD_RS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/build.rs"));

/// Lays down the dependency-free probe crate whose binary prints its
/// `ONEUP_BUILD_IDENTITY`.
fn write_probe_crate(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"idprobe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(root.join("build.rs"), BUILD_RS).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() {\n    println!(\"{}\", env!(\"ONEUP_BUILD_IDENTITY\"));\n}\n",
    )
    .unwrap();
}

/// Runs `git` in `dir`, isolated from the developer's global/system config
/// (signing, hooks, templates), and asserts success.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.name", "probe"]);
    git(dir, &["config", "user.email", "probe@example.com"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", "initial"]);
}

/// Builds the probe crate (reusing `target_dir` so cargo's fingerprint reuse —
/// the machinery under test — is exercised across builds) and returns the
/// identity its binary prints.
fn build_and_read_identity(crate_dir: &Path, target_dir: &Path) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let output = Command::new(cargo)
        .args(["run", "--quiet"])
        .current_dir(crate_dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "cargo run failed in {}: {}",
        crate_dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    let identity = String::from_utf8(output.stdout).unwrap().trim().to_string();
    assert!(!identity.is_empty(), "probe binary printed no identity");
    identity
}

/// A bare unstaged edit to a tracked source file changes no git metadata, yet
/// the rebuilt binary is compiled from different source — so it must carry a
/// *different* identity (a refreshed dirty digest), and two different unstaged
/// deltas at the same HEAD must differ from each other too.
#[test]
fn unstaged_tracked_edit_refreshes_dirty_digest() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("repo");
    let target_dir = tmp.path().join("target");
    write_probe_crate(&crate_dir);
    init_repo(&crate_dir);

    let clean = build_and_read_identity(&crate_dir, &target_dir);
    assert!(
        !clean.contains(".dirty"),
        "freshly committed tree must stamp clean: {clean}"
    );

    let main_rs = crate_dir.join("src/main.rs");
    let base = std::fs::read_to_string(&main_rs).unwrap();

    std::fs::write(&main_rs, format!("{base}// edit-a\n")).unwrap();
    let dirty_a = build_and_read_identity(&crate_dir, &target_dir);
    assert!(
        dirty_a.starts_with(&format!("{clean}.dirty.")),
        "unstaged edit must refresh the stamp with a dirty digest: got {dirty_a}, clean was {clean}"
    );

    std::fs::write(&main_rs, format!("{base}// edit-b\n")).unwrap();
    let dirty_b = build_and_read_identity(&crate_dir, &target_dir);
    assert!(
        dirty_b.starts_with(&format!("{clean}.dirty.")),
        "second unstaged edit must also stamp dirty: {dirty_b}"
    );
    assert_ne!(
        dirty_a, dirty_b,
        "two different unstaged deltas at the same HEAD must stamp distinct identities"
    );
}

/// With packed-only refs, a linked worktree's branch has no loose ref file
/// until a commit creates it. Advancing the branch through that transition
/// (via plumbing that touches neither the worktree's HEAD, its index, nor
/// packed-refs) must still retrigger the stamp, or the rebuilt binary would
/// keep the pre-commit hash.
#[test]
fn packed_to_loose_ref_transition_refreshes_commit_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("repo");
    let worktree_dir = tmp.path().join("wt");
    let target_dir = tmp.path().join("target");
    write_probe_crate(&crate_dir);
    init_repo(&crate_dir);

    git(&crate_dir, &["branch", "side"]);
    git(&crate_dir, &["pack-refs", "--all", "--prune"]);
    assert!(
        !crate_dir.join(".git/refs/heads/side").exists(),
        "precondition: the branch ref must be packed-only"
    );
    git(
        &crate_dir,
        &["worktree", "add", worktree_dir.to_str().unwrap(), "side"],
    );

    let before = build_and_read_identity(&worktree_dir, &target_dir);

    // Advance `side` with a same-tree commit via plumbing: `update-ref` writes
    // the previously absent loose ref file and nothing else the build script
    // registered before this fix, and the identical tree keeps the worktree
    // clean so only the commit hash may change.
    let tree = git(&crate_dir, &["rev-parse", "side^{tree}"]);
    let parent = git(&crate_dir, &["rev-parse", "side"]);
    let advanced = git(
        &crate_dir,
        &["commit-tree", &tree, "-p", &parent, "-m", "advance"],
    );
    git(&crate_dir, &["update-ref", "refs/heads/side", &advanced]);

    let after = build_and_read_identity(&worktree_dir, &target_dir);
    let expected_hash = git(&worktree_dir, &["rev-parse", "--short", "HEAD"]);
    assert_ne!(
        before, after,
        "advancing the packed-only branch must refresh the stamped identity"
    );
    assert_eq!(
        after,
        format!("0.0.0+{expected_hash}"),
        "the rebuilt binary must stamp the advanced commit, clean"
    );
}
