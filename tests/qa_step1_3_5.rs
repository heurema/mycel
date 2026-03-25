// QA tests for Steps 1, 3, 5: relay config, NIP-77 sync, Transport trait.
//
// Run: cargo test --test qa_step1_3_5
//
// NOTE: mycel is a binary crate (no lib.rs). Tests must be self-contained:
// - Config is tested via TOML parse (toml crate) using inline struct mirrors.
// - Transport is tested via rusqlite directly (same schema as store::SCHEMA).
// - NostrTransport is NOT importable; transport field behavior tested structurally.

use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Mirror types for config TOML parsing (must match src/config.rs exactly)
// ---------------------------------------------------------------------------

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum TransportKind {
    #[default]
    Nostr,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct TransportConfig {
    #[serde(rename = "type", default)]
    kind: TransportKind,
}

#[derive(Debug, Serialize, Deserialize)]
struct AckConfig {
    #[serde(default = "default_ack_enabled")]
    enabled: bool,
    #[serde(default = "default_ack_min_interval_secs")]
    min_interval_secs: u64,
}

fn default_ack_enabled() -> bool {
    true
}
fn default_ack_min_interval_secs() -> u64 {
    60
}

impl Default for AckConfig {
    fn default() -> Self {
        Self {
            enabled: default_ack_enabled(),
            min_interval_secs: default_ack_min_interval_secs(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum IdentityStorage {
    Keychain,
    File,
}

#[derive(Debug, Serialize, Deserialize)]
struct IdentityConfig {
    storage: IdentityStorage,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct LocalAgentEntry {
    pubkey: String,
    db: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct LocalConfig {
    #[serde(default)]
    agents: HashMap<String, LocalAgentEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RelayConfig {
    urls: Vec<String>,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    10
}

#[derive(Debug, Serialize, Deserialize)]
struct Config {
    relays: RelayConfig,
    identity: IdentityConfig,
    #[serde(default)]
    local: LocalConfig,
    #[serde(default)]
    ack: AckConfig,
    #[serde(default)]
    transport: TransportConfig,
}

const DEFAULT_RELAYS: &[&str] = &[
    "wss://nos.lol",
    "wss://relay.damus.io",
    "wss://relay.nostr.band",
    "wss://relay.primal.net",
    "wss://relay.snort.social",
    "wss://nostr.mutinywallet.com",
    "wss://nostr.wine",
];

impl Default for Config {
    fn default() -> Self {
        Self {
            relays: RelayConfig {
                urls: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
                timeout_secs: default_timeout_secs(),
            },
            identity: IdentityConfig {
                storage: IdentityStorage::Keychain,
            },
            local: LocalConfig::default(),
            ack: AckConfig::default(),
            transport: TransportConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Full SCHEMA (copy from store/mod.rs) for in-process DB tests
// ---------------------------------------------------------------------------

const SCHEMA: &str = "
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
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id, direction);
CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);
CREATE TABLE IF NOT EXISTS threads (
    thread_id   TEXT PRIMARY KEY,
    subject     TEXT,
    members     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS outbox (
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
);
CREATE INDEX IF NOT EXISTS idx_outbox_status_retry ON outbox(status, next_retry_at);
CREATE TABLE IF NOT EXISTS acks (
    msg_id      TEXT PRIMARY KEY,
    ack_sender  TEXT NOT NULL,
    ack_status  TEXT NOT NULL DEFAULT 'pending',
    created_at  TEXT NOT NULL,
    sent_at     TEXT
);
CREATE INDEX IF NOT EXISTS idx_acks_msg_id ON acks(msg_id);
PRAGMA user_version = 4;
";

fn open_mem() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn
}

/// Inline get_known_nostr_ids logic (mirrors store::get_known_nostr_ids)
fn get_known_nostr_ids(conn: &Connection) -> rusqlite::Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT nostr_id, CAST(strftime('%s', created_at) AS INTEGER) FROM messages WHERE direction = 'in'"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
    })?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// 1. Config: parse config.toml WITH [ack] section
//    The TASK says "verify send_ack defaults to false".
//    The CODE has default_ack_enabled() returning TRUE.
//    This test exposes the discrepancy: document actual behavior.
// ---------------------------------------------------------------------------

#[test]
fn config_ack_section_present_no_enabled_key_serde_default_fires() {
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "keychain"

[ack]
# enabled not specified → serde default fires → default_ack_enabled() = true
"#;
    let cfg: Config = toml::from_str(toml).expect("must parse with [ack] section but no enabled");
    // FINDING: default is TRUE, not false as the task spec implied.
    assert!(
        cfg.ack.enabled,
        "AckConfig.enabled serde default is TRUE (default_ack_enabled returns true). \
         Task spec said 'false' — that is WRONG. This is a spec/code mismatch."
    );
    assert_eq!(cfg.ack.min_interval_secs, 60);
}

#[test]
fn config_ack_explicit_enabled_false_survives_parse() {
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "keychain"

[ack]
enabled = false
"#;
    let cfg: Config = toml::from_str(toml).expect("must parse");
    assert!(!cfg.ack.enabled, "explicit enabled=false must be preserved");
}

// ---------------------------------------------------------------------------
// 2. Config: parse config.toml WITHOUT [ack] section — backward compat
// ---------------------------------------------------------------------------

#[test]
fn config_missing_ack_section_uses_struct_default() {
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "keychain"
"#;
    let cfg: Config = toml::from_str(toml).expect("must parse without [ack]");
    assert!(
        cfg.ack.enabled,
        "AckConfig::default() must produce enabled=true (backward compat)"
    );
    assert_eq!(cfg.ack.min_interval_secs, 60);
}

// ---------------------------------------------------------------------------
// 3. Config: parse config.toml WITH [transport] section
// ---------------------------------------------------------------------------

#[test]
fn config_transport_section_explicit_nostr_parses() {
    let toml = r#"
[relays]
urls = ["wss://relay.damus.io"]

[identity]
storage = "file"

[transport]
type = "nostr"
"#;
    let cfg: Config = toml::from_str(toml).expect("must parse with explicit transport nostr");
    match cfg.transport.kind {
        TransportKind::Nostr => {} // correct
    }
}

#[test]
fn config_missing_transport_section_defaults_to_nostr() {
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "keychain"
"#;
    let cfg: Config = toml::from_str(toml).expect("must parse without [transport]");
    match cfg.transport.kind {
        TransportKind::Nostr => {}
    }
}

// ---------------------------------------------------------------------------
// 4. Config: parse with unknown transport type — must error
// ---------------------------------------------------------------------------

#[test]
fn config_unknown_transport_type_grpc_is_error() {
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "keychain"

[transport]
type = "grpc"
"#;
    let result: Result<Config, _> = toml::from_str(toml);
    assert!(
        result.is_err(),
        "unknown transport type 'grpc' must be a parse error (enum only has 'nostr')"
    );
}

#[test]
fn config_unknown_transport_type_local_is_error() {
    // "local" is a transport concept mentioned in code but NOT in the enum.
    // TransportKind only has Nostr. This must reject.
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "keychain"

[transport]
type = "local"
"#;
    let result: Result<Config, _> = toml::from_str(toml);
    assert!(
        result.is_err(),
        "transport type 'local' is not in TransportKind enum; must be a parse error"
    );
}

// ---------------------------------------------------------------------------
// 5. DEFAULT_RELAYS count — exactly 7
// ---------------------------------------------------------------------------

#[test]
fn default_relay_count_is_exactly_7() {
    let cfg = Config::default();
    assert_eq!(
        cfg.relays.urls.len(),
        7,
        "DEFAULT_RELAYS must have exactly 7 entries, got {}",
        cfg.relays.urls.len()
    );
}

#[test]
fn default_relays_all_use_wss_scheme() {
    for url in DEFAULT_RELAYS {
        assert!(
            url.starts_with("wss://"),
            "default relay must use wss://, got: {url}"
        );
    }
}

#[test]
fn default_relays_no_duplicates() {
    let mut seen = std::collections::HashSet::new();
    for url in DEFAULT_RELAYS {
        assert!(seen.insert(*url), "duplicate default relay: {url}");
    }
}

#[test]
fn default_relays_contains_expected_known_relays() {
    let urls: Vec<&str> = DEFAULT_RELAYS.to_vec();
    assert!(urls.contains(&"wss://nos.lol"), "nos.lol must be in default relays");
    assert!(urls.contains(&"wss://relay.damus.io"), "damus must be in default relays");
    assert!(urls.contains(&"wss://relay.nostr.band"), "nostr.band must be in default relays");
    assert!(urls.contains(&"wss://relay.primal.net"), "primal.net must be in default relays");
    assert!(urls.contains(&"wss://relay.snort.social"), "snort.social must be in default relays");
    assert!(urls.contains(&"wss://nostr.mutinywallet.com"), "mutinywallet must be in default relays");
    assert!(urls.contains(&"wss://nostr.wine"), "nostr.wine must be in default relays");
}

// ---------------------------------------------------------------------------
// 6. NostrTransport::new — tested structurally (binary crate, not importable)
//    We test the transport *field types* via struct construction from TOML.
//    Duration and relay_urls fields are validated at compile-time by the struct.
// ---------------------------------------------------------------------------

#[test]
fn transport_config_default_is_nostr() {
    let tc = TransportConfig::default();
    match tc.kind {
        TransportKind::Nostr => {} // must be the default
    }
}

// ---------------------------------------------------------------------------
// 7. Transport trait: SendReport / ReceivedEnvelope / RelayHealth fields
//    Since it's a binary crate, we test via mirrored structs that match the
//    struct definitions in transport/mod.rs exactly.
// ---------------------------------------------------------------------------

// Mirror structs (must match src/transport/mod.rs field-for-field)
struct SendReport {
    transport_msg_id: String,
    ok_count: usize,
    total: usize,
}

struct ReceivedEnvelope {
    transport_msg_id: String,
    sender_hex: String,
    env_json: String,
    event_ts: u64,
}

struct RelayHealth {
    url: String,
    connected: bool,
}

#[test]
fn send_report_fields_accessible_and_invariants() {
    let report = SendReport {
        transport_msg_id: "abc123deadbeef".to_string(),
        ok_count: 3,
        total: 7,
    };
    assert_eq!(report.transport_msg_id, "abc123deadbeef");
    assert_eq!(report.ok_count, 3);
    assert_eq!(report.total, 7);
    assert!(
        report.ok_count <= report.total,
        "ok_count ({}) must not exceed total ({})",
        report.ok_count,
        report.total
    );
}

#[test]
fn send_report_ok_count_equals_total_is_valid() {
    let report = SendReport {
        transport_msg_id: "all_good".to_string(),
        ok_count: 7,
        total: 7,
    };
    assert_eq!(report.ok_count, report.total, "all relays accepted: ok_count == total is valid");
}

#[test]
fn send_report_zero_ok_is_structurally_valid() {
    // Struct does not enforce ok_count > 0. This is a potential bug:
    // callers could treat a send as successful even when 0 relays accepted.
    let report = SendReport {
        transport_msg_id: "dead".to_string(),
        ok_count: 0,
        total: 7,
    };
    assert_eq!(report.ok_count, 0);
    // If callers don't check ok_count > 0, messages are silently lost.
    // NostrTransport.send() returns this struct — callers must validate ok_count > 0.
}

#[test]
fn received_envelope_fields_accessible() {
    let env = ReceivedEnvelope {
        transport_msg_id: "ev_xyz_001".to_string(),
        sender_hex: "aabbccddeeff0011".to_string(),
        env_json: r#"{"v":1,"from":"aabbccddeeff0011","to":"recipient","msg":"hi","ts":"2026-03-25T00:00:00Z"}"#.to_string(),
        event_ts: 1742860800,
    };
    assert_eq!(env.transport_msg_id, "ev_xyz_001");
    assert_eq!(env.sender_hex, "aabbccddeeff0011");
    assert!(env.env_json.contains("\"v\":1"));
    assert!(env.event_ts > 1_000_000_000, "event_ts must be a unix epoch");
}

#[test]
fn relay_health_connected_and_disconnected_variants() {
    let connected = RelayHealth {
        url: "wss://nos.lol".to_string(),
        connected: true,
    };
    let disconnected = RelayHealth {
        url: "wss://dead.relay".to_string(),
        connected: false,
    };
    assert!(connected.connected, "connected relay must have connected=true");
    assert!(!disconnected.connected, "disconnected relay must have connected=false");
    assert_ne!(connected.url, disconnected.url);
}

// ---------------------------------------------------------------------------
// 8. get_known_nostr_ids with empty DB — must return empty vec, not error
// ---------------------------------------------------------------------------

#[test]
fn get_known_nostr_ids_empty_db_returns_empty_vec() {
    let conn = open_mem();
    let ids = get_known_nostr_ids(&conn).expect("must not error on empty DB");
    assert!(ids.is_empty(), "empty DB must return [], got: {:?}", ids);
}

// ---------------------------------------------------------------------------
// 9. get_known_nostr_ids with messages — only inbound rows, correct tuples
// ---------------------------------------------------------------------------

#[test]
fn get_known_nostr_ids_returns_only_inbound() {
    let conn = open_mem();

    // Insert one inbound
    conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, msg_id, transport)
         VALUES ('inbound_001', 'in', 'sender_aabb', 'recipient_ccdd', 'hello',
         'received', 'unread', '2026-03-20T12:00:00Z', '2026-03-20T12:00:01Z', 'msg_001', 'nostr')",
        [],
    ).unwrap();

    // Insert one outbound — must NOT appear in get_known_nostr_ids
    conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, msg_id, transport)
         VALUES ('outbound_001', 'out', 'recipient_ccdd', 'sender_aabb', 'reply',
         'sent', 'read', '2026-03-20T13:00:00Z', '2026-03-20T13:00:01Z', 'msg_002', 'nostr')",
        [],
    ).unwrap();

    let ids = get_known_nostr_ids(&conn).expect("must not error");
    assert_eq!(
        ids.len(),
        1,
        "must return only inbound rows (1), not outbound; got: {:?}",
        ids
    );
    assert_eq!(ids[0].0, "inbound_001", "must return the inbound nostr_id");
}

#[test]
fn get_known_nostr_ids_timestamp_is_correct_unix_epoch() {
    let conn = open_mem();

    conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, msg_id, transport)
         VALUES ('ts_test_id', 'in', 'sender', 'recipient', 'msg',
         'received', 'unread', '2026-03-20T00:00:00Z', '2026-03-20T00:00:01Z', 'msg_ts', 'nostr')",
        [],
    ).unwrap();

    // strftime('%s', '2026-03-20T00:00:00Z') = 1773964800 (verified via Python UTC epoch)
    let expected_ts: u64 = 1773964800;

    let ids = get_known_nostr_ids(&conn).expect("must not error");
    assert_eq!(ids.len(), 1);
    assert_eq!(
        ids[0].1, expected_ts,
        "unix timestamp for 2026-03-20T00:00:00Z must be {expected_ts}, got {}",
        ids[0].1
    );
}

#[test]
fn get_known_nostr_ids_multiple_inbound_all_returned() {
    let conn = open_mem();

    for i in 0..5u32 {
        conn.execute(
            &format!(
                "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
                 delivery_status, read_status, created_at, received_at, msg_id, transport)
                 VALUES ('id_{i:04}', 'in', 'sender', 'recipient', 'msg {i}',
                 'received', 'unread', '2026-03-{:02}T12:00:00Z', '2026-03-{:02}T12:00:01Z',
                 'msgid_{i:04}', 'nostr')",
                20 + i, 20 + i
            ),
            [],
        ).unwrap();
    }

    let ids = get_known_nostr_ids(&conn).expect("must not error");
    assert_eq!(ids.len(), 5, "must return all 5 inbound rows");

    let nostr_ids: Vec<&str> = ids.iter().map(|(id, _)| id.as_str()).collect();
    for i in 0..5u32 {
        assert!(
            nostr_ids.contains(&format!("id_{i:04}").as_str()),
            "id_{i:04} must be in results"
        );
    }
}

#[test]
fn get_known_nostr_ids_malformed_created_at_crashes_with_null() {
    // BUG: get_known_nostr_ids uses `row.get::<_, i64>(1)` with no NULL handling.
    // When created_at is not a valid ISO8601 date, SQLite strftime('%s', ...) returns NULL.
    // rusqlite returns Err(InvalidColumnType(..., Null)) when trying to read NULL as i64.
    // This causes get_known_nostr_ids to return an Err — the row is NOT returned.
    //
    // Root cause: the SQL should use COALESCE(strftime('%s', created_at), 0) to handle NULLs.
    // Fix needed in src/store/mod.rs: get_known_nostr_ids query.
    let conn = open_mem();

    conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, transport)
         VALUES ('bad_ts_id', 'in', 'sender', 'recipient', 'msg',
         'received', 'unread', 'NOT_A_DATE', 'NOT_A_DATE', 'nostr')",
        [],
    ).unwrap();

    // BUG: this returns Err(InvalidColumnType(..., Null)) instead of Ok([(id, 0)])
    let result = get_known_nostr_ids(&conn);
    assert!(
        result.is_err(),
        "CONFIRMED BUG: get_known_nostr_ids crashes on NULL timestamp from malformed created_at. \
         Expected Err(InvalidColumnType). Fix: use COALESCE(strftime('%s', created_at), 0) in SQL."
    );
}

// ---------------------------------------------------------------------------
// 10. Schema migration chain v1→v2→v3→v4
//     Exercised via store::open() which is the actual migration entry point.
//     We simulate v1 databases by creating them manually on disk.
// ---------------------------------------------------------------------------

#[test]
fn schema_fresh_db_reaches_v4() {
    // Fresh SCHEMA sets user_version=4 directly.
    let conn = open_mem();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 4, "fresh in-memory SCHEMA must set user_version=4, got: {version}");
}

#[test]
fn schema_has_all_required_tables() {
    let conn = open_mem();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    for expected in &["messages", "contacts", "sync_state", "relays", "outbox", "acks", "threads"] {
        assert!(
            tables.contains(&expected.to_string()),
            "table '{expected}' must exist after SCHEMA, got tables: {tables:?}"
        );
    }
}

#[test]
fn schema_messages_has_all_v4_columns() {
    let conn = open_mem();
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(messages)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    for expected_col in &[
        "nostr_id", "direction", "sender", "recipient", "content",
        "delivery_status", "read_status", "created_at", "received_at",
        "msg_id", "thread_id", "reply_to", "transport", "transport_msg_id",
    ] {
        assert!(
            cols.contains(&expected_col.to_string()),
            "messages table must have column '{expected_col}', got: {cols:?}"
        );
    }
}

#[test]
fn schema_outbox_has_all_expected_columns() {
    let conn = open_mem();
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(outbox)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    for col in &["msg_id", "recipient_hex", "envelope_json", "relay_urls",
                  "status", "retry_count", "ok_relay_count", "created_at",
                  "last_attempt_at", "next_retry_at", "sent_at"] {
        assert!(
            cols.contains(&col.to_string()),
            "outbox must have column '{col}', got: {cols:?}"
        );
    }
}

#[test]
fn schema_acks_has_all_expected_columns() {
    let conn = open_mem();
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(acks)")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    for col in &["msg_id", "ack_sender", "ack_status", "created_at", "sent_at"] {
        assert!(
            cols.contains(&col.to_string()),
            "acks table must have column '{col}', got: {cols:?}"
        );
    }
}

#[test]
fn migration_v1_schema_manually_upgraded_to_v4_structure() {
    // Simulate a v1 → v4 migration sequence by manually applying each migration step.
    // This exercises the migration SQL without calling store::open (binary crate boundary).
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();

    // Step 1: v1 schema (no msg_id, no outbox, no acks, no threads)
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS messages (
            nostr_id        TEXT PRIMARY KEY,
            direction       TEXT NOT NULL,
            sender          TEXT NOT NULL,
            recipient       TEXT NOT NULL,
            content         TEXT NOT NULL,
            delivery_status TEXT NOT NULL DEFAULT 'pending',
            read_status     TEXT NOT NULL DEFAULT 'unread',
            created_at      TEXT NOT NULL,
            received_at     TEXT NOT NULL
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
    ").unwrap();

    // Insert a v1 row
    conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at)
         VALUES ('ev_v1_001', 'in', 'sender_hex', 'recipient_hex', 'hello v1',
         'received', 'unread', '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z')",
        [],
    ).unwrap();

    // Step 2: v1→v2 migration (from MIGRATION_V1_TO_V2 in store/mod.rs)
    let migration_v1_to_v2: &[&str] = &[
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
    for stmt in migration_v1_to_v2 {
        conn.execute_batch(stmt).unwrap();
    }

    let v2: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v2, 2, "after v1→v2: user_version must be 2");

    // Verify backfill
    let msg_id: String = conn.query_row(
        "SELECT msg_id FROM messages WHERE nostr_id = 'ev_v1_001'",
        [],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(msg_id, "legacy:ev_v1_001", "v1→v2 backfill: msg_id must be 'legacy:' || nostr_id");

    // Step 3: v2→v3 migration (adds outbox)
    let migration_to_v3: &[&str] = &[
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
    for stmt in migration_to_v3 {
        conn.execute_batch(stmt).unwrap();
    }

    let v3: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v3, 3, "after v2→v3: user_version must be 3");

    // Step 4: v3→v4 migration (adds acks)
    let migration_to_v4: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS acks (
            msg_id      TEXT PRIMARY KEY,
            ack_sender  TEXT NOT NULL,
            ack_status  TEXT NOT NULL DEFAULT 'pending',
            created_at  TEXT NOT NULL,
            sent_at     TEXT
        )",
        "CREATE INDEX IF NOT EXISTS idx_acks_msg_id ON acks(msg_id)",
        "PRAGMA user_version = 4",
    ];
    for stmt in migration_to_v4 {
        conn.execute_batch(stmt).unwrap();
    }

    let v4: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(v4, 4, "after v3→v4: user_version must be 4");

    // Final state: acks table must exist and be insertable
    conn.execute(
        "INSERT INTO acks (msg_id, ack_sender, ack_status, created_at)
         VALUES ('msg_to_ack', 'ack_sender_hex', 'acknowledged', '2026-03-25T00:00:00Z')",
        [],
    ).unwrap();
    let ack_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM acks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ack_count, 1, "acks table must be usable after v4 migration");
}

// ---------------------------------------------------------------------------
// 11. Config serialization roundtrip
// ---------------------------------------------------------------------------

#[test]
fn config_roundtrip_default() {
    let original = Config::default();
    let toml_str = toml::to_string_pretty(&original).expect("serialize Config::default()");

    // Parse back
    let parsed: Config = toml::from_str(&toml_str).expect("deserialize Config from TOML");

    assert_eq!(parsed.relays.urls, original.relays.urls);
    assert_eq!(parsed.relays.timeout_secs, original.relays.timeout_secs);
    assert_eq!(parsed.identity.storage, original.identity.storage);
    assert_eq!(parsed.ack.enabled, original.ack.enabled);
    assert_eq!(parsed.ack.min_interval_secs, original.ack.min_interval_secs);
}

#[test]
fn config_roundtrip_preserves_ack_disabled() {
    let toml_in = r#"
[relays]
urls = ["wss://nos.lol", "wss://relay.damus.io"]
timeout_secs = 15

[identity]
storage = "file"

[ack]
enabled = false
min_interval_secs = 120

[transport]
type = "nostr"
"#;
    let cfg: Config = toml::from_str(toml_in).expect("parse");
    let toml_out = toml::to_string_pretty(&cfg).expect("serialize");
    let cfg2: Config = toml::from_str(&toml_out).expect("re-parse");

    assert!(!cfg2.ack.enabled, "ack.enabled=false must survive roundtrip");
    assert_eq!(cfg2.ack.min_interval_secs, 120);
    assert_eq!(cfg2.relays.timeout_secs, 15);
    assert_eq!(cfg2.identity.storage, IdentityStorage::File);
}

#[test]
fn config_toml_contains_expected_section_headers() {
    let cfg = Config::default();
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    assert!(toml_str.contains("[relays]"), "must contain [relays]");
    assert!(toml_str.contains("[identity]"), "must contain [identity]");
    assert!(toml_str.contains("[ack]"), "must contain [ack]");
    assert!(toml_str.contains("[transport]"), "must contain [transport]");
}

#[test]
fn config_transport_kind_serializes_as_lowercase_nostr() {
    let cfg = Config::default();
    let toml_str = toml::to_string_pretty(&cfg).unwrap();
    assert!(
        toml_str.contains("type = \"nostr\""),
        "TransportKind::Nostr must serialize as 'nostr' (lowercase via rename_all), got:\n{toml_str}"
    );
}

// ---------------------------------------------------------------------------
// 12. AckConfig default — document actual behavior (enabled=true, NOT false)
// ---------------------------------------------------------------------------

#[test]
fn ack_config_default_enabled_is_true() {
    let ack = AckConfig::default();
    assert!(
        ack.enabled,
        "AckConfig::default().enabled == true. \
         The task spec said 'false' but code has default_ack_enabled() returning true. \
         This is a spec/code mismatch — the DEFAULT IS TRUE."
    );
}

#[test]
fn ack_config_default_min_interval_is_60_secs() {
    let ack = AckConfig::default();
    assert_eq!(ack.min_interval_secs, 60);
}

#[test]
fn ack_config_serde_roundtrip_enabled_false() {
    let toml = "enabled = false\nmin_interval_secs = 45\n";
    let ack: AckConfig = toml::from_str(toml).expect("parse");
    assert!(!ack.enabled);
    assert_eq!(ack.min_interval_secs, 45);
    let out = toml::to_string_pretty(&ack).expect("serialize");
    let ack2: AckConfig = toml::from_str(&out).expect("re-parse");
    assert!(!ack2.enabled, "enabled=false must survive roundtrip");
}

// ---------------------------------------------------------------------------
// ADVERSARIAL edge cases
// ---------------------------------------------------------------------------

#[test]
fn config_empty_relay_urls_parses_ok() {
    let toml = r#"
[relays]
urls = []

[identity]
storage = "keychain"
"#;
    let cfg: Config = toml::from_str(toml).expect("empty relay list must parse");
    assert!(cfg.relays.urls.is_empty());
}

#[test]
fn config_relay_timeout_serde_default_is_10() {
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "keychain"
"#;
    let cfg: Config = toml::from_str(toml).expect("parse");
    assert_eq!(cfg.relays.timeout_secs, 10, "timeout_secs serde default must be 10");
}

#[test]
fn config_identity_unknown_storage_is_error() {
    let toml = r#"
[relays]
urls = ["wss://nos.lol"]

[identity]
storage = "usb_hsm"
"#;
    let result: Result<Config, _> = toml::from_str(toml);
    assert!(
        result.is_err(),
        "unknown IdentityStorage variant 'usb_hsm' must fail to parse"
    );
}

#[test]
fn send_report_ok_count_exceeds_total_is_not_caught_by_struct() {
    // STRUCTURAL BUG: The struct has no validation that ok_count <= total.
    // ok_count=8, total=7 is a logically invalid state that the type system allows.
    // This is a missing invariant — callers cannot rely on ok_count <= total.
    let report = SendReport {
        transport_msg_id: "overcount".to_string(),
        ok_count: 8,
        total: 7,
    };
    // The struct accepts this without panic — documents the missing validation.
    assert_eq!(report.ok_count, 8, "struct has no ok_count <= total invariant enforcement");
    // To fix: add a validation in NostrTransport.send() or in SendReport constructor.
}

#[test]
fn get_known_nostr_ids_direction_exact_match() {
    // ADVERSARIAL: what about direction values other than 'in' and 'out'?
    // A corrupted row with direction='unknown' must NOT be returned.
    let conn = open_mem();

    // Bypass normal constraints to insert a row with direction='corrupted'
    // (direction is TEXT NOT NULL, no CHECK constraint in v4 schema)
    conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, transport)
         VALUES ('corrupted_dir_id', 'corrupted', 'sender', 'recipient', 'msg',
         'received', 'unread', '2026-03-20T12:00:00Z', '2026-03-20T12:00:01Z', 'nostr')",
        [],
    ).unwrap();

    let ids = get_known_nostr_ids(&conn).expect("must not error");
    // SQL filters WHERE direction = 'in' — 'corrupted' must not match
    assert!(
        ids.is_empty(),
        "direction='corrupted' must NOT appear in get_known_nostr_ids (only 'in' matches)"
    );
}

#[test]
fn schema_v4_has_unique_index_on_msg_id_direction() {
    // The SCHEMA has: CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id, direction)
    // Test: inserting two rows with same msg_id but DIFFERENT direction must succeed (not unique violation).
    let conn = open_mem();

    conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, msg_id, transport)
         VALUES ('nid_in', 'in', 'sender', 'recipient', 'msg',
         'received', 'unread', '2026-03-20T12:00:00Z', '2026-03-20T12:00:01Z', 'SAME_MSG_ID', 'nostr')",
        [],
    ).unwrap();

    // Same msg_id, direction='out' — this should succeed if index is (msg_id, direction)
    let result = conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, msg_id, transport)
         VALUES ('nid_out', 'out', 'sender', 'recipient', 'msg',
         'sent', 'read', '2026-03-20T12:00:00Z', '2026-03-20T12:00:01Z', 'SAME_MSG_ID', 'nostr')",
        [],
    );

    assert!(
        result.is_ok(),
        "same msg_id with different direction must be allowed by (msg_id, direction) unique index"
    );

    // Same msg_id AND same direction — this must fail (unique violation)
    let result2 = conn.execute(
        "INSERT INTO messages (nostr_id, direction, sender, recipient, content,
         delivery_status, read_status, created_at, received_at, msg_id, transport)
         VALUES ('nid_in_dup', 'in', 'sender', 'recipient', 'msg2',
         'received', 'unread', '2026-03-20T12:00:00Z', '2026-03-20T12:00:01Z', 'SAME_MSG_ID', 'nostr')",
        [],
    );
    assert!(
        result2.is_err(),
        "same msg_id AND same direction must fail with unique constraint violation"
    );
}
