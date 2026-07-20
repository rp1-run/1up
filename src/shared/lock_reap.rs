//! Opportunistic, best-effort reaping of provably-stale per-project lock files.
//!
//! `1up` mints one lock file per project in the XDG data dir — `mcp-{key}.lock`
//! (`cli::mcp`) and `startup-{key}.lock` (`cli::start`), keyed by a hash of the
//! project path. Nothing deletes them on the normal path, so a machine that
//! opens many distinct projects accumulates one file per project forever (issue
//! #117 observed 4076). This module sweeps that debt opportunistically.
//!
//! This module is also the single namespace authority for those lock files:
//! [`project_lock_key`] + [`lock_file_name`] construct the names the CLI mints,
//! and [`is_reapable_name`] parses exactly that shape back (allowed prefix, 32
//! lowercase hex chars, `.lock`), so the reaper can never touch a file the CLI
//! would not have created.
//!
//! A file is reaped only when it is *provably* stale on two independent axes:
//! its mtime is older than [`LOCK_REAP_MAX_AGE_SECS`] (no `1up` touched it for a
//! week) AND a non-blocking exclusive `flock` probe succeeds (no live process
//! holds it right now). The lock is held across the unlink so a concurrent
//! creator that acquires the flock between our probe and our delete cannot have
//! its brand-new lock removed. Because a pathname can be unlinked and recreated
//! between the scan and the unlink (a concurrent reaper plus a concurrent
//! startup), deletion is additionally gated on filesystem identity: the flocked
//! descriptor must still be the exact `(dev, ino, mtime)` selected during the
//! scan, and the pathname must still resolve to that same inode immediately
//! before the unlink. Any mismatch abandons the candidate.
//!
//! Every step is best-effort: any IO error skips that one file with a `debug!`
//! line and never propagates, and the whole run is bounded by both a candidate
//! count and a wall-clock budget that is enforced *while scanning directory
//! entries* as well as between deletions, so it can never meaningfully delay
//! the startup boundary it hangs off.
//!
//! The pure selector and its helpers are exercised on every platform by the unit
//! tests but only invoked in production on unix (the driver is `#[cfg(unix)]`),
//! so the module tolerates dead code on the non-unix build like `shared::fs`.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::shared::constants::{
    LOCK_FILE_SUFFIX, LOCK_KEY_HEX_LEN, LOCK_REAP_MAX_AGE_SECS, LOCK_REAP_MAX_CANDIDATES_PER_RUN,
    LOCK_REAP_NAME_PREFIXES,
};

/// A single lock-file candidate: enough for the pure selector (`name`, `mtime`)
/// plus the `path` the driver needs to act on it and the filesystem identity
/// (`dev`, `ino`) captured at scan time so the driver can prove, after it holds
/// the flock, that it is still looking at the inode that was selected (and not
/// a fresh lock recreated at the same pathname).
#[derive(Clone, Debug)]
pub struct LockCandidate {
    pub path: PathBuf,
    pub name: String,
    pub mtime: SystemTime,
    /// `st_dev` at selection time (0 in pure tests that never stat).
    pub dev: u64,
    /// `st_ino` at selection time (0 in pure tests that never stat).
    pub ino: u64,
}

/// The 32-lowercase-hex per-project lock key: the first 16 bytes of the
/// SHA-256 of the (caller-prepared) project root path, hex-encoded.
///
/// Shared by the lock creators (`cli::mcp`, `cli::start`) and the reaper's
/// name parser so both sides agree on exactly one key shape. Callers decide
/// whether to canonicalize the path first (`cli::mcp` does, `cli::start`
/// hashes the resolved root as-is).
pub fn project_lock_key(root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest[..LOCK_KEY_HEX_LEN / 2]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Construct the canonical lock-file name for `prefix` (one of
/// [`LOCK_REAP_NAME_PREFIXES`]) and a [`project_lock_key`]-shaped `key`.
///
/// The single constructor for the lock-file namespace: every name this
/// produces is accepted by [`is_reapable_name`], and nothing else is.
pub fn lock_file_name(prefix: &str, key: &str) -> String {
    debug_assert!(
        LOCK_REAP_NAME_PREFIXES.contains(&prefix),
        "unknown lock prefix {prefix:?}"
    );
    debug_assert!(is_lock_key(key), "malformed lock key {key:?}");
    format!("{prefix}{key}{LOCK_FILE_SUFFIX}")
}

/// True when `key` has the exact shape [`project_lock_key`] produces:
/// [`LOCK_KEY_HEX_LEN`] lowercase hexadecimal characters.
fn is_lock_key(key: &str) -> bool {
    key.len() == LOCK_KEY_HEX_LEN
        && key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// True when `name` is exactly a name [`lock_file_name`] would mint:
/// a known prefix (`mcp-` / `startup-`), then exactly 32 lowercase hex
/// characters, then `.lock` — nothing more. The single authority for which
/// files the reaper may ever touch; `1up` never creates any other shape, so
/// anything else in the XDG data dir is untouchable.
pub(crate) fn is_reapable_name(name: &str) -> bool {
    LOCK_REAP_NAME_PREFIXES.iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(LOCK_FILE_SUFFIX))
            .is_some_and(is_lock_key)
    })
}

/// Max-heap-by-mtime wrapper so a [`BinaryHeap`] evicts the *newest* candidate
/// first, retaining a bounded set of the oldest.
struct NewestFirst(LockCandidate);

impl PartialEq for NewestFirst {
    fn eq(&self, other: &Self) -> bool {
        self.0.mtime == other.0.mtime
    }
}

impl Eq for NewestFirst {}

impl PartialOrd for NewestFirst {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NewestFirst {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.mtime.cmp(&other.0.mtime)
    }
}

/// Pure, incrementally-fed candidate selection: offered candidates pass the
/// name gate ([`is_reapable_name`]) and the age gate (mtime older than
/// `max_age` relative to `now`), and at most `cap` of the *oldest* survivors
/// are retained (a bounded max-heap eviction, so memory and sort cost are
/// O(cap) regardless of directory size).
///
/// Side-effect-free and deterministic so the selection contract is unit-tested
/// without touching the filesystem or the clock. It intentionally does not do
/// the flock liveness probe or the identity re-checks — those are the impure
/// driver's job — so age/name selection stays testable in isolation.
struct ReapSelector {
    now: SystemTime,
    max_age: Duration,
    cap: usize,
    oldest: BinaryHeap<NewestFirst>,
}

impl ReapSelector {
    fn new(now: SystemTime, max_age: Duration, cap: usize) -> Self {
        Self {
            now,
            max_age,
            cap,
            oldest: BinaryHeap::new(),
        }
    }

    /// Offer one candidate; it is retained only if it passes the name and age
    /// gates and is among the `cap` oldest seen so far.
    fn offer(&mut self, candidate: LockCandidate) {
        if !is_reapable_name(&candidate.name) {
            return;
        }
        let stale = self
            .now
            .duration_since(candidate.mtime)
            .map(|age| age >= self.max_age)
            .unwrap_or(false);
        if !stale {
            return;
        }
        self.oldest.push(NewestFirst(candidate));
        if self.oldest.len() > self.cap {
            // Evict the newest so the retained set is always the oldest `cap`.
            self.oldest.pop();
        }
    }

    /// The retained candidates, oldest first, so a capped or budget-cut run
    /// drains the most-stale files deterministically regardless of directory
    /// iteration order.
    fn into_oldest_first(self) -> Vec<LockCandidate> {
        self.oldest
            .into_sorted_vec()
            .into_iter()
            .map(|entry| entry.0)
            .collect()
    }
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
    let mut over_budget = move || started.elapsed() >= budget;

    reap_stale_locks_bounded(
        xdg_root,
        SystemTime::now(),
        Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
        LOCK_REAP_MAX_CANDIDATES_PER_RUN,
        &mut over_budget,
    );
}

/// Non-unix no-op: the reaper relies on `flock` liveness probing, which mirrors
/// the platform gating of the lock files themselves (they are only created on
/// unix), so there is nothing to reap elsewhere.
#[cfg(not(unix))]
pub fn reap_stale_locks(_xdg_root: &std::path::Path) {}

/// Deadline-injected driver behind [`reap_stale_locks`]: `over_budget` is
/// consulted before every directory entry during the scan and before every
/// deletion, so neither a huge directory listing nor slow per-file IO can push
/// the run meaningfully past the caller's budget. Injected (rather than read
/// from the clock inline) so tests can prove the stop behavior
/// deterministically.
#[cfg(unix)]
fn reap_stale_locks_bounded(
    xdg_root: &std::path::Path,
    now: SystemTime,
    max_age: Duration,
    cap: usize,
    over_budget: &mut dyn FnMut() -> bool,
) {
    let mut selector = ReapSelector::new(now, max_age, cap);
    collect_candidates(xdg_root, &mut selector, over_budget);

    for candidate in selector.into_oldest_first() {
        if over_budget() {
            tracing::debug!("lock reap time budget reached; deferring remaining files");
            break;
        }
        reap_one(xdg_root, &candidate);
    }
}

/// Stream `xdg_root`'s entries into `selector`, best-effort: an unreadable
/// directory or entry is a `debug!` and a skip, never an error. The budget is
/// checked per entry, and non-matching names are rejected *before* the
/// metadata call so the common case (a large data dir full of non-lock files)
/// costs one name comparison per entry, not one stat.
#[cfg(unix)]
fn collect_candidates(
    xdg_root: &std::path::Path,
    selector: &mut ReapSelector,
    over_budget: &mut dyn FnMut() -> bool,
) {
    use std::os::unix::fs::MetadataExt;

    let read_dir = match std::fs::read_dir(xdg_root) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            tracing::debug!(
                "lock reap skipped; cannot read {}: {err}",
                xdg_root.display()
            );
            return;
        }
    };

    for entry in read_dir.flatten() {
        if over_budget() {
            tracing::debug!("lock reap time budget reached during scan; deferring");
            return;
        }
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => continue,
        };
        // Name gate first: no stat is ever issued for a file the reaper could
        // not touch anyway.
        if !is_reapable_name(&name) {
            continue;
        }
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
        selector.offer(LockCandidate {
            path: entry.path(),
            name,
            mtime,
            dev: metadata.dev(),
            ino: metadata.ino(),
        });
    }
}

/// True when `path` still names exactly the inode behind the open descriptor
/// `file` (same `st_dev`/`st_ino`, and a regular file, checked without
/// following a symlink leaf).
///
/// Used by the lock *acquirers* (`cli::mcp`, `cli::start`) right after a
/// successful flock: if a concurrent reaper unlinked the lock file between
/// their open and their flock, the acquirer holds a lock on an orphaned inode
/// that no longer excludes anyone — a second process could create and lock a
/// fresh file at the same pathname. Returning `false` tells the acquirer to
/// drop the orphaned descriptor and re-acquire at the pathname.
#[cfg(unix)]
pub fn flock_still_names_path(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let fd_meta = match file.metadata() {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    let path_meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    path_meta.file_type().is_file()
        && fd_meta.dev() == path_meta.dev()
        && fd_meta.ino() == path_meta.ino()
}

/// Attempt to reap a single already-age-selected candidate. Acquires a
/// non-blocking exclusive `flock`; on `EWOULDBLOCK` the lock is live and left
/// alone; on success the lock is held across the unlink so a concurrent creator
/// cannot lose a freshly-acquired lock.
///
/// The flock alone is not sufficient: between the scan and this call the stale
/// pathname may have been unlinked (by a concurrent reaper) and recreated (by a
/// concurrent startup), in which case the descriptor we just flocked is a
/// *fresh* lock that must survive. Deletion is therefore double-gated on
/// identity: (1) fstat of the flocked descriptor must equal the selected
/// candidate's `(dev, ino, mtime)` — a recreated file has a new inode or a
/// fresh mtime — and (2) an lstat of the pathname immediately before the unlink
/// must still resolve to that same `(dev, ino)`. Once the flock is held and
/// gate (1) passes, nothing else can unlink the path (both reapers and no one
/// else unlink only while holding the flock), so gate (2) is a cheap final
/// invariant check. Any error or mismatch is a `debug!` and a skip.
#[cfg(unix)]
fn reap_one(xdg_root: &std::path::Path, candidate: &LockCandidate) {
    use std::fs::OpenOptions;
    use std::os::unix::fs::MetadataExt;

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

    let lock = match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => lock,
        Err((_, Errno::EWOULDBLOCK)) => {
            // Held by a live process; leave it.
            return;
        }
        Err((_, errno)) => {
            tracing::debug!(
                "lock reap skip {}: flock failed: {errno}",
                candidate.path.display()
            );
            return;
        }
    };

    // Identity gate 1: the flocked descriptor must be the exact inode selected
    // during the scan, untouched since (same dev/ino AND same mtime). A lock
    // recreated at this pathname after the scan fails this and survives.
    let fd_meta = match lock.metadata() {
        Ok(meta) => meta,
        Err(err) => {
            tracing::debug!(
                "lock reap skip {}: fstat failed: {err}",
                candidate.path.display()
            );
            return;
        }
    };
    if fd_meta.dev() != candidate.dev
        || fd_meta.ino() != candidate.ino
        || fd_meta.modified().ok() != Some(candidate.mtime)
    {
        tracing::debug!(
            "lock reap skip {}: file changed since selection",
            candidate.path.display()
        );
        return;
    }

    // Identity gate 2: the pathname must still resolve to that same inode
    // immediately before the unlink.
    let path_meta = match std::fs::symlink_metadata(&candidate.path) {
        Ok(meta) => meta,
        Err(err) => {
            tracing::debug!(
                "lock reap skip {}: already gone: {err}",
                candidate.path.display()
            );
            return;
        }
    };
    if path_meta.dev() != candidate.dev || path_meta.ino() != candidate.ino {
        tracing::debug!(
            "lock reap skip {}: pathname re-bound since selection",
            candidate.path.display()
        );
        return;
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure batch form of [`ReapSelector`], kept as the unit-test surface for
    /// the selection contract.
    fn select_reapable(
        candidates: Vec<LockCandidate>,
        now: SystemTime,
        max_age: Duration,
        cap: usize,
    ) -> Vec<LockCandidate> {
        let mut selector = ReapSelector::new(now, max_age, cap);
        for candidate in candidates {
            selector.offer(candidate);
        }
        selector.into_oldest_first()
    }

    /// A 32-lowercase-hex key derived from `seed`, the exact shape
    /// `project_lock_key` produces.
    fn hex_key(seed: u64) -> String {
        format!("{seed:032x}")
    }

    fn candidate(name: &str, mtime: SystemTime) -> LockCandidate {
        LockCandidate {
            path: PathBuf::from(name),
            name: name.to_string(),
            mtime,
            dev: 0,
            ino: 0,
        }
    }

    #[test]
    fn lock_name_round_trips_through_parser() {
        let key = project_lock_key(Path::new("/some/project/root"));
        assert_eq!(key.len(), LOCK_KEY_HEX_LEN);
        assert!(is_lock_key(&key), "constructor key must satisfy the parser");
        for prefix in LOCK_REAP_NAME_PREFIXES {
            assert!(
                is_reapable_name(&lock_file_name(prefix, &key)),
                "constructed name must be accepted for prefix {prefix}"
            );
        }
    }

    #[test]
    fn malformed_names_are_rejected() {
        let key = hex_key(0xabcd);
        let rejected = [
            String::new(),
            ".lock".to_string(),
            "mcp-".to_string(),
            "mcp-.lock".to_string(),
            // 31 hex chars: one short.
            format!("mcp-{}.lock", &key[..31]),
            // 33 hex chars: one long.
            format!("mcp-{key}0.lock"),
            // Non-hex character in the key.
            format!("mcp-{}g.lock", &key[..31]),
            // Uppercase hex is not a shape 1up ever mints.
            format!("mcp-{}.lock", key.to_uppercase()),
            // Extra trailing component after the suffix.
            format!("mcp-{key}.lock.bak"),
            // Key present but no known prefix.
            format!("{key}.lock"),
            // Right suffix, wrong prefix.
            "rebuild.lock".to_string(),
            format!("startup-{key}.lockx"),
            "daemon.pid".to_string(),
            "update-check.json".to_string(),
        ];
        for name in rejected {
            assert!(!is_reapable_name(&name), "must reject {name:?}");
        }

        assert!(is_reapable_name(&format!("mcp-{key}.lock")));
        assert!(is_reapable_name(&format!("startup-{key}.lock")));
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
            // Right prefix and suffix but not a 32-hex key: must not match.
            candidate("mcp-abc.lock", ancient),
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
            candidate(&format!("mcp-{}.lock", hex_key(1)), now),
            candidate(
                &format!("startup-{}.lock", hex_key(2)),
                now - Duration::from_secs(60),
            ),
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
        let mcp_name = format!("mcp-{}.lock", hex_key(1));
        let startup_name = format!("startup-{}.lock", hex_key(2));
        let candidates = vec![candidate(&mcp_name, stale), candidate(&startup_name, stale)];

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

        assert_eq!(selected, vec![mcp_name, startup_name]);
    }

    #[test]
    fn cap_is_respected_and_keeps_oldest() {
        let now = SystemTime::now();
        let candidates = (0..10)
            .map(|i| {
                candidate(
                    &format!("mcp-{}.lock", hex_key(i)),
                    now - Duration::from_secs(LOCK_REAP_MAX_AGE_SECS + i + 1),
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
        assert_eq!(
            names,
            vec![
                format!("mcp-{}.lock", hex_key(9)),
                format!("mcp-{}.lock", hex_key(8)),
                format!("mcp-{}.lock", hex_key(7)),
            ]
        );
    }
}

#[cfg(all(test, unix))]
mod driver_tests {
    use super::*;

    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::MetadataExt;

    use nix::fcntl::{Flock, FlockArg};

    fn hex_key(seed: u64) -> String {
        format!("{seed:032x}")
    }

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

    /// A `LockCandidate` capturing `path`'s current identity, exactly as the
    /// scan would have.
    fn candidate_for(path: &std::path::Path) -> LockCandidate {
        let meta = std::fs::symlink_metadata(path).unwrap();
        LockCandidate {
            path: path.to_path_buf(),
            name: path.file_name().unwrap().to_str().unwrap().to_string(),
            mtime: meta.modified().unwrap(),
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }

    #[test]
    fn old_unheld_lock_is_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join(format!("mcp-{}.lock", hex_key(0xa)));
        File::create(&lock).unwrap();
        backdate(&lock, LOCK_REAP_MAX_AGE_SECS + 60);

        reap_stale_locks(&root);

        assert!(!lock.exists(), "stale unheld lock should be reaped");
    }

    #[test]
    fn old_held_lock_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join(format!("startup-{}.lock", hex_key(0xb)));
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
        let lock = root.join(format!("mcp-{}.lock", hex_key(0xc)));
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

    /// Regression for the replacement race: a candidate is selected, then the
    /// pathname is unlinked (a concurrent reaper) and recreated (a concurrent
    /// startup) before `reap_one` runs. Identity gate 1 must abandon the
    /// candidate so the replacement survives.
    ///
    /// Filesystems are free to hand the replacement the *same* inode number
    /// as the just-unlinked original (APFS and tmpfs both can), so this test
    /// must not — and does not — assume the inode differs. The deterministic
    /// discriminator is the mtime component of the `(dev, ino, mtime)`
    /// identity triple: the candidate was selected with the original's
    /// explicitly-backdated mtime (> `LOCK_REAP_MAX_AGE_SECS` old), while the
    /// replacement is created fresh, so the two mtimes differ by ~a week
    /// regardless of inode reuse. A fresh replacement loses no coverage:
    /// `reap_one` never re-checks age, so a broken identity gate would unlink
    /// the fresh file all the same.
    ///
    /// The one variant deliberately not asserted here: a replacement that
    /// reuses the inode AND carries the identical stale mtime is equal on the
    /// whole identity triple, i.e. indistinguishable from the original by
    /// design — it *is* the stale file for every observable purpose, deleting
    /// it is correct behavior, and that outcome is already covered by
    /// `old_unheld_lock_is_reaped`.
    #[test]
    fn replaced_lock_file_is_not_reaped_even_if_inode_reused() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join(format!("mcp-{}.lock", hex_key(0xd)));
        File::create(&lock).unwrap();
        backdate(&lock, LOCK_REAP_MAX_AGE_SECS + 60);
        let selected = candidate_for(&lock);

        // Replacement between selection and reap: unlink + recreate fresh.
        // The replacement's mtime is "now", ~LOCK_REAP_MAX_AGE_SECS newer
        // than the selected candidate's, so identity gate 1 fails on mtime
        // even if the filesystem reuses the original's inode number.
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, b"replacement").unwrap();

        reap_one(&root, &selected);

        assert!(
            lock.exists(),
            "a lock recreated at the selected pathname must survive the reap"
        );
        assert_eq!(
            std::fs::read(&lock).unwrap(),
            b"replacement",
            "the surviving file must be the replacement, not a resurrected original"
        );
    }

    /// Same race family, other observable: the selected inode survives but is
    /// *touched* (fresh mtime, i.e. back in active use) between selection and
    /// reap. The mtime component of identity gate 1 must abandon it.
    #[test]
    fn touched_since_selection_is_not_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join(format!("startup-{}.lock", hex_key(0xe)));
        File::create(&lock).unwrap();
        backdate(&lock, LOCK_REAP_MAX_AGE_SECS + 60);
        let selected = candidate_for(&lock);

        // Same inode, but touched after selection.
        filetime::set_file_mtime(&lock, SystemTime::now().into()).unwrap();

        reap_one(&root, &selected);

        assert!(
            lock.exists(),
            "a lock touched after selection must survive the reap"
        );
    }

    /// Deterministic budget test: with the deadline already expired, the scan
    /// must stop before statting/collecting anything and the reap loop must
    /// delete nothing, no matter how many stale files exist.
    #[test]
    fn expired_budget_reaps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let locks: Vec<PathBuf> = (0..10)
            .map(|i| {
                let lock = root.join(format!("mcp-{}.lock", hex_key(i)));
                File::create(&lock).unwrap();
                backdate(&lock, LOCK_REAP_MAX_AGE_SECS + 60);
                lock
            })
            .collect();

        let mut over_budget = || true;
        reap_stale_locks_bounded(
            &root,
            SystemTime::now(),
            Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
            LOCK_REAP_MAX_CANDIDATES_PER_RUN,
            &mut over_budget,
        );

        for lock in locks {
            assert!(
                lock.exists(),
                "an expired budget must reap nothing: {} was deleted",
                lock.display()
            );
        }
    }

    /// Deterministic budget test: the deadline is enforced *between* unlinks,
    /// too. The injected budget trips as soon as the first file disappears, so
    /// exactly one of the ten stale locks is reaped.
    #[test]
    fn budget_stops_between_reaps() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        for i in 0..10u64 {
            let lock = root.join(format!("mcp-{}.lock", hex_key(i)));
            File::create(&lock).unwrap();
            backdate(&lock, LOCK_REAP_MAX_AGE_SECS + 60);
        }

        // Over budget the moment any file has been deleted: false for the
        // whole scan and the first reap, true for every check after it.
        let count_root = root.clone();
        let mut over_budget = move || std::fs::read_dir(&count_root).unwrap().count() < 10;
        reap_stale_locks_bounded(
            &root,
            SystemTime::now(),
            Duration::from_secs(LOCK_REAP_MAX_AGE_SECS),
            LOCK_REAP_MAX_CANDIDATES_PER_RUN,
            &mut over_budget,
        );

        let remaining = std::fs::read_dir(&root).unwrap().count();
        assert_eq!(
            remaining, 9,
            "the deadline must stop the run after the first reap"
        );
    }

    #[test]
    fn flock_still_names_path_true_for_intact_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join(format!("mcp-{}.lock", hex_key(0xf)));
        let file = File::create(&lock).unwrap();

        assert!(flock_still_names_path(&file, &lock));
    }

    #[test]
    fn flock_still_names_path_false_after_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join(format!("mcp-{}.lock", hex_key(0x10)));
        let file = File::create(&lock).unwrap();

        std::fs::remove_file(&lock).unwrap();

        assert!(
            !flock_still_names_path(&file, &lock),
            "an unlinked descriptor no longer names its pathname"
        );
    }

    #[test]
    fn flock_still_names_path_false_after_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let lock = root.join(format!("mcp-{}.lock", hex_key(0x11)));
        let file = File::create(&lock).unwrap();

        std::fs::remove_file(&lock).unwrap();
        File::create(&lock).unwrap();

        assert!(
            !flock_still_names_path(&file, &lock),
            "a replaced pathname names a different inode than the descriptor"
        );
    }
}
