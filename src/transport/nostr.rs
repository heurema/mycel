use anyhow::Result;
use async_trait::async_trait;
use nostr_sdk::prelude::*;
use std::time::Duration;

use super::{Collector, OutboundTransport, ReceivedEnvelope, RelayHealth, SendReport};
use crate::nostr as mycel_nostr;

pub struct NostrTransport {
    pub relay_urls: Vec<String>,
    pub timeout: Duration,
}

impl NostrTransport {
    pub fn new(relay_urls: Vec<String>, timeout_secs: u64) -> Self {
        Self {
            relay_urls,
            timeout: Duration::from_secs(timeout_secs),
        }
    }
}

async fn do_send(
    relay_urls: Vec<String>,
    timeout: Duration,
    keys: Keys,
    recipient: PublicKey,
    env_json: String,
) -> Result<SendReport> {
    let pubkey = keys.public_key();
    let client = mycel_nostr::build_client(keys, &relay_urls).await?;
    let rumor = EventBuilder::new(Kind::PrivateDirectMessage, env_json).build(pubkey);
    let (event_id, ok_count) =
        mycel_nostr::publish_gift_wrap(&client, &relay_urls, &recipient, rumor, timeout).await?;
    client.disconnect().await;
    Ok(SendReport {
        transport_msg_id: event_id.to_hex(),
        ok_count,
        total: relay_urls.len(),
    })
}

async fn do_receive(
    relay_urls: Vec<String>,
    timeout: Duration,
    keys: Keys,
    since: u64,
) -> Result<Vec<ReceivedEnvelope>> {
    let pubkey = keys.public_key();
    let client = mycel_nostr::build_client(keys.clone(), &relay_urls).await?;

    // Build filter and fetch directly to avoid closure lifetime issues with &str references
    let filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(pubkey)
        .since(Timestamp::from(since));
    let events = client
        .fetch_events_from(relay_urls.clone(), filter, timeout)
        .await
        .map_err(|e| anyhow::anyhow!("fetch_events_from failed: {e}"))?;

    let mut out = Vec::new();
    for event in events.iter() {
        if let Ok(u) = UnwrappedGift::from_gift_wrap(&keys, event).await {
            out.push(ReceivedEnvelope {
                transport_msg_id: event.id.to_hex(),
                sender_hex: u.sender.to_hex(),
                env_json: u.rumor.content.clone(),
                event_ts: event.created_at.as_secs(),
            });
        }
    }
    client.disconnect().await;
    Ok(out)
}

#[async_trait]
impl OutboundTransport for NostrTransport {
    async fn send(&self, keys: &Keys, recipient: &PublicKey, env_json: &str) -> Result<SendReport> {
        do_send(
            self.relay_urls.clone(),
            self.timeout,
            keys.clone(),
            *recipient,
            env_json.to_string(),
        )
        .await
    }

    async fn health(&self) -> Vec<RelayHealth> {
        let relay_urls = self.relay_urls.clone();
        let keys = Keys::generate();
        let mut results = Vec::new();
        if let Ok(client) = mycel_nostr::build_client(keys, &relay_urls).await {
            let relays = client.relays().await;
            for (url, relay) in relays {
                results.push(RelayHealth {
                    url: url.to_string(),
                    connected: relay.is_connected(),
                });
            }
            client.disconnect().await;
        }
        results
    }
}

#[async_trait]
impl Collector for NostrTransport {
    async fn collect(&self, keys: &Keys, since: u64) -> Result<Vec<ReceivedEnvelope>> {
        do_receive(self.relay_urls.clone(), self.timeout, keys.clone(), since).await
    }
}
