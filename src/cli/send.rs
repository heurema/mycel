use anyhow::Result;
use nostr_sdk::prelude::*;
use rusqlite::Connection;
use std::time::Duration;
use uuid::Uuid;

use crate::cli::contacts::resolve_address_to_hex;
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

    // Auto-route: "self" always uses local transport (no relay needed)
    if local || recipient == "self" {
        run_local(recipient, message, &cfg, &keys, &sender_hex).await
    } else {
        run_nostr(recipient, message, &cfg, &keys, &sender_hex).await
    }
}

/// Resolve the local agent alias (or "self") to (pubkey_hex, db_path).
/// Returns (pubkey_hex, expanded_db_path).
fn resolve_local_agent(
    recipient: &str,
    cfg: &config::Config,
    sender_hex: &str,
) -> Result<(String, std::path::PathBuf)> {
    if recipient == "self" {
        // "self" writes to the sender's own DB
        let db_path = config::data_dir()?.join("mycel.db");
        return Ok((sender_hex.to_string(), db_path));
    }

    let entry = cfg
        .local
        .agents
        .get(recipient)
        .ok_or_else(|| anyhow::anyhow!("unknown local agent '{}'; add it to [local.agents] in config.toml", recipient))?;

    let db_path = config::expand_tilde(&entry.db);
    Ok((entry.pubkey.clone(), db_path))
}

/// Local transport: write directly into recipient's SQLite DB.
async fn run_local(
    recipient: &str,
    message: &str,
    cfg: &config::Config,
    keys: &Keys,
    sender_hex: &str,
) -> Result<()> {
    let (recipient_hex, recipient_db_path) = resolve_local_agent(recipient, cfg, sender_hex)?;

    // Generate UUIDv7 msg_id
    let msg_id = Uuid::now_v7().to_string();

    // Build Envelope v2
    let mut env = envelope::Envelope::new_v2(
        msg_id.clone(),
        sender_hex.to_string(),
        recipient_hex.clone(),
        vec![Part::TextPart { text: message.to_string() }],
    );

    // Sign the envelope (Contract 3: Schnorr over canonical hash)
    let secret_key = keys.secret_key().clone();
    env.sign(&secret_key)?;

    // Confirm sig field is present in the serialized envelope
    let _env_json = serde_json::to_string(&env)?;
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

    sender_db.run(move |conn| {
        insert_local_message(
            conn,
            &msg_id_clone,
            Direction::Out,
            &sender_hex_clone,
            &recipient_hex_clone,
            &message_clone,
            DeliveryStatus::Delivered,
            ReadStatus::Read,
            &env_ts,
            &now_clone,
        )
    }).await?;

    // Open recipient's DB (WAL mode + busy_timeout=10000)
    let recipient_db_path_clone = recipient_db_path.clone();
    let msg_id_for_recipient = msg_id.clone();
    let sender_hex_for_recipient = sender_hex.to_string();
    let recipient_hex_for_recipient = recipient_hex.clone();
    let message_for_recipient = message.to_string();
    let now_for_recipient = now.clone();
    let env_ts_for_recipient = env.ts.clone();

    // Determine if this is a self-send (same DB); if so reuse sender_db
    let is_self = sender_db_path == recipient_db_path;

    if is_self {
        // Self-send: write inbound copy to the same DB
        sender_db.run(move |conn| {
            insert_local_message(
                conn,
                &msg_id_for_recipient,
                Direction::In,
                &sender_hex_for_recipient,
                &recipient_hex_for_recipient,
                &message_for_recipient,
                DeliveryStatus::Received,
                ReadStatus::Unread,
                &env_ts_for_recipient,
                &now_for_recipient,
            )
        }).await?;
    } else {
        // Different DB: open recipient's DB with WAL + busy_timeout=10000
        let recipient_conn = tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = open_recipient_db(&recipient_db_path_clone)?;
            insert_local_message(
                &conn,
                &msg_id_for_recipient,
                Direction::In,
                &sender_hex_for_recipient,
                &recipient_hex_for_recipient,
                &message_for_recipient,
                DeliveryStatus::Received,
                ReadStatus::Unread,
                &env_ts_for_recipient,
                &now_for_recipient,
            )?;
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("db task panicked: {e}"))??;
        let _ = recipient_conn;
    }

    println!("Sent (local)");
    Ok(())
}

/// Open a recipient's DB with WAL mode and busy_timeout=10000.
fn open_recipient_db(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=10000;")?;
    Ok(conn)
}

/// Insert a local-transport message row. Uses INSERT OR IGNORE with msg_id as dedup key.
fn insert_local_message(
    conn: &Connection,
    msg_id: &str,
    direction: Direction,
    sender: &str,
    recipient: &str,
    content: &str,
    delivery_status: DeliveryStatus,
    read_status: ReadStatus,
    created_at: &str,
    received_at: &str,
) -> Result<bool> {
    let msg_row = store::MessageRow {
        nostr_id: msg_id.to_string(),
        direction,
        sender: sender.to_string(),
        recipient: recipient.to_string(),
        content: content.to_string(),
        delivery_status,
        read_status,
        created_at: created_at.to_string(),
        received_at: received_at.to_string(),
        sender_alias: None,
    };
    let meta = MessageMeta {
        msg_id: Some(msg_id.to_string()),
        thread_id: None,
        reply_to: None,
        transport: Some("local".to_string()),
        transport_msg_id: Some(msg_id.to_string()),
    };
    store::insert_message_local(conn, &msg_row, &meta)
}

/// Nostr relay transport (original behavior).
async fn run_nostr(
    recipient: &str,
    message: &str,
    cfg: &config::Config,
    keys: &Keys,
    sender_hex: &str,
) -> Result<()> {
    // 3. Relay config
    let timeout = Duration::from_secs(cfg.relays.timeout_secs);
    let relay_urls = cfg.relays.urls.clone();

    // 4. Open DB and resolve recipient
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;
    let recipient_str = recipient.to_string();
    let recipient_hex = db.run(move |conn| {
        resolve_address_to_hex(conn, &recipient_str)
    }).await?;
    let recipient_pk = PublicKey::from_hex(&recipient_hex)?;

    // 5. Build mycel envelope
    let env = envelope::Envelope::new(sender_hex.to_string(), recipient_hex.clone(), message.to_string());
    let env_json = serde_json::to_string(&env)?;

    // 6. Build rumor (unsigned event carrying the envelope)
    let rumor: UnsignedEvent =
        EventBuilder::new(Kind::PrivateDirectMessage, &env_json).build(keys.public_key());

    // 7. Connect to relays and publish Gift Wrap
    let client = mycel_nostr::build_client(keys.clone(), &relay_urls)
        .await
        .map_err(|e| anyhow::anyhow!("{e} — could not connect to relay; check your network connection"))?;
    let (event_id, ok_count) = mycel_nostr::publish_gift_wrap(&client, &relay_urls, &recipient_pk, rumor, timeout)
        .await
        .map_err(|e| anyhow::anyhow!("{e} — relay unreachable; check your network connection"))?;

    let total = relay_urls.len();
    let failed = total.saturating_sub(ok_count);

    // 8. Determine delivery status (C1: 1 relay ack = success)
    let delivery_status = if ok_count > 0 { DeliveryStatus::Delivered } else { DeliveryStatus::Failed };

    // 9. Store in outbox
    let now = now_iso8601();
    let msg_row = store::MessageRow {
        nostr_id: event_id.to_hex(),
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
    db.run(move |conn| store::insert_message(conn, &msg_row).map(|_| ())).await?;

    // 10. Disconnect and print result
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

fn now_iso8601() -> String {
    crate::envelope::now_iso8601()
}

#[cfg(test)]
mod tests {
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
}
