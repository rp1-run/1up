use std::path::Path;

use clap::Args;

use crate::daemon::lifecycle;
#[cfg(unix)]
use crate::shared::constants::{
    LOCK_ACQUIRE_IDENTITY_RETRIES, MCP_LOCK_PREFIX, SECURE_STATE_FILE_MODE,
};
#[cfg(unix)]
use crate::shared::fs::{ensure_secure_xdg_root, validate_regular_file_path};
#[cfg(unix)]
use crate::shared::lock_reap::{flock_still_names_path, lock_file_name, project_lock_key};
use crate::shared::project;
#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
#[cfg(unix)]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::path::PathBuf;

#[derive(Args)]
pub struct McpArgs {
    /// Project root directory (defaults to current directory)
    #[arg(long, default_value = ".")]
    pub path: String,
}

pub async fn exec(args: McpArgs) -> anyhow::Result<()> {
    // Capture launch_subdir before normalization
    let launch_path = Path::new(&args.path);
    let canonical_launch_dir = match launch_path.canonicalize() {
        Ok(p) => p,
        Err(_) => launch_path.to_path_buf(),
    };

    let resolved = project::resolve_project_root(launch_path)?;
    let _instance_lock = acquire_mcp_instance_lock(&resolved.state_root)?;
    ensure_daemon_for_mcp(&resolved.state_root, &resolved.source_root);

    // Determine launch_subdir as the relative portion if the launch dir differs from source_root
    let launch_subdir = if canonical_launch_dir != resolved.source_root
        && canonical_launch_dir.starts_with(&resolved.source_root)
    {
        Some(canonical_launch_dir)
    } else {
        None
    };

    crate::mcp::server::serve_stdio(resolved.state_root, resolved.source_root, launch_subdir).await
}

fn ensure_daemon_for_mcp(project_root: &Path, source_root: &Path) {
    if !lifecycle::supports_daemon() {
        return;
    }

    let project_id = match project::ensure_project_id_for_auto_init(project_root) {
        Ok((project_id, _)) => project_id,
        Err(err) => {
            tracing::debug!("MCP daemon auto-start skipped; failed to initialize project: {err}");
            return;
        }
    };

    if let Err(err) = lifecycle::ensure_daemon(&project_id, project_root, source_root) {
        tracing::debug!("MCP daemon auto-start skipped: {err}");
    }
}

#[cfg(unix)]
struct McpInstanceLock {
    _lock: Flock<File>,
}

#[cfg(not(unix))]
struct McpInstanceLock;

#[cfg(unix)]
fn acquire_mcp_instance_lock(project_root: &Path) -> anyhow::Result<McpInstanceLock> {
    let xdg_root = ensure_secure_xdg_root()?;
    // Opportunistic, best-effort sweep of abandoned per-project lock files. This
    // is a natural integration point: we have just resolved the XDG root and are
    // about to create another lock file in it, and MCP server start is a process
    // boundary where a bounded background-debt sweep is acceptable. It never
    // errors and never meaningfully delays startup.
    crate::shared::lock_reap::reap_stale_locks(&xdg_root);
    acquire_mcp_instance_lock_in(&xdg_root, project_root)
}

/// Acquisition core, split from [`acquire_mcp_instance_lock`] so tests can
/// drive it against an isolated root instead of the real XDG data dir.
///
/// A successful flock is not sufficient on its own: a concurrent stale-lock
/// reaper may unlink the lock file between our open and our flock, leaving us
/// holding an orphaned inode that excludes nobody — a second instance would
/// then create and lock a fresh file at the same pathname. After every
/// successful flock we therefore verify the pathname still names the locked
/// inode and, if not, drop the orphan and re-acquire, bounded by
/// [`LOCK_ACQUIRE_IDENTITY_RETRIES`].
#[cfg(unix)]
fn acquire_mcp_instance_lock_in(
    xdg_root: &Path,
    project_root: &Path,
) -> anyhow::Result<McpInstanceLock> {
    let lock_path = mcp_lock_path(xdg_root, project_root);
    let validated_path = validate_regular_file_path(&lock_path, xdg_root)?;

    for _ in 0..=LOCK_ACQUIRE_IDENTITY_RETRIES {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(SECURE_STATE_FILE_MODE)
            .open(&validated_path)?;

        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => {
                if flock_still_names_path(&lock, &validated_path) {
                    return Ok(McpInstanceLock { _lock: lock });
                }
                // Reaped/replaced between our open and flock; the descriptor
                // is orphaned and excludes nobody. Drop it and re-acquire.
                drop(lock);
            }
            Err((_, Errno::EWOULDBLOCK)) => anyhow::bail!(
                "another 1up mcp instance is already running for {}",
                project_root.display()
            ),
            Err((_, errno)) => anyhow::bail!(
                "failed to lock MCP instance file {}: {errno}",
                validated_path.display()
            ),
        }
    }
    anyhow::bail!(
        "MCP instance lock {} kept being replaced during acquisition",
        validated_path.display()
    )
}

#[cfg(not(unix))]
fn acquire_mcp_instance_lock(_project_root: &Path) -> anyhow::Result<McpInstanceLock> {
    Ok(McpInstanceLock)
}

#[cfg(unix)]
fn mcp_lock_path(xdg_root: &Path, project_root: &Path) -> PathBuf {
    xdg_root.join(lock_file_name(MCP_LOCK_PREFIX, &mcp_lock_key(project_root)))
}

#[cfg(unix)]
fn mcp_lock_key(project_root: &Path) -> String {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    project_lock_key(&canonical)
}

#[cfg(all(test, unix))]
mod lock_tests {
    use super::*;

    /// Canonicalize the tempdir (macOS `/var` -> `/private/var`) so secure-fs
    /// path validation sees a symlink-free root, matching production
    /// `ensure_secure_xdg_root` guarantees.
    fn root_of(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().canonicalize().unwrap()
    }

    #[test]
    fn mcp_lock_name_matches_reaper_namespace() {
        let name_path = mcp_lock_path(Path::new("/xdg"), Path::new("/some/project"));
        let name = name_path.file_name().unwrap().to_str().unwrap();
        assert!(
            crate::shared::lock_reap::is_reapable_name(name),
            "minted lock name {name:?} must parse under the reaper's strict namespace"
        );
    }

    #[test]
    fn instance_lock_acquires_and_excludes_second_acquirer() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let project = Path::new("/some/project");

        let _held = acquire_mcp_instance_lock_in(&root, project).unwrap();
        let err = acquire_mcp_instance_lock_in(&root, project)
            .map(|_| ())
            .unwrap_err();
        assert!(
            err.to_string().contains("already running"),
            "second acquirer must observe the held lock, got: {err}"
        );
    }

    #[test]
    fn instance_lock_survives_prior_unlinked_holder_descriptor() {
        // A descriptor orphaned by the reaper (open + unlinked pathname) must
        // not exclude a fresh acquirer: acquisition recreates the path and
        // verifies its own descriptor still names it.
        let dir = tempfile::tempdir().unwrap();
        let root = root_of(&dir);
        let project = Path::new("/some/project");
        let lock_path = mcp_lock_path(&root, project);

        let orphan = File::create(&lock_path).unwrap();
        let orphan_lock = Flock::lock(orphan, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, errno)| errno)
            .unwrap();
        std::fs::remove_file(&lock_path).unwrap();

        let _held = acquire_mcp_instance_lock_in(&root, project)
            .expect("an orphaned (unlinked) holder must not block acquisition");
        drop(orphan_lock);
    }
}
