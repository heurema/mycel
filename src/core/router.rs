use anyhow::Result;
use rusqlite::Connection;

use super::directory::{self, LocalEndpoint, NostrEndpoint};
use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendRoute {
    Local(LocalEndpoint),
    Nostr(NostrEndpoint),
}

pub fn resolve_send_route(
    conn: &Connection,
    cfg: &Config,
    sender_hex: &str,
    recipient: &str,
    prefer_local: bool,
) -> Result<SendRoute> {
    if recipient == "self" {
        let endpoint = directory::resolve_local_endpoint(conn, cfg, "self", sender_hex)?
            .ok_or_else(|| anyhow::anyhow!("self endpoint is unavailable"))?;
        return Ok(SendRoute::Local(endpoint));
    }

    if prefer_local {
        let endpoint = directory::resolve_local_endpoint(conn, cfg, recipient, sender_hex)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown local agent '{}'; add it to [local.agents] in config.toml",
                    recipient
                )
            })?;
        return Ok(SendRoute::Local(endpoint));
    }

    Ok(SendRoute::Nostr(directory::resolve_nostr_endpoint(
        conn, recipient,
    )?))
}

pub fn resolve_thread_member_routes(member_pubkeys: &[String]) -> Result<Vec<NostrEndpoint>> {
    member_pubkeys
        .iter()
        .map(|pubkey_hex| directory::nostr_endpoint_from_hex(pubkey_hex, pubkey_hex, pubkey_hex))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::LocalAgentEntry, store, types::TrustTier};

    #[test]
    fn test_route_self_to_local() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let cfg = Config::default();

        let route = resolve_send_route(&conn, &cfg, "sender_hex", "self", false).unwrap();
        match route {
            SendRoute::Local(endpoint) => assert_eq!(endpoint.agent_ref, "self"),
            SendRoute::Nostr(_) => panic!("self must route locally"),
        }
    }

    #[test]
    fn test_route_explicit_local_alias() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let mut cfg = Config::default();
        cfg.local.agents.insert(
            "codex".to_string(),
            LocalAgentEntry {
                pubkey: "abc".to_string(),
                db: "~/tmp/codex.db".to_string(),
            },
        );

        let route = resolve_send_route(&conn, &cfg, "sender_hex", "codex", true).unwrap();
        match route {
            SendRoute::Local(endpoint) => {
                assert_eq!(endpoint.agent_ref, "codex");
                assert_eq!(endpoint.pubkey_hex, "abc");
            }
            SendRoute::Nostr(_) => panic!("explicit local alias must route locally"),
        }
    }

    #[test]
    fn test_route_contact_alias_to_nostr() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        let cfg = Config::default();
        let keys = nostr_sdk::Keys::generate();

        store::insert_contact(
            &conn,
            &store::ContactRow {
                pubkey: keys.public_key().to_hex(),
                alias: Some("alice".to_string()),
                trust_tier: TrustTier::Known,
                added_at: crate::envelope::now_iso8601(),
            },
        )
        .unwrap();

        let route = resolve_send_route(&conn, &cfg, "sender_hex", "alice", false).unwrap();
        match route {
            SendRoute::Nostr(endpoint) => {
                assert_eq!(endpoint.pubkey_hex, keys.public_key().to_hex())
            }
            SendRoute::Local(_) => panic!("contact alias without --local must route via nostr"),
        }
    }
}
