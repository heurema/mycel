use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::types::{DeliveryStatus, Direction, ReadStatus, TrustTier};

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
    received_at      TEXT NOT NULL,
    msg_id           TEXT,
    thread_id        TEXT,
    reply_to         TEXT,
    transport        TEXT NOT NULL DEFAULT 'nostr',
    transport_msg_id TEXT
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

CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id);
CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);

CREATE TABLE IF NOT EXISTS threads (
    thread_id   TEXT PRIMARY KEY,
    subject     TEXT,
    members     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

PRAGMA user_version = 2;
";

/// Migration script to upgrade a user_version=1 database to user_version=2.
/// Adds new columns, backfills legacy rows, creates indexes and threads table.
/// This is run inside open() when user_version < 2.
const MIGRATION_V1_TO_V2: &[&str] = &[
    "ALTER TABLE messages ADD COLUMN msg_id TEXT",
    "ALTER TABLE messages ADD COLUMN thread_id TEXT",
    "ALTER TABLE messages ADD COLUMN reply_to TEXT",
    "ALTER TABLE messages ADD COLUMN transport TEXT NOT NULL DEFAULT 'nostr'",
    "ALTER TABLE messages ADD COLUMN transport_msg_id TEXT",
    "UPDATE messages SET msg_id = 'legacy:' || nostr_id WHERE msg_id IS NULL",
    "UPDATE messages SET transport_msg_id = nostr_id WHERE transport_msg_id IS NULL",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id)",
    "CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id)",
    "CREATE TABLE IF NOT EXISTS threads (
        thread_id   TEXT PRIMARY KEY,
        subject     TEXT,
        members     TEXT NOT NULL DEFAULT '[]',
        created_at  TEXT NOT NULL,
        updated_at  TEXT NOT NULL
    )",
    "PRAGMA user_version = 2",
];

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

    // Read user_version before applying schema so we know whether to run migration.
    // For existing v1 databases user_version=1; for brand-new databases user_version=0.
    let existing_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    // Apply the full v2 schema (all CREATE TABLE IF NOT EXISTS are idempotent).
    conn.execute_batch(SCHEMA)?;

    // For databases that existed before v2, run the migration to add new columns and backfill.
    // For fresh databases (version=0), SCHEMA already created the v2 layout so migration is
    // still safe (duplicate-column ALTERs are caught and skipped).
    if existing_version < 2 {
        migrate_v1_to_v2(&conn)?;
    }

    // Restrict DB file to owner-only access (0o600)
    #[cfg(unix)]
    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(conn)
}

/// Applies the v1 → v2 schema migration.
/// Idempotent: each step uses ALTER TABLE (harmless if column already exists via SQLite
/// "duplicate column" error, which we ignore) or IF NOT EXISTS clauses.
fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    for stmt in MIGRATION_V1_TO_V2 {
        // ALTER TABLE ADD COLUMN fails if column already exists; treat that as a no-op.
        match conn.execute_batch(stmt) {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("duplicate column name") {
                    // Column already exists — idempotent, skip
                } else {
                    return Err(e.into());
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Async wrapper — spawn_blocking for all rusqlite calls in async contexts
// ---------------------------------------------------------------------------

/// Async handle to a SQLite connection. All operations run on the blocking
/// thread pool via `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self::new(open(path)?))
    }

    /// Run a synchronous closure on the blocking thread pool with access
    /// to the underlying `Connection`.
    pub async fn run<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let conn = conn
                .lock()
                .map_err(|e| anyhow::anyhow!("db lock poisoned: {e}"))?;
            f(&conn)
        })
        .await
        .map_err(|e| anyhow::anyhow!("db task panicked: {e}"))?
    }
}

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub nostr_id: String,
    pub direction: Direction,
    pub sender: String,
    pub recipient: String,
    pub content: String,
    pub delivery_status: DeliveryStatus,
    pub read_status: ReadStatus,
    pub created_at: String,
    pub received_at: String,
    /// Sender alias from contacts (populated via JOIN on read, not stored)
    pub sender_alias: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ContactRow {
    pub pubkey: String,
    pub alias: Option<String>,
    pub trust_tier: TrustTier,
    pub added_at: String,
}

// ---------------------------------------------------------------------------
// Message operations
// ---------------------------------------------------------------------------

/// Insert a message; returns Ok(true) if inserted, Ok(false) if duplicate (INSERT OR IGNORE)
pub fn insert_message(conn: &Connection, msg: &MessageRow) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            None::<&str>,   // msg_id: caller may backfill via insert_message_v2
            None::<&str>,   // thread_id
            None::<&str>,   // reply_to
            "nostr",        // transport: default for v1-compat inserts
            None::<&str>,   // transport_msg_id
        ],
    )?;
    Ok(rows > 0)
}

/// Insert a message with full v2 metadata fields.
/// Returns Ok(true) if inserted, Ok(false) if duplicate (INSERT OR IGNORE on nostr_id).
pub fn insert_message_v2(conn: &Connection, msg: &MessageRow, meta: &crate::types::MessageMeta) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            meta.msg_id.as_deref(),
            meta.thread_id.as_deref(),
            meta.reply_to.as_deref(),
            meta.transport.as_deref().unwrap_or("nostr"),
            meta.transport_msg_id.as_deref(),
        ],
    )?;
    Ok(rows > 0)
}

/// Insert a local-transport message using msg_id as the dedup key (INSERT OR IGNORE).
/// Returns Ok(true) if inserted, Ok(false) if a row with the same msg_id already exists.
/// nostr_id is set to msg_id for local messages (no Nostr event ID).
pub fn insert_message_local(conn: &Connection, msg: &MessageRow, meta: &crate::types::MessageMeta) -> Result<bool> {
    let msg_id = meta.msg_id.as_deref().unwrap_or("");
    // Use msg_id as nostr_id placeholder for local messages so the PK is populated.
    let nostr_id = if msg.nostr_id.is_empty() { msg_id } else { &msg.nostr_id };
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
         WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?10 AND msg_id IS NOT NULL AND msg_id != '')",
        rusqlite::params![
            nostr_id,
            msg.direction,
            msg.sender,
            msg.recipient,
            msg.content,
            msg.delivery_status,
            msg.read_status,
            msg.created_at,
            msg.received_at,
            meta.msg_id.as_deref(),
            meta.thread_id.as_deref(),
            meta.reply_to.as_deref(),
            meta.transport.as_deref().unwrap_or("local"),
            meta.transport_msg_id.as_deref(),
        ],
    )?;
    Ok(rows > 0)
}

/// Get messages filtered by direction and optional trust tier list.
/// Pass `&[]` for trust_tiers to get all messages.
pub fn get_messages(
    conn: &Connection,
    direction: Direction,
    trust_tiers: &[TrustTier],
) -> Result<Vec<MessageRow>> {
    if trust_tiers.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT m.nostr_id, m.direction, m.sender, m.recipient, m.content,
                    m.delivery_status, m.read_status, m.created_at, m.received_at,
                    c.alias
             FROM messages m
             LEFT JOIN contacts c ON c.pubkey = m.sender
             WHERE m.direction = ?1
             ORDER BY m.created_at ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![direction], map_message_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    } else {
        let placeholders: String = trust_tiers
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT m.nostr_id, m.direction, m.sender, m.recipient, m.content,
                    m.delivery_status, m.read_status, m.created_at, m.received_at,
                    c.alias
             FROM messages m
             LEFT JOIN contacts c ON c.pubkey = m.sender
             WHERE m.direction = ?1
               AND COALESCE(c.trust_tier, 'unknown') IN ({})
             ORDER BY m.created_at ASC",
            placeholders
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(direction)];
        for t in trust_tiers {
            params_vec.push(Box::new(*t));
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
        sender_alias: row.get(9)?,
    })
}

// ---------------------------------------------------------------------------
// Contact operations
// ---------------------------------------------------------------------------

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
pub fn update_trust_tier(conn: &Connection, pubkey: &str, trust_tier: TrustTier) -> Result<bool> {
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

// ---------------------------------------------------------------------------
// Sync state
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Thread operations
// ---------------------------------------------------------------------------

/// A thread row from the threads table.
#[derive(Debug, Clone)]
pub struct ThreadRow {
    pub thread_id: String,
    pub subject: Option<String>,
    /// Members JSON array (serialized Vec<ThreadMember>).
    pub members: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Get a thread by thread_id. Returns None if not found.
pub fn get_thread(conn: &Connection, thread_id: &str) -> Result<Option<ThreadRow>> {
    let result = conn
        .query_row(
            "SELECT thread_id, subject, members, created_at, updated_at FROM threads WHERE thread_id = ?1",
            rusqlite::params![thread_id],
            |row| {
                Ok(ThreadRow {
                    thread_id: row.get(0)?,
                    subject: row.get(1)?,
                    members: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

/// Insert a new thread. Returns Ok(true) if inserted, Ok(false) if already exists.
pub fn insert_thread(conn: &Connection, thread: &ThreadRow) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO threads (thread_id, subject, members, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            thread.thread_id,
            thread.subject,
            thread.members,
            thread.created_at,
            thread.updated_at,
        ],
    )?;
    Ok(rows > 0)
}

/// Update thread members JSON column and updated_at timestamp.
pub fn update_thread_members(conn: &Connection, thread_id: &str, members_json: &str, updated_at: &str) -> Result<bool> {
    let rows = conn.execute(
        "UPDATE threads SET members = ?1, updated_at = ?2 WHERE thread_id = ?3",
        rusqlite::params![members_json, updated_at, thread_id],
    )?;
    Ok(rows > 0)
}

/// Add a member to the thread's JSON members array.
/// Deserializes current members, appends new member if not already present, re-serializes.
pub fn add_thread_member(conn: &Connection, thread_id: &str, pubkey: &str, joined_at: &str) -> Result<bool> {
    use crate::types::ThreadMember;

    let thread = get_thread(conn, thread_id)?;
    let thread = match thread {
        Some(t) => t,
        None => return Ok(false),
    };

    let mut members: Vec<ThreadMember> = serde_json::from_str(&thread.members)
        .unwrap_or_default();

    // Idempotent: skip if already a member
    if members.iter().any(|m| m.pubkey == pubkey) {
        return Ok(true);
    }

    members.push(ThreadMember {
        pubkey: pubkey.to_string(),
        joined_at: joined_at.to_string(),
    });

    let members_json = serde_json::to_string(&members)?;
    let now = crate::envelope::now_iso8601();
    update_thread_members(conn, thread_id, &members_json, &now)
}

/// Remove a member from the thread's JSON members array.
pub fn remove_thread_member(conn: &Connection, thread_id: &str, pubkey: &str) -> Result<bool> {
    use crate::types::ThreadMember;

    let thread = get_thread(conn, thread_id)?;
    let thread = match thread {
        Some(t) => t,
        None => return Ok(false),
    };

    let members: Vec<ThreadMember> = serde_json::from_str(&thread.members)
        .unwrap_or_default();
    let filtered: Vec<ThreadMember> = members.into_iter().filter(|m| m.pubkey != pubkey).collect();
    let members_json = serde_json::to_string(&filtered)?;
    let now = crate::envelope::now_iso8601();
    update_thread_members(conn, thread_id, &members_json, &now)
}

/// Get all messages for a thread ordered by created_at ASC.
pub fn get_thread_messages(conn: &Connection, thread_id: &str) -> Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT m.nostr_id, m.direction, m.sender, m.recipient, m.content,
                m.delivery_status, m.read_status, m.created_at, m.received_at,
                c.alias
         FROM messages m
         LEFT JOIN contacts c ON c.pubkey = m.sender
         WHERE m.thread_id = ?1
         ORDER BY m.created_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![thread_id], map_message_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Get all messages for a thread with full v2 metadata (msg_id, reply_to, transport_msg_id).
pub fn get_thread_messages_full(conn: &Connection, thread_id: &str) -> Result<Vec<(MessageRow, crate::types::MessageMeta)>> {
    let mut stmt = conn.prepare(
        "SELECT m.nostr_id, m.direction, m.sender, m.recipient, m.content,
                m.delivery_status, m.read_status, m.created_at, m.received_at,
                c.alias,
                m.msg_id, m.thread_id, m.reply_to, m.transport, m.transport_msg_id
         FROM messages m
         LEFT JOIN contacts c ON c.pubkey = m.sender
         WHERE m.thread_id = ?1
         ORDER BY m.created_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![thread_id], |row| {
        let msg = MessageRow {
            nostr_id: row.get(0)?,
            direction: row.get(1)?,
            sender: row.get(2)?,
            recipient: row.get(3)?,
            content: row.get(4)?,
            delivery_status: row.get(5)?,
            read_status: row.get(6)?,
            created_at: row.get(7)?,
            received_at: row.get(8)?,
            sender_alias: row.get(9)?,
        };
        let meta = crate::types::MessageMeta {
            msg_id: row.get(10)?,
            thread_id: row.get(11)?,
            reply_to: row.get(12)?,
            transport: row.get(13)?,
            transport_msg_id: row.get(14)?,
        };
        Ok((msg, meta))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Look up the transport_msg_id for a given msg_id (for e-tag resolution).
pub fn get_transport_msg_id_by_msg_id(conn: &Connection, msg_id: &str) -> Result<Option<String>> {
    let result = conn
        .query_row(
            "SELECT transport_msg_id FROM messages WHERE msg_id = ?1 LIMIT 1",
            rusqlite::params![msg_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(result)
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
            direction: Direction::In,
            sender: "sender_hex".to_string(),
            recipient: "recipient_hex".to_string(),
            content: "hello".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
            sender_alias: None,
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
            direction: Direction::In,
            sender: "spubkey".to_string(),
            recipient: "rpubkey".to_string(),
            content: "test msg".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
            sender_alias: None,
        };
        insert_message(&conn, &msg).unwrap();

        let msgs = get_messages(&conn, Direction::In, &[]).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "test msg");

        // No 'out' messages
        let out = get_messages(&conn, Direction::Out, &[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn test_store_get_messages_trust_filter() {
        let conn = open_mem();

        // Insert a contact with trust_tier=known
        let contact = ContactRow {
            pubkey: "known_pubkey".to_string(),
            alias: Some("alice".to_string()),
            trust_tier: TrustTier::Known,
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        // Insert message from known sender
        let msg_known = MessageRow {
            nostr_id: "id_known".to_string(),
            direction: Direction::In,
            sender: "known_pubkey".to_string(),
            recipient: "me".to_string(),
            content: "from known".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
            sender_alias: None,
        };
        insert_message(&conn, &msg_known).unwrap();

        // Insert message from unknown sender (no contact record)
        let msg_unknown = MessageRow {
            nostr_id: "id_unknown".to_string(),
            direction: Direction::In,
            sender: "unknown_pubkey".to_string(),
            recipient: "me".to_string(),
            content: "from unknown".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-03-20T00:00:00Z".to_string(),
            received_at: "2026-03-20T00:00:01Z".to_string(),
            sender_alias: None,
        };
        insert_message(&conn, &msg_unknown).unwrap();

        // Filter: known only
        let known_msgs = get_messages(&conn, Direction::In, &[TrustTier::Known]).unwrap();
        assert_eq!(known_msgs.len(), 1);
        assert_eq!(known_msgs[0].sender, "known_pubkey");

        // Filter: unknown only
        let unknown_msgs = get_messages(&conn, Direction::In, &[TrustTier::Unknown]).unwrap();
        assert_eq!(unknown_msgs.len(), 1);
        assert_eq!(unknown_msgs[0].sender, "unknown_pubkey");

        // Both
        let all_msgs = get_messages(&conn, Direction::In, &[TrustTier::Known, TrustTier::Unknown]).unwrap();
        assert_eq!(all_msgs.len(), 2);
    }

    #[test]
    fn test_store_insert_contact() {
        let conn = open_mem();
        let contact = ContactRow {
            pubkey: "pubkey_hex".to_string(),
            alias: Some("alice".to_string()),
            trust_tier: TrustTier::Known,
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        let loaded = get_contact_by_pubkey(&conn, "pubkey_hex").unwrap();
        assert!(loaded.is_some());
        let c = loaded.unwrap();
        assert_eq!(c.alias, Some("alice".to_string()));
        assert_eq!(c.trust_tier, TrustTier::Known);
    }

    #[test]
    fn test_store_get_contact_by_alias() {
        let conn = open_mem();
        let contact = ContactRow {
            pubkey: "pk_hex".to_string(),
            alias: Some("Bob".to_string()),
            trust_tier: TrustTier::Known,
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
            trust_tier: TrustTier::Unknown,
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
                trust_tier: TrustTier::Known,
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
            trust_tier: TrustTier::Known,
            added_at: "2026-03-20T00:00:00Z".to_string(),
        };
        insert_contact(&conn, &contact).unwrap();

        let updated = update_trust_tier(&conn, "pk_test", TrustTier::Blocked).unwrap();
        assert!(updated);

        let c = get_contact_by_pubkey(&conn, "pk_test").unwrap().unwrap();
        assert_eq!(c.trust_tier, TrustTier::Blocked);

        // Non-existent key returns false
        let not_found = update_trust_tier(&conn, "nonexistent", TrustTier::Blocked).unwrap();
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

    #[test]
    fn schema_v2_has_new_columns_and_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();

        // Check messages table has new columns
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(messages)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(cols.contains(&"msg_id".to_string()), "missing msg_id column");
        assert!(cols.contains(&"thread_id".to_string()), "missing thread_id column");
        assert!(cols.contains(&"reply_to".to_string()), "missing reply_to column");
        assert!(cols.contains(&"transport".to_string()), "missing transport column");
        assert!(cols.contains(&"transport_msg_id".to_string()), "missing transport_msg_id column");

        // Check threads table exists
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"threads".to_string()), "missing threads table");

        // Check indexes exist
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_messages_msg_id".to_string()), "missing idx_messages_msg_id");
        assert!(indexes.contains(&"idx_messages_thread_id".to_string()), "missing idx_messages_thread_id");

        // Check user_version = 2
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2, "user_version should be 2");
    }

    #[test]
    fn migration_v1_to_v2_backfills_legacy_ids() {
        // Simulate a v1 database by creating the old schema manually
        let v1_schema = "
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
            PRAGMA user_version = 1;
        ";
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(v1_schema).unwrap();

        // Insert a v1 row
        conn.execute(
            "INSERT INTO messages (nostr_id, direction, sender, recipient, content, delivery_status, read_status, created_at, received_at)
             VALUES ('nostr123', 'in', 'sender', 'recipient', 'hello', 'received', 'unread', '2026-03-20T00:00:00Z', '2026-03-20T00:00:01Z')",
            [],
        ).unwrap();

        // Run migration
        migrate_v1_to_v2(&conn).unwrap();

        // Verify backfill: msg_id should be 'legacy:nostr123'
        let msg_id: String = conn
            .query_row("SELECT msg_id FROM messages WHERE nostr_id = 'nostr123'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(msg_id, "legacy:nostr123");

        // Verify transport_msg_id = nostr_id
        let tmid: String = conn
            .query_row("SELECT transport_msg_id FROM messages WHERE nostr_id = 'nostr123'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tmid, "nostr123");

        // Verify user_version = 2
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn insert_message_v2_stores_meta_fields() {
        let conn = open_mem();
        // Run migration so new columns exist
        migrate_v1_to_v2(&conn).unwrap();

        let msg = MessageRow {
            nostr_id: "nid_v2".to_string(),
            direction: Direction::In,
            sender: "spubkey".to_string(),
            recipient: "rpubkey".to_string(),
            content: "v2 message".to_string(),
            delivery_status: DeliveryStatus::Received,
            read_status: ReadStatus::Unread,
            created_at: "2026-03-23T00:00:00Z".to_string(),
            received_at: "2026-03-23T00:00:01Z".to_string(),
            sender_alias: None,
        };
        let meta = crate::types::MessageMeta {
            msg_id: Some("019d1a2b-test".to_string()),
            thread_id: Some("thread-abc".to_string()),
            reply_to: None,
            transport: Some("nostr".to_string()),
            transport_msg_id: Some("nid_v2".to_string()),
        };

        let inserted = insert_message_v2(&conn, &msg, &meta).unwrap();
        assert!(inserted, "v2 insert should succeed");

        // Verify stored meta fields
        let stored_msg_id: String = conn
            .query_row("SELECT msg_id FROM messages WHERE nostr_id = 'nid_v2'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stored_msg_id, "019d1a2b-test");
    }
}
