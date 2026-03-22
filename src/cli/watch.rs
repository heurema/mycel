use anyhow::Result;
use std::path::PathBuf;
use std::time::Duration;

use crate::{config, crypto, nostr as mycel_nostr, store, sync};

const DEFAULT_INTERVAL: u64 = 30;
const MAX_BACKOFF: u64 = 300; // 5 minutes

pub async fn run(interval: Option<u64>) -> Result<()> {
    let poll_secs = interval.unwrap_or(DEFAULT_INTERVAL);

    // 1. Load config + unlock key once
    let cfg = config::load()?;
    let enc_path = config::config_dir()?.join("key.enc");
    let keys = crypto::load_keys(&enc_path, cfg.identity.storage)?;
    let relay_urls = cfg.relays.urls;
    let timeout = Duration::from_secs(cfg.relays.timeout_secs);

    // 2. Acquire singleton lock
    let state_dir = config::data_dir()?;
    let lock_path = state_dir.join("watch.lock");
    let state_path = state_dir.join("watch.state.json");
    acquire_lock(&lock_path)?;

    // 3. Open DB once
    let db = store::Db::open(&state_dir.join("mycel.db"))?;

    // 4. Build persistent client (connect once)
    eprintln!("Connecting to {} relay(s)...", relay_urls.len());
    let client = mycel_nostr::build_client(keys.clone(), &relay_urls)
        .await
        .map_err(|e| anyhow::anyhow!("{e} — could not connect to relay"))?;

    eprintln!("Watching inbox (poll every {poll_secs}s, Ctrl+C to stop)");

    // 5. Setup graceful shutdown
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    let mut consecutive_errors: u32 = 0;
    let mut total_received: u64 = 0;
    let started_at = crate::envelope::now_iso8601();

    // 6. Initial sync
    match sync::sync_once(&keys, &client, &db, &relay_urls, timeout).await {
        Ok(report) => {
            total_received += report.new_messages;
            consecutive_errors = 0;
            if report.new_messages > 0 {
                notify_new(report.new_messages);
            }
            eprintln!("Initial sync: {} event(s), {} new", report.fetched, report.new_messages);
        }
        Err(e) => {
            eprintln!("Initial sync error: {e}");
            consecutive_errors += 1;
        }
    }

    write_state(&state_path, &started_at, total_received, poll_secs, None);

    // 7. Poll loop
    loop {
        let sleep_secs = if consecutive_errors > 0 {
            let backoff = poll_secs * 2u64.pow(consecutive_errors.min(6));
            backoff.min(MAX_BACKOFF)
        } else {
            poll_secs
        };

        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
            _ = &mut shutdown => {
                eprintln!("\nShutting down...");
                break;
            }
        }

        match sync::sync_once(&keys, &client, &db, &relay_urls, timeout).await {
            Ok(report) => {
                total_received += report.new_messages;
                consecutive_errors = 0;
                if report.new_messages > 0 {
                    notify_new(report.new_messages);
                }
                write_state(&state_path, &started_at, total_received, poll_secs, None);
            }
            Err(e) => {
                consecutive_errors += 1;
                let err_msg = format!("{e}");
                tracing::warn!("sync error (attempt {consecutive_errors}): {err_msg}");
                if consecutive_errors <= 3 {
                    eprintln!("Sync error: {err_msg} (retrying in {sleep_secs}s)");
                }
                write_state(&state_path, &started_at, total_received, poll_secs, Some(&err_msg));
            }
        }
    }

    // 8. Cleanup
    client.disconnect().await;
    let _ = std::fs::remove_file(&lock_path);
    let _ = std::fs::remove_file(&state_path);

    Ok(())
}

fn notify_new(count: u64) {
    // Terminal bell + summary
    eprint!("\x07"); // BEL
    eprintln!("{count} new message(s)");
}

fn acquire_lock(lock_path: &PathBuf) -> Result<()> {
    if lock_path.exists() {
        let content = std::fs::read_to_string(lock_path).unwrap_or_default();
        if let Ok(pid) = content.trim().parse::<u32>() {
            // Check if process is alive
            if is_process_alive(pid) {
                anyhow::bail!(
                    "another mycel watch is running (PID {pid}). \
                     Kill it first or remove {}", lock_path.display()
                );
            }
        }
        // Stale lock — remove it
        let _ = std::fs::remove_file(lock_path);
    }
    std::fs::write(lock_path, format!("{}", std::process::id()))?;
    Ok(())
}

fn is_process_alive(pid: u32) -> bool {
    // kill(pid, 0) checks existence without sending a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn write_state(
    path: &PathBuf,
    started_at: &str,
    messages_received: u64,
    poll_interval_secs: u64,
    last_error: Option<&str>,
) {
    let now = crate::envelope::now_iso8601();
    let pid = std::process::id();
    let status = if last_error.is_some() { "error" } else { "ok" };
    let error_field = match last_error {
        Some(e) => format!(",\"last_error\":\"{}\"", e.replace('"', "\\\"").replace('\n', " ")),
        None => String::new(),
    };
    let json = format!(
        "{{\"pid\":{pid},\"status\":\"{status}\",\"started_at\":\"{started_at}\",\
         \"last_poll_at\":\"{now}\",\"messages_received\":{messages_received},\
         \"poll_interval_secs\":{poll_interval_secs}{error_field}}}"
    );
    let _ = std::fs::write(path, json);
}
