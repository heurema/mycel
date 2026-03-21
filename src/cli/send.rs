use anyhow::Result;
use nostr_sdk::prelude::*;
use std::time::Duration;

use crate::cli::contacts::resolve_address_to_hex;
use crate::error::MycelError;
use crate::types::{DeliveryStatus, Direction, ReadStatus};
use crate::{config, crypto, envelope, nostr as mycel_nostr, store};

pub async fn run(recipient: &str, message: &str) -> Result<()> {
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

    // 3. Relay config
    let timeout = Duration::from_secs(cfg.relays.timeout_secs);
    let relay_urls = cfg.relays.urls;

    // 4. Open DB and resolve recipient
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;
    let recipient_str = recipient.to_string();
    let recipient_hex = db.run(move |conn| {
        resolve_address_to_hex(conn, &recipient_str)
    }).await?;
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
    let failed = total.saturating_sub(ok_count);

    // 8. Determine delivery status (C1: 1 relay ack = success)
    let delivery_status = if ok_count > 0 { DeliveryStatus::Delivered } else { DeliveryStatus::Failed };

    // 9. Store in outbox
    let now = now_iso8601();
    let msg_row = store::MessageRow {
        nostr_id: event_id.to_hex(),
        direction: Direction::Out,
        sender: sender_hex,
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
