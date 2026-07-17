//! Opportunistic, best-effort reaping of provably-stale per-project lock files.
//!
//! `1up` mints one lock file per project in the XDG data dir — `mcp-{key}.lock`
//! (`cli::mcp`) and `startup-{key}.lock` (`cli::start`), keyed by a hash of the
//! project path. Nothing deletes them on the normal path, so a machine that
//! opens many distinct projects accumulates one file per project forever (issue
//! #117 observed 4076). This module sweeps that debt opportunistically.
//!
//! A file is reaped only when it is *provably* stale on two independent axes:
//! its mtime is older than [`LOCK_REAP_MAX_AGE_SECS`] (no `1up` touched it for a
//! week) AND a non-blocking exclusive `flock` probe succeeds (no live process
//! holds it right now). The lock is held across the unlink so a concurrent
//! creator that acquires the flock between our probe and our delete cannot have
//! its brand-new lock removed. Every step is best-effort: any IO error skips
//! that one file with a `debug!` line and never propagates, and the whole run is
//! bounded by both a candidate count and a wall-clock budget so it can never
//! meaningfully delay the startup boundary it hangs off.
//!
//! The pure selector and its helpers are exercised on every platform by the unit
//! tests but only invoked in production on unix (the driver is `#[cfg(unix)]`),
//! so the module tolerates dead code on the non-unix build like `shared::fs`.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::shared::constants::{
    LOCK_FILE_SUFFIX, LOCK_REAP_MAX_AGE_SECS, LOCK_REAP_MAX_CANDIDATES_PER_RUN,
    LOCK_REAP_NAME_PREFIXES,
};

/// A single lock-file candidate: enough for the pure selector (`name`, `mtime`)
/// plus the `path` the driver needs to act on it.
#[derive(Clone, Debug)]
pub struct LockCandidate {
    pub path: PathBuf,
    pub name: String,
    pub mtime: SystemTime,
}

/// True when `name` matches a known per-project lock-file pattern
/// (`mcp-*.lock` / `startup-*.lock`). The single authority for which files the
/// reaper may ever touch.
fn is_reapable_name(name: &str) -> bool {
    name.ends_with(LOCK_FILE_SUFFIX)
        && LOCK_REAP_NAME_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Pure candidate selection: given a directory listing, `now`, an age threshold,
/// and a per-run cap, return the subset that is safe to *consider* deleting —
/// filenames matching a known lock pattern whose mtime is older than `max_age`,
/// oldest first, capped at `cap`.
///
/// Side-effect-free and deterministic so the selection contract is unit-tested
/// without touching the filesystem or the clock. It intentionally does not do
/// the flock liveness probe — that is the impure driver's job — so age selection
/// stays testable in isolation.
fn select_reapable(
    mut candidates: Vec<LockCandidate>,
    now: SystemTime,
    max_age: Duration,
    cap: usize,
) -> Vec<LockCandidate> {
    candidates.retain(|candidate| {
        is_reapable_name(&candidate.name)
            && now
                .duration_since(candidate.mtime)
                .map(|age| age >= max_age)
                .unwrap_or(false)
    });
    // Oldest first so a capped run drains the most-stale files and the cap is
    // deterministic regardless of directory iteration order.
    candidates.sort_by_key(|candidate| candidate.mtime);
    candidates.truncate(cap);
    candidates
}

/// Reap provably-stale per-project lock files under `xdg_root`, best-effort.
///
/// Wired as a fire-and-forget call at the two process-startup boundaries that
/// already resolve `xdg_root` and create files in it — `cli::mcp` (MCP server
/// start) and `cli::start` (daemon/startup) — because those are the natural
/// points to amortize a small opportunistic sweep: the caller has just paid for
/// XDG-root resolution, and a startup boundary is where a sub-`LOCK_REAP_TIME_BUDGET_MS`
/// delay is acceptable. Never returns an error and never panics; a failure at
/// any step degrades to skipping work.
#[cfg(unix)]
pub fn reap_stale_locks(xdg_root: &std::path::Path) {
    use std::time::Instant;

    use crate::shared::constants::LOCK_REAP_TIME_BUDGET_MS;

    let started = Instant::now();
    let budget = Duration::from_millis(LOCK_REAP_TIME_BUDGET_MS);

    let candidates = collect_candidates(xdg_root);
    let selected = select_reapable(
        candidates,
        SystemTime::now(),
        Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
        LOCK_REAP_MAX_CANDIDATES_PER_RUN,
    );

    for candidate in selected {
        if started.elapsed() >= budget {
            tracing::debug!("lock reap time budget reached; deferring remaining files");
            break;
        }
        reap_one(xdg_root, &candidate);
    }
}

/// Non-unix no-op: the reaper relies on `flock` liveness probing, which mirrors
/// the platform gating of the lock files themselves (they are only created on
/// unix), so there is nothing to reap elsewhere.
#[cfg(not(unix))]
pub fn reap_stale_locks(_xdg_root: &std::path::Path) {}

/// Read `xdg_root` into `LockCandidate`s, skipping anything that is not a plain
/// regular file we can stat. Best-effort: an unreadable directory or entry is a
/// `debug!` and an empty/short result, never an error.
#[cfg(unix)]
fn collect_candidates(xdg_root: &std::path::Path) -> Vec<LockCandidate> {
    let read_dir = match std::fs::read_dir(xdg_root) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            tracing::debug!(
                "lock reap skipped; cannot read {}: {err}",
                xdg_root.display()
            );
            return Vec::new();
        }
    };

    let mut candidates = Vec::new();
    for entry in read_dir.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.file_type().is_file() {
            continue;
        }
        let mtime = match metadata.modified() {
            Ok(mtime) => mtime,
            Err(_) => continue,
        };
        candidates.push(LockCandidate {
            path: entry.path(),
            name,
            mtime,
        });
    }
    candidates
}

/// Attempt to reap a single already-age-selected candidate. Acquires a
/// non-blocking exclusive `flock`; on `EWOULDBLOCK` the lock is live and left
/// alone; on success the lock is held across the unlink so a concurrent creator
/// cannot lose a freshly-acquired lock. Any error is a `debug!` and a skip.
#[cfg(unix)]
fn reap_one(xdg_root: &std::path::Path, candidate: &LockCandidate) {
    use std::fs::OpenOptions;

    use nix::errno::Errno;
    use nix::fcntl::{Flock, FlockArg};

    use crate::shared::fs::remove_regular_file;

    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(&candidate.path)
    {
        Ok(file) => file,
        Err(err) => {
            tracing::debug!(
                "lock reap skip {}: open failed: {err}",
                candidate.path.display()
            );
            return;
        }
    };

    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => {
            match remove_regular_file(&candidate.path, xdg_root) {
                Ok(_) => tracing::debug!("reaped stale lock {}", candidate.path.display()),
                Err(err) => tracing::debug!(
                    "lock reap skip {}: unlink failed: {err}",
                    candidate.path.display()
                ),
            }
            // Release only after the unlink so the exclusion window covers it.
            drop(lock);
        }
        Err((_, Errno::EWOULDBLOCK)) => {
            // Held by a live process; leave it.
        }
        Err((_, errno)) => {
            tracing::debug!(
                "lock reap skip {}: flock failed: {errno}",
                candidate.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, mtime: SystemTime) -> LockCandidate {
        LockCandidate {
            path: PathBuf::from(name),
            name: name.to_string(),
            mtime,
        }
    }

    #[test]
    fn old_unmatched_name_is_never_selected() {
        let now = SystemTime::now();
        let ancient = now - Duration::from_secs(LOCK_REAP_MAX_AGE_SECS * 10);
        let candidates = vec![
            candidate("daemon.pid", ancient),
            candidate("update-check.json", ancient),
            // Right suffix, wrong prefix: must not match.
            candidate("rebuild.lock", ancient),
        ];

        let selected = select_reapable(
            candidates,
            now,
            Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
            LOCK_REAP_MAX_CANDIDATES_PER_RUN,
        );

        assert!(selected.is_empty());
    }

    #[test]
    fn young_matching_lock_is_excluded() {
        let now = SystemTime::now();
        let candidates = vec![
            candidate("mcp-abc.lock", now),
            candidate("startup-abc.lock", now - Duration::from_secs(60)),
        ];

        let selected = select_reapable(
            candidates,
            now,
            Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
            LOCK_REAP_MAX_CANDIDATES_PER_RUN,
        );

        assert!(selected.is_empty());
    }

    #[test]
    fn old_matching_locks_are_selected() {
        let now = SystemTime::now();
        let stale = now - Duration::from_secs(LOCK_REAP_MAX_AGE_SECS + 1);
        let candidates = vec![
            candidate("mcp-abc.lock", stale),
            candidate("startup-def.lock", stale),
        ];

        let mut selected: Vec<String> = select_reapable(
            candidates,
            now,
            Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
            LOCK_REAP_MAX_CANDIDATES_PER_RUN,
        )
        .into_iter()
        .map(|candidate| candidate.name)
        .collect();
        selected.sort();

        assert_eq!(selected, vec!["mcp-abc.lock", "startup-def.lock"]);
    }

    #[test]
    fn cap_is_respected_and_keeps_oldest() {
        let now = SystemTime::now();
        let candidates = (0..10)
            .map(|i| {
                candidate(
                    &format!("mcp-{i}.lock"),
                    now - Duration::from_secs(LOCK_REAP_MAX_AGE_SECS + i as u64 + 1),
                )
            })
            .collect();

        let selected = select_reapable(
            candidates,
            now,
            Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
            3,
        );

        assert_eq!(selected.len(), 3);
        // Oldest-first: the three largest offsets (i = 9, 8, 7).
        let names: Vec<&str> = selected.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["mcp-9.lock", "mcp-8.lock", "mcp-7.lock"]);
    }
}

#[cfg(all(test, unix))]
mod driver_tests {
    use super::*;

    use std::fs::{File, OpenOptions};

    use nix::fcntl::{Flock, FlockArg};

    fn backdate(path: &std::path::Path, secs: u64) {
        let stale = SystemTime::now() - Duration::from_secs(secs);
        filetime::set_file_mtime(path, stale.into()).unwrap();
    }

    /// Canonicalize the tempdir so the reaper's secure-fs unlink sees a
    /// symlink-free root (macOS `/var` -> `/private/var`), matching the
    /// symlink-free `xdg_root` `ensure_secure_xdg_root` guarantees in production.
    fn root_of(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().unwrap()
    }

    #[test]
    fn old_unheld_lock_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join("mcp-abc.lock");
        File::create(&lock).unwrap();
        backdate(&lock, LOCK_REAP_MAX_AGE_SECS + 60);

        reap_stale_locks(&root);

        assert!(!lock.exists(), "stale unheld lock should be reaped");
    }

    #[test]
    fn old_held_lock_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join("startup-abc.lock");
        File::create(&lock).unwrap();
        backdate(&lock, LOCK_REAP_MAX_AGE_SECS + 60);

        // Hold a real exclusive flock for the duration of the reap.
        let holder = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        let _guard = Flock::lock(holder, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, errno)| errno)
            .unwrap();

        reap_stale_locks(&root);

        assert!(
            lock.exists(),
            "held lock must not be reaped even when stale"
        );
    }

    #[test]
    fn young_lock_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join("mcp-fresh.lock");
        File::create(&lock).unwrap();

        reap_stale_locks(&root);

        assert!(lock.exists(), "recently-touched lock must not be reaped");
    }

    #[test]
    fn non_matching_filename_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let other = root.join("update-check.json");
        File::create(&other).unwrap();
        backdate(&other, LOCK_REAP_MAX_AGE_SECS * 10);

        reap_stale_locks(&root);

        assert!(other.exists(), "non-lock files must never be reaped");
    }
}
