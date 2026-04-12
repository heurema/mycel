#![allow(dead_code)]

pub mod nostr;

use anyhow::Result;
use async_trait::async_trait;
use nostr_sdk::PublicKey;
use nostr_sdk::prelude::Keys;

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
pub trait OutboundTransport: Send + Sync {
    async fn send(&self, keys: &Keys, recipient: &PublicKey, env_json: &str) -> Result<SendReport>;
    async fn health(&self) -> Vec<RelayHealth> {
        vec![]
    }
}

#[async_trait]
pub trait Collector: Send + Sync {
    async fn collect(&self, keys: &Keys, since: u64) -> Result<Vec<ReceivedEnvelope>>;
}

#[async_trait]
pub trait Transport: OutboundTransport + Collector {
    async fn receive(&self, keys: &Keys, since: u64) -> Result<Vec<ReceivedEnvelope>>;
}

#[async_trait]
impl<T> Transport for T
where
    T: OutboundTransport + Collector + Send + Sync,
{
    async fn receive(&self, keys: &Keys, since: u64) -> Result<Vec<ReceivedEnvelope>> {
        self.collect(keys, since).await
    }
}
