use std::collections::HashSet;
use std::path::Path;

use clap::Args;

use crate::cli::output::{formatter_for, StopResultInfo, StopStatus};
use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct StopArgs {
    /// Project root to stop (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,

    /// Print stable plain text output for simple scripts
    #[arg(long, conflicts_with = "format")]
    pub plain: bool,

    /// Output format override (defaults to human)
    #[arg(long, short = 'f', hide = true, conflicts_with = "plain")]
    pub format: Option<OutputFormat>,
}

pub async fn exec(args: StopArgs, format: OutputFormat) -> anyhow::Result<()> {
    let path = Path::new(&args.path);
    let resolved_result = crate::shared::project::resolve_project_root(path);
    let (project_root, worktree_context) = match resolved_result {
        Ok(resolved) => (resolved.state_root, Some(resolved.worktree_context)),
        Err(_) => {
            // Fallback: path doesn't exist or can't be resolved. Try registry-keyed deregister.
            match registry_deregister_fallback(path)? {
                Some((root, success)) => {
                    if success {
                        let fmt = formatter_for(format);
                        let result = StopResultInfo {
                            status: StopStatus::Stopped,
                            project_root: root.clone(),
                            registered: false,
                            daemon_running: false,
                            pid: None,
                            message: format!(
                                "Project at {} was deleted or inaccessible. Deregistered via fallback.",
                                root.display()
                            ),
                        };
                        println!("{}", fmt.format_stop_result(&result));
                        return Ok(());
                    }
                    (root, None)
                }
                None => {
                    // Could not resolve and no registry entry found
                    return Err(anyhow::anyhow!(
                        "Could not resolve project at {} or find it in registry",
                        path.display()
                    ));
                }
            }
        }
    };

    let fmt = formatter_for(format);

    // If we have a context, deregister it normally. Otherwise use fallback.
    let worktree_context = match worktree_context {
        Some(ctx) => ctx,
        None => {
            // This is the fallback path where we found a registry entry but couldn't resolve it
            // We already deregistered above, so we're done
            return Ok(());
        }
    };

    if !lifecycle::supports_daemon() {
        let result = StopResultInfo {
            status: StopStatus::Unsupported,
            project_root,
            registered: false,
            daemon_running: false,
            pid: None,
            message: "Background daemon workflows are not supported on this platform.".to_string(),
        };
        println!("{}", fmt.format_stop_result(&result));
        return Ok(());
    }

    let daemon_pid = lifecycle::is_daemon_running()?;

    let mut registry = Registry::load()?;
    let was_registered = registry.deregister_context(&worktree_context)?;

    if !was_registered {
        let message = match daemon_pid {
            Some(pid) => format!(
                "Project at {} was not registered. Daemon (pid={pid}) left running.",
                project_root.display()
            ),
            None => format!(
                "Project at {} was not registered and no daemon is currently running.",
                project_root.display()
            ),
        };
        let result = StopResultInfo {
            status: StopStatus::NotRegistered,
            project_root,
            registered: false,
            daemon_running: daemon_pid.is_some(),
            pid: daemon_pid,
            message,
        };
        println!("{}", fmt.format_stop_result(&result));
        return Ok(());
    }

    let Some(pid) = daemon_pid else {
        let result = StopResultInfo {
            status: StopStatus::DaemonNotRunning,
            project_root,
            registered: false,
            daemon_running: false,
            pid: None,
            message: "Project deregistered. No daemon is currently running.".to_string(),
        };
        println!("{}", fmt.format_stop_result(&result));
        return Ok(());
    };

    let daemon_running_after = if registry.is_empty() {
        lifecycle::send_sigterm(pid)?;
        false
    } else {
        lifecycle::send_sighup(pid)?;
        true
    };
    let message = if daemon_running_after {
        format!(
            "Project deregistered. Daemon (pid={pid}) notified to stop watching {}.",
            project_root.display()
        )
    } else {
        format!("Project deregistered. No projects remaining; daemon (pid={pid}) stopped.")
    };
    let result = StopResultInfo {
        status: StopStatus::Stopped,
        project_root,
        registered: false,
        daemon_running: daemon_running_after,
        pid: Some(pid),
        message,
    };
    println!("{}", fmt.format_stop_result(&result));

    Ok(())
}

/// Fallback deregister when a project path no longer exists on disk.
/// Searches the registry for a matching entry and deregisters by context_id.
/// Returns (project_root, success_flag) if an entry was found and deregistered.
fn registry_deregister_fallback(
    deleted_path: &Path,
) -> anyhow::Result<Option<(std::path::PathBuf, bool)>> {
    let mut registry = Registry::load()?;

    // Try to find a matching registry entry. First attempt exact canonicalization of the input.
    if let Ok(canonical) = deleted_path.canonicalize() {
        if let Some(entry) = registry
            .projects
            .iter()
            .find(|e| e.project_root == canonical)
        {
            let context_id = entry.context_id();
            let project_root = entry.project_root.clone();
            let mut context_ids = HashSet::new();
            context_ids.insert(context_id);
            registry.deregister_context_ids(&context_ids)?;
            return Ok(Some((project_root, true)));
        }
    }

    // If canonicalization failed, try string matching against the input path.
    // This handles the case where the directory was deleted.
    let input_str = deleted_path.to_string_lossy();
    if let Some(entry) = registry
        .projects
        .iter()
        .find(|e| e.project_root.to_string_lossy() == input_str)
    {
        let context_id = entry.context_id();
        let project_root = entry.project_root.clone();
        let mut context_ids = HashSet::new();
        context_ids.insert(context_id);
        registry.deregister_context_ids(&context_ids)?;
        return Ok(Some((project_root, true)));
    }

    Ok(None)
}
