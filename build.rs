//! Build script that stamps a build-identity string into the binary.
//!
//! The daemon version-handshake needs to discriminate two builds that share the
//! same Cargo semver but were produced from different source (a different commit,
//! or an uncommitted working tree). Bare semver cannot: a same-version daemon
//! from a *different* build would otherwise be trusted as authoritative. This
//! script composes `{CARGO_PKG_VERSION}+{git-short-hash}[.dirty]` and exposes it
//! as the `ONEUP_BUILD_IDENTITY` compile-time env var (read via `env!` in
//! `src/shared/constants.rs`).
//!
//! It must never fail the build: a checkout without git (e.g. a source tarball)
//! degrades to `{CARGO_PKG_VERSION}+unknown`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());

    let identity = match git_short_hash() {
        Some(hash) => {
            let suffix = if git_worktree_is_dirty() {
                ".dirty"
            } else {
                ""
            };
            format!("{version}+{hash}{suffix}")
        }
        // No git, not a repo, or git failed (source tarball, CI without .git):
        // degrade gracefully rather than failing the build.
        None => format!("{version}+unknown"),
    };

    println!("cargo:rustc-env=ONEUP_BUILD_IDENTITY={identity}");

    // Refresh the stamp when git state moves. HEAD moves on checkout, the ref
    // file and packed-refs move on commit, and the index moves on `git add`.
    // A bare unstaged working-tree edit does not touch any of these, so the
    // `.dirty` suffix only refreshes on the next commit, stage, or clean
    // rebuild; the trust-critical path (release builds) is a clean committed
    // tree, where the stamp is exact.
    emit_git_rerun_triggers();
}

/// Short commit hash for `HEAD`, or `None` when git is unavailable / this is
/// not a repository.
fn git_short_hash() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let hash = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if hash.is_empty() {
        None
    } else {
        Some(hash)
    }
}

/// Whether the working tree has uncommitted changes to *tracked* files.
///
/// Untracked files (build output, eval artifacts) are deliberately ignored:
/// they are not compiled into the binary, so they must not discriminate the
/// build identity. This matches `git describe --dirty` semantics.
fn git_worktree_is_dirty() -> bool {
    let output = match Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        // If the dirty probe cannot run, prefer marking dirty: an unknown
        // working-tree state must never masquerade as a clean, authoritative
        // build.
        _ => return true,
    };
    !output.stdout.is_empty()
}

/// Emits `cargo:rerun-if-changed` for the git files that move when the commit
/// or index changes, so the stamp refreshes without a manual clean build.
fn emit_git_rerun_triggers() {
    let Some(git_dir) = git_dir() else {
        return;
    };

    for rel in ["HEAD", "index", "packed-refs"] {
        let path = git_dir.join(rel);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    if let Some(ref_path) = head_ref_path(&git_dir) {
        if ref_path.exists() {
            println!("cargo:rerun-if-changed={}", ref_path.display());
        }
    }
}

/// Resolves the git directory (handles linked worktrees, where `.git` is a
/// file pointing at the real git dir).
fn git_dir() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if dir.is_empty() {
        None
    } else {
        Some(PathBuf::from(dir))
    }
}

/// The concrete ref file that `HEAD` points at (e.g. `refs/heads/main`), so a
/// commit that advances the branch tip retriggers the build script.
fn head_ref_path(git_dir: &Path) -> Option<PathBuf> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let reference = head.trim().strip_prefix("ref: ")?;
    Some(git_dir.join(reference))
}
