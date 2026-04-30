use std::path::Path;

use clap::Args;

use crate::daemon::lifecycle;
#[cfg(unix)]
use crate::shared::constants::SECURE_STATE_FILE_MODE;
#[cfg(unix)]
use crate::shared::fs::{ensure_secure_xdg_root, validate_regular_file_path};
use crate::shared::project;
#[cfg(unix)]
use nix::errno::Errno;
#[cfg(unix)]
use nix::fcntl::{Flock, FlockArg};
#[cfg(unix)]
use sha2::{Digest, Sha256};
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
    let resolved = project::resolve_project_root(Path::new(&args.path))?;
    let _instance_lock = acquire_mcp_instance_lock(&resolved.state_root)?;
    ensure_daemon_for_mcp(&resolved.state_root, &resolved.source_root);
    crate::mcp::server::serve_stdio(resolved.state_root, resolved.source_root).await
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
    let lock_path = mcp_lock_path(&xdg_root, project_root);
    let validated_path = validate_regular_file_path(&lock_path, &xdg_root)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(SECURE_STATE_FILE_MODE)
        .open(&validated_path)?;

    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(lock) => Ok(McpInstanceLock { _lock: lock }),
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

#[cfg(not(unix))]
fn acquire_mcp_instance_lock(_project_root: &Path) -> anyhow::Result<McpInstanceLock> {
    Ok(McpInstanceLock)
}

#[cfg(unix)]
fn mcp_lock_path(xdg_root: &Path, project_root: &Path) -> PathBuf {
    xdg_root.join(format!("mcp-{}.lock", mcp_lock_key(project_root)))
}

#[cfg(unix)]
fn mcp_lock_key(project_root: &Path) -> String {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
