use anyhow::{Context, Result};
use clap::Subcommand;
use nostr_sdk::prelude::*;
use rusqlite::Connection;

use crate::{config, store};

#[derive(Subcommand)]
pub enum ContactsAction {
    /// Add a contact to allowlist
    Add {
        /// npub address
        address: String,
        /// Friendly alias
        #[arg(long)]
        alias: Option<String>,
    },
    /// Block a contact
    Block {
        /// npub address or alias
        address: String,
    },
    /// List all contacts
    List,
}

pub async fn run(action: ContactsAction) -> Result<()> {
    let db_path = config::data_dir()?.join("mycel.db");
    let conn = store::open(&db_path)?;
    run_with_conn(&conn, action)
}

pub fn run_with_conn(conn: &Connection, action: ContactsAction) -> Result<()> {
    match action {
        ContactsAction::Add { address, alias } => {
            let pubkey_hex = resolve_npub_to_hex(&address)?;
            let now = now_iso8601();
            let contact = store::ContactRow {
                pubkey: pubkey_hex.clone(),
                alias: alias.clone(),
                trust_tier: "known".to_string(),
                added_at: now,
            };
            store::insert_contact(conn, &contact)?;
            let display = alias.as_deref().unwrap_or(&address);
            let short_npub = shorten_npub(&address);
            println!("Added {display} ({short_npub})");
        }
        ContactsAction::Block { address } => {
            // Try alias first, then npub/hex
            let pubkey_hex = resolve_address_to_hex(conn, &address)?;
            let updated = store::update_trust_tier(conn, &pubkey_hex, "blocked")?;
            if !updated {
                // Contact not in DB yet — insert as blocked
                let now = now_iso8601();
                let contact = store::ContactRow {
                    pubkey: pubkey_hex,
                    alias: None,
                    trust_tier: "blocked".to_string(),
                    added_at: now,
                };
                store::insert_contact(conn, &contact)?;
            }
            println!("Blocked {address}");
        }
        ContactsAction::List => {
            let contacts = store::list_contacts(conn)?;
            if contacts.is_empty() {
                println!("No contacts.");
            } else {
                println!("{:<20} {:<66} trust", "alias", "address");
                println!("{}", "-".repeat(95));
                for c in contacts {
                    let alias_display = c.alias.as_deref().unwrap_or("-");
                    let npub = pubkey_hex_to_npub(&c.pubkey);
                    let short_npub = shorten_npub(&npub);
                    println!("{:<20} {:<66} {}", alias_display, short_npub, c.trust_tier);
                }
            }
        }
    }
    Ok(())
}

/// Resolve an address (alias or npub/hex) to pubkey hex.
/// Tries alias lookup first, then treats as npub/hex.
pub fn resolve_address_to_hex(conn: &Connection, address: &str) -> Result<String> {
    // Try alias lookup
    if let Some(c) = store::get_contact_by_alias(conn, address)? {
        return Ok(c.pubkey);
    }
    // Try as npub/hex
    resolve_npub_to_hex(address)
}

/// Parse npub1... or hex public key, return hex.
pub fn resolve_npub_to_hex(address: &str) -> Result<String> {
    let safe_addr: String = address.chars()
        .filter(|c| !c.is_control() && *c != '\x1b')
        .take(128)
        .collect();
    let pk = PublicKey::parse(address)
        .with_context(|| format!("invalid address: '{safe_addr}' (expected npub1... or hex)"))?;
    Ok(pk.to_hex())
}

/// Shorten an npub to first 8 + ... + last 4 chars for display.
fn shorten_npub(npub: &str) -> String {
    if npub.len() <= 16 {
        return npub.to_string();
    }
    format!("{}...{}", &npub[..12], &npub[npub.len() - 4..])
}

/// Convert pubkey hex to npub bech32, fallback to hex if fails.
fn pubkey_hex_to_npub(hex: &str) -> String {
    match PublicKey::from_hex(hex) {
        Ok(pk) => pk.to_bech32().unwrap_or_else(|_| hex.to_string()),
        Err(_) => hex.to_string(),
    }
}

fn now_iso8601() -> String {
    crate::envelope::now_iso8601()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr_sdk::Keys;
    use rusqlite::Connection;

    fn open_mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::SCHEMA).unwrap();
        conn
    }

    fn gen_npub() -> String {
        let keys = Keys::generate();
        keys.public_key().to_bech32().unwrap()
    }

    #[test]
    fn test_contacts_add() {
        let conn = open_mem();
        let npub = gen_npub();

        run_with_conn(
            &conn,
            ContactsAction::Add {
                address: npub.clone(),
                alias: Some("alice".to_string()),
            },
        )
        .unwrap();

        // Verify contact was inserted
        let contacts = store::list_contacts(&conn).unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].alias, Some("alice".to_string()));
        assert_eq!(contacts[0].trust_tier, "known");
    }

    #[test]
    fn test_contacts_block() {
        let conn = open_mem();
        let npub = gen_npub();

        // Add contact first
        run_with_conn(
            &conn,
            ContactsAction::Add {
                address: npub.clone(),
                alias: Some("bob".to_string()),
            },
        )
        .unwrap();

        // Block by alias
        run_with_conn(
            &conn,
            ContactsAction::Block {
                address: "bob".to_string(),
            },
        )
        .unwrap();

        let contacts = store::list_contacts(&conn).unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].trust_tier, "blocked");
    }

    #[test]
    fn test_contacts_block_by_npub() {
        let conn = open_mem();
        let npub = gen_npub();

        // Block directly by npub (contact not in DB yet)
        run_with_conn(
            &conn,
            ContactsAction::Block {
                address: npub.clone(),
            },
        )
        .unwrap();

        let contacts = store::list_contacts(&conn).unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].trust_tier, "blocked");
    }

    #[test]
    fn test_contacts_list() {
        let conn = open_mem();

        // Empty list
        run_with_conn(&conn, ContactsAction::List).unwrap();

        // Add contacts
        let npub1 = gen_npub();
        let npub2 = gen_npub();

        run_with_conn(
            &conn,
            ContactsAction::Add {
                address: npub1.clone(),
                alias: Some("alice".to_string()),
            },
        )
        .unwrap();
        run_with_conn(
            &conn,
            ContactsAction::Add {
                address: npub2.clone(),
                alias: Some("bob".to_string()),
            },
        )
        .unwrap();

        let contacts = store::list_contacts(&conn).unwrap();
        assert_eq!(contacts.len(), 2);

        run_with_conn(&conn, ContactsAction::List).unwrap();
    }

    #[test]
    fn test_contacts_add_invalid_npub() {
        let conn = open_mem();
        let result = run_with_conn(
            &conn,
            ContactsAction::Add {
                address: "not-a-valid-npub".to_string(),
                alias: None,
            },
        );
        assert!(result.is_err());
    }
}
