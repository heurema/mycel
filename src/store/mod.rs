use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    nostr_id         TEXT PRIMARY KEY,
    direction        TEXT NOT NULL,
    sender           TEXT NOT NULL,
    recipient        TEXT NOT NULL,
    content          TEXT NOT NULL,
    delivery_status  TEXT NOT NULL DEFAULT 'pending',
    read_status      TEXT NOT NULL DEFAULT 'unread',
    created_at       TEXT NOT NULL,
    received_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS contacts (
    pubkey      TEXT PRIMARY KEY,
    alias       TEXT,
    trust_tier  TEXT NOT NULL DEFAULT 'unknown',
    added_at    TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_contacts_alias_unique
    ON contacts(LOWER(alias)) WHERE alias IS NOT NULL;

CREATE TABLE IF NOT EXISTS sync_state (
    relay_url   TEXT PRIMARY KEY,
    last_sync   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS relays (
    url         TEXT PRIMARY KEY,
    enabled     INTEGER NOT NULL DEFAULT 1
);

PRAGMA user_version = 1;
";

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// A stored message row
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub nostr_id: String,
    pub direction: String,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub delivery_status: String,
    pub read_status: String,
    pub created_at: String,
    pub received_at: String,
}

/// A stored contact row
#[derive(Debug, Clone)]
pub struct ContactRow {
    pub pubkey: String,
    pub alias: Option<String>,
    pub trust_tier: String,
    pub added_at: String,
}

/// Insert a message; returns Ok(true) if inserted, Ok(false) if duplicate (INSERT OR IGNORE)
pub fn insert_message(conn: &Connection, msg: &MessageRow) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status, created_at, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            msg.nostr_id,
            msg.direction,
            msg.sender,
            msg.recipient,
            msg.content,
            msg.delivery_status,
            msg.read_status,
            msg.created_at,
            msg.received_at,
        ],
    )?;
    Ok(rows > 0)
}

/// Get messages filtered by direction and optional trust tier list.
/// `trust_tiers` is a list of accepted sender trust tiers; pass &[] to get all.
pub fn get_messages(
    conn: &Connection,
    direction: &str,
    trust_tiers: &[&str],
) -> Result<Vec<MessageRow>> {
    if trust_tiers.is_empty() {
        // No trust filter: return all messages for that direction
        let mut stmt = conn.prepare(
            "SELECT nostr_id, direction, sender, recipient, content, delivery_status, read_status, created_at, received_at
             FROM messages WHERE direction = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![direction], map_message_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    } else {
        // With trust filter: join messages with contacts
        let placeholders: String = trust_tiers
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT m.nostr_id, m.direction, m.sender, m.recipient, m.content,
                    m.delivery_status, m.read_status, m.created_at, m.received_at
             FROM messages m
             LEFT JOIN contacts c ON c.pubkey = m.sender
             WHERE m.direction = ?1
               AND COALESCE(c.trust_tier, 'unknown') IN ({})
             ORDER BY m.created_at ASC",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(direction.to_string())];
        for t in trust_tiers {
            params_vec.push(Box::new(t.to_string()));
        }
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(params_refs.as_slice(), map_message_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn map_message_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        nostr_id: row.get(0)?,
        direction: row.get(1)?,
        sender: row.get(2)?,
        recipient: row.get(3)?,
        content: row.get(4)?,
        delivery_status: row.get(5)?,
        read_status: row.get(6)?,
        created_at: row.get(7)?,
        received_at: row.get(8)?,
    })
}

/// Insert a contact; INSERT OR REPLACE to allow updates
pub fn insert_contact(conn: &Connection, contact: &ContactRow) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO contacts (pubkey, alias, trust_tier, added_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            contact.pubkey,
            contact.alias,
            contact.trust_tier,
            contact.added_at,
        ],
    )?;
    Ok(())
}

/// Get contact by alias (case-insensitive)
pub fn get_contact_by_alias(conn: &Connection, alias: &str) -> Result<Option<ContactRow>> {
    let result = conn
        .query_row(
            "SELECT pubkey, alias, trust_tier, added_at FROM contacts
             WHERE lower(alias) = lower(?1) LIMIT 1",
            rusqlite::params![alias],
            map_contact_row,
        )
        .optional()?;
    Ok(result)
}

/// Get contact by pubkey (hex)
pub fn get_contact_by_pubkey(conn: &Connection, pubkey: &str) -> Result<Option<ContactRow>> {
    let result = conn
        .query_row(
            "SELECT pubkey, alias, trust_tier, added_at FROM contacts WHERE pubkey = ?1",
            rusqlite::params![pubkey],
            map_contact_row,
        )
        .optional()?;
    Ok(result)
}

/// List all contacts
pub fn list_contacts(conn: &Connection) -> Result<Vec<ContactRow>> {
    let mut stmt = conn.prepare(
        "SELECT pubkey, alias, trust_tier, added_at FROM contacts ORDER BY added_at ASC",
    )?;
    let rows = stmt.query_map([], map_contact_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Update trust tier for a contact by pubkey
pub fn update_trust_tier(conn: &Connection, pubkey: &str, trust_tier: &str) -> Result<bool> {
    let rows = conn.execute(
        "UPDATE contacts SET trust_tier = ?1 WHERE pubkey = ?2",
        rusqlite::params![trust_tier, pubkey],
    )?;
    Ok(rows > 0)
}

fn map_contact_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContactRow> {
    Ok(ContactRow {
        pubkey: row.get(0)?,
        alias: row.get(1)?,
        trust_tier: row.get(2)?,
        added_at: row.get(3)?,
    })
}

/// Get sync cursor for a relay URL (0 if not set)
pub fn get_sync_cursor(conn: &Connection, relay_url: &str) -> Result<u64> {
    let result = conn
        .query_row(
            "SELECT last_sync FROM sync_state WHERE relay_url = ?1",
            rusqlite::params![relay_url],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(result.unwrap_or(0).max(0) as u64)
}

/// Update sync cursor for a relay URL
pub fn update_sync_cursor(conn: &Connection, relay_url: &str, last_sync: u64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO sync_state (relay_url, last_sync) VALUES (?1, ?2)",
        rusqlite::params![relay_url, i64::try_from(last_sync).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    #[test]
    fn schema_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"messages".to_string()));
        assert!(tables.contains(&"contacts".to_string()));
        assert!(tables.contains(&"sync_state".to_string()));
        assert!(tables.contains(&"relays".to_string()));
    }

    #[test]
    fn test_store_insert_message() {
        let conn = open_mem();
        let msg = MessageRow {
            nostr_id: "abc123".to_string(),
            direction: "in".to_string(),
            sender: "sender_hex".to_string(),
            recipient: "recipient_hex".to_string(),
            content: "hello".to_string(),
            delivery_status: "received".to_string(),
            read_status: "unread".to_string(),
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
        };
        let inserted = insert_message(&conn, &msg).unwrap();
        assert!(inserted, "first insert should succeed");

        // Duplicate insert should be ignored
        let dup = insert_message(&conn, &msg).unwrap();
        assert!(!dup, "duplicate insert should return false");
    }

    #[test]
    fn test_store_get_messages() {
        let conn = open_mem();
        let msg = MessageRow {
            nostr_id: "id1".to_string(),
            direction: "in".to_string(),
            sender: "spubkey".to_string(),
            recipient: "rpubkey".to_string(),
            content: "test msg".to_string(),
            delivery_status: "received".to_string(),
            read_status: "unread".to_string(),
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
        };
        insert_message(&conn, &msg).unwrap();

        let msgs = get_messages(&conn, "in", &[]).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "test msg");

        // No 'out' messages
        let out = get_messages(&conn, "out", &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_store_get_messages_trust_filter() {
        let conn = open_mem();

        // Insert a contact with trust_tier=known
        let contact = ContactRow {
            pubkey: "known_pubkey".to_string(),
            alias: Some("alice".to_string()),
            trust_tier: "known".to_string(),
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        // Insert message from known sender
        let msg_known = MessageRow {
            nostr_id: "id_known".to_string(),
            direction: "in".to_string(),
            sender: "known_pubkey".to_string(),
            recipient: "me".to_string(),
            content: "from known".to_string(),
            delivery_status: "received".to_string(),
            read_status: "unread".to_string(),
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
        };
        insert_message(&conn, &msg_known).unwrap();

        // Insert message from unknown sender (no contact record)
        let msg_unknown = MessageRow {
            nostr_id: "id_unknown".to_string(),
            direction: "in".to_string(),
            sender: "unknown_pubkey".to_string(),
            recipient: "me".to_string(),
            content: "from unknown".to_string(),
            delivery_status: "received".to_string(),
            read_status: "unread".to_string(),
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
        };
        insert_message(&conn, &msg_unknown).unwrap();

        // Filter: known only
        let known_msgs = get_messages(&conn, "in", &["known"]).unwrap();
        assert_eq!(known_msgs.len(), 1);
        assert_eq!(known_msgs[0].sender, "known_pubkey");

        // Filter: unknown only
        let unknown_msgs = get_messages(&conn, "in", &["unknown"]).unwrap();
        assert_eq!(unknown_msgs.len(), 1);
        assert_eq!(unknown_msgs[0].sender, "unknown_pubkey");

        // Both
        let all_msgs = get_messages(&conn, "in", &["known", "unknown"]).unwrap();
        assert_eq!(all_msgs.len(), 2);
    }

    #[test]
    fn test_store_insert_contact() {
        let conn = open_mem();
        let contact = ContactRow {
            pubkey: "pubkey_hex".to_string(),
            alias: Some("alice".to_string()),
            trust_tier: "known".to_string(),
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        let loaded = get_contact_by_pubkey(&conn, "pubkey_hex").unwrap();
        assert!(loaded.is_some());
        let c = loaded.unwrap();
        assert_eq!(c.alias, Some("alice".to_string()));
        assert_eq!(c.trust_tier, "known");
    }

    #[test]
    fn test_store_get_contact_by_alias() {
        let conn = open_mem();
        let contact = ContactRow {
            pubkey: "pk_hex".to_string(),
            alias: Some("Bob".to_string()),
            trust_tier: "known".to_string(),
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        let found = get_contact_by_alias(&conn, "bob").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().pubkey, "pk_hex");

        let not_found = get_contact_by_alias(&conn, "charlie").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn test_store_get_contact_by_pubkey() {
        let conn = open_mem();
        let contact = ContactRow {
            pubkey: "mypubkey".to_string(),
            alias: None,
            trust_tier: "unknown".to_string(),
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        let found = get_contact_by_pubkey(&conn, "mypubkey").unwrap();
        assert!(found.is_some());
        let not_found = get_contact_by_pubkey(&conn, "other").unwrap();
        assert!(not_found.is_none());
    }

    #[test]
    fn test_store_list_contacts() {
        let conn = open_mem();
        for i in 0..3 {
            let contact = ContactRow {
                pubkey: format!("pk_{i}"),
                alias: Some(format!("alias_{i}")),
                trust_tier: "known".to_string(),
                added_at: format!("2026-03-20T00:0{i}:00Z"),
            };
            insert_contact(&conn, &contact).unwrap();
        }
        let contacts = list_contacts(&conn).unwrap();
        assert_eq!(contacts.len(), 3);
    }

    #[test]
    fn test_store_update_trust_tier() {
        let conn = open_mem();
        let contact = ContactRow {
            pubkey: "pk_test".to_string(),
            alias: Some("test".to_string()),
            trust_tier: "known".to_string(),
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        let updated = update_trust_tier(&conn, "pk_test", "blocked").unwrap();
        assert!(updated);

        let c = get_contact_by_pubkey(&conn, "pk_test").unwrap().unwrap();
        assert_eq!(c.trust_tier, "blocked");

        // Non-existent key returns false
        let not_found = update_trust_tier(&conn, "nonexistent", "blocked").unwrap();
        assert!(!not_found);
    }

    #[test]
    fn test_store_sync_cursor() {
        let conn = open_mem();

        // Default is 0
        let cursor = get_sync_cursor(&conn, "wss://relay.test").unwrap();
        assert_eq!(cursor, 0);

        // Update and retrieve
        update_sync_cursor(&conn, "wss://relay.test", 1234567890).unwrap();
        let cursor2 = get_sync_cursor(&conn, "wss://relay.test").unwrap();
        assert_eq!(cursor2, 1234567890);

        // Different relay has independent cursor
        let other = get_sync_cursor(&conn, "wss://other.relay").unwrap();
        assert_eq!(other, 0);
    }
}
