//! Build script that stamps a build-identity string into the binary.
//!
//! The daemon version-handshake needs to discriminate two builds that share the
//! same Cargo semver but were produced from different source (a different commit,
//! or an uncommitted working tree). Bare semver cannot: a same-version daemon
//! from a *different* build would otherwise be trusted as authoritative. This
//! script composes `{CARGO_PKG_VERSION}+{git-short-hash}[.dirty[.{digest}]]`
//! and exposes it as the `ONEUP_BUILD_IDENTITY` compile-time env var (read via
//! `env!` in `src/shared/constants.rs`). The dirty suffix carries a short
//! content digest of the tracked-file delta so two *different* dirty builds at
//! the same HEAD also stamp differently (see [`dirty_suffix`]).
//!
//! It must never fail the build: a checkout without git (e.g. a source tarball)
//! degrades to `{CARGO_PKG_VERSION}+unknown`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());

    let identity = match git_short_hash() {
        Some(hash) => format!("{version}+{hash}{}", dirty_suffix()),
        // No git, not a repo, or git failed (source tarball, CI without .git):
        // degrade gracefully rather than failing the build.
        None => format!("{version}+unknown"),
    };

    println!("cargo:rustc-env=ONEUP_BUILD_IDENTITY={identity}");

    // Refresh the stamp when git state moves. HEAD moves on checkout, the ref
    // file and packed-refs move on commit, and the index moves on `git add`.
    emit_git_rerun_triggers();

    // Refresh the stamp on bare unstaged edits too. Git metadata alone misses
    // them: an unstaged tracked-file edit recompiles the crate without touching
    // HEAD, the index, or any ref, so Cargo would reuse the previous
    // `ONEUP_BUILD_IDENTITY` and the rebuilt binary — compiled from *different*
    // source — would carry the old dirty digest and still pass the exact-match
    // authority gate. Registering every tracked file closes that hole: any
    // tracked edit reruns this script, the `git diff HEAD` digest refreshes,
    // and the changed env var recompiles the consumers.
    emit_tracked_file_rerun_triggers();
}

/// Suffix discriminating dirty builds, or `""` for a clean tree.
///
/// A bare boolean `.dirty` would stamp two *different* dirty builds at the
/// same HEAD identically, so the daemon's exact-match trust gate would still
/// trust a daemon left over from an earlier, differently-dirty build — the
/// same wrong-build failure mode this stamp exists to prevent. To
/// discriminate, the suffix folds in a content digest of the tracked-file
/// delta: `.dirty.{first 8 lowercase hex chars of the digest}`.
///
/// The digested input is the output of exactly one command, `git diff HEAD`,
/// which covers both staged and unstaged changes to tracked files relative to
/// HEAD — the same tracked-files-only scope as the dirty probe. The digest is
/// computed with `git hash-object --stdin` (git's blob object id) rather than
/// SHA-256 so the build script needs no crate dependencies; 8 hex chars gives
/// collision *discrimination* between concurrent working states, not
/// cryptographic integrity. The script reruns whenever git metadata or any
/// tracked file changes (see `emit_git_rerun_triggers` and
/// `emit_tracked_file_rerun_triggers`), so the digest tracks every rebuild
/// that can consume changed tracked source.
///
/// If the digest probe fails, degrade to plain `.dirty`: still conservatively
/// marked dirty, merely without cross-dirty-build discrimination.
fn dirty_suffix() -> String {
    if !git_worktree_is_dirty() {
        return String::new();
    }
    match dirty_digest() {
        Some(digest) => format!(".dirty.{digest}"),
        None => ".dirty".to_string(),
    }
}

/// First 8 lowercase hex chars of `git hash-object --stdin` over the
/// `git diff HEAD` output, or `None` when either probe fails.
fn dirty_digest() -> Option<String> {
    let diff = git().args(["diff", "HEAD"]).output().ok()?;
    if !diff.status.success() {
        return None;
    }

    let mut hasher = git()
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    hasher.stdin.take()?.write_all(&diff.stdout).ok()?;
    let output = hasher.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    let hash = String::from_utf8(output.stdout).ok()?.trim().to_lowercase();
    if hash.len() < 8 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(hash[..8].to_string())
}

/// A `git` command with optional locking disabled (`GIT_OPTIONAL_LOCKS=0`),
/// so probe commands like `status` and `diff` never opportunistically rewrite
/// `.git/index` as a side effect. Without this, every build's probes would
/// freshen the index stat cache, and the registered `index` rerun trigger
/// would re-dirty the build script on every subsequent build.
fn git() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// Short commit hash for `HEAD`, or `None` when git is unavailable / this is
/// not a repository.
fn git_short_hash() -> Option<String> {
    let output = git().args(["rev-parse", "--short", "HEAD"]).output().ok()?;
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
    let output = match git()
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

    // Per-worktree files: in a linked worktree `--absolute-git-dir` is
    // `.git/worktrees/<name>`, which holds that worktree's HEAD and index.
    for rel in ["HEAD", "index"] {
        let path = git_dir.join(rel);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    // Shared (common) files: packed-refs and refs/heads/<branch> live in the
    // *common* git dir, which differs from `git_dir` in a linked worktree.
    // Without these, a commit made in a linked worktree (empty commit,
    // `reset --soft`, message-only amend) advances HEAD without touching any
    // registered file, baking a stale hash into the next build. For the main
    // worktree the common dir equals `git_dir`, so this also covers it.
    let common_dir = git_common_dir().unwrap_or_else(|| git_dir.clone());

    let packed_refs = common_dir.join("packed-refs");
    if packed_refs.exists() {
        println!("cargo:rerun-if-changed={}", packed_refs.display());
    }

    // Registered even when the loose ref file does not exist yet: with
    // packed-only refs (post `git pack-refs`, or a linked worktree cut from
    // one) the first commit *creates* the loose ref, and an existence guard
    // here would have suppressed the one directive that could observe it —
    // baking the pre-commit hash into the next build. Cargo treats a missing
    // registered path as always-changed, so until the loose ref appears this
    // script reruns each build; that is a few git probes, and the unchanged
    // identity env means no recompilation cascades.
    if let Some(ref_path) = head_ref_path(&git_dir, &common_dir) {
        println!("cargo:rerun-if-changed={}", ref_path.display());
    }
}

/// Emits `cargo:rerun-if-changed` for every git-tracked file, so a bare
/// unstaged edit — which changes no git metadata — still reruns this script
/// and refreshes the dirty digest before the crate rebuilds from the edited
/// source.
///
/// Untracked files are deliberately not registered: they never enter the
/// digest, so their churn must not rerun the script. A tracked file deleted
/// from the working tree registers a missing path, which Cargo treats as
/// always-changed — the script then reruns each build (cheap) while the
/// digest, which already reflects the deletion via `git diff HEAD`, stays
/// stable, so no recompilation cascades. Degrades to no-op without git.
fn emit_tracked_file_rerun_triggers() {
    let Ok(output) = git().args(["ls-files", "-z"]).output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for path in output
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::str::from_utf8(p).ok())
    {
        println!("cargo:rerun-if-changed={path}");
    }
}

/// Resolves the git directory (handles linked worktrees, where `.git` is a
/// file pointing at the real git dir).
fn git_dir() -> Option<PathBuf> {
    let output = git()
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

/// Resolves the *common* git directory shared by all worktrees (where
/// `packed-refs` and `refs/heads/*` live). Equals [`git_dir`] for the main
/// worktree. Git may print it relative to the current directory, so a
/// relative result is resolved against the build script's cwd (the package
/// root).
fn git_common_dir() -> Option<PathBuf> {
    let output = git()
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let dir = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if dir.is_empty() {
        return None;
    }
    let dir = PathBuf::from(dir);
    if dir.is_absolute() {
        Some(dir)
    } else {
        Some(std::env::current_dir().ok()?.join(dir))
    }
}

/// The concrete ref file that `HEAD` points at (e.g. `refs/heads/main`), so a
/// commit that advances the branch tip retriggers the build script. `HEAD`
/// itself is per-worktree, but the ref it names resolves against the common
/// dir: in a linked worktree `refs/heads/<branch>` exists only there.
fn head_ref_path(git_dir: &Path, common_dir: &Path) -> Option<PathBuf> {
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let reference = head.trim().strip_prefix("ref: ")?;
    Some(common_dir.join(reference))
}
