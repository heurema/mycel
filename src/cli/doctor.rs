use anyhow::Result;
use nostr_sdk::ToBech32;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::{config, crypto, store};

pub async fn run() -> Result<()> {
    let cfg_dir = config::config_dir()?;
    let data_dir = config::data_dir()?;
    let cfg = config::load()?;

    let mut issues = 0u32;

    issues += check_key(&cfg_dir, &cfg);
    issues += check_database(&data_dir).await;
    issues += check_config(&cfg_dir, &cfg);
    issues += check_relays(&cfg).await;

    println!();
    if issues == 0 {
        println!("All checks passed.");
    } else {
        println!("{issues} issue(s) found.");
    }

    Ok(())
}

fn check_key(cfg_dir: &std::path::Path, cfg: &config::Config) -> u32 {
    let enc_path = cfg_dir.join("key.enc");
    print!("Key:      ");
    if crypto::is_initialized(&enc_path) {
        match crypto::load_keys(&enc_path, cfg.identity.storage) {
            Ok(keys) => {
                let npub = keys.public_key().to_bech32().unwrap_or_default();
                if npub.len() >= 16 {
                    println!("OK ({}...{})", &npub[..12], &npub[npub.len() - 4..]);
                } else {
                    println!("OK ({npub})");
                }
                0
            }
            Err(e) => {
                println!("ERROR — cannot unlock: {e}");
                1
            }
        }
    } else {
        println!("NOT FOUND — run `mycel init`");
        1
    }
}

async fn check_database(data_dir: &std::path::Path) -> u32 {
    let db_path = data_dir.join("mycel.db");
    print!("Database: ");
    if db_path.exists() {
        let db_path_owned = db_path.to_path_buf();
        match store::Db::open(&db_path_owned) {
            Ok(db) => {
                match db
                    .run(|conn| {
                        let count: i64 = conn
                            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
                            .unwrap_or(0);
                        let contacts: i64 = conn
                            .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))
                            .unwrap_or(0);
                        Ok((count, contacts))
                    })
                    .await
                {
                    Ok((count, contacts)) => {
                        println!("OK ({count} messages, {contacts} contacts)");
                        0
                    }
                    Err(e) => {
                        println!("ERROR — {e}");
                        1
                    }
                }
            }
            Err(e) => {
                println!("ERROR — {e}");
                1
            }
        }
    } else {
        println!("NOT FOUND — run `mycel init`");
        1
    }
}

fn check_config(cfg_dir: &std::path::Path, cfg: &config::Config) -> u32 {
    let config_path = cfg_dir.join("config.toml");
    print!("Config:   ");
    if config_path.exists() {
        println!(
            "OK ({} relays configured, storage: {})",
            cfg.relays.urls.len(),
            cfg.identity.storage
        );
    } else {
        println!("NOT FOUND (using defaults)");
    }
    0
}

async fn check_relays(cfg: &config::Config) -> u32 {
    let relay_timeout = Duration::from_secs(cfg.relays.timeout_secs);
    println!("Relays:");
    let mut reachable = 0u32;
    for url in &cfg.relays.urls {
        print!("  {url:<40} ");
        if check_relay_tcp(url, relay_timeout).await {
            println!("reachable");
            reachable += 1;
        } else {
            println!("unreachable");
        }
    }
    let total = cfg.relays.urls.len() as u32;
    if reachable == 0 && total > 0 {
        println!("Warning: no relays reachable. Check your network.");
        1
    } else {
        println!("{reachable}/{total} relays reachable.");
        0
    }
}

async fn check_relay_tcp(url: &str, connect_timeout: Duration) -> bool {
    let Some((host, port)) = extract_host_port(url) else {
        return false;
    };
    let addr = format!("{host}:{port}");
    matches!(
        timeout(connect_timeout, TcpStream::connect(&addr)).await,
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

impl std::fmt::Display for config::IdentityStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            config::IdentityStorage::Keychain => f.write_str("keychain"),
            config::IdentityStorage::File => f.write_str("file"),
        }
    }
}
