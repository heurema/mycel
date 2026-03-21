use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::types::{DeliveryStatus, Direction, ReadStatus, TrustTier};
use crate::{config, crypto, envelope, error::SYNC_OVERLAP_SECS, nostr as mycel_nostr, store};

pub async fn run(json: bool, all: bool) -> Result<()> {
    // 1. Load config and keys
    let cfg = config::load()?;
    let enc_path = config::config_dir()?.join("key.enc");
    let keys = crypto::load_keys(&enc_path, cfg.identity.storage)?;
    let my_pubkey = keys.public_key();
    let my_hex = my_pubkey.to_hex();
    let relay_urls = cfg.relays.urls;
    let timeout = Duration::from_secs(cfg.relays.timeout_secs);

    // 2. Open DB and compute sync cursor (blocking)
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;
    let min_cursor = {
        let urls = relay_urls.clone();
        db.run(move |conn| compute_sync_cursor(conn, &urls)).await?
    };
    let since = min_cursor.saturating_sub(SYNC_OVERLAP_SECS);

    // 3. Fetch Gift Wraps from relays (async)
    let client = mycel_nostr::build_client(keys.clone(), &relay_urls)
        .await
        .map_err(|e| anyhow::anyhow!("{e} — could not connect to relay; check your network connection"))?;
    let events = mycel_nostr::fetch_gift_wraps(&client, &relay_urls, &my_pubkey, since, timeout)
        .await
        .map_err(|e| anyhow::anyhow!("{e} — relay unreachable during inbox fetch; check your network connection"))?;

    if !json {
        eprintln!("Fetched {} event(s) from {} relay(s)", events.len(), relay_urls.len());
    }

    // 4. Unwrap Gift Wraps (async — NIP-59 decryption)
    let unwrapped = unwrap_events(&keys, &events).await;

    // 5. Process + store (blocking)
    let new_count = {
        let my_hex = my_hex.clone();
        db.run(move |conn| process_and_store(conn, &my_hex, &unwrapped)).await?
    };

    // 6. Update sync cursors (blocking)
    if !events.is_empty() {
        let max_ts = events.iter().map(|e| e.created_at.as_secs()).max().unwrap_or(0);
        let urls = relay_urls.clone();
        db.run(move |conn| {
            let cursor = max_ts.max(min_cursor);
            for url in &urls {
                store::update_sync_cursor(conn, url, cursor)?;
            }
            Ok(())
        }).await?;
    }

    // 7. Disconnect
    client.disconnect().await;

    // 8. Display messages from DB
    let trust_filter: Vec<TrustTier> = if all {
        vec![TrustTier::Known, TrustTier::Unknown]
    } else {
        vec![TrustTier::Known]
    };
    let messages = db.run(move |conn| store::get_messages(conn, Direction::In, &trust_filter)).await?;
    display_messages(&messages, json, new_count)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn compute_sync_cursor(conn: &rusqlite::Connection, relay_urls: &[String]) -> Result<u64> {
    let mut min_cursor: u64 = u64::MAX;
    for url in relay_urls {
        let cursor = store::get_sync_cursor(conn, url)?;
        min_cursor = min_cursor.min(cursor);
    }
    Ok(if min_cursor == u64::MAX { 0 } else { min_cursor })
}

/// Data extracted from a successfully unwrapped Gift Wrap event.
struct UnwrappedMessage {
    nostr_id: String,
    event_ts: u64,
    sender_hex: String,
    rumor_content: String,
}

async fn unwrap_events(keys: &Keys, events: &[Event]) -> Vec<UnwrappedMessage> {
    let mut result = Vec::new();
    for event in events {
        let Ok(unwrapped) = UnwrappedGift::from_gift_wrap(keys, event).await else {
            continue;
        };
        result.push(UnwrappedMessage {
            nostr_id: event.id.to_hex(),
            event_ts: event.created_at.as_secs(),
            sender_hex: unwrapped.sender.to_hex(),
            rumor_content: unwrapped.rumor.content.clone(),
        });
    }
    result
}

fn process_and_store(
    conn: &rusqlite::Connection,
    my_hex: &str,
    unwrapped: &[UnwrappedMessage],
) -> Result<u64> {
    let mut new_count = 0u64;

    for msg in unwrapped {
        let Some(row) = parse_and_validate(conn, my_hex, msg)? else {
            continue;
        };
        if store::insert_message(conn, &row)? && row.delivery_status != DeliveryStatus::Blocked {
            new_count += 1;
        }
    }

    Ok(new_count)
}

fn parse_and_validate(
    conn: &rusqlite::Connection,
    my_hex: &str,
    msg: &UnwrappedMessage,
) -> Result<Option<store::MessageRow>> {
    // Parse mycel envelope from rumor content
    let env: envelope::Envelope = match serde_json::from_str(&msg.rumor_content) {
        Ok(e) => e,
        Err(_) => return Ok(None), // Not a mycel event
    };

    // Validate envelope version
    if env.v != 1 {
        return Ok(None);
    }

    // Validate sender identity: env.from must match cryptographic sender
    if env.from != msg.sender_hex {
        tracing::warn!(
            "envelope sender mismatch: env.from={} != signed={}",
            &env.from[..env.from.len().min(12)],
            &msg.sender_hex[..msg.sender_hex.len().min(12)]
        );
        return Ok(None);
    }

    // Validate message size
    if env.msg.len() > crate::error::MAX_MESSAGE_SIZE {
        return Ok(None);
    }

    // Check trust tier
    let trust_tier = match store::get_contact_by_pubkey(conn, &msg.sender_hex)? {
        Some(c) => c.trust_tier,
        None => TrustTier::Unknown,
    };

    let delivery_status = if trust_tier == TrustTier::Blocked {
        DeliveryStatus::Blocked
    } else {
        DeliveryStatus::Received
    };

    let read_status = if trust_tier == TrustTier::Blocked {
        ReadStatus::Blocked
    } else {
        ReadStatus::Unread
    };

    let now = crate::envelope::now_iso8601();
    let canonical_ts = crate::envelope::timestamp_to_iso8601(msg.event_ts);

    Ok(Some(store::MessageRow {
        nostr_id: msg.nostr_id.clone(),
        direction: Direction::In,
        sender: msg.sender_hex.clone(),
        recipient: my_hex.to_string(),
        content: env.msg,
        delivery_status,
        read_status,
        created_at: canonical_ts,
        received_at: now,
        sender_alias: None, // populated by JOIN on read
    }))
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
    // Use alias from JOIN if available
    if let Some(ref alias) = msg.sender_alias {
        return alias.clone();
    }
    // Fallback: shorten npub
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
/// Strip ANSI escape sequences and control characters (except \n).
/// Truncate at MAX_MESSAGE_SIZE bytes with [truncated] marker.
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

    // Strip ALL escape sequences: CSI, OSC, DCS, SS3, and bare ESC
    let mut result = String::with_capacity(truncated.len());
    let mut chars = truncated.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                // CSI: ESC [ ... final_byte (letter or ~)
                Some(&'[') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() || next == '~' { break; }
                    }
                }
                // OSC: ESC ] ... (terminated by BEL \x07 or ST = ESC \)
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
                // SS3: ESC O <single char>
                Some(&'O') => {
                    chars.next();
                    if let Some(&next) = chars.peek()
                        && next.is_ascii_alphabetic()
                    {
                        chars.next();
                    }
                }
                // DCS: ESC P ... ST
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
                // Bare ESC or unknown — skip
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
        let cleaned = sanitize_for_terminal(with_ansi);
        assert_eq!(cleaned, "hello red world");

        let with_control = "hello\x07\x08\nworld\x00";
        let cleaned = sanitize_for_terminal(with_control);
        assert_eq!(cleaned, "hello\nworld");

        let normal = "just a normal message";
        assert_eq!(sanitize_for_terminal(normal), normal);
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
        assert_eq!(parsed["nostr_id"], "abc123");
    }

    #[test]
    fn test_inbox_all() {
        let trust_all: Vec<TrustTier> = vec![TrustTier::Known, TrustTier::Unknown];
        assert!(trust_all.contains(&TrustTier::Known));
        assert!(trust_all.contains(&TrustTier::Unknown));
        assert!(!trust_all.contains(&TrustTier::Blocked));

        let trust_default: Vec<TrustTier> = vec![TrustTier::Known];
        assert!(trust_default.contains(&TrustTier::Known));
        assert!(!trust_default.contains(&TrustTier::Unknown));
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

        let all = store::get_messages(&conn, Direction::In, &[TrustTier::Known, TrustTier::Unknown]).unwrap();
        assert!(all.is_empty());
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

        let first = store::insert_message(&conn, &msg).unwrap();
        assert!(first, "first insert should succeed");

        let dup = store::insert_message(&conn, &msg).unwrap();
        assert!(!dup, "duplicate should be silently ignored");

        let all = store::get_messages(&conn, Direction::In, &[]).unwrap();
        assert_eq!(all.len(), 1, "only one message should exist");
    }

    #[test]
    fn test_empty_message_rejected() {
        assert!(crate::envelope::validate_message_size("").is_ok());
    }
}
