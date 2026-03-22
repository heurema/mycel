// Shared sync core: fetch from relays → decrypt → store in SQLite.
// Used by both `inbox` (one-shot) and `watch` (loop).

use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::types::{DeliveryStatus, Direction, ReadStatus, TrustTier};
use crate::{envelope, error::SYNC_OVERLAP_SECS, nostr as mycel_nostr, store};

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

    // 2. Fetch Gift Wraps (async)
    let events = mycel_nostr::fetch_gift_wraps(client, relay_urls, &my_pubkey, since, timeout).await?;
    let fetched = events.len();

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
                if let Some(row) = parse_and_validate(conn, &my_hex_owned, msg)? {
                    if store::insert_message(conn, &row)? && row.delivery_status != DeliveryStatus::Blocked {
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
) -> Result<Option<store::MessageRow>> {
    let env: envelope::Envelope = match serde_json::from_str(&msg.rumor_content) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };

    if env.v != 1 {
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

    if env.msg.len() > crate::error::MAX_MESSAGE_SIZE {
        return Ok(None);
    }

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

    Ok(Some(store::MessageRow {
        nostr_id: msg.nostr_id.clone(),
        direction: Direction::In,
        sender: msg.sender_hex.clone(),
        recipient: my_hex.to_string(),
        content: env.msg,
        delivery_status,
        read_status,
        created_at: crate::envelope::timestamp_to_iso8601(msg.event_ts),
        received_at: crate::envelope::now_iso8601(),
        sender_alias: None,
    }))
}
