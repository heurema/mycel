use anyhow::{bail, Result};
use nostr_sdk::prelude::*;
use std::time::{Duration, SystemTime};

use crate::cli::contacts::resolve_address_to_hex;
use crate::{config, crypto, envelope, nostr as mycel_nostr, store};

pub async fn run(recipient: &str, message: &str) -> Result<()> {
    // 1. Validate message size (C7)
    envelope::validate_message_size(message)?;

    // Reject empty messages
    if message.trim().is_empty() {
        bail!("message cannot be empty");
    }

    // 2. Load keypair
    let enc_path = config::config_dir()?.join("key.enc");
    let keys = crypto::load_keys(&enc_path)?;
    let sender_hex = keys.public_key().to_hex();

    // 3. Load config for relay URLs and timeout
    let cfg = config::load()?;
    let timeout = Duration::from_secs(cfg.relays.timeout_secs);
    let relay_urls = cfg.relays.urls;

    // 4. Open DB and resolve recipient
    let db_path = config::data_dir()?.join("mycel.db");
    let conn = store::open(&db_path)?;
    let recipient_hex = resolve_address_to_hex(&conn, recipient)?;
    let recipient_pk = PublicKey::from_hex(&recipient_hex)?;

    // 5. Build mycel envelope
    let env = envelope::Envelope::new(sender_hex.clone(), recipient_hex.clone(), message.to_string());
    let env_json = serde_json::to_string(&env)?;

    // 6. Build rumor (unsigned event carrying the envelope)
    let rumor: UnsignedEvent =
        EventBuilder::new(Kind::PrivateDirectMessage, &env_json).build(keys.public_key());

    // 7. Connect to relays and publish Gift Wrap
    let client = mycel_nostr::build_client(keys, &relay_urls)
        .await
        .map_err(|e| anyhow::anyhow!("{e} — could not connect to relay; check your network connection"))?;
    let (event_id, ok_count) = mycel_nostr::publish_gift_wrap(&client, &relay_urls, &recipient_pk, rumor, timeout)
        .await
        .map_err(|e| anyhow::anyhow!("{e} — relay unreachable; check your network connection"))?;

    let total = relay_urls.len();
    let failed = total - ok_count;

    // 8. Determine delivery status (C1: 1 relay ack = success)
    let delivery_status = if ok_count > 0 { "delivered" } else { "failed" };

    // 9. Store in outbox
    let now = now_iso8601();
    let msg_row = store::MessageRow {
        nostr_id: event_id.to_hex(),
        direction: "out".to_string(),
        sender: sender_hex,
        recipient: recipient_hex,
        content: message.to_string(),
        delivery_status: delivery_status.to_string(),
        read_status: "read".to_string(),
        created_at: env.ts.clone(),
        received_at: now,
    };
    store::insert_message(&conn, &msg_row)?;

    // 10. Disconnect and print result
    client.disconnect().await;

    if ok_count == 0 {
        let tried = relay_urls.join(", ");
        bail!(
            "no relay accepted the message (relays tried: {tried}); check relay URLs in your config"
        );
    } else if failed > 0 {
        println!("Sent ({ok_count}/{total} relays, {failed} failed)");
    } else {
        println!("Sent ({ok_count}/{total} relays)");
    }

    Ok(())
}

fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (days, rem) = (secs / 86400, secs % 86400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let (mins, secs) = (rem / 60, rem % 60);
    let (year, month, day) = crate::envelope::days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{secs:02}Z")
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
