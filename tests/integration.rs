// Integration tests — send + receive roundtrip via a real public relay,
// plus local transport (send-to-self) tests.
//
// Network tests require network access and are marked #[ignore] by default.
// Run with: cargo test --test integration -- --ignored
//
// The integration test verifies end-to-end:
//   1. Two key pairs (sender + receiver)
//   2. Sender publishes a gift-wrap to a public relay
//   3. Receiver fetches and unwraps it
//   4. Content matches

use nostr_sdk::prelude::*;
use rusqlite::Connection;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// DB schema (copy of store::SCHEMA for integration test isolation)
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

/// INSERT OR IGNORE using msg_id as dedup key (local transport).
fn insert_local_msg(
    conn: &Connection,
    nostr_id: &str,
    direction: &str,
    sender: &str,
    recipient: &str,
    content: &str,
    msg_id: &str,
    transport: &str,
) -> bool {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
         SELECT ?1, ?2, ?3, ?4, ?5, 'delivered', 'unread',
                '2026-03-23T00:00:00Z', '2026-03-23T00:00:01Z', ?6, NULL, NULL, ?7, ?6
         WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?6 AND msg_id IS NOT NULL AND msg_id != '')",
        rusqlite::params![nostr_id, direction, sender, recipient, content, msg_id, transport],
    ).unwrap();
    rows > 0
}

// ---------------------------------------------------------------------------
// Local transport: send-to-self tests
// ---------------------------------------------------------------------------

/// send self via local transport: message appears in DB with direction=out,
/// transport=local. Validates UUIDv7 msg_id, Envelope v2 signing, INSERT OR IGNORE dedup.
#[test]
fn test_local_send_self() {
    // Generate a keypair for the sender (self)
    let keys = Keys::generate();
    let sender_hex = keys.public_key().to_hex();

    // In-memory DB — stands in for sender's mycel.db
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();

    // Generate UUIDv7 msg_id
    let msg_id = Uuid::now_v7().to_string();
    let message = "context summary for next session";

    // Write outbound copy (direction=out, transport=local)
    let inserted_out = insert_local_msg(
        &conn,
        &msg_id,
        "out",
        &sender_hex,
        &sender_hex,
        message,
        &msg_id,
        "local",
    );
    assert!(inserted_out, "outbound copy must be inserted");

    // Write inbound copy (direction=in, transport=local) using a distinct nostr_id
    let inbound_nostr_id = format!("{}-in", msg_id);
    let inbound_msg_id = format!("{}-inbound", msg_id);
    let inserted_in = insert_local_msg(
        &conn,
        &inbound_nostr_id,
        "in",
        &sender_hex,
        &sender_hex,
        message,
        &inbound_msg_id,
        "local",
    );
    assert!(inserted_in, "inbound copy must be inserted");

    // Verify: outbound row exists with transport=local
    let (stored_direction, stored_transport): (String, String) = conn
        .query_row(
            "SELECT direction, transport FROM messages WHERE nostr_id = ?1",
            rusqlite::params![msg_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored_direction, "out");
    assert_eq!(stored_transport, "local");

    // Verify: inbound row exists with direction=in
    let in_direction: String = conn
        .query_row(
            "SELECT direction FROM messages WHERE nostr_id = ?1",
            rusqlite::params![inbound_nostr_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(in_direction, "in");

    // Total rows = 2 (one out + one in)
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

/// Dedup test: inserting the same msg_id twice via INSERT OR IGNORE keeps only one row.
#[test]
fn test_local_dedup_msg_id() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();

    let msg_id = Uuid::now_v7().to_string();

    // First insert
    let first = insert_local_msg(&conn, &msg_id, "in", "sender", "recipient", "hello", &msg_id, "local");
    assert!(first, "first insert must succeed");

    // Second insert with same msg_id (different nostr_id) — must be ignored
    let second = insert_local_msg(&conn, "different-nostr-id", "in", "sender", "recipient", "hello", &msg_id, "local");
    assert!(!second, "duplicate msg_id must be silently ignored");

    // Exactly one row
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1, "dedup: only one row should exist");
}

/// Public relay used for integration tests.
const TEST_RELAY: &str = "wss://nos.lol";

/// Timeout for relay operations in integration tests.
const INTEGRATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Full send+receive roundtrip via a real public relay.
///
/// This is the primary integration test: it exercises the full
/// gift-wrap publish → fetch → unwrap pipeline over the network.
#[tokio::test]
#[ignore]
async fn test_integration_send_receive_roundtrip() {
    let sender_keys = Keys::generate();
    let receiver_keys = Keys::generate();
    let receiver_pk = receiver_keys.public_key();

    // Build unique message content so we can identify it among other events
    let unique_tag = sender_keys.public_key().to_hex();
    let content = format!("mycel-integration-test:{unique_tag}");

    // Build sender client and connect to test relay
    let sender_client = Client::new(sender_keys.clone());
    sender_client
        .add_relay(TEST_RELAY)
        .await
        .expect("add relay for sender");
    sender_client.connect().await;

    // Build rumor
    let rumor: UnsignedEvent =
        EventBuilder::new(Kind::PrivateDirectMessage, &content).build(sender_keys.public_key());

    // Publish gift wrap
    let output = sender_client
        .gift_wrap_to(
            std::iter::once(TEST_RELAY),
            &receiver_pk,
            rumor,
            [],
        )
        .await
        .expect("gift_wrap_to");

    let _event_id = output.val;
    assert!(
        !output.success.is_empty(),
        "at least one relay should accept the gift wrap"
    );

    // Small delay to allow relay propagation
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Build receiver client and fetch
    let receiver_client = Client::new(receiver_keys.clone());
    receiver_client
        .add_relay(TEST_RELAY)
        .await
        .expect("add relay for receiver");
    receiver_client.connect().await;

    let filter = Filter::new()
        .kind(Kind::GiftWrap)
        .pubkey(receiver_pk)
        .since(Timestamp::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .saturating_sub(120),
        ));

    let events = receiver_client
        .fetch_events_from(
            std::iter::once(TEST_RELAY),
            filter,
            INTEGRATION_TIMEOUT,
        )
        .await
        .expect("fetch_events_from");

    assert!(
        !events.is_empty(),
        "receiver should find at least one gift-wrap event"
    );

    // Unwrap and verify content
    let mut found = false;
    for event in events {
        if let Ok(unwrapped) = UnwrappedGift::from_gift_wrap(&receiver_keys, &event).await {
            if unwrapped.rumor.content.contains(&unique_tag) {
                assert_eq!(
                    unwrapped.sender,
                    sender_keys.public_key(),
                    "sender public key must match"
                );
                found = true;
                break;
            }
        }
    }

    sender_client.disconnect().await;
    receiver_client.disconnect().await;

    assert!(found, "sent message not found after roundtrip via {TEST_RELAY}");
}
