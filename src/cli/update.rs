use anyhow::bail;
use chrono::Utc;
use clap::Args;
use colored::Colorize;

use crate::daemon::lifecycle;
use crate::shared::reminder::VERSION;
use crate::shared::types::OutputFormat;
use crate::shared::update::{
    build_cache_from_manifest, build_update_check_client, build_update_status,
    detect_install_channel, fetch_update_manifest, read_update_cache, refresh_cache_if_stale,
    self_update, write_update_cache, InstallChannel, UpdateCheckCache, UpdateStatus,
};

#[derive(Args)]
pub struct UpdateArgs {
    /// Force a fresh update check against the remote manifest
    #[arg(long)]
    pub check: bool,

    /// Display cached update status
    #[arg(long)]
    pub status: bool,
}

pub async fn exec(args: UpdateArgs, format: OutputFormat) -> anyhow::Result<()> {
    if args.check {
        return exec_check(format).await;
    }

    if args.status {
        return exec_status(format).await;
    }

    exec_update(format).await
}

/// `1up update --check`: force a fresh manifest fetch, update cache, display result.
async fn exec_check(format: OutputFormat) -> anyhow::Result<()> {
    let client = build_update_check_client()
        .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {e}"))?;

    let manifest = fetch_update_manifest(&client)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let cache = build_cache_from_manifest(&manifest);
    write_update_cache(&cache);

    let status = build_update_status(&cache);
    println!("{}", render_check_output(format, &cache, &status));
    Ok(())
}

/// `1up update --status`: display cached update information.
async fn exec_status(format: OutputFormat) -> anyhow::Result<()> {
    let cache = read_update_cache();
    println!("{}", render_status_output(format, cache.as_ref()));
    Ok(())
}

/// `1up update` (no flags): refresh if stale, then apply update or print channel instruction.
async fn exec_update(format: OutputFormat) -> anyhow::Result<()> {
    let cache = refresh_cache_if_stale().await;

    let cache = match cache {
        Some(c) => c,
        None => {
            bail!("Unable to check for updates. Verify network connectivity and retry with `1up update --check`.");
        }
    };

    let status = build_update_status(&cache);

    if status == UpdateStatus::UpToDate {
        println!("{}", render_up_to_date(format, &cache));
        return Ok(());
    }

    let channel = detect_install_channel();

    match channel {
        InstallChannel::Homebrew | InstallChannel::Scoop => {
            println!(
                "{}",
                render_channel_instruction(format, &cache, &status, channel)
            );
            return Ok(());
        }
        InstallChannel::Manual | InstallChannel::Unknown => {}
    }

    stop_daemon_for_update()?;

    let client = build_update_check_client()
        .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {e}"))?;
    let manifest = fetch_update_manifest(&client)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let result = self_update(&manifest)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!(
        "{}",
        render_update_result(format, &result.old_version, &result.new_version)
    );
    Ok(())
}

/// Stops the daemon before an update action, verifying it has exited.
///
/// Polls `is_process_alive()` with bounded retries after sending SIGTERM.
/// Returns an error if the daemon cannot be stopped.
fn stop_daemon_for_update() -> anyhow::Result<()> {
    if !lifecycle::supports_daemon() {
        return Ok(());
    }

    let pid = match lifecycle::is_daemon_running()? {
        Some(pid) => pid,
        None => return Ok(()),
    };

    eprintln!("Stopping daemon (pid={pid}) before update...");
    lifecycle::send_sigterm(pid)?;

    let max_attempts = 30;
    let poll_interval = std::time::Duration::from_millis(100);

    for _ in 0..max_attempts {
        if !lifecycle::is_process_alive(pid) {
            eprintln!("Daemon stopped.");
            return Ok(());
        }
        std::thread::sleep(poll_interval);
    }

    bail!(
        "Daemon (pid={pid}) did not exit after {}ms. Update aborted. \
         Try `1up stop` manually, then retry `1up update`.",
        max_attempts * 100
    );
}

// --- Output rendering helpers ---

fn render_check_output(
    format: OutputFormat,
    cache: &UpdateCheckCache,
    status: &UpdateStatus,
) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "current_version": VERSION,
            "latest_version": &cache.latest_version,
            "update_available": !matches!(status, UpdateStatus::UpToDate),
            "install_channel": &cache.install_channel,
            "yanked": cache.yanked,
            "minimum_safe_version": &cache.minimum_safe_version,
            "message": &cache.message,
            "notes_url": &cache.notes_url,
            "upgrade_instruction": &cache.upgrade_instruction,
        }))
        .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
        OutputFormat::Human => {
            let mut out = String::new();
            out.push_str(&format!("Current version: {}\n", VERSION.bold()));
            out.push_str(&format!(
                "Latest version:  {}\n",
                cache.latest_version.bold()
            ));
            match status {
                UpdateStatus::UpToDate => {
                    out.push_str(&format!("Status: {}\n", "up to date".green()));
                }
                UpdateStatus::UpdateAvailable { latest } => {
                    out.push_str(&format!(
                        "Status: {}\n",
                        format!("update available ({latest})").yellow()
                    ));
                    out.push_str(&format!("Run: {}\n", cache.upgrade_instruction));
                }
                UpdateStatus::Yanked { message, .. } => {
                    out.push_str(&format!(
                        "Status: {}\n",
                        "YANKED -- upgrade immediately".red()
                    ));
                    if let Some(msg) = message {
                        out.push_str(&format!("Message: {msg}\n"));
                    }
                    out.push_str(&format!("Run: {}\n", cache.upgrade_instruction));
                }
                UpdateStatus::BelowMinimumSafe {
                    minimum_safe,
                    message,
                    ..
                } => {
                    out.push_str(&format!(
                        "Status: {}\n",
                        format!("below minimum safe version ({minimum_safe})").red()
                    ));
                    if let Some(msg) = message {
                        out.push_str(&format!("Message: {msg}\n"));
                    }
                    out.push_str(&format!("Run: {}\n", cache.upgrade_instruction));
                }
            }
            out.push_str(&format!("Install source: {}\n", cache.install_channel));
            out
        }
        OutputFormat::Plain => {
            let update_available = !matches!(status, UpdateStatus::UpToDate);
            let status_label = match status {
                UpdateStatus::UpToDate => "up_to_date",
                UpdateStatus::UpdateAvailable { .. } => "update_available",
                UpdateStatus::Yanked { .. } => "yanked",
                UpdateStatus::BelowMinimumSafe { .. } => "below_minimum_safe",
            };
            let mut out = format!(
                "current:{}\tlatest:{}\tstatus:{}\tupdate_available:{}\tchannel:{}\tinstruction:{}",
                VERSION,
                cache.latest_version,
                status_label,
                update_available,
                cache.install_channel,
                cache.upgrade_instruction,
            );
            if cache.yanked {
                out.push_str("\tyanked:true");
            }
            if let Some(ref min_safe) = cache.minimum_safe_version {
                out.push_str(&format!("\tminimum_safe_version:{min_safe}"));
            }
            if let Some(ref msg) = cache.message {
                out.push_str(&format!("\tmessage:{msg}"));
            }
            out.push('\n');
            out
        }
    }
}

fn render_status_output(format: OutputFormat, cache: Option<&UpdateCheckCache>) -> String {
    match cache {
        None => match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&serde_json::json!({
                    "current_version": VERSION,
                    "cached": false,
                    "message": "No cached update information. Run `1up update --check` to check for updates.",
                }))
                .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
            }
            OutputFormat::Human => {
                format!(
                    "Current version: {}\nNo cached update information.\nRun `1up update --check` to check for updates.\n",
                    VERSION.bold()
                )
            }
            OutputFormat::Plain => {
                format!("current:{VERSION}\tcached:false\n")
            }
        },
        Some(cache) => {
            let status = build_update_status(cache);
            let cache_age_secs = Utc::now()
                .signed_duration_since(cache.checked_at)
                .num_seconds()
                .max(0);

            match format {
                OutputFormat::Json => {
                    let update_available = !matches!(status, UpdateStatus::UpToDate);
                    serde_json::to_string_pretty(&serde_json::json!({
                        "current_version": VERSION,
                        "cached": true,
                        "latest_version": &cache.latest_version,
                        "update_available": update_available,
                        "install_channel": &cache.install_channel,
                        "checked_at": cache.checked_at.to_rfc3339(),
                        "cache_age_secs": cache_age_secs,
                        "yanked": cache.yanked,
                        "minimum_safe_version": &cache.minimum_safe_version,
                        "message": &cache.message,
                        "notes_url": &cache.notes_url,
                        "upgrade_instruction": &cache.upgrade_instruction,
                    }))
                    .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
                }
                OutputFormat::Human => {
                    let mut out = String::new();
                    out.push_str(&format!("Current version: {}\n", VERSION.bold()));
                    out.push_str(&format!("Latest version:  {}\n", cache.latest_version.bold()));
                    out.push_str(&format!("Install source:  {}\n", cache.install_channel));
                    out.push_str(&format!(
                        "Last checked:    {} ({})\n",
                        cache.checked_at.to_rfc3339(),
                        render_age(cache_age_secs),
                    ));
                    match &status {
                        UpdateStatus::UpToDate => {
                            out.push_str(&format!("Status:          {}\n", "up to date".green()));
                        }
                        UpdateStatus::UpdateAvailable { latest } => {
                            out.push_str(&format!(
                                "Status:          {}\n",
                                format!("update available ({latest})").yellow()
                            ));
                            out.push_str(&format!("Run: {}\n", cache.upgrade_instruction));
                        }
                        UpdateStatus::Yanked { message, .. } => {
                            out.push_str(&format!(
                                "Status:          {}\n",
                                "YANKED -- upgrade immediately".red()
                            ));
                            if let Some(msg) = message {
                                out.push_str(&format!("Message:         {msg}\n"));
                            }
                            out.push_str(&format!("Run: {}\n", cache.upgrade_instruction));
                        }
                        UpdateStatus::BelowMinimumSafe {
                            minimum_safe,
                            message,
                            ..
                        } => {
                            out.push_str(&format!(
                                "Status:          {}\n",
                                format!("below minimum safe version ({minimum_safe})").red()
                            ));
                            if let Some(msg) = message {
                                out.push_str(&format!("Message:         {msg}\n"));
                            }
                            out.push_str(&format!("Run: {}\n", cache.upgrade_instruction));
                        }
                    }
                    out
                }
                OutputFormat::Plain => {
                    let status_label = match &status {
                        UpdateStatus::UpToDate => "up_to_date",
                        UpdateStatus::UpdateAvailable { .. } => "update_available",
                        UpdateStatus::Yanked { .. } => "yanked",
                        UpdateStatus::BelowMinimumSafe { .. } => "below_minimum_safe",
                    };
                    let update_available = !matches!(status, UpdateStatus::UpToDate);
                    let mut out = format!(
                        "current:{}\tlatest:{}\tstatus:{}\tupdate_available:{}\tchannel:{}\tchecked_at:{}\tcache_age_secs:{}\tinstruction:{}",
                        VERSION,
                        cache.latest_version,
                        status_label,
                        update_available,
                        cache.install_channel,
                        cache.checked_at.to_rfc3339(),
                        cache_age_secs,
                        cache.upgrade_instruction,
                    );
                    if cache.yanked {
                        out.push_str("\tyanked:true");
                    }
                    if let Some(ref min_safe) = cache.minimum_safe_version {
                        out.push_str(&format!("\tminimum_safe_version:{min_safe}"));
                    }
                    if let Some(ref msg) = cache.message {
                        out.push_str(&format!("\tmessage:{msg}"));
                    }
                    out.push('\n');
                    out
                }
            }
        }
    }
}

fn render_up_to_date(format: OutputFormat, cache: &UpdateCheckCache) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "current_version": VERSION,
            "latest_version": &cache.latest_version,
            "update_available": false,
            "message": "Already up to date.",
        }))
        .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
        OutputFormat::Human => {
            format!("Already up to date (version {}).", VERSION)
        }
        OutputFormat::Plain => {
            format!(
                "current:{VERSION}\tlatest:{}\tupdate_available:false\n",
                cache.latest_version
            )
        }
    }
}

fn render_channel_instruction(
    format: OutputFormat,
    cache: &UpdateCheckCache,
    status: &UpdateStatus,
    channel: InstallChannel,
) -> String {
    let instruction = &cache.upgrade_instruction;

    match format {
        OutputFormat::Json => {
            let status_label = match status {
                UpdateStatus::UpToDate => "up_to_date",
                UpdateStatus::UpdateAvailable { .. } => "update_available",
                UpdateStatus::Yanked { .. } => "yanked",
                UpdateStatus::BelowMinimumSafe { .. } => "below_minimum_safe",
            };
            serde_json::to_string_pretty(&serde_json::json!({
                "current_version": VERSION,
                "latest_version": &cache.latest_version,
                "update_available": true,
                "install_channel": channel,
                "managed": true,
                "status": status_label,
                "upgrade_instruction": instruction,
                "message": &cache.message,
            }))
            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
        }
        OutputFormat::Human => {
            let mut out = String::new();
            out.push_str(&format!(
                "Update available: 1up {} (current: {})\n",
                cache.latest_version.bold(),
                VERSION
            ));
            match status {
                UpdateStatus::Yanked { message, .. } => {
                    out.push_str(&format!(
                        "{}\n",
                        "WARNING: this version has been recalled. Upgrade immediately.".red()
                    ));
                    if let Some(msg) = message {
                        out.push_str(&format!("Message: {msg}\n"));
                    }
                }
                UpdateStatus::BelowMinimumSafe {
                    minimum_safe,
                    message,
                    ..
                } => {
                    out.push_str(&format!(
                        "{}\n",
                        format!(
                            "WARNING: current version is below minimum safe version ({minimum_safe}). Upgrade immediately."
                        )
                        .red()
                    ));
                    if let Some(msg) = message {
                        out.push_str(&format!("Message: {msg}\n"));
                    }
                }
                _ => {}
            }
            out.push_str(&format!(
                "1up is managed by {}. Run: {}\n",
                channel, instruction
            ));
            out
        }
        OutputFormat::Plain => {
            let mut out = format!(
                "current:{VERSION}\tlatest:{}\tupdate_available:true\tchannel:{}\tmanaged:true\tinstruction:{}",
                cache.latest_version, channel, instruction
            );
            if let Some(ref msg) = cache.message {
                out.push_str(&format!("\tmessage:{msg}"));
            }
            out.push('\n');
            out
        }
    }
}

fn render_update_result(format: OutputFormat, old_version: &str, new_version: &str) -> String {
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "updated": true,
            "old_version": old_version,
            "new_version": new_version,
            "message": format!("Updated 1up from {old_version} to {new_version}."),
        }))
        .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
        OutputFormat::Human => {
            format!(
                "Updated 1up from {} to {}.",
                old_version,
                new_version.green().bold()
            )
        }
        OutputFormat::Plain => {
            format!("updated:true\told_version:{old_version}\tnew_version:{new_version}\n")
        }
    }
}

fn render_age(secs: i64) -> String {
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}
