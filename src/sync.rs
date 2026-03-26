// Shared sync core: fetch from relays → decrypt → store in SQLite.
// Used by both `inbox` (one-shot) and `watch` (loop).

use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::{
    core::ingest,
    error::{MAX_EVENTS_PER_SYNC, SYNC_OVERLAP_SECS},
    nostr as mycel_nostr, store,
};

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
        })
        .await?
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

    // 4. Persist raw frames + run shared ingest (blocking)
    let new_messages = {
        let my_hex_owned = my_hex.clone();
        db.run(move |conn| {
            for msg in &unwrapped {
                let frame = store::IngressFrameRow {
                    frame_id: format!("nostr:{}", msg.nostr_id),
                    transport: "nostr".to_string(),
                    endpoint_id: None,
                    agent_ref: None,
                    transport_msg_id: Some(msg.nostr_id.clone()),
                    sender_hint: Some(msg.sender_hex.clone()),
                    recipient_hint: Some(my_hex_owned.clone()),
                    envelope_json: msg.rumor_content.clone(),
                    auth_meta_json: Some(serde_json::to_string(&NostrIngressMeta {
                        event_ts: msg.event_ts,
                    })?),
                    received_at: crate::envelope::now_iso8601(),
                    processed_at: None,
                    status: "pending".to_string(),
                    error: None,
                };
                let _ = store::insert_ingress_frame(conn, &frame)?;
            }
            let report = ingest::ingest_pending_conn(conn)?;
            Ok(report.accepted)
        })
        .await?
    };

    // 5. Update sync cursors (blocking)
    if !events.is_empty() {
        let max_ts = events
            .iter()
            .map(|e| e.created_at.as_secs())
            .max()
            .unwrap_or(0);
        let urls = relay_urls.to_vec();
        db.run(move |conn| {
            let cursor = max_ts.max(min_cursor);
            for url in &urls {
                store::update_sync_cursor(conn, url, cursor)?;
            }
            Ok(())
        })
        .await?;
    }

    Ok(SyncReport {
        fetched,
        new_messages,
    })
}

/// Try NIP-77 Negentropy reconciliation for Gift Wrap events.
/// Returns fetched events on success, or an error if the relay doesn't support it.
async fn try_negentropy_sync(
    client: &Client,
    relay_urls: &[String],
    recipient: &PublicKey,
    timeout: Duration,
) -> Result<Vec<Event>> {
    let filter = Filter::new().kind(Kind::GiftWrap).pubkey(*recipient);

    let opts = SyncOptions::new()
        .direction(SyncDirection::Down)
        .initial_timeout(timeout);

    let output = client
        .sync_with(relay_urls.iter().map(|s| s.as_str()), filter.clone(), &opts)
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
        .fetch_events_from(relay_urls.iter().map(|s| s.as_str()), id_filter, timeout)
        .await?;

    Ok(events.into_iter().collect())
}

struct UnwrappedData {
    nostr_id: String,
    event_ts: u64,
    sender_hex: String,
    rumor_content: String,
}

#[derive(serde::Serialize)]
struct NostrIngressMeta {
    event_ts: u64,
}
