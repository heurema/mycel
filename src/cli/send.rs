use anyhow::Result;
use nostr_sdk::prelude::*;
use rusqlite::Connection;
use std::time::Duration;
use uuid::Uuid;

use crate::core::directory::{LocalEndpoint, NostrEndpoint};
use crate::core::ingest;
use crate::core::router::{self, SendRoute};
use crate::error::MycelError;
use crate::types::{DeliveryStatus, Direction, MessageMeta, Part, ReadStatus};
use crate::{config, crypto, envelope, nostr as mycel_nostr, store};

/// Entry point for `mycel send`. When `--local` is set, delivers directly to recipient's DB.
pub async fn run(recipient: &str, message: &str, local: bool) -> Result<()> {
    // 1. Reject empty messages first, then validate size
    if message.trim().is_empty() {
        return Err(MycelError::EmptyMessage.into());
    }
    envelope::validate_message_size(message)?;

    // 2. Load keypair
    let cfg = config::load()?;
    let enc_path = config::config_dir()?.join("key.enc");
    let keys = crypto::load_keys(&enc_path, cfg.identity.storage)?;
    let sender_hex = keys.public_key().to_hex();
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;

    let route = {
        let cfg = cfg.clone();
        let sender_hex = sender_hex.clone();
        let recipient = recipient.to_string();
        db.run(move |conn| router::resolve_send_route(conn, &cfg, &sender_hex, &recipient, local))
            .await?
    };

    match route {
        SendRoute::Local(endpoint) => run_local(&endpoint, message, &keys, &sender_hex).await,
        SendRoute::Nostr(endpoint) => {
            let flush_relay_urls = cfg.relays.urls.clone();
            if let Err(e) = store::flush_outbox(&db, &keys, flush_relay_urls).await {
                tracing::warn!("flush_outbox failed: {e}");
            }
            run_nostr(endpoint, message, &cfg, &keys, &sender_hex, db).await
        }
    }
}

/// Local transport: write directly into recipient's SQLite DB.
async fn run_local(
    endpoint: &LocalEndpoint,
    message: &str,
    keys: &Keys,
    sender_hex: &str,
) -> Result<()> {
    let endpoint_id = endpoint.endpoint_id.clone();
    let agent_ref = endpoint.agent_ref.clone();
    let recipient_hex = endpoint.pubkey_hex.clone();
    let recipient_db_path = endpoint.db_path.clone();

    // Generate UUIDv7 msg_id
    let msg_id = Uuid::now_v7().to_string();

    // Build Envelope v2
    let mut env = envelope::Envelope::new_v2(
        msg_id.clone(),
        sender_hex.to_string(),
        recipient_hex.clone(),
        vec![Part::TextPart {
            text: message.to_string(),
        }],
    );

    // Sign the envelope (Contract 3: Schnorr over canonical hash)
    let secret_key = keys.secret_key().clone();
    env.sign(&secret_key)?;

    // Confirm sig field is present in the serialized envelope
    let env_json = serde_json::to_string(&env)?;
    // The "sig" field is present because env.sig is Some(_) after sign()

    let now = now_iso8601();

    // Open sender's own DB to record the outbox copy
    let sender_db_path = config::data_dir()?.join("mycel.db");
    let sender_db = store::Db::open(&sender_db_path)?;

    let msg_id_clone = msg_id.clone();
    let sender_hex_clone = sender_hex.to_string();
    let recipient_hex_clone = recipient_hex.clone();
    let message_clone = message.to_string();
    let now_clone = now.clone();
    let env_ts = env.ts.clone();

    sender_db
        .run(move |conn| {
            insert_outbound_local_copy(
                conn,
                &msg_id_clone,
                &sender_hex_clone,
                &recipient_hex_clone,
                &message_clone,
                &env_ts,
                &now_clone,
            )
        })
        .await?;

    // Open recipient's DB (WAL mode + busy_timeout=10000)
    let recipient_db_path_clone = recipient_db_path.clone();
    let msg_id_for_recipient = msg_id.clone();
    let sender_hex_for_recipient = sender_hex.to_string();
    let recipient_hex_for_recipient = recipient_hex.clone();
    let env_json_for_recipient = env_json.clone();
    let now_for_recipient = now.clone();

    // Determine if this is a self-send (same DB); if so reuse sender_db
    let is_self = sender_db_path == recipient_db_path;

    if is_self {
        sender_db
            .run(move |conn| {
                let frame = store::IngressFrameRow {
                    frame_id: format!("local:{}", msg_id_for_recipient),
                    transport: "local_direct".to_string(),
                    endpoint_id: Some(endpoint_id.clone()),
                    agent_ref: Some(agent_ref.clone()),
                    transport_msg_id: Some(msg_id_for_recipient),
                    sender_hint: Some(sender_hex_for_recipient),
                    recipient_hint: Some(recipient_hex_for_recipient),
                    envelope_json: env_json_for_recipient,
                    auth_meta_json: None,
                    received_at: now_for_recipient,
                    processed_at: None,
                    status: "pending".to_string(),
                    error: None,
                };
                let _ = store::insert_ingress_frame(conn, &frame)?;
                ingest::ingest_pending_conn(conn).map(|_| true)
            })
            .await?;
    } else {
        // Different DB: open recipient's DB with WAL + busy_timeout=10000
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = open_recipient_db(&recipient_db_path_clone)?;
            let frame = store::IngressFrameRow {
                frame_id: format!("local:{}", msg_id_for_recipient),
                transport: "local_direct".to_string(),
                endpoint_id: Some(endpoint_id),
                agent_ref: Some(agent_ref),
                transport_msg_id: Some(msg_id_for_recipient),
                sender_hint: Some(sender_hex_for_recipient),
                recipient_hint: Some(recipient_hex_for_recipient),
                envelope_json: env_json_for_recipient,
                auth_meta_json: None,
                received_at: now_for_recipient,
                processed_at: None,
                status: "pending".to_string(),
                error: None,
            };
            let _ = store::insert_ingress_frame(&conn, &frame)?;
            let _ = ingest::ingest_pending_conn(&conn)?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("db task panicked: {e}"))??;
    }

    println!("Sent (local)");
    Ok(())
}

/// Open a recipient's DB with WAL mode, busy_timeout=10000, and full schema.
/// Uses store::open() to ensure schema + migration are applied (not just raw Connection).
fn open_recipient_db(path: &std::path::Path) -> Result<Connection> {
    // Ensure parent directory exists (recipient may not have run `mycel init`)
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = store::open(path)?;
    conn.execute_batch("PRAGMA busy_timeout=10000;")?;
    Ok(conn)
}

/// Insert the sender-side outbox copy for a local-direct delivery.
fn insert_outbound_local_copy(
    conn: &Connection,
    msg_id: &str,
    sender: &str,
    recipient: &str,
    content: &str,
    created_at: &str,
    received_at: &str,
) -> Result<bool> {
    let msg_row = store::MessageRow {
        nostr_id: msg_id.to_string(),
        direction: Direction::Out,
        sender: sender.to_string(),
        recipient: recipient.to_string(),
        content: content.to_string(),
        delivery_status: DeliveryStatus::Delivered,
        read_status: ReadStatus::Read,
        created_at: created_at.to_string(),
        received_at: received_at.to_string(),
        sender_alias: None,
    };
    let meta = MessageMeta {
        msg_id: Some(msg_id.to_string()),
        thread_id: None,
        reply_to: None,
        transport: Some("local_direct".to_string()),
        transport_msg_id: Some(msg_id.to_string()),
        source_frame_id: None,
    };
    store::insert_message_local(conn, &msg_row, &meta)
}

/// Nostr relay transport (original behavior).
async fn run_nostr(
    endpoint: NostrEndpoint,
    message: &str,
    cfg: &config::Config,
    keys: &Keys,
    sender_hex: &str,
    db: store::Db,
) -> Result<()> {
    // 3. Relay config
    let timeout = Duration::from_secs(cfg.relays.timeout_secs);
    let relay_urls = cfg.relays.urls.clone();

    // 4. Resolve recipient
    let recipient_hex = endpoint.pubkey_hex;
    let recipient_pk = endpoint.public_key;

    // 5. Build mycel Envelope v2 with a stable logical msg_id before transport wrap.
    let env = build_remote_nostr_envelope(sender_hex, &recipient_hex, message);
    let envelope_json = serde_json::to_string(&env)?;
    let msg_id = env.msg_id.clone();

    // 5a. INSERT outbox row before network attempt (store-and-forward)
    let relay_urls_json = serde_json::to_string(&relay_urls)?;
    let now = now_iso8601();
    {
        let outbox_row = store::OutboxRow {
            msg_id: msg_id.clone(),
            recipient_hex: recipient_hex.clone(),
            envelope_json: envelope_json.clone(),
            relay_urls: relay_urls_json,
            status: "pending".to_string(),
            retry_count: 0,
            ok_relay_count: 0,
            created_at: now.clone(),
            last_attempt_at: None,
            next_retry_at: None,
            sent_at: None,
        };
        let outbox_row_clone = outbox_row.clone();
        db.run(move |conn| store::insert_outbox(conn, &outbox_row_clone))
            .await?;
    }

    // 6. Build rumor (unsigned event carrying the envelope)
    let rumor: UnsignedEvent =
        EventBuilder::new(Kind::PrivateDirectMessage, &envelope_json).build(keys.public_key());

    // 7. Connect to relays and publish Gift Wrap
    let client = mycel_nostr::build_client(keys.clone(), &relay_urls)
        .await
        .map_err(|e| {
            anyhow::anyhow!("{e} — could not connect to relay; check your network connection")
        })?;

    // Fetch recipient's kind:10050 inbox relay list; fall back to own config relays if not found
    let publish_relays =
        match mycel_nostr::fetch_inbox_relays(&client, &relay_urls, &recipient_pk, timeout).await {
            Ok(inbox_relays) if !inbox_relays.is_empty() => inbox_relays,
            Ok(_) => {
                tracing::warn!(
                    "recipient has no kind:10050 inbox relay list; using own config relays"
                );
                relay_urls.clone()
            }
            Err(e) => {
                tracing::warn!(
                    "could not fetch recipient inbox relays: {e}; using own config relays"
                );
                relay_urls.clone()
            }
        };

    let publish_result =
        mycel_nostr::publish_gift_wrap(&client, &publish_relays, &recipient_pk, rumor, timeout)
            .await;

    // 8. Update outbox status based on publish result
    let now2 = now_iso8601();
    match &publish_result {
        Ok((_event_id, ok_count)) if *ok_count > 0 => {
            // UPDATE outbox SET status='sent'
            let mid = msg_id.clone();
            let cnt = *ok_count as u32;
            let ts = now2.clone();
            db.run(move |conn| store::update_outbox_sent(conn, &mid, cnt, &ts))
                .await?;
        }
        _ => {
            // Failure: increment retry_count and set next_retry_at
            let new_retry_count = 1u32;
            let next_retry_at = store::compute_next_retry_at(new_retry_count);
            let mid = msg_id.clone();
            let ts = now2.clone();
            db.run(move |conn| {
                store::update_outbox_retry(conn, &mid, new_retry_count, &next_retry_at, &ts)
            })
            .await?;
        }
    }

    let (event_id, ok_count) = publish_result
        .map_err(|e| anyhow::anyhow!("{e} — relay unreachable; check your network connection"))?;
    let event_id_hex = event_id.to_hex();

    let total = publish_relays.len();
    let failed = total.saturating_sub(ok_count);

    // 9. Determine delivery status (C1: 1 relay ack = success)
    let delivery_status = if ok_count > 0 {
        DeliveryStatus::Delivered
    } else {
        DeliveryStatus::Failed
    };

    // 10. Store in messages table
    let msg_row = store::MessageRow {
        nostr_id: event_id_hex.clone(),
        direction: Direction::Out,
        sender: sender_hex.to_string(),
        recipient: recipient_hex,
        content: message.to_string(),
        delivery_status,
        read_status: ReadStatus::Read,
        created_at: env.ts.clone(),
        received_at: now,
        sender_alias: None,
    };
    let meta = MessageMeta {
        msg_id: Some(msg_id),
        thread_id: None,
        reply_to: None,
        transport: Some("nostr".to_string()),
        transport_msg_id: Some(event_id_hex),
        source_frame_id: None,
    };
    db.run(move |conn| store::insert_message_v2(conn, &msg_row, &meta).map(|_| ()))
        .await?;

    // 11. Disconnect and print result
    client.disconnect().await;

    if ok_count == 0 {
        eprintln!("Error: message not delivered — 0/{total} relays accepted the event");
        return Err(MycelError::NoRelays.into());
    } else if failed > 0 {
        println!("Sent ({ok_count}/{total} relays, {failed} failed)");
    } else {
        println!("Sent ({ok_count}/{total} relays)");
    }

    Ok(())
}

fn build_remote_nostr_envelope(
    sender_hex: &str,
    recipient_hex: &str,
    message: &str,
) -> envelope::Envelope {
    let msg_id = Uuid::now_v7().to_string();
    build_remote_nostr_envelope_with_msg_id(msg_id, sender_hex, recipient_hex, message)
}

fn build_remote_nostr_envelope_with_msg_id(
    msg_id: String,
    sender_hex: &str,
    recipient_hex: &str,
    message: &str,
) -> envelope::Envelope {
    envelope::Envelope::new_v2(
        msg_id,
        sender_hex.to_string(),
        recipient_hex.to_string(),
        vec![Part::TextPart {
            text: message.to_string(),
        }],
    )
}

fn now_iso8601() -> String {
    crate::envelope::now_iso8601()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::validate_message_size;
    use crate::error::MAX_MESSAGE_SIZE;

    #[test]
    fn test_send_message_size_rejection() {
        let big = "x".repeat(MAX_MESSAGE_SIZE + 1);
        assert!(validate_message_size(&big).is_err());
    }

    #[test]
    fn test_send_empty_message() {
        // Empty and whitespace-only messages should be rejected at the run() level,
        // but we test the size validation passes for empty (it's under the cap)
        assert!(validate_message_size("").is_ok());
        assert!(validate_message_size("   ").is_ok());
    }

    #[test]
    fn remote_nostr_envelope_uses_v2_stable_msg_id() {
        let env = build_remote_nostr_envelope_with_msg_id(
            "019de95f-0000-7000-8000-000000000001".to_string(),
            "sender_hex",
            "recipient_hex",
            "hello remote",
        );

        assert_eq!(env.v, 2);
        assert_eq!(env.msg_id, "019de95f-0000-7000-8000-000000000001");
        assert!(env.msg.is_empty(), "v2 remote send must not use legacy msg");
        assert_eq!(env.parts.len(), 1);
        assert!(
            env.sig.is_none(),
            "Nostr transport must not require local sig"
        );
        match &env.parts[0] {
            Part::TextPart { text } => assert_eq!(text, "hello remote"),
            _ => panic!("remote send must build a text part"),
        }
    }

    #[test]
    fn remote_outbox_envelope_json_msg_id_matches_row() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();

        let env = build_remote_nostr_envelope_with_msg_id(
            "019de95f-0000-7000-8000-000000000002".to_string(),
            "sender_hex",
            "recipient_hex",
            "persist me",
        );
        let envelope_json = serde_json::to_string(&env).unwrap();
        let row = store::OutboxRow {
            msg_id: env.msg_id.clone(),
            recipient_hex: "recipient_hex".to_string(),
            envelope_json,
            relay_urls: "[]".to_string(),
            status: "pending".to_string(),
            retry_count: 0,
            ok_relay_count: 0,
            created_at: "2026-05-03T00:00:00Z".to_string(),
            last_attempt_at: None,
            next_retry_at: None,
            sent_at: None,
        };
        store::insert_outbox(&conn, &row).unwrap();

        let (stored_row_msg_id, stored_envelope_json): (String, String) = conn
            .query_row(
                "SELECT msg_id, envelope_json FROM outbox WHERE msg_id = ?1",
                rusqlite::params![row.msg_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let stored_env: envelope::Envelope = serde_json::from_str(&stored_envelope_json).unwrap();

        assert_eq!(stored_env.v, 2);
        assert!(!stored_env.msg_id.is_empty());
        assert_eq!(stored_row_msg_id, stored_env.msg_id);
    }
}
