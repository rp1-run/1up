use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
                        // H5: the entry is deregistered, but the daemon may still
                        // be alive and watching the now-deleted root. Notify it and
                        // report the true daemon state instead of hardcoding
                        // `daemon: false` / no pid.
                        return finish_stop_after_fallback(root, format);
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

/// Notify a still-running daemon and report the true daemon state after a fallback
/// deregister (the project path was deleted). The deleted-path path must not
/// silently claim `daemon: false` while the worker keeps running and watching the
/// gone root: probe the daemon and, mirroring the normal path, `SIGTERM` it when no
/// projects remain or `SIGHUP` it otherwise.
fn finish_stop_after_fallback(project_root: PathBuf, format: OutputFormat) -> anyhow::Result<()> {
    let fmt = formatter_for(format);

    if !lifecycle::supports_daemon() {
        let message = format!(
            "Project at {} was deleted or inaccessible. Deregistered via fallback (daemon unsupported on this platform).",
            project_root.display()
        );
        let result = StopResultInfo {
            status: StopStatus::Stopped,
            project_root,
            registered: false,
            daemon_running: false,
            pid: None,
            message,
        };
        println!("{}", fmt.format_stop_result(&result));
        return Ok(());
    }

    let Some(pid) = lifecycle::is_daemon_running()? else {
        let message = format!(
            "Project at {} was deleted or inaccessible. Deregistered via fallback; no daemon running.",
            project_root.display()
        );
        let result = StopResultInfo {
            status: StopStatus::Stopped,
            project_root,
            registered: false,
            daemon_running: false,
            pid: None,
            message,
        };
        println!("{}", fmt.format_stop_result(&result));
        return Ok(());
    };

    // Reload the registry to decide whether any projects remain: if this was the
    // last one, stop the daemon (SIGTERM); otherwise tell it to re-scan (SIGHUP).
    let registry = Registry::load()?;
    let daemon_running_after = if registry.is_empty() {
        lifecycle::send_sigterm(pid)?;
        false
    } else {
        lifecycle::send_sighup(pid)?;
        true
    };
    let message = if daemon_running_after {
        format!(
            "Project at {} was deleted or inaccessible. Deregistered via fallback; daemon (pid={pid}) notified to stop watching it.",
            project_root.display()
        )
    } else {
        format!(
            "Project at {} was deleted or inaccessible. Deregistered via fallback; no projects remaining, daemon (pid={pid}) stopped.",
            project_root.display()
        )
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

/// Lexically absolutize a path WITHOUT requiring it to exist (it may be deleted),
/// resolving `.`/`..` components so a relative deleted path compares against the
/// stored absolute registry roots. Does not resolve symlinks (impossible for a
/// deleted path).
fn lexical_absolute(path: &Path) -> PathBuf {
    let base = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in base.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Fallback deregister when a project path no longer exists on disk. Searches the
/// registry for an entry matching the requested path by either its state root
/// (`project_root`) or its source root — a deleted *linked worktree* is identified
/// by `source_root`, whose `state_root` (the main repo) still resolves. Relative
/// deleted paths are lexically absolutized first so they compare against the stored
/// absolute roots. Returns `(project_root, success_flag)` if an entry was found and
/// deregistered.
fn registry_deregister_fallback(deleted_path: &Path) -> anyhow::Result<Option<(PathBuf, bool)>> {
    let mut registry = Registry::load()?;

    // `canonicalize` still succeeds if the path exists but `resolve_project_root`
    // failed for another reason; fall back to a lexical absolutization and the raw
    // input for a genuinely deleted path.
    let canonical = deleted_path.canonicalize().ok();
    let requested_abs = lexical_absolute(deleted_path);
    let requested_str = deleted_path.to_string_lossy();

    let matched = registry.projects.iter().find(|entry| {
        let state = entry.project_root.as_path();
        let source = entry.source_root();
        if let Some(canonical) = canonical.as_deref() {
            if state == canonical || source == canonical {
                return true;
            }
        }
        state == requested_abs
            || source == requested_abs
            || state.to_string_lossy() == requested_str
            || source.to_string_lossy() == requested_str
    });

    if let Some(entry) = matched {
        let project_root = entry.project_root.clone();
        let context_id = entry.context_id();
        let mut context_ids = HashSet::new();
        context_ids.insert(context_id);
        registry.deregister_context_ids(&context_ids)?;
        return Ok(Some((project_root, true)));
    }

    Ok(None)
}

#[cfg(all(test, unix))]
mod tests {
    use super::lexical_absolute;
    use std::path::Path;

    #[test]
    fn lexical_absolute_normalizes_without_touching_disk() {
        // `..`/`.` are resolved lexically so a deleted path still normalizes.
        assert_eq!(lexical_absolute(Path::new("/a/b/../c")), Path::new("/a/c"));
        assert_eq!(lexical_absolute(Path::new("/a/./b/")), Path::new("/a/b"));
        assert_eq!(lexical_absolute(Path::new("/a/b/..")), Path::new("/a"));
        // A relative path is made absolute against the current directory.
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(lexical_absolute(Path::new("sub/dir")), cwd.join("sub/dir"));
    }
}
