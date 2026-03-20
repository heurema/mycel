use anyhow::Result;
use nostr_sdk::ToBech32;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::{config, crypto, store};

pub async fn run() -> Result<()> {
    let cfg_dir = config::config_dir()?;
    let data_dir = config::data_dir()?;
    let enc_path = cfg_dir.join("key.enc");
    let db_path = data_dir.join("mycel.db");
    let cfg = config::load()?;

    let mut issues = 0u32;

    // 1. Key status
    print!("Key:      ");
    if crypto::is_initialized(&enc_path) {
        match crypto::load_keys(&enc_path) {
            Ok(keys) => {
                let npub = keys.public_key().to_bech32().unwrap_or_default();
                println!("OK ({}...{})", &npub[..12], &npub[npub.len() - 4..]);
            }
            Err(e) => {
                println!("ERROR — cannot unlock: {e}");
                issues += 1;
            }
        }
    } else {
        println!("NOT FOUND — run `mycel init`");
        issues += 1;
    }

    // 2. Database status
    print!("Database: ");
    if db_path.exists() {
        match store::open(&db_path) {
            Ok(conn) => {
                let count: i64 = conn
                    .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
                    .unwrap_or(0);
                let contacts: i64 = conn
                    .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))
                    .unwrap_or(0);
                println!("OK ({count} messages, {contacts} contacts)");
            }
            Err(e) => {
                println!("ERROR — {e}");
                issues += 1;
            }
        }
    } else {
        println!("NOT FOUND — run `mycel init`");
        issues += 1;
    }

    // 3. Config
    print!("Config:   ");
    let config_path = cfg_dir.join("config.toml");
    if config_path.exists() {
        println!("OK ({} relays configured)", cfg.relays.urls.len());
    } else {
        println!("NOT FOUND (using defaults)");
    }

    // 4. Relay connectivity
    println!("Relays:");
    let mut reachable = 0u32;
    for url in &cfg.relays.urls {
        print!("  {url:<40} ");
        if check_relay(url).await {
            println!("reachable");
            reachable += 1;
        } else {
            println!("unreachable");
        }
    }
    let total = cfg.relays.urls.len() as u32;
    if reachable == 0 && total > 0 {
        println!("Warning: no relays reachable. Check your network.");
        issues += 1;
    } else {
        println!("{reachable}/{total} relays reachable.");
    }

    // Summary
    println!();
    if issues == 0 {
        println!("All checks passed.");
    } else {
        println!("{issues} issue(s) found.");
    }

    Ok(())
}

async fn check_relay(url: &str) -> bool {
    let Some((host, port)) = extract_host_port(url) else {
        return false;
    };
    let addr = format!("{host}:{port}");
    matches!(
        timeout(Duration::from_secs(5), TcpStream::connect(&addr)).await,
        Ok(Ok(_))
    )
}

fn extract_host_port(url: &str) -> Option<(String, u16)> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))?;
    let host_part = rest.split('/').next()?;
    if let Some(pos) = host_part.rfind(':') {
        let host = &host_part[..pos];
        let port: u16 = host_part[pos + 1..].parse().ok()?;
        Some((host.to_string(), port))
    } else {
        let port = if url.starts_with("wss://") { 443 } else { 80 };
        Some((host_part.to_string(), port))
    }
}
