use anyhow::Result;

use crate::config;

pub fn run(json: bool) -> Result<()> {
    let state_path = config::data_dir()?.join("watch.state.json");
    let lock_path = config::data_dir()?.join("watch.lock");

    if !state_path.exists() && !lock_path.exists() {
        if json {
            println!("{{\"status\":\"not_running\"}}");
        } else {
            println!("Watch: not running");
        }
        return Ok(());
    }

    let pid_alive = if lock_path.exists() {
        let content = std::fs::read_to_string(&lock_path).unwrap_or_default();
        content.trim().parse::<u32>()
            .map(is_process_alive)
            .unwrap_or(false)
    } else {
        false
    };

    if json {
        if pid_alive && state_path.exists() {
            let state = std::fs::read_to_string(&state_path).unwrap_or_default();
            println!("{state}");
        } else {
            println!("{{\"status\":\"dead\"}}");
        }
    } else if pid_alive && state_path.exists() {
        let state = std::fs::read_to_string(&state_path).unwrap_or_default();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&state) {
            let pid = v["pid"].as_u64().unwrap_or(0);
            let status = v["status"].as_str().unwrap_or("unknown");
            let last_poll = v["last_poll_at"].as_str().unwrap_or("-");
            let received = v["messages_received"].as_u64().unwrap_or(0);
            let interval = v["poll_interval_secs"].as_u64().unwrap_or(0);

            println!("Watch:    running (PID {pid})");
            println!("Status:   {status}");
            println!("Last poll: {last_poll}");
            println!("Received: {received} message(s)");
            println!("Interval: {interval}s");

            if let Some(err) = v["last_error"].as_str() {
                println!("Error:    {err}");
            }
        } else {
            println!("Watch: running (state file unreadable)");
        }
    } else {
        println!("Watch: dead (stale lock file)");
        println!("Clean up: rm {}", lock_path.display());
    }

    Ok(())
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    false
}
