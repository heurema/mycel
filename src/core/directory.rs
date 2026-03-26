use anyhow::Result;
use nostr_sdk::PublicKey;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::config::{self, Config};
use crate::store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEndpoint {
    pub endpoint_id: String,
    pub agent_ref: String,
    pub pubkey_hex: String,
    pub db_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NostrEndpoint {
    pub endpoint_id: String,
    pub agent_ref: String,
    pub pubkey_hex: String,
    pub public_key: PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LocalEndpointMeta {
    pubkey_hex: String,
}

pub fn resolve_local_endpoint(
    conn: &Connection,
    cfg: &Config,
    agent_ref: &str,
    sender_hex: &str,
) -> Result<Option<LocalEndpoint>> {
    if agent_ref == "self" || cfg.local.agents.contains_key(agent_ref) {
        sync_local_endpoints(conn, cfg, sender_hex)?;
        return get_local_endpoint(conn, agent_ref);
    }

    Ok(None)
}

pub fn resolve_nostr_endpoint(conn: &Connection, agent_ref: &str) -> Result<NostrEndpoint> {
    if let Some(pubkey_hex) = try_resolve_address_to_hex(conn, agent_ref)? {
        upsert_nostr_endpoint(conn, agent_ref, &pubkey_hex)?;
        if let Some(endpoint) = get_nostr_endpoint(conn, agent_ref)? {
            return Ok(endpoint);
        }
        return nostr_endpoint_from_hex(
            &format!("discovered:nostr:{agent_ref}"),
            agent_ref,
            &pubkey_hex,
        );
    }

    get_nostr_endpoint(conn, agent_ref)?
        .ok_or_else(|| anyhow::anyhow!("unknown recipient '{}'", agent_ref))
}

pub fn nostr_endpoint_from_hex(
    endpoint_id: &str,
    agent_ref: &str,
    pubkey_hex: &str,
) -> Result<NostrEndpoint> {
    Ok(NostrEndpoint {
        endpoint_id: endpoint_id.to_string(),
        agent_ref: agent_ref.to_string(),
        pubkey_hex: pubkey_hex.to_string(),
        public_key: PublicKey::from_hex(pubkey_hex)?,
    })
}

fn sync_local_endpoints(conn: &Connection, cfg: &Config, sender_hex: &str) -> Result<()> {
    let now = crate::envelope::now_iso8601();
    let self_row = store::AgentEndpointRow {
        endpoint_id: "system:self:local_direct".to_string(),
        agent_ref: "self".to_string(),
        transport: "local_direct".to_string(),
        address: config::data_dir()?
            .join("mycel.db")
            .to_string_lossy()
            .into_owned(),
        priority: 0,
        enabled: true,
        metadata_json: Some(serde_json::to_string(&LocalEndpointMeta {
            pubkey_hex: sender_hex.to_string(),
        })?),
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    store::upsert_agent_endpoint(conn, &self_row)?;

    for (agent_ref, entry) in &cfg.local.agents {
        let row = store::AgentEndpointRow {
            endpoint_id: format!("config:local_direct:{agent_ref}"),
            agent_ref: agent_ref.clone(),
            transport: "local_direct".to_string(),
            address: config::expand_tilde(&entry.db)
                .to_string_lossy()
                .into_owned(),
            priority: 100,
            enabled: true,
            metadata_json: Some(serde_json::to_string(&LocalEndpointMeta {
                pubkey_hex: entry.pubkey.clone(),
            })?),
            created_at: now.clone(),
            updated_at: now.clone(),
        };
        store::upsert_agent_endpoint(conn, &row)?;
    }

    Ok(())
}

fn get_local_endpoint(conn: &Connection, agent_ref: &str) -> Result<Option<LocalEndpoint>> {
    let Some(row) = store::get_agent_endpoint(conn, agent_ref, "local_direct")? else {
        return Ok(None);
    };

    let meta: LocalEndpointMeta =
        serde_json::from_str(row.metadata_json.as_deref().ok_or_else(|| {
            anyhow::anyhow!("local endpoint '{}' missing metadata", row.endpoint_id)
        })?)?;

    Ok(Some(LocalEndpoint {
        endpoint_id: row.endpoint_id,
        agent_ref: row.agent_ref,
        pubkey_hex: meta.pubkey_hex,
        db_path: PathBuf::from(row.address),
    }))
}

fn get_nostr_endpoint(conn: &Connection, agent_ref: &str) -> Result<Option<NostrEndpoint>> {
    let Some(row) = store::get_agent_endpoint(conn, agent_ref, "nostr")? else {
        return Ok(None);
    };
    Ok(Some(nostr_endpoint_from_hex(
        &row.endpoint_id,
        &row.agent_ref,
        &row.address,
    )?))
}

fn upsert_nostr_endpoint(conn: &Connection, agent_ref: &str, pubkey_hex: &str) -> Result<()> {
    let now = crate::envelope::now_iso8601();
    let row = store::AgentEndpointRow {
        endpoint_id: format!("discovered:nostr:{agent_ref}"),
        agent_ref: agent_ref.to_string(),
        transport: "nostr".to_string(),
        address: pubkey_hex.to_string(),
        priority: 100,
        enabled: true,
        metadata_json: None,
        created_at: now.clone(),
        updated_at: now,
    };
    store::upsert_agent_endpoint(conn, &row)
}

fn try_resolve_address_to_hex(conn: &Connection, address: &str) -> Result<Option<String>> {
    if let Some(contact) = store::get_contact_by_alias(conn, address)? {
        return Ok(Some(contact.pubkey));
    }

    match PublicKey::parse(address) {
        Ok(pubkey) => Ok(Some(pubkey.to_hex())),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, store};

    #[test]
    fn test_resolve_self_endpoint() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let endpoint = resolve_local_endpoint(&conn, &Config::default(), "self", "sender_hex")
            .unwrap()
            .expect("self endpoint");
        assert_eq!(endpoint.endpoint_id, "system:self:local_direct");
        assert_eq!(endpoint.agent_ref, "self");
        assert_eq!(endpoint.pubkey_hex, "sender_hex");
        assert!(endpoint.db_path.ends_with("mycel.db"));
    }

    #[test]
    fn test_resolve_local_endpoint_from_config() {
        let mut cfg = Config::default();
        cfg.local.agents.insert(
            "codex".to_string(),
            crate::config::LocalAgentEntry {
                pubkey: "abc".to_string(),
                db: "~/tmp/mycel.db".to_string(),
            },
        );

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();

        let endpoint = resolve_local_endpoint(&conn, &cfg, "codex", "sender_hex")
            .unwrap()
            .expect("local endpoint");

        assert_eq!(endpoint.endpoint_id, "config:local_direct:codex");
        assert_eq!(endpoint.agent_ref, "codex");
        assert_eq!(endpoint.pubkey_hex, "abc");
        assert!(
            endpoint
                .db_path
                .to_string_lossy()
                .ends_with("/tmp/mycel.db")
        );
    }

    #[test]
    fn test_resolve_nostr_endpoint_from_contact_alias() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();

        let keys = nostr_sdk::Keys::generate();
        store::insert_contact(
            &conn,
            &store::ContactRow {
                pubkey: keys.public_key().to_hex(),
                alias: Some("alice".to_string()),
                trust_tier: crate::types::TrustTier::Known,
                added_at: crate::envelope::now_iso8601(),
            },
        )
        .unwrap();

        let endpoint = resolve_nostr_endpoint(&conn, "alice").unwrap();
        assert_eq!(endpoint.endpoint_id, "discovered:nostr:alice");
        assert_eq!(endpoint.agent_ref, "alice");
        assert_eq!(endpoint.pubkey_hex, keys.public_key().to_hex());
    }

    #[test]
    fn test_resolve_nostr_endpoint_from_directory_store() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let keys = nostr_sdk::Keys::generate();
        let now = crate::envelope::now_iso8601();

        store::upsert_agent_endpoint(
            &conn,
            &store::AgentEndpointRow {
                endpoint_id: "manual:nostr:relay-bot".to_string(),
                agent_ref: "relay-bot".to_string(),
                transport: "nostr".to_string(),
                address: keys.public_key().to_hex(),
                priority: 10,
                enabled: true,
                metadata_json: None,
                created_at: now.clone(),
                updated_at: now,
            },
        )
        .unwrap();

        let endpoint = resolve_nostr_endpoint(&conn, "relay-bot").unwrap();
        assert_eq!(endpoint.endpoint_id, "manual:nostr:relay-bot");
        assert_eq!(endpoint.pubkey_hex, keys.public_key().to_hex());
    }
}
