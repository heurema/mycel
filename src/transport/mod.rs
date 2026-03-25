pub mod nostr;

use anyhow::Result;
use async_trait::async_trait;
use nostr_sdk::prelude::Keys;
use nostr_sdk::PublicKey;

#[derive(Debug)]
pub struct SendReport {
    pub transport_msg_id: String,
    pub ok_count: usize,
    pub total: usize,
}

#[derive(Debug)]
pub struct ReceivedEnvelope {
    pub transport_msg_id: String,
    pub sender_hex: String,
    pub env_json: String,
    pub event_ts: u64,
}

#[derive(Debug)]
pub struct RelayHealth {
    pub url: String,
    pub connected: bool,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, keys: &Keys, recipient: &PublicKey, env_json: &str) -> Result<SendReport>;
    async fn receive(&self, keys: &Keys, since: u64) -> Result<Vec<ReceivedEnvelope>>;
    async fn health(&self) -> Vec<RelayHealth> {
        vec![]
    }
}
