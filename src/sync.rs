// Shared sync core: fetch from relays → decrypt → store in SQLite.
// Used by both `inbox` (one-shot) and `watch` (loop).

use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::types::{DeliveryStatus, Direction, MessageMeta, Part, ReadStatus, TrustTier};
use crate::{envelope, error::{MAX_EVENTS_PER_SYNC, SYNC_OVERLAP_SECS}, nostr as mycel_nostr, store};

/// Result of a single sync cycle.
#[derive(Debug, Clone)]
pub struct SyncReport {
    pub fetched: usize,
    pub new_messages: u64,
}

/// Run one sync cycle: fetch from relays → decrypt → store.
/// The caller owns the Client (may be persistent or ephemeral).
pub async fn sync_once(
    keys: &Keys,
    client: &Client,
    db: &store::Db,
    relay_urls: &[String],
    timeout: Duration,
) -> Result<SyncReport> {
    let my_pubkey = keys.public_key();
    let my_hex = my_pubkey.to_hex();

    // 1. Compute sync cursor (blocking)
    let min_cursor = {
        let urls = relay_urls.to_vec();
        db.run(move |conn| {
            let mut min: u64 = u64::MAX;
            for url in &urls {
                min = min.min(store::get_sync_cursor(conn, url)?);
            }
            Ok(if min == u64::MAX { 0 } else { min })
        }).await?
    };
    let since = min_cursor.saturating_sub(SYNC_OVERLAP_SECS);

    // 2. Fetch Gift Wraps (async), cap to MAX_EVENTS_PER_SYNC
    let mut events = mycel_nostr::fetch_gift_wraps(client, relay_urls, &my_pubkey, since, timeout).await?;
    let fetched = events.len();
    if events.len() > MAX_EVENTS_PER_SYNC {
        tracing::warn!(
            "relay returned {} events, capping to {}",
            events.len(),
            MAX_EVENTS_PER_SYNC,
        );
        // Keep the oldest events so sync cursor advances predictably
        events.sort_by_key(|e| e.created_at);
        events.truncate(MAX_EVENTS_PER_SYNC);
    }

    // 3. Unwrap (async — NIP-59 decrypt)
    let mut unwrapped = Vec::new();
    for event in &events {
        let Ok(u) = UnwrappedGift::from_gift_wrap(keys, event).await else {
            continue;
        };
        unwrapped.push(UnwrappedData {
            nostr_id: event.id.to_hex(),
            event_ts: event.created_at.as_secs(),
            sender_hex: u.sender.to_hex(),
            rumor_content: u.rumor.content.clone(),
        });
    }

    // 4. Process + store (blocking)
    let new_messages = {
        let my_hex_owned = my_hex.clone();
        db.run(move |conn| {
            let mut count = 0u64;
            for msg in &unwrapped {
                if let Some((row, meta)) = parse_and_validate(conn, &my_hex_owned, msg)? {
                    // Skip storage entirely for blocked senders
                    if row.delivery_status == DeliveryStatus::Blocked {
                        continue;
                    }
                    if store::insert_message_v2(conn, &row, &meta)? {
                        count += 1;
                    }
                }
            }
            Ok(count)
        }).await?
    };

    // 5. Update sync cursors (blocking)
    if !events.is_empty() {
        let max_ts = events.iter().map(|e| e.created_at.as_secs()).max().unwrap_or(0);
        let urls = relay_urls.to_vec();
        db.run(move |conn| {
            let cursor = max_ts.max(min_cursor);
            for url in &urls {
                store::update_sync_cursor(conn, url, cursor)?;
            }
            Ok(())
        }).await?;
    }

    Ok(SyncReport { fetched, new_messages })
}

struct UnwrappedData {
    nostr_id: String,
    event_ts: u64,
    sender_hex: String,
    rumor_content: String,
}

fn parse_and_validate(
    conn: &rusqlite::Connection,
    my_hex: &str,
    msg: &UnwrappedData,
) -> Result<Option<(store::MessageRow, MessageMeta)>> {
    let env: envelope::Envelope = match serde_json::from_str(&msg.rumor_content) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };

    if env.v != 1 && env.v != 2 { // v1 and v2 accepted; unknown versions dropped
        return Ok(None);
    }

    if env.from != msg.sender_hex {
        tracing::warn!(
            "envelope sender mismatch: env.from={} != signed={}",
            &env.from[..env.from.len().min(12)],
            &msg.sender_hex[..msg.sender_hex.len().min(12)]
        );
        return Ok(None);
    }

    // Size limit: check entire raw envelope (prevents oversized DataPart bypass)
    if msg.rumor_content.len() > crate::error::MAX_MESSAGE_SIZE {
        return Ok(None);
    }

    // Extract text content: v2 uses parts[], v1 uses legacy msg field
    let content: String = if !env.parts.is_empty() {
        env.parts.iter().filter_map(|p| match p {
            Part::TextPart { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("\n")
    } else {
        env.msg.clone()
    };

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

    // v2: use msg_id for dedup (outbox retries reuse msg_id with new nostr_id)
    // v1 (legacy): msg_id is None — dedup falls back to nostr_id PRIMARY KEY
    let msg_id = if env.v == 2 && !env.msg_id.is_empty() {
        Some(env.msg_id.clone())
    } else {
        None
    };

    // Only accept thread metadata from Known contacts (prevents thread injection)
    let (thread_id, reply_to) = if trust_tier == TrustTier::Known {
        (env.thread_id.clone(), env.reply_to.clone())
    } else {
        (None, None)
    };

    let meta = MessageMeta {
        msg_id,
        thread_id,
        reply_to,
        transport: Some("nostr".to_string()),
        transport_msg_id: Some(msg.nostr_id.clone()),
    };

    let row = store::MessageRow {
        nostr_id: msg.nostr_id.clone(),
        direction: Direction::In,
        sender: msg.sender_hex.clone(),
        recipient: my_hex.to_string(),
        content,
        delivery_status,
        read_status,
        created_at: crate::envelope::timestamp_to_iso8601(msg.event_ts),
        received_at: crate::envelope::now_iso8601(),
        sender_alias: None,
    };

    Ok(Some((row, meta)))
}

/// Extract text content from envelope parts (v2) or legacy msg field (v1).
#[allow(dead_code)]
pub(crate) fn extract_content(env: &envelope::Envelope) -> String {
    if !env.parts.is_empty() {
        env.parts.iter().filter_map(|p| match p {
            Part::TextPart { text } => Some(text.as_str()),
            _ => None,
        }).collect::<Vec<_>>().join("\n")
    } else {
        env.msg.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Part;

    fn make_v1_content(from: &str, to: &str, msg: &str) -> String {
        format!(
            r#"{{"v":1,"from":"{from}","to":"{to}","msg":"{msg}","ts":"2026-03-25T00:00:00Z"}}"#
        )
    }

    fn make_v2_content(from: &str, to: &str, msg_id: &str, text: &str) -> String {
        format!(
            r#"{{"v":2,"msg_id":"{msg_id}","from":"{from}","to":"{to}","ts":"2026-03-25T00:00:00Z","parts":[{{"type":"text","text":"{text}"}}]}}"#
        )
    }

    fn open_mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        conn
    }

    #[test]
    fn test_parse_and_validate_v1() {
        let conn = open_mem();
        let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
        let content = make_v1_content(sender, recipient, "hello v1");
        let msg = UnwrappedData {
            nostr_id: "nostr_v1_id".to_string(),
            event_ts: 1742860800,
            sender_hex: sender.to_string(),
            rumor_content: content,
        };
        let result = parse_and_validate(&conn, recipient, &msg).unwrap();
        assert!(result.is_some(), "v1 envelope must be accepted");
        let (row, meta) = result.unwrap();
        assert_eq!(row.content, "hello v1");
        assert_eq!(row.nostr_id, "nostr_v1_id");
        assert!(meta.msg_id.is_none(), "v1 has no msg_id dedup");
    }

    #[test]
    fn test_parse_and_validate_v2() {
        let conn = open_mem();
        let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
        let msg_id = "01950000-0000-7000-8000-000000000042";
        let content = make_v2_content(sender, recipient, msg_id, "hello v2");
        let msg = UnwrappedData {
            nostr_id: "nostr_v2_id".to_string(),
            event_ts: 1742860800,
            sender_hex: sender.to_string(),
            rumor_content: content,
        };
        let result = parse_and_validate(&conn, recipient, &msg).unwrap();
        assert!(result.is_some(), "v2 envelope must be accepted");
        let (row, meta) = result.unwrap();
        assert_eq!(row.content, "hello v2");
        assert_eq!(row.nostr_id, "nostr_v2_id");
        assert_eq!(meta.msg_id.as_deref(), Some(msg_id), "v2 must carry msg_id for dedup");
    }

    #[test]
    fn test_parse_and_validate_unknown_version() {
        let conn = open_mem();
        let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
        // v3 is not a supported version
        let content = format!(
            r#"{{"v":3,"msg_id":"someid","from":"{sender}","to":"{recipient}","ts":"2026-03-25T00:00:00Z","parts":[]}}"#
        );
        let msg = UnwrappedData {
            nostr_id: "nostr_v3_id".to_string(),
            event_ts: 1742860800,
            sender_hex: sender.to_string(),
            rumor_content: content,
        };
        let result = parse_and_validate(&conn, recipient, &msg).unwrap();
        assert!(result.is_none(), "unknown envelope version must be rejected");
    }

    #[test]
    fn test_extract_content_v2_parts() {
        let env = envelope::Envelope::new_v2(
            "01950000-0000-7000-8000-000000000001".to_string(),
            "from_hex".to_string(),
            "to_hex".to_string(),
            vec![Part::TextPart { text: "part text".to_string() }],
        );
        assert_eq!(extract_content(&env), "part text");
    }

    #[test]
    fn test_extract_content_v1_msg() {
        let env = envelope::Envelope::new(
            "from_hex".to_string(),
            "to_hex".to_string(),
            "legacy text".to_string(),
        );
        // v1: parts is populated from msg field via EnvelopeWire conversion
        // extract_content uses parts if non-empty
        let content = extract_content(&env);
        assert!(!content.is_empty(), "v1 content must be extracted");
    }
}
