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
    let resolved = crate::shared::project::resolve_project_root(std::path::Path::new(&args.path))?;
    let project_root = resolved.state_root;
    let worktree_context = resolved.worktree_context;
    let fmt = formatter_for(format);

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
