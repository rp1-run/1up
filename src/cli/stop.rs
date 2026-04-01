use clap::Args;

use crate::cli::output::formatter_for;
use crate::daemon::lifecycle;
use crate::daemon::registry::Registry;
use crate::shared::types::OutputFormat;

#[derive(Args)]
pub struct StopArgs {
    /// Project root directory (defaults to current directory)
    #[arg(default_value = ".")]
    pub path: String,
}

pub async fn exec(args: StopArgs, format: OutputFormat) -> anyhow::Result<()> {
    let project_root = std::path::Path::new(&args.path).canonicalize()?;
    let fmt = formatter_for(format);

    let daemon_pid = match lifecycle::is_daemon_running()? {
        Some(pid) => pid,
        None => {
            let msg = "No daemon is currently running.";
            println!("{}", fmt.format_message(msg));
            return Ok(());
        }
    };

    let mut registry = Registry::load()?;
    let was_registered = registry.deregister(&project_root)?;

    if !was_registered {
        let msg = format!(
            "Project at {} was not registered. Daemon (pid={daemon_pid}) left running.",
            project_root.display()
        );
        println!("{}", fmt.format_message(&msg));
        return Ok(());
    }

    if registry.is_empty() {
        lifecycle::send_sigterm(daemon_pid)?;
        let msg = format!(
            "Project deregistered. No projects remaining -- daemon (pid={daemon_pid}) stopped."
        );
        println!("{}", fmt.format_message(&msg));
    } else {
        lifecycle::send_sighup(daemon_pid)?;
        let msg = format!(
            "Project deregistered. Daemon (pid={daemon_pid}) notified to stop watching {}.",
            project_root.display()
        );
        println!("{}", fmt.format_message(&msg));
    }

    Ok(())
}
