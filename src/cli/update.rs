use anyhow::bail;
use chrono::Utc;
use clap::Args;

use crate::cli::output::{formatter_for, UpdateResult, UpdateStatusInfo};
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
    let info = build_status_info_from_cache(&cache, &status, None);
    let fmt = formatter_for(format);
    println!("{}", fmt.format_update_status(&info));
    Ok(())
}

/// `1up update --status`: display cached update information.
async fn exec_status(format: OutputFormat) -> anyhow::Result<()> {
    let cache = read_update_cache();
    let info = match cache {
        Some(ref c) => {
            let status = build_update_status(c);
            let cache_age_secs = Utc::now()
                .signed_duration_since(c.checked_at)
                .num_seconds()
                .max(0);
            build_status_info_from_cache(c, &status, Some(cache_age_secs))
        }
        None => UpdateStatusInfo {
            current_version: VERSION.to_string(),
            cached: false,
            latest_version: None,
            update_available: false,
            status: UpdateStatus::UpToDate,
            install_channel: None,
            checked_at: None,
            cache_age_secs: None,
            yanked: false,
            minimum_safe_version: None,
            message: None,
            notes_url: None,
            upgrade_instruction: None,
        },
    };
    let fmt = formatter_for(format);
    println!("{}", fmt.format_update_status(&info));
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
    let fmt = formatter_for(format);

    if status == UpdateStatus::UpToDate {
        let result = UpdateResult::UpToDate {
            current_version: VERSION.to_string(),
            latest_version: cache.latest_version.clone(),
        };
        println!("{}", fmt.format_update_result(&result));
        return Ok(());
    }

    let channel = detect_install_channel();

    match channel {
        InstallChannel::Homebrew | InstallChannel::Scoop => {
            let result = UpdateResult::ChannelManaged {
                current_version: VERSION.to_string(),
                latest_version: cache.latest_version.clone(),
                install_channel: channel,
                upgrade_instruction: cache.upgrade_instruction.clone(),
                status: status.clone(),
                message: cache.message.clone(),
            };
            println!("{}", fmt.format_update_result(&result));
            return Ok(());
        }
        InstallChannel::Unknown => {
            eprintln!(
                "Could not detect how 1up was installed. Self-update is not available.\n\
                 Check https://github.com/rp1-run/1up/releases for manual update instructions."
            );
            return Ok(());
        }
        InstallChannel::Manual => {}
    }

    stop_daemon_for_update()?;

    let client = build_update_check_client()
        .map_err(|e| anyhow::anyhow!("failed to create HTTP client: {e}"))?;
    let manifest = fetch_update_manifest(&client)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let self_update_result = self_update(&manifest)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let result = UpdateResult::Updated {
        old_version: self_update_result.old_version,
        new_version: self_update_result.new_version,
    };
    println!("{}", fmt.format_update_result(&result));
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

// --- Helpers ---

fn build_status_info_from_cache(
    cache: &UpdateCheckCache,
    status: &UpdateStatus,
    cache_age_secs: Option<i64>,
) -> UpdateStatusInfo {
    UpdateStatusInfo {
        current_version: VERSION.to_string(),
        cached: true,
        latest_version: Some(cache.latest_version.clone()),
        update_available: !matches!(status, UpdateStatus::UpToDate),
        status: status.clone(),
        install_channel: Some(cache.install_channel),
        checked_at: if cache_age_secs.is_some() {
            Some(cache.checked_at)
        } else {
            None
        },
        cache_age_secs,
        yanked: cache.yanked,
        minimum_safe_version: cache.minimum_safe_version.clone(),
        message: cache.message.clone(),
        notes_url: cache.notes_url.clone(),
        upgrade_instruction: Some(cache.upgrade_instruction.clone()),
    }
}
