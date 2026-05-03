use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::core::ingest;
use crate::types::{AckStatus, Direction, TrustTier};
#[cfg(test)]
use crate::types::{DeliveryStatus, ReadStatus};
use crate::{config, crypto, nostr as mycel_nostr, store, sync};

pub async fn run(json: bool, all: bool, local: bool) -> Result<()> {
    let cfg = config::load()?;
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;

    let local_ingest = ingest::ingest_pending(&db).await?;

    let new_count = if local {
        // --local: read from SQLite only, no relay fetch
        local_ingest.accepted
    } else {
        // Normal mode: sync from relays first
        let enc_path = config::config_dir()?.join("key.enc");
        let keys = crypto::load_keys(&enc_path, cfg.identity.storage)?;
        let relay_urls = &cfg.relays.urls;
        let timeout = Duration::from_secs(cfg.relays.timeout_secs);

        // Flush outbox (retry pending outbound messages) before fetching inbox
        if let Err(e) = store::flush_outbox(&db, &keys, relay_urls.clone()).await {
            tracing::warn!("flush_outbox failed: {e}");
        }

        let client = mycel_nostr::build_client(keys.clone(), relay_urls)
            .await
            .map_err(|e| {
                anyhow::anyhow!("{e} — could not connect to relay; check your network connection")
            })?;

        let report = sync::sync_once(&keys, &client, &db, relay_urls, timeout)
            .await
            .map_err(|e| anyhow::anyhow!("{e} — relay unreachable during inbox fetch"))?;

        if !json {
            eprintln!(
                "Fetched {} event(s), {} new",
                report.fetched, report.new_messages
            );
        }

        // Send ACKs for newly received messages if config.ack.enabled
        if cfg.ack.enabled && report.new_messages > 0 {
            send_ack(&db, &keys, &cfg.relays.urls, &keys.public_key().to_hex()).await;
        }

        client.disconnect().await;
        report.new_messages + local_ingest.accepted
    };

    // Display messages from DB
    let trust_filter: Vec<TrustTier> = if all {
        vec![TrustTier::Known, TrustTier::Unknown]
    } else {
        vec![TrustTier::Known]
    };
    if json {
        let messages = db
            .run(move |conn| store::get_messages_with_meta(conn, Direction::In, &trust_filter))
            .await?;
        display_json_messages(&messages)?;
    } else {
        let messages = db
            .run(move |conn| store::get_messages(conn, Direction::In, &trust_filter))
            .await?;
        display_messages(&messages, new_count);
    }

    Ok(())
}

/// Record local ACK rows for newly received v2 messages when config.ack.enabled is true.
///
/// TODO(v0.4.x): build an Envelope v2 AckPart and route it back to the original
/// sender over Nostr. For v0.4.1 this only records local ACK state keyed by the
/// original logical msg_id; there is no reverse Gift Wrap sender loop yet.
async fn send_ack(db: &store::Db, _keys: &Keys, _relay_urls: &[String], my_hex: &str) {
    let my_hex = my_hex.to_string();
    if let Err(e) = db
        .run(move |conn| record_local_ack_rows(conn, &my_hex))
        .await
    {
        tracing::warn!("send_ack: failed to record ACKs: {e}");
    }
}

fn record_local_ack_rows(conn: &rusqlite::Connection, my_hex: &str) -> Result<()> {
    let msgs = store::get_messages_with_meta(conn, Direction::In, &[TrustTier::Known])?;
    for row in &msgs {
        let Some(msg_id) = row.meta.msg_id.as_deref().filter(|id| !id.is_empty()) else {
            continue;
        };
        let ack_row = store::AckRow {
            msg_id: msg_id.to_string(),
            ack_sender: my_hex.to_string(),
            ack_status: AckStatus::Acknowledged,
            created_at: crate::envelope::now_iso8601(),
            sent_at: None,
        };
        let _ = store::insert_ack(conn, &ack_row)?;
    }
    Ok(())
}

fn display_json_messages(messages: &[store::MessageWithMetaRow]) -> Result<()> {
    for msg in messages {
        println!("{}", serde_json::to_string(&message_json_v2(msg))?);
    }
    Ok(())
}

fn display_messages(messages: &[store::MessageRow], new_count: u64) {
    if messages.is_empty() {
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
}

fn message_json_v2(row: &store::MessageWithMetaRow) -> serde_json::Value {
    let msg = &row.message;
    let from_npub = PublicKey::from_hex(&msg.sender)
        .ok()
        .and_then(|pk| pk.to_bech32().ok())
        .unwrap_or_else(|| msg.sender.clone());
    let transport = row
        .meta
        .transport
        .clone()
        .unwrap_or_else(|| "nostr".to_string());

    serde_json::json!({
        "v": 2,
        "msg_id": row.meta.msg_id.as_deref(),
        "transport": transport,
        "transport_msg_id": row.meta.transport_msg_id.as_deref(),
        "source_frame_id": row.meta.source_frame_id.as_deref(),
        "from": from_npub,
        "from_hex": msg.sender.as_str(),
        "alias": msg.sender_alias.as_deref(),
        "trust": row.trust_tier.to_string(),
        "thread_id": row.meta.thread_id.as_deref(),
        "reply_to": row.meta.reply_to.as_deref(),
        "content": msg.content.as_str(),
        "created_at": msg.created_at.as_str(),
        "received_at": msg.received_at.as_str(),
        "read_status": msg.read_status.to_string(),
        "delivery_status": msg.delivery_status.to_string(),
    })
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
                        if next.is_ascii_alphabetic() || next == '~' {
                            break;
                        }
                    }
                }
                Some(&']') => {
                    chars.next();
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
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
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        if next == '\x07' {
                            break;
                        }
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
    fn test_inbox_json_v2_format_includes_stable_contract_fields() {
        let keys = Keys::generate();
        let sender_hex = keys.public_key().to_hex();
        let sender_npub = keys.public_key().to_bech32().unwrap();
        let row = store::MessageWithMetaRow {
            message: store::MessageRow {
                nostr_id: "event-json".to_string(),
                direction: Direction::In,
                sender: sender_hex.clone(),
                recipient: "recipient_hex".to_string(),
                content: "hello json".to_string(),
                delivery_status: DeliveryStatus::Received,
                read_status: ReadStatus::Unread,
                created_at: "2026-05-03T00:00:00Z".to_string(),
                received_at: "2026-05-03T00:00:01Z".to_string(),
                sender_alias: None,
            },
            meta: crate::types::MessageMeta {
                msg_id: Some("019de95f-0000-7000-8000-000000000005".to_string()),
                thread_id: None,
                reply_to: None,
                transport: Some("nostr".to_string()),
                transport_msg_id: Some("event-json".to_string()),
                source_frame_id: None,
            },
            trust_tier: TrustTier::Known,
        };

        let parsed = message_json_v2(&row);
        assert_eq!(parsed["v"], 2);
        assert_eq!(parsed["msg_id"], "019de95f-0000-7000-8000-000000000005");
        assert_eq!(parsed["transport"], "nostr");
        assert_eq!(parsed["thread_id"], serde_json::Value::Null);
        assert_eq!(parsed["reply_to"], serde_json::Value::Null);
        assert_eq!(parsed["source_frame_id"], serde_json::Value::Null);
        assert_eq!(parsed["from"], sender_npub);
        assert_eq!(parsed["from_hex"], sender_hex);
        assert_eq!(parsed["alias"], serde_json::Value::Null);
        assert_eq!(parsed["trust"], "known");
        assert_eq!(parsed["read_status"], "unread");
        assert_eq!(parsed["delivery_status"], "received");
        assert!(parsed.as_object().unwrap().contains_key("transport_msg_id"));
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
        assert_eq!(
            store::get_messages(&conn, Direction::In, &[])
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_record_local_ack_rows_uses_logical_msg_id_not_nostr_id() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();

        let sender = "known_sender".to_string();
        store::insert_contact(
            &conn,
            &store::ContactRow {
                pubkey: sender.clone(),
                alias: None,
                trust_tier: TrustTier::Known,
                added_at: "2026-05-03T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let msg = store::MessageRow {
            nostr_id: "nostr-event-id".to_string(),
            direction: Direction::In,
            sender,
            recipient: "recipient".to_string(),
            content: "needs ack".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-05-03T00:00:00Z".to_string(),
            received_at: "2026-05-03T00:00:01Z".to_string(),
            sender_alias: None,
        };
        let meta = crate::types::MessageMeta {
            msg_id: Some("logical-msg-id".to_string()),
            thread_id: None,
            reply_to: None,
            transport: Some("nostr".to_string()),
            transport_msg_id: Some("nostr-event-id".to_string()),
            source_frame_id: Some("nostr:nostr-event-id".to_string()),
        };
        store::insert_message_v2(&conn, &msg, &meta).unwrap();

        record_local_ack_rows(&conn, "my_hex").unwrap();

        let ack_msg_id: String = conn
            .query_row("SELECT msg_id FROM acks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ack_msg_id, "logical-msg-id");
    }
}
