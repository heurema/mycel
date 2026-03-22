use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::types::{Direction, TrustTier};
#[cfg(test)]
use crate::types::{DeliveryStatus, ReadStatus};
use crate::{config, crypto, nostr as mycel_nostr, store, sync};

pub async fn run(json: bool, all: bool, local: bool) -> Result<()> {
    let cfg = config::load()?;
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;

    let new_count = if local {
        // --local: read from SQLite only, no relay fetch
        0
    } else {
        // Normal mode: sync from relays first
        let enc_path = config::config_dir()?.join("key.enc");
        let keys = crypto::load_keys(&enc_path, cfg.identity.storage)?;
        let relay_urls = &cfg.relays.urls;
        let timeout = Duration::from_secs(cfg.relays.timeout_secs);

        let client = mycel_nostr::build_client(keys.clone(), relay_urls)
            .await
            .map_err(|e| anyhow::anyhow!("{e} — could not connect to relay; check your network connection"))?;

        let report = sync::sync_once(&keys, &client, &db, relay_urls, timeout)
            .await
            .map_err(|e| anyhow::anyhow!("{e} — relay unreachable during inbox fetch"))?;

        if !json {
            eprintln!("Fetched {} event(s), {} new", report.fetched, report.new_messages);
        }

        client.disconnect().await;
        report.new_messages
    };

    // Display messages from DB
    let trust_filter: Vec<TrustTier> = if all {
        vec![TrustTier::Known, TrustTier::Unknown]
    } else {
        vec![TrustTier::Known]
    };
    let messages = db.run(move |conn| store::get_messages(conn, Direction::In, &trust_filter)).await?;
    display_messages(&messages, json, new_count)?;

    Ok(())
}

fn display_messages(messages: &[store::MessageRow], json: bool, new_count: u64) -> Result<()> {
    if json {
        for msg in messages {
            let npub_from = PublicKey::from_hex(&msg.sender)
                .ok()
                .and_then(|pk| pk.to_bech32().ok())
                .unwrap_or_else(|| msg.sender.clone());
            let line = serde_json::json!({
                "v": 1,
                "nostr_id": msg.nostr_id,
                "from": npub_from,
                "content": msg.content,
                "ts": msg.created_at,
                "status": msg.read_status.to_string(),
            });
            println!("{}", serde_json::to_string(&line)?);
        }
    } else if messages.is_empty() {
        println!("No messages.");
    } else {
        if new_count > 0 {
            println!("{new_count} new message(s).\n");
        }
        for msg in messages {
            let sender_display = sanitize_for_terminal(&sender_label(msg));
            let content = sanitize_for_terminal(&msg.content);
            let ts = sanitize_for_terminal(&msg.created_at);
            println!("[{}] {}: {}", ts, sender_display, content);
        }
    }
    Ok(())
}

fn sender_label(msg: &store::MessageRow) -> String {
    if let Some(ref alias) = msg.sender_alias {
        return alias.clone();
    }
    match PublicKey::from_hex(&msg.sender) {
        Ok(pk) => match pk.to_bech32() {
            Ok(npub) if npub.len() > 16 => format!("{}...{}", &npub[..12], &npub[npub.len() - 4..]),
            Ok(npub) => npub,
            Err(_) => msg.sender[..msg.sender.len().min(12)].to_string(),
        },
        Err(_) => msg.sender[..msg.sender.len().min(12)].to_string(),
    }
}

/// C6: Sanitize content for terminal display.
pub fn sanitize_for_terminal(content: &str) -> String {
    let max = crate::error::MAX_MESSAGE_SIZE;
    let truncated = if content.len() > max {
        let mut end = max;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}[truncated]", &content[..end])
    } else {
        content.to_string()
    };

    let mut result = String::with_capacity(truncated.len());
    let mut chars = truncated.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some(&'[') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() || next == '~' { break; }
                    }
                }
                Some(&']') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next == '\x07' { break; }
                        if next == '\x1b' {
                            if chars.peek() == Some(&'\\') { chars.next(); }
                            break;
                        }
                    }
                }
                Some(&'O') => {
                    chars.next();
                    if let Some(&next) = chars.peek()
                        && next.is_ascii_alphabetic()
                    {
                        chars.next();
                    }
                }
                Some(&'P') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next == '\x1b' {
                            if chars.peek() == Some(&'\\') { chars.next(); }
                            break;
                        }
                        if next == '\x07' { break; }
                    }
                }
                _ => {}
            }
            continue;
        }
        if c.is_control() && c != '\n' {
            continue;
        }
        result.push(c);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terminal_safety() {
        let with_ansi = "hello \x1b[31mred\x1b[0m world";
        assert_eq!(sanitize_for_terminal(with_ansi), "hello red world");

        let with_control = "hello\x07\x08\nworld\x00";
        assert_eq!(sanitize_for_terminal(with_control), "hello\nworld");

        assert_eq!(sanitize_for_terminal("normal"), "normal");
    }

    #[test]
    fn test_terminal_safety_truncation() {
        let big = "x".repeat(9000);
        let result = sanitize_for_terminal(&big);
        assert!(result.ends_with("[truncated]"));
        assert!(result.len() <= 8192 + "[truncated]".len());
    }

    #[test]
    fn test_inbox_json_format() {
        let line = serde_json::json!({
            "v": 1,
            "nostr_id": "abc123",
            "from": "npub1test...",
            "content": "hello",
            "ts": "2026-03-20T00:00:00Z",
            "status": "unread",
        });
        let serialized = serde_json::to_string(&line).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["v"], 1);
    }

    #[test]
    fn test_inbox_all() {
        let trust_all: Vec<TrustTier> = vec![TrustTier::Known, TrustTier::Unknown];
        assert!(trust_all.contains(&TrustTier::Known));
        assert!(!trust_all.contains(&TrustTier::Blocked));
    }

    #[test]
    fn test_blocked_sender_dropped() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();

        let contact = store::ContactRow {
            pubkey: "blocked_sender".to_string(),
            alias: None,
            trust_tier: TrustTier::Blocked,
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        store::insert_contact(&conn, &contact).unwrap();

        let msg = store::MessageRow {
            nostr_id: "msg_from_blocked".to_string(),
            direction: Direction::In,
            sender: "blocked_sender".to_string(),
            recipient: "me".to_string(),
            content: "you shouldn't see this".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
            sender_alias: None,
        };
        store::insert_message(&conn, &msg).unwrap();

        let known = store::get_messages(&conn, Direction::In, &[TrustTier::Known]).unwrap();
        assert!(known.is_empty());
    }

    #[test]
    fn test_dedup_nostr_id() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();

        let msg = store::MessageRow {
            nostr_id: "dedup_test_id".to_string(),
            direction: Direction::In,
            sender: "sender".to_string(),
            recipient: "recipient".to_string(),
            content: "first".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
            sender_alias: None,
        };

        assert!(store::insert_message(&conn, &msg).unwrap());
        assert!(!store::insert_message(&conn, &msg).unwrap());
        assert_eq!(store::get_messages(&conn, Direction::In, &[]).unwrap().len(), 1);
    }
}
