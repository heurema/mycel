// Shared sync core: fetch from relays → decrypt → store in SQLite.
// Used by both `inbox` (one-shot) and `watch` (loop).

use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::types::{AckStatus, DeliveryStatus, Direction, MessageMeta, Part, ReadStatus, TrustTier};
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

    // 2. Try NIP-77 Negentropy sync first, fallback to since-filter fetch
    let mut events = match try_negentropy_sync(client, relay_urls, &my_pubkey, timeout).await {
        Ok(evts) if !evts.is_empty() => {
            tracing::info!("negentropy sync returned {} event(s)", evts.len());
            evts
        }
        Ok(_) => {
            // Negentropy returned empty — relay may support it but have nothing new,
            // or our local DB is empty. Fallback to since-filter for bootstrapping.
            mycel_nostr::fetch_gift_wraps(client, relay_urls, &my_pubkey, since, timeout).await?
        }
        Err(e) => {
            tracing::debug!("negentropy unavailable, falling back to since-filter: {e}");
            mycel_nostr::fetch_gift_wraps(client, relay_urls, &my_pubkey, since, timeout).await?
        }
    };
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

/// Try NIP-77 Negentropy reconciliation for Gift Wrap events.
/// Returns fetched events on success, or an error if the relay doesn't support it.
async fn try_negentropy_sync(
    client: &Client,
    relay_urls: &[String],
    recipient: &PublicKey,
    timeout: Duration,
) -> Result<Vec<Event>> {
    let filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(*recipient);

    let opts = SyncOptions::new()
        .direction(SyncDirection::Down)
        .initial_timeout(timeout);

    let output = client
        .sync_with(
            relay_urls.iter().map(|s| s.as_str()),
            filter.clone(),
            &opts,
        )
        .await?;

    // Reconciliation tells us which event IDs the relay has that we don't.
    // We need to fetch the actual events for those IDs.
    let remote_ids: Vec<EventId> = output.val.remote.into_iter().collect();
    if remote_ids.is_empty() {
        return Ok(vec![]);
    }

    // Fetch the missing events by ID
    let id_filter = Filter::new().ids(remote_ids);
    let events = client
        .fetch_events_from(
            relay_urls.iter().map(|s| s.as_str()),
            id_filter,
            timeout,
        )
        .await?;

    Ok(events.into_iter().collect())
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

    // Anti-storm: if this envelope contains an AckPart, it is an ACK control message.
    // Never store an ACK as a regular message — handle_incoming_ack processes it separately.
    // This prevents ACK-of-ACK storms (duplicate ack suppression at parse time).
    let is_ack = env.parts.iter().any(|p| matches!(p, Part::AckPart { .. }));
    if is_ack {
        // ACK dedup: ack-type envelopes are not stored in the messages table
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

/// Anti-storm window: suppress duplicate ACK sends within 60 seconds.
/// ACK_DEDUP: if an ack for (msg_id, ack_sender) was inserted within the last 60 seconds,
/// do not insert a duplicate ack row (prevents relay echo storms).
const ANTI_STORM_SECS: u64 = 60; // 60 second dedup window for ACK anti-storm

/// Handle an incoming ACK envelope: extract AckPart, validate, store in acks table.
/// Returns Ok(true) if a new ACK was stored, Ok(false) if duplicate or invalid.
pub fn handle_incoming_ack(
    conn: &rusqlite::Connection,
    env: &envelope::Envelope,
    sender_hex: &str,
) -> anyhow::Result<bool> {
    // Find the AckPart in the envelope
    let ack_part = env.parts.iter().find_map(|p| {
        if let Part::AckPart { original_msg_id, status, ack_ts } = p {
            Some((original_msg_id.clone(), *status, ack_ts.clone()))
        } else {
            None
        }
    });

    let (original_msg_id, status, ack_ts) = match ack_part {
        Some(a) => a,
        None => return Ok(false),
    };

    // Anti-storm: check if we already have an ack from this sender for this msg within 60 seconds.
    // duplicate ack suppression: query acks table for recent entry matching (msg_id, ack_sender).
    let existing_recent: Option<String> = conn.query_row(
        "SELECT created_at FROM acks WHERE msg_id = ?1 AND ack_sender = ?2
         AND (CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', created_at) AS INTEGER)) < ?3
         LIMIT 1",
        rusqlite::params![original_msg_id, sender_hex, ANTI_STORM_SECS as i64],
        |row| row.get(0),
    ).ok();

    if existing_recent.is_some() {
        // duplicate ack: within anti-storm window — suppress
        tracing::debug!(
            "ack dedup: suppressed duplicate ACK for msg_id={} from sender={}",
            &original_msg_id[..original_msg_id.len().min(12)],
            &sender_hex[..sender_hex.len().min(12)],
        );
        return Ok(false);
    }

    let ack_status = match status {
        AckStatus::Acknowledged => AckStatus::Acknowledged,
        AckStatus::Pending => AckStatus::Pending,
        AckStatus::Failed => AckStatus::Failed,
    };

    let ack_row = store::AckRow {
        msg_id: original_msg_id,
        ack_sender: sender_hex.to_string(),
        ack_status,
        created_at: ack_ts,
        sent_at: None,
    };

    store::insert_ack(conn, &ack_row)
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
