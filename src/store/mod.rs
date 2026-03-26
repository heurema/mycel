use anyhow::Result;
use nostr_sdk::prelude::*;
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use crate::error::MAX_RETRIES;
use crate::types::{AckStatus, DeliveryStatus, Direction, ReadStatus, TrustTier};

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
    transport_msg_id TEXT,
    source_frame_id  TEXT
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

CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id, direction);
CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);
CREATE INDEX IF NOT EXISTS idx_messages_source_frame_id ON messages(source_frame_id);

CREATE TABLE IF NOT EXISTS threads (
    thread_id   TEXT PRIMARY KEY,
    subject     TEXT,
    members     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS outbox (
    msg_id          TEXT PRIMARY KEY CHECK(msg_id != ''),
    recipient_hex   TEXT NOT NULL,
    envelope_json   TEXT NOT NULL CHECK(envelope_json != ''),
    relay_urls      TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending',
    retry_count     INTEGER NOT NULL DEFAULT 0,
    ok_relay_count  INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    last_attempt_at TEXT,
    next_retry_at   TEXT,
    sent_at         TEXT
);

CREATE INDEX IF NOT EXISTS idx_outbox_status_retry
    ON outbox(status, next_retry_at);

CREATE TABLE IF NOT EXISTS acks (
    msg_id      TEXT NOT NULL,
    ack_sender  TEXT NOT NULL,
    ack_status  TEXT NOT NULL DEFAULT 'pending',
    created_at  TEXT NOT NULL,
    sent_at     TEXT,
    PRIMARY KEY (msg_id, ack_sender)
);

CREATE INDEX IF NOT EXISTS idx_acks_msg_id ON acks(msg_id);

CREATE TABLE IF NOT EXISTS ingress_frames (
    frame_id           TEXT PRIMARY KEY,
    transport          TEXT NOT NULL,
    endpoint_id        TEXT,
    agent_ref          TEXT,
    transport_msg_id   TEXT,
    sender_hint        TEXT,
    recipient_hint     TEXT,
    envelope_json      TEXT NOT NULL,
    auth_meta_json     TEXT,
    received_at        TEXT NOT NULL,
    processed_at       TEXT,
    status             TEXT NOT NULL DEFAULT 'pending',
    error              TEXT
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_ingress_transport_msg
    ON ingress_frames(transport, transport_msg_id);

CREATE TABLE IF NOT EXISTS agent_endpoints (
    endpoint_id        TEXT PRIMARY KEY,
    agent_ref          TEXT NOT NULL,
    transport          TEXT NOT NULL,
    address            TEXT NOT NULL,
    priority           INTEGER NOT NULL DEFAULT 100,
    enabled            INTEGER NOT NULL DEFAULT 1,
    metadata_json      TEXT,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_endpoints_unique
    ON agent_endpoints(agent_ref, transport, address);

CREATE INDEX IF NOT EXISTS idx_agent_endpoints_lookup
    ON agent_endpoints(agent_ref, transport, enabled, priority);

PRAGMA user_version = 6;
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

/// Migration script to upgrade a user_version=1 or user_version=2 database to user_version=3.
/// Adds the outbox table and index for store-and-retry delivery.
/// MIGRATION_V1_TO_V3: applied when user_version < 3 (after v2 migration if needed).
const MIGRATION_V1_TO_V3: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS outbox (
        msg_id          TEXT PRIMARY KEY,
        recipient_hex   TEXT NOT NULL,
        envelope_json   TEXT NOT NULL,
        relay_urls      TEXT NOT NULL,
        status          TEXT NOT NULL DEFAULT 'pending',
        retry_count     INTEGER NOT NULL DEFAULT 0,
        ok_relay_count  INTEGER NOT NULL DEFAULT 0,
        created_at      TEXT NOT NULL,
        last_attempt_at TEXT,
        next_retry_at   TEXT,
        sent_at         TEXT
    )",
    "CREATE INDEX IF NOT EXISTS idx_outbox_status_retry ON outbox(status, next_retry_at)",
    "PRAGMA user_version = 3",
];

/// Migration script to upgrade a user_version=3 database to user_version=4.
/// Adds the acks table for application-level acknowledgement tracking.
/// MIGRATION_V3_TO_V4: applied when user_version < 4.
const MIGRATION_V3_TO_V4: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS acks (
        msg_id      TEXT NOT NULL,
        ack_sender  TEXT NOT NULL,
        ack_status  TEXT NOT NULL DEFAULT 'pending',
        created_at  TEXT NOT NULL,
        sent_at     TEXT,
        PRIMARY KEY (msg_id, ack_sender)
    )",
    "CREATE INDEX IF NOT EXISTS idx_acks_msg_id ON acks(msg_id)",
    "PRAGMA user_version = 4",
];

/// Migration script to upgrade a user_version=4 database to user_version=5.
/// Adds ingress_frames and links normalized messages back to source_frame_id.
const MIGRATION_V4_TO_V5: &[&str] = &[
    "ALTER TABLE messages ADD COLUMN source_frame_id TEXT",
    "CREATE INDEX IF NOT EXISTS idx_messages_source_frame_id ON messages(source_frame_id)",
    "CREATE TABLE IF NOT EXISTS ingress_frames (
        frame_id           TEXT PRIMARY KEY,
        transport          TEXT NOT NULL,
        endpoint_id        TEXT,
        agent_ref          TEXT,
        transport_msg_id   TEXT,
        sender_hint        TEXT,
        recipient_hint     TEXT,
        envelope_json      TEXT NOT NULL,
        auth_meta_json     TEXT,
        received_at        TEXT NOT NULL,
        processed_at       TEXT,
        status             TEXT NOT NULL DEFAULT 'pending',
        error              TEXT
    )",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_ingress_transport_msg ON ingress_frames(transport, transport_msg_id)",
    "PRAGMA user_version = 5",
];

/// Migration script to upgrade a user_version=5 database to user_version=6.
/// Adds the agent_endpoints directory table.
const MIGRATION_V5_TO_V6: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS agent_endpoints (
        endpoint_id        TEXT PRIMARY KEY,
        agent_ref          TEXT NOT NULL,
        transport          TEXT NOT NULL,
        address            TEXT NOT NULL,
        priority           INTEGER NOT NULL DEFAULT 100,
        enabled            INTEGER NOT NULL DEFAULT 1,
        metadata_json      TEXT,
        created_at         TEXT NOT NULL,
        updated_at         TEXT NOT NULL
    )",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_endpoints_unique ON agent_endpoints(agent_ref, transport, address)",
    "CREATE INDEX IF NOT EXISTS idx_agent_endpoints_lookup ON agent_endpoints(agent_ref, transport, enabled, priority)",
    "PRAGMA user_version = 6",
];

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

    // Read user_version before applying schema so we know whether to run migrations first.
    // For existing databases we must add missing columns before SCHEMA creates indexes on them.
    let existing_version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if existing_version == 0 {
        conn.execute_batch(SCHEMA)?;
    } else {
        if existing_version < 2 {
            migrate_v1_to_v2(&conn)?;
        }
        if existing_version < 3 {
            migrate_to_v3(&conn)?;
        }
        if existing_version < 4 {
            migrate_to_v4(&conn)?;
        }
        if existing_version < 5 {
            migrate_to_v5(&conn)?;
        }
        if existing_version < 6 {
            migrate_to_v6(&conn)?;
        }
        conn.execute_batch(SCHEMA)?;
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

/// Applies the migration to v3: adds outbox table and index.
/// Idempotent: all steps use IF NOT EXISTS clauses.
fn migrate_to_v3(conn: &Connection) -> Result<()> {
    for stmt in MIGRATION_V1_TO_V3 {
        conn.execute_batch(stmt)?;
    }
    Ok(())
}

/// Applies the migration to v4: adds acks table.
/// Idempotent: all steps use IF NOT EXISTS clauses.
fn migrate_to_v4(conn: &Connection) -> Result<()> {
    for stmt in MIGRATION_V3_TO_V4 {
        conn.execute_batch(stmt)?;
    }
    Ok(())
}

/// Applies the migration to v5: adds ingress_frames and source_frame_id.
/// Idempotent: duplicate-column ADD COLUMN errors are skipped.
fn migrate_to_v5(conn: &Connection) -> Result<()> {
    for stmt in MIGRATION_V4_TO_V5 {
        match conn.execute_batch(stmt) {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("duplicate column name") {
                    // source_frame_id already exists
                } else {
                    return Err(e.into());
                }
            }
        }
    }
    Ok(())
}

/// Applies the migration to v6: adds agent_endpoints.
/// Idempotent: all steps use IF NOT EXISTS clauses.
fn migrate_to_v6(conn: &Connection) -> Result<()> {
    for stmt in MIGRATION_V5_TO_V6 {
        conn.execute_batch(stmt)?;
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

/// Raw transport frame stored before shared ingest/materialization.
#[derive(Debug, Clone)]
pub struct IngressFrameRow {
    pub frame_id: String,
    pub transport: String,
    pub endpoint_id: Option<String>,
    pub agent_ref: Option<String>,
    pub transport_msg_id: Option<String>,
    pub sender_hint: Option<String>,
    pub recipient_hint: Option<String>,
    pub envelope_json: String,
    pub auth_meta_json: Option<String>,
    pub received_at: String,
    pub processed_at: Option<String>,
    pub status: String,
    pub error: Option<String>,
}

/// A directory entry describing how to reach an agent over a specific transport.
#[derive(Debug, Clone)]
pub struct AgentEndpointRow {
    pub endpoint_id: String,
    pub agent_ref: String,
    pub transport: String,
    pub address: String,
    pub priority: i64,
    pub enabled: bool,
    pub metadata_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// A row in the outbox table — tracks pending/sent/failed outbound Nostr messages.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub msg_id: String,
    pub recipient_hex: String,
    pub envelope_json: String,
    pub relay_urls: String, // JSON array of relay URL strings
    pub status: String,     // 'pending' | 'sent' | 'failed_permanent'
    pub retry_count: u32,
    pub ok_relay_count: u32,
    pub created_at: String,
    pub last_attempt_at: Option<String>,
    pub next_retry_at: Option<String>,
    pub sent_at: Option<String>,
}

/// A row in the acks table — tracks application-level acknowledgements sent/received.
#[derive(Debug, Clone)]
pub struct AckRow {
    /// The msg_id of the message being acknowledged.
    pub msg_id: String,
    /// The pubkey (hex) of the party sending the ACK.
    pub ack_sender: String,
    /// Current status of this ACK.
    pub ack_status: AckStatus,
    /// ISO 8601 UTC timestamp when the ACK was created locally.
    pub created_at: String,
    /// ISO 8601 UTC timestamp when the ACK was sent, if sent.
    pub sent_at: Option<String>,
}

// ---------------------------------------------------------------------------
// Ack operations
// ---------------------------------------------------------------------------

/// Insert an ACK row. Returns Ok(true) if inserted, Ok(false) if duplicate (INSERT OR IGNORE).
pub fn insert_ack(conn: &Connection, ack: &AckRow) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO acks (msg_id, ack_sender, ack_status, created_at, sent_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            ack.msg_id,
            ack.ack_sender,
            ack.ack_status,
            ack.created_at,
            ack.sent_at,
        ],
    )?;
    Ok(rows > 0)
}

/// Return all ACK rows in 'pending' status.
pub fn get_pending_acks(conn: &Connection) -> Result<Vec<AckRow>> {
    let mut stmt = conn.prepare(
        "SELECT msg_id, ack_sender, ack_status, created_at, sent_at
         FROM acks WHERE ack_status = 'pending'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AckRow {
            msg_id: row.get(0)?,
            ack_sender: row.get(1)?,
            ack_status: row.get(2)?,
            created_at: row.get(3)?,
            sent_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// Ingress operations
// ---------------------------------------------------------------------------

/// Insert a raw ingress frame. Returns Ok(true) if inserted, Ok(false) if duplicate.
pub fn insert_ingress_frame(conn: &Connection, frame: &IngressFrameRow) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO ingress_frames
            (frame_id, transport, endpoint_id, agent_ref, transport_msg_id,
             sender_hint, recipient_hint, envelope_json, auth_meta_json,
             received_at, processed_at, status, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        rusqlite::params![
            frame.frame_id,
            frame.transport,
            frame.endpoint_id,
            frame.agent_ref,
            frame.transport_msg_id,
            frame.sender_hint,
            frame.recipient_hint,
            frame.envelope_json,
            frame.auth_meta_json,
            frame.received_at,
            frame.processed_at,
            frame.status,
            frame.error,
        ],
    )?;
    Ok(rows > 0)
}

/// Return all pending ingress frames oldest-first.
pub fn get_pending_ingress_frames(conn: &Connection) -> Result<Vec<IngressFrameRow>> {
    let mut stmt = conn.prepare(
        "SELECT frame_id, transport, endpoint_id, agent_ref, transport_msg_id,
                sender_hint, recipient_hint, envelope_json, auth_meta_json,
                received_at, processed_at, status, error
         FROM ingress_frames
         WHERE status = 'pending'
         ORDER BY received_at ASC, frame_id ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(IngressFrameRow {
            frame_id: row.get(0)?,
            transport: row.get(1)?,
            endpoint_id: row.get(2)?,
            agent_ref: row.get(3)?,
            transport_msg_id: row.get(4)?,
            sender_hint: row.get(5)?,
            recipient_hint: row.get(6)?,
            envelope_json: row.get(7)?,
            auth_meta_json: row.get(8)?,
            received_at: row.get(9)?,
            processed_at: row.get(10)?,
            status: row.get(11)?,
            error: row.get(12)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Mark an ingress frame as processed with final status.
pub fn update_ingress_frame_result(
    conn: &Connection,
    frame_id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE ingress_frames
         SET status = ?1, error = ?2, processed_at = ?3
         WHERE frame_id = ?4",
        rusqlite::params![status, error, crate::envelope::now_iso8601(), frame_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent endpoint directory operations
// ---------------------------------------------------------------------------

/// Insert or update an agent endpoint by endpoint_id while preserving created_at.
pub fn upsert_agent_endpoint(conn: &Connection, endpoint: &AgentEndpointRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_endpoints
            (endpoint_id, agent_ref, transport, address, priority, enabled,
             metadata_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(endpoint_id) DO UPDATE SET
            agent_ref = excluded.agent_ref,
            transport = excluded.transport,
            address = excluded.address,
            priority = excluded.priority,
            enabled = excluded.enabled,
            metadata_json = excluded.metadata_json,
            updated_at = excluded.updated_at",
        rusqlite::params![
            endpoint.endpoint_id,
            endpoint.agent_ref,
            endpoint.transport,
            endpoint.address,
            endpoint.priority,
            endpoint.enabled,
            endpoint.metadata_json,
            endpoint.created_at,
            endpoint.updated_at,
        ],
    )?;
    Ok(())
}

/// Return the highest-priority enabled endpoint for an agent and transport.
pub fn get_agent_endpoint(
    conn: &Connection,
    agent_ref: &str,
    transport: &str,
) -> Result<Option<AgentEndpointRow>> {
    conn.query_row(
        "SELECT endpoint_id, agent_ref, transport, address, priority, enabled,
                metadata_json, created_at, updated_at
         FROM agent_endpoints
         WHERE agent_ref = ?1 AND transport = ?2 AND enabled = 1
         ORDER BY priority ASC, updated_at DESC
         LIMIT 1",
        rusqlite::params![agent_ref, transport],
        |row| {
            Ok(AgentEndpointRow {
                endpoint_id: row.get(0)?,
                agent_ref: row.get(1)?,
                transport: row.get(2)?,
                address: row.get(3)?,
                priority: row.get(4)?,
                enabled: row.get(5)?,
                metadata_json: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Message operations
// ---------------------------------------------------------------------------

/// Insert a message; returns Ok(true) if inserted, Ok(false) if duplicate (INSERT OR IGNORE)
pub fn insert_message(conn: &Connection, msg: &MessageRow) -> Result<bool> {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id,
             source_frame_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
            None::<&str>, // msg_id: caller may backfill via insert_message_v2
            None::<&str>, // thread_id
            None::<&str>, // reply_to
            "nostr",      // transport: default for v1-compat inserts
            None::<&str>, // transport_msg_id
            None::<&str>, // source_frame_id
        ],
    )?;
    Ok(rows > 0)
}

/// Insert a message with full v2 metadata fields.
/// Returns Ok(true) if inserted, Ok(false) if duplicate.
///
/// Dedup strategy:
///   - v2 (msg_id present): INSERT ... SELECT ... WHERE NOT EXISTS (msg_id match) —
///     handles outbox retries that reuse the same msg_id with a new nostr_id.
///   - v1 / legacy: (msg_id absent) INSERT OR IGNORE on nostr_id PRIMARY KEY.
pub fn insert_message_v2(
    conn: &Connection,
    msg: &MessageRow,
    meta: &crate::types::MessageMeta,
) -> Result<bool> {
    let rows = if meta
        .msg_id
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        // v2 msg_id dedup: use WHERE NOT EXISTS to prevent duplicate logical messages
        // even when nostr_id differs (e.g. outbox retry with new event ID)
        conn.execute(
            "INSERT OR IGNORE INTO messages
                (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
                 created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id,
                 source_frame_id)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15
             WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?10 AND direction = ?2 AND msg_id IS NOT NULL AND msg_id != '')",
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
                meta.source_frame_id.as_deref(),
            ],
        )?
    } else {
        // legacy: no msg_id, fall back to nostr_id PK dedup
        conn.execute(
            "INSERT OR IGNORE INTO messages
                (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
                 created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id,
                 source_frame_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                meta.source_frame_id.as_deref(),
            ],
        )?
    };
    Ok(rows > 0)
}

/// Insert a local-transport message using msg_id as the dedup key (INSERT OR IGNORE).
/// Returns Ok(true) if inserted, Ok(false) if a row with the same msg_id already exists.
/// nostr_id is set to msg_id for local messages (no Nostr event ID).
pub fn insert_message_local(
    conn: &Connection,
    msg: &MessageRow,
    meta: &crate::types::MessageMeta,
) -> Result<bool> {
    let msg_id = meta.msg_id.as_deref().unwrap_or("");
    // Guard: empty msg_id would bypass dedup — callers must provide a real ID
    if msg_id.is_empty() {
        return Err(anyhow::anyhow!(
            "msg_id is required for local message insert"
        ));
    }
    // Use msg_id as nostr_id placeholder for local messages so the PK is populated.
    let nostr_id = if msg.nostr_id.is_empty() {
        msg_id
    } else {
        &msg.nostr_id
    };
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id,
             source_frame_id)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15
         WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?10 AND direction = ?2 AND msg_id IS NOT NULL AND msg_id != '')",
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
            meta.source_frame_id.as_deref(),
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
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(direction)];
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
#[allow(dead_code)] // Thread member management CLI lands in v0.3
pub fn update_thread_members(
    conn: &Connection,
    thread_id: &str,
    members_json: &str,
    updated_at: &str,
) -> Result<bool> {
    let rows = conn.execute(
        "UPDATE threads SET members = ?1, updated_at = ?2 WHERE thread_id = ?3",
        rusqlite::params![members_json, updated_at, thread_id],
    )?;
    Ok(rows > 0)
}

/// Add a member to the thread's JSON members array.
/// Deserializes current members, appends new member if not already present, re-serializes.
#[allow(dead_code)] // Thread member management CLI lands in v0.3
pub fn add_thread_member(
    conn: &Connection,
    thread_id: &str,
    pubkey: &str,
    joined_at: &str,
) -> Result<bool> {
    use crate::types::ThreadMember;

    let thread = get_thread(conn, thread_id)?;
    let thread = match thread {
        Some(t) => t,
        None => return Ok(false),
    };

    let mut members: Vec<ThreadMember> = serde_json::from_str(&thread.members).unwrap_or_default();

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
#[allow(dead_code)] // Thread member management CLI lands in v0.3
pub fn remove_thread_member(conn: &Connection, thread_id: &str, pubkey: &str) -> Result<bool> {
    use crate::types::ThreadMember;

    let thread = get_thread(conn, thread_id)?;
    let thread = match thread {
        Some(t) => t,
        None => return Ok(false),
    };

    let members: Vec<ThreadMember> = serde_json::from_str(&thread.members).unwrap_or_default();
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
pub fn get_thread_messages_full(
    conn: &Connection,
    thread_id: &str,
) -> Result<Vec<(MessageRow, crate::types::MessageMeta)>> {
    let mut stmt = conn.prepare(
        "SELECT m.nostr_id, m.direction, m.sender, m.recipient, m.content,
                m.delivery_status, m.read_status, m.created_at, m.received_at,
                c.alias,
                m.msg_id, m.thread_id, m.reply_to, m.transport, m.transport_msg_id, m.source_frame_id
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
            source_frame_id: row.get(15)?,
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

// ---------------------------------------------------------------------------
// Outbox operations
// ---------------------------------------------------------------------------

/// Compute next retry timestamp as ISO8601 UTC string.
/// Formula: now + min(30 * 2^retry_count, 3600) seconds (exponential backoff, capped at 3600).
pub fn compute_next_retry_at(retry_count: u32) -> String {
    // 30 * 2^retry_count, capped at 3600
    let backoff_secs: u64 = (30u64.saturating_mul(1u64 << retry_count.min(31))).min(3600);
    use std::time::SystemTime;
    let now_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::envelope::timestamp_to_iso8601(now_secs + backoff_secs)
}

/// Insert an outbox row for a pending outbound message.
pub fn insert_outbox(conn: &Connection, row: &OutboxRow) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO outbox
            (msg_id, recipient_hex, envelope_json, relay_urls, status, retry_count,
             ok_relay_count, created_at, last_attempt_at, next_retry_at, sent_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            row.msg_id,
            row.recipient_hex,
            row.envelope_json,
            row.relay_urls,
            row.status,
            row.retry_count,
            row.ok_relay_count,
            row.created_at,
            row.last_attempt_at,
            row.next_retry_at,
            row.sent_at,
        ],
    )?;
    Ok(())
}

/// Update outbox row to sent status after successful relay publish.
pub fn update_outbox_sent(
    conn: &Connection,
    msg_id: &str,
    ok_relay_count: u32,
    sent_at: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'sent', sent_at = ?1, ok_relay_count = ?2,
                           last_attempt_at = ?1
         WHERE msg_id = ?3",
        rusqlite::params![sent_at, ok_relay_count, msg_id],
    )?;
    Ok(())
}

/// Update outbox row on relay failure: increment retry_count and set next_retry_at.
pub fn update_outbox_retry(
    conn: &Connection,
    msg_id: &str,
    retry_count: u32,
    next_retry_at: &str,
    now: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET retry_count = ?1, next_retry_at = ?2, last_attempt_at = ?3
         WHERE msg_id = ?4",
        rusqlite::params![retry_count, next_retry_at, now, msg_id],
    )?;
    Ok(())
}

/// Mark an outbox row as failed_permanent (max retries exceeded).
pub fn update_outbox_failed(conn: &Connection, msg_id: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE outbox SET status = 'failed_permanent', last_attempt_at = ?1 WHERE msg_id = ?2",
        rusqlite::params![now, msg_id],
    )?;
    Ok(())
}

/// Flush pending outbox rows: retry any pending messages whose next_retry_at is in the past.
/// Also runs cleanup:
/// - delete sent rows older than 7 days by `sent_at` (fallback `created_at`)
/// - delete failed_permanent rows older than 30 days by `last_attempt_at` (fallback `created_at`)
pub async fn flush_outbox(db: &Db, keys: &Keys, relay_urls: Vec<String>) -> Result<()> {
    use crate::envelope::Envelope;
    use crate::nostr as mycel_nostr;

    db.run(move |conn| {
        conn.execute(
            "DELETE FROM outbox
             WHERE status = 'sent'
               AND COALESCE(sent_at, created_at) < datetime('now', '-7 days')",
            [],
        )?;
        conn.execute(
            "DELETE FROM outbox
             WHERE status = 'failed_permanent'
               AND COALESCE(last_attempt_at, created_at) < datetime('now', '-30 days')",
            [],
        )?;
        Ok(())
    })
    .await?;

    // Fetch pending rows due for retry
    let pending_rows: Vec<OutboxRow> = db
        .run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT msg_id, recipient_hex, envelope_json, relay_urls, status, retry_count,
                    ok_relay_count, created_at, last_attempt_at, next_retry_at, sent_at
             FROM outbox
             WHERE status = 'pending'
               AND (next_retry_at IS NULL OR next_retry_at <= datetime('now'))",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(OutboxRow {
                    msg_id: row.get(0)?,
                    recipient_hex: row.get(1)?,
                    envelope_json: row.get(2)?,
                    relay_urls: row.get(3)?,
                    status: row.get(4)?,
                    retry_count: row.get::<_, u32>(5)?,
                    ok_relay_count: row.get::<_, u32>(6)?,
                    created_at: row.get(7)?,
                    last_attempt_at: row.get(8)?,
                    next_retry_at: row.get(9)?,
                    sent_at: row.get(10)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;

    if pending_rows.is_empty() {
        return Ok(());
    }

    // Build a Nostr client for publishing
    let timeout = Duration::from_secs(10);
    let client = match mycel_nostr::build_client(keys.clone(), &relay_urls).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("flush_outbox: could not connect to relays: {e}");
            return Ok(());
        }
    };

    for row in pending_rows {
        let now = crate::envelope::now_iso8601();

        // Deserialize envelope to rebuild rumor
        let env: Envelope = match serde_json::from_str(&row.envelope_json) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "flush_outbox: poison-pill — invalid envelope_json for {}: {e}",
                    row.msg_id
                );
                let mid = row.msg_id.clone();
                let _ = db.run(move |conn| {
                    conn.execute(
                        "UPDATE outbox SET status = 'failed_permanent', last_attempt_at = datetime('now') WHERE msg_id = ?1",
                        rusqlite::params![mid],
                    )?;
                    Ok(())
                }).await;
                continue;
            }
        };

        // Parse recipient pubkey
        let recipient_pk = match PublicKey::from_hex(&row.recipient_hex) {
            Ok(pk) => pk,
            Err(e) => {
                tracing::warn!(
                    "flush_outbox: poison-pill — invalid recipient for {}: {e}",
                    row.msg_id
                );
                let mid = row.msg_id.clone();
                let _ = db.run(move |conn| {
                    conn.execute(
                        "UPDATE outbox SET status = 'failed_permanent', last_attempt_at = datetime('now') WHERE msg_id = ?1",
                        rusqlite::params![mid],
                    )?;
                    Ok(())
                }).await;
                continue;
            }
        };

        // Re-serialize envelope for the rumor content (same msg_id, fresh gift-wrap)
        let env_json = match serde_json::to_string(&env) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("flush_outbox: serialize envelope for {}: {e}", row.msg_id);
                continue;
            }
        };

        // Rebuild rumor from envelope_json
        let rumor: UnsignedEvent =
            EventBuilder::new(Kind::PrivateDirectMessage, &env_json).build(keys.public_key());

        // Parse stored relay_urls (JSON array), fall back to config relay_urls
        let publish_relays: Vec<String> =
            serde_json::from_str(&row.relay_urls).unwrap_or_else(|_| relay_urls.clone());

        // Attempt publish
        let result =
            mycel_nostr::publish_gift_wrap(&client, &publish_relays, &recipient_pk, rumor, timeout)
                .await;

        let new_retry_count = row.retry_count + 1;
        let msg_id = row.msg_id.clone();

        match result {
            Ok((_event_id, ok_count)) if ok_count > 0 => {
                let now_clone = now.clone();
                db.run(move |conn| update_outbox_sent(conn, &msg_id, ok_count as u32, &now_clone))
                    .await?;
            }
            _ => {
                // Failure or zero relays accepted
                if new_retry_count >= MAX_RETRIES {
                    let now_clone = now.clone();
                    db.run(move |conn| update_outbox_failed(conn, &msg_id, &now_clone))
                        .await?;
                } else {
                    let next_retry = compute_next_retry_at(new_retry_count);
                    let now_clone = now.clone();
                    db.run(move |conn| {
                        update_outbox_retry(conn, &msg_id, new_retry_count, &next_retry, &now_clone)
                    })
                    .await?;
                }
            }
        }
    }

    client.disconnect().await;
    Ok(())
}

/// Return (nostr_id_hex, unix_timestamp) for all inbound messages.
/// Used to build the negentropy known-items set for NIP-77 reconciliation.
pub fn get_known_nostr_ids(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT nostr_id, COALESCE(CAST(strftime('%s', created_at) AS INTEGER), 0) FROM messages WHERE direction = 'in'"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
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
        assert!(tables.contains(&"agent_endpoints".to_string()));
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
        let all_msgs = get_messages(
            &conn,
            Direction::In,
            &[TrustTier::Known, TrustTier::Unknown],
        )
        .unwrap();
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
    fn test_upsert_and_get_agent_endpoint() {
        let conn = open_mem();
        let now = crate::envelope::now_iso8601();

        upsert_agent_endpoint(
            &conn,
            &AgentEndpointRow {
                endpoint_id: "config:local_direct:codex".to_string(),
                agent_ref: "codex".to_string(),
                transport: "local_direct".to_string(),
                address: "/tmp/codex.db".to_string(),
                priority: 100,
                enabled: true,
                metadata_json: Some("{\"pubkey_hex\":\"abc\"}".to_string()),
                created_at: now.clone(),
                updated_at: now.clone(),
            },
        )
        .unwrap();

        upsert_agent_endpoint(
            &conn,
            &AgentEndpointRow {
                endpoint_id: "config:local_direct:codex".to_string(),
                agent_ref: "codex".to_string(),
                transport: "local_direct".to_string(),
                address: "/tmp/codex-v2.db".to_string(),
                priority: 50,
                enabled: true,
                metadata_json: Some("{\"pubkey_hex\":\"def\"}".to_string()),
                created_at: "2020-01-01T00:00:00Z".to_string(),
                updated_at: crate::envelope::now_iso8601(),
            },
        )
        .unwrap();

        let endpoint = get_agent_endpoint(&conn, "codex", "local_direct")
            .unwrap()
            .expect("endpoint");
        assert_eq!(endpoint.endpoint_id, "config:local_direct:codex");
        assert_eq!(endpoint.address, "/tmp/codex-v2.db");
        assert_eq!(endpoint.priority, 50);
        assert_eq!(
            endpoint.metadata_json.as_deref(),
            Some("{\"pubkey_hex\":\"def\"}")
        );
        assert_eq!(
            endpoint.created_at, now,
            "created_at must be preserved on upsert"
        );
    }

    #[test]
    fn schema_has_required_columns_and_indexes() {
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
        assert!(
            cols.contains(&"msg_id".to_string()),
            "missing msg_id column"
        );
        assert!(
            cols.contains(&"thread_id".to_string()),
            "missing thread_id column"
        );
        assert!(
            cols.contains(&"reply_to".to_string()),
            "missing reply_to column"
        );
        assert!(
            cols.contains(&"transport".to_string()),
            "missing transport column"
        );
        assert!(
            cols.contains(&"transport_msg_id".to_string()),
            "missing transport_msg_id column"
        );
        assert!(
            cols.contains(&"source_frame_id".to_string()),
            "missing source_frame_id column"
        );

        // Check threads table exists
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            tables.contains(&"threads".to_string()),
            "missing threads table"
        );
        assert!(
            tables.contains(&"ingress_frames".to_string()),
            "missing ingress_frames table"
        );
        assert!(
            tables.contains(&"agent_endpoints".to_string()),
            "missing agent_endpoints table"
        );

        // Check indexes exist
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            indexes.contains(&"idx_messages_msg_id".to_string()),
            "missing idx_messages_msg_id"
        );
        assert!(
            indexes.contains(&"idx_messages_thread_id".to_string()),
            "missing idx_messages_thread_id"
        );
        assert!(
            indexes.contains(&"idx_messages_source_frame_id".to_string()),
            "missing idx_messages_source_frame_id"
        );
        assert!(
            indexes.contains(&"idx_ingress_transport_msg".to_string()),
            "missing idx_ingress_transport_msg"
        );
        assert!(
            indexes.contains(&"idx_agent_endpoints_unique".to_string()),
            "missing idx_agent_endpoints_unique"
        );
        assert!(
            indexes.contains(&"idx_agent_endpoints_lookup".to_string()),
            "missing idx_agent_endpoints_lookup"
        );

        // Check user_version = 6
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 6, "user_version should be 6");
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
            .query_row(
                "SELECT msg_id FROM messages WHERE nostr_id = 'nostr123'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(msg_id, "legacy:nostr123");

        // Verify transport_msg_id = nostr_id
        let tmid: String = conn
            .query_row(
                "SELECT transport_msg_id FROM messages WHERE nostr_id = 'nostr123'",
                [],
                |row| row.get(0),
            )
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
            source_frame_id: None,
        };

        let inserted = insert_message_v2(&conn, &msg, &meta).unwrap();
        assert!(inserted, "v2 insert should succeed");

        // Verify stored meta fields
        let stored_msg_id: String = conn
            .query_row(
                "SELECT msg_id FROM messages WHERE nostr_id = 'nid_v2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_msg_id, "019d1a2b-test");
    }

    #[test]
    fn open_migrates_v1_db_via_file() {
        // E2E migration test: create a v1 DB on disk, then open() it.
        // This is the exact path that failed in production: SCHEMA's CREATE INDEX
        // on msg_id ran before ALTER TABLE ADD COLUMN msg_id.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("mycel.db");

        // Step 1: Create a v1 database with real data
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            conn.execute_batch(
                "
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
                CREATE TABLE IF NOT EXISTS sync_state (
                    relay_url   TEXT PRIMARY KEY,
                    last_sync   INTEGER NOT NULL DEFAULT 0
                );
                PRAGMA user_version = 1;
            ",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (nostr_id, direction, sender, recipient, content, \
                 delivery_status, read_status, created_at, received_at) \
                 VALUES ('ev_abc', 'in', 'sender_hex', 'recip_hex', 'old msg', \
                         'received', 'unread', '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z')",
                [],
            )
            .unwrap();
        }

        // Step 2: open() must migrate without error
        let conn = open(&db_path).expect("open() must migrate v1 DB without error");

        // Step 3: Verify migration happened
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 6, "user_version must be 6 after migration");

        let msg_id: String = conn
            .query_row(
                "SELECT msg_id FROM messages WHERE nostr_id = 'ev_abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(msg_id, "legacy:ev_abc", "v1 msg must get legacy: prefix");

        let transport: String = conn
            .query_row(
                "SELECT transport FROM messages WHERE nostr_id = 'ev_abc'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(transport, "nostr", "v1 messages must get transport=nostr");
    }
}
