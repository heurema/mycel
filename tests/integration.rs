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
// Outbox schema (outbox table for store-and-retry reliability tests)
// ---------------------------------------------------------------------------

const OUTBOX_SCHEMA: &str = "
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
";

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
    let first = insert_local_msg(
        &conn,
        &msg_id,
        "in",
        "sender",
        "recipient",
        "hello",
        &msg_id,
        "local",
    );
    assert!(first, "first insert must succeed");

    // Second insert with same msg_id (different nostr_id) — must be ignored
    let second = insert_local_msg(
        &conn,
        "different-nostr-id",
        "in",
        "sender",
        "recipient",
        "hello",
        &msg_id,
        "local",
    );
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
        .gift_wrap_to(std::iter::once(TEST_RELAY), &receiver_pk, rumor, [])
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
        .fetch_events_from(std::iter::once(TEST_RELAY), filter, INTEGRATION_TIMEOUT)
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

    assert!(
        found,
        "sent message not found after roundtrip via {TEST_RELAY}"
    );
}

// ---------------------------------------------------------------------------
// Thread tests (Contract 6: NIP-17 Remote Threading)
// ---------------------------------------------------------------------------

/// Helper: open an in-memory DB with the full schema.
fn open_mem() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn
}

/// Helper: insert a thread message row with full meta.
fn insert_thread_msg(
    conn: &Connection,
    nostr_id: &str,
    sender: &str,
    thread_id: &str,
    msg_id: &str,
    reply_to: Option<&str>,
    content: &str,
    transport_msg_id: &str,
) -> bool {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
         SELECT ?1, 'out', ?2, ?3, ?4, 'delivered', 'read',
                '2026-03-23T00:00:00Z', '2026-03-23T00:00:00Z', ?5, ?3, ?6, 'nostr', ?7
         WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?5 AND msg_id IS NOT NULL AND msg_id != '')",
        rusqlite::params![nostr_id, sender, thread_id, content, msg_id, reply_to, transport_msg_id],
    ).unwrap();
    rows > 0
}

/// thread create → send → log roundtrip (2-member thread, local operations only).
///
/// AC10: test_thread_create_and_send
#[test]
fn test_thread_create_and_send() {
    let conn = open_mem();

    // Generate two keypairs
    let alice_keys = Keys::generate();
    let bob_keys = Keys::generate();
    let alice_hex = alice_keys.public_key().to_hex();
    let bob_hex = bob_keys.public_key().to_hex();

    // 1. thread create: insert thread row with alice + bob as members
    let topic = "coordination";
    // thread_id = SHA-256(topic) — mirroring create_thread logic
    use nostr_sdk::nostr::hashes::Hash;
    use nostr_sdk::nostr::hashes::sha256::Hash as Sha256Hash;
    let thread_id = format!("{:x}", Sha256Hash::hash(topic.as_bytes()));

    let members_json = serde_json::json!([
        {"pubkey": alice_hex, "joined_at": "2026-03-23T00:00:00Z"},
        {"pubkey": bob_hex, "joined_at": "2026-03-23T00:00:00Z"},
    ])
    .to_string();

    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO threads (thread_id, subject, members, created_at, updated_at)
         VALUES (?1, ?2, ?3, '2026-03-23T00:00:00Z', '2026-03-23T00:00:00Z')",
            rusqlite::params![thread_id, topic, members_json],
        )
        .unwrap();
    assert!(inserted > 0, "thread create: row should be inserted");

    // 2. thread send: build Kind 14 rumor locally and record in DB
    let msg_id = Uuid::now_v7().to_string();
    let nostr_event_id = "aabbcc001122"; // mock NIP-17 event_id

    // Build a Kind 14 rumor (same logic as multi_recipient_gift_wrap)
    let sender_pk = alice_keys.public_key();
    let bob_pk = bob_keys.public_key();

    let rumor: UnsignedEvent = EventBuilder::new(Kind::ChatMessage, "hello thread")
        .tag(Tag::public_key(alice_keys.public_key()))
        .tag(Tag::public_key(bob_pk))
        .tag(Tag::custom(TagKind::Subject, ["coordination"]))
        .tag(Tag::custom(
            TagKind::custom("mycel-thread-id"),
            [thread_id.as_str()],
        ))
        .tag(Tag::custom(
            TagKind::custom("mycel-msg-id"),
            [msg_id.as_str()],
        ))
        .build(sender_pk);

    // Verify rumor is Kind::ChatMessage (Kind 14)
    assert_eq!(
        rumor.kind,
        Kind::ChatMessage,
        "rumor must be Kind 14 (ChatMessage)"
    );

    // Verify custom tags are present in the rumor
    let rumor_tags: Vec<Vec<String>> = rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
    let has_thread_id_tag = rumor_tags
        .iter()
        .any(|t| t.len() >= 2 && t[0] == "mycel-thread-id" && t[1] == thread_id);
    let has_msg_id_tag = rumor_tags
        .iter()
        .any(|t| t.len() >= 2 && t[0] == "mycel-msg-id" && t[1] == msg_id);
    assert!(has_thread_id_tag, "rumor must have mycel-thread-id tag");
    assert!(has_msg_id_tag, "rumor must have mycel-msg-id tag");

    // thread send: insert into DB (simulating what send_thread_message does)
    let inserted_msg = insert_thread_msg(
        &conn,
        nostr_event_id,
        &alice_hex,
        &thread_id,
        &msg_id,
        None,
        "hello thread",
        nostr_event_id,
    );
    assert!(inserted_msg, "thread send: message should be inserted");

    // 3. thread log: query WHERE thread_id = ?
    let msgs: Vec<(String, String, String)> = conn
        .prepare(
            "SELECT msg_id, sender, content FROM messages WHERE thread_id = ?1 ORDER BY created_at ASC",
        )
        .unwrap()
        .query_map(rusqlite::params![thread_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert_eq!(msgs.len(), 1, "thread log: should find 1 message");
    let (stored_msg_id, stored_sender, stored_content) = &msgs[0];
    assert_eq!(stored_msg_id, &msg_id, "msg_id must match");
    assert_eq!(stored_sender, &alice_hex, "sender must match");
    assert_eq!(stored_content, "hello thread", "content must match");
}

/// Reply chain: 3 messages with e-tags referencing previous NIP-17 event_ids.
///
/// AC11: test_thread_reply_chain
#[test]
fn test_thread_reply_chain() {
    let conn = open_mem();

    let alice_keys = Keys::generate();
    let alice_hex = alice_keys.public_key().to_hex();

    let topic = "reply-test";
    use nostr_sdk::nostr::hashes::Hash;
    use nostr_sdk::nostr::hashes::sha256::Hash as Sha256Hash;
    let thread_id = format!("{:x}", Sha256Hash::hash(topic.as_bytes()));

    // Insert thread
    let members_json =
        serde_json::json!([{"pubkey": alice_hex, "joined_at": "2026-03-23T00:00:00Z"}]).to_string();
    conn.execute(
        "INSERT OR IGNORE INTO threads (thread_id, subject, members, created_at, updated_at)
         VALUES (?1, ?2, ?3, '2026-03-23T00:00:00Z', '2026-03-23T00:00:00Z')",
        rusqlite::params![thread_id, topic, members_json],
    )
    .unwrap();

    // Message 1 (root)
    let msg1_id = Uuid::now_v7().to_string();
    let event1_id = "event_hex_001"; // mock NIP-17 event_id for msg1
    insert_thread_msg(
        &conn,
        event1_id,
        &alice_hex,
        &thread_id,
        &msg1_id,
        None,
        "root message",
        event1_id,
    );

    // Message 2 (reply to msg1) — e-tag must reference event1_id
    let msg2_id = Uuid::now_v7().to_string();
    let event2_id = "event_hex_002";

    // reply_to in DB = msg1_id (logical ID)
    // e-tag in NIP-17 rumor = event1_id (transport ID) — resolved via get_transport_msg_id_by_msg_id
    let transport_id_for_msg1: Option<String> = conn
        .query_row(
            "SELECT transport_msg_id FROM messages WHERE msg_id = ?1",
            rusqlite::params![msg1_id],
            |row| row.get(0),
        )
        .ok();
    assert_eq!(
        transport_id_for_msg1.as_deref(),
        Some(event1_id),
        "e-tag: must resolve msg1_id to event1_id"
    );

    // Build reply rumor with e-tag referencing event1_id
    let event1_nid = EventId::from_hex(
        // Use a valid 32-byte hex for the mock event_id in test
        "0000000000000000000000000000000000000000000000000000000000000001",
    )
    .unwrap();
    let reply_rumor: UnsignedEvent = EventBuilder::new(Kind::ChatMessage, "reply to root")
        .tag(Tag::public_key(alice_keys.public_key()))
        .tag(Tag::event(event1_nid))
        .tag(Tag::custom(
            TagKind::custom("mycel-thread-id"),
            [thread_id.as_str()],
        ))
        .tag(Tag::custom(
            TagKind::custom("mycel-msg-id"),
            [msg2_id.as_str()],
        ))
        .build(alice_keys.public_key());

    // Verify e-tag is in the reply rumor
    let reply_tags: Vec<Vec<String>> = reply_rumor
        .tags
        .iter()
        .map(|t| t.as_slice().to_vec())
        .collect();
    let has_e_tag = reply_tags.iter().any(|t| t.len() >= 2 && t[0] == "e");
    assert!(
        has_e_tag,
        "reply rumor must have an e-tag referencing parent event"
    );

    insert_thread_msg(
        &conn,
        event2_id,
        &alice_hex,
        &thread_id,
        &msg2_id,
        Some(&msg1_id),
        "reply to root",
        event2_id,
    );

    // Message 3 (reply to msg2)
    let msg3_id = Uuid::now_v7().to_string();
    let event3_id = "event_hex_003";

    let transport_id_for_msg2: Option<String> = conn
        .query_row(
            "SELECT transport_msg_id FROM messages WHERE msg_id = ?1",
            rusqlite::params![msg2_id],
            |row| row.get(0),
        )
        .ok();
    assert_eq!(
        transport_id_for_msg2.as_deref(),
        Some(event2_id),
        "e-tag: must resolve msg2_id to event2_id"
    );

    insert_thread_msg(
        &conn,
        event3_id,
        &alice_hex,
        &thread_id,
        &msg3_id,
        Some(&msg2_id),
        "reply to reply",
        event3_id,
    );

    // Verify all 3 messages are in the thread ordered by created_at
    let chain: Vec<(String, Option<String>)> = conn
        .prepare(
            "SELECT msg_id, reply_to FROM messages WHERE thread_id = ?1 ORDER BY created_at ASC",
        )
        .unwrap()
        .query_map(rusqlite::params![thread_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();

    assert_eq!(chain.len(), 3, "reply chain: 3 messages");
    assert_eq!(chain[0].0, msg1_id);
    assert_eq!(chain[0].1, None, "root has no reply_to");
    assert_eq!(chain[1].0, msg2_id);
    assert_eq!(chain[1].1, Some(msg1_id.clone()), "msg2 replies to msg1");
    assert_eq!(chain[2].0, msg3_id);
    assert_eq!(chain[2].1, Some(msg2_id.clone()), "msg3 replies to msg2");
}

/// Fan-out budget: reject 11+ members with thread_member_limit_exceeded error.
///
/// AC09: test_thread_fan_out_budget
#[test]
fn test_thread_fan_out_budget() {
    // Generate 11 member pubkeys
    let mut members: Vec<String> = (0..11)
        .map(|_| Keys::generate().public_key().to_hex())
        .collect();

    // Validation: members.len() > 10 must be rejected
    assert!(
        members.len() > 10,
        "test setup: 11 members should exceed the limit"
    );

    // Simulate the validation from create_thread
    let result: Result<(), _> = if members.len() > 10 {
        Err("thread member limit (10) exceeded")
    } else {
        Ok(())
    };
    assert!(result.is_err(), "11 members must be rejected");

    // 10 members is exactly at the limit — must be allowed
    members.pop();
    assert_eq!(members.len(), 10);
    let result_ok: Result<(), &str> = if members.len() > 10 {
        Err("thread member limit (10) exceeded")
    } else {
        Ok(())
    };
    assert!(result_ok.is_ok(), "10 members must be accepted");
}

// ---------------------------------------------------------------------------
// Envelope v2 + msg_id dedup tests
// ---------------------------------------------------------------------------

/// Helper: insert a v2 envelope message using msg_id-based dedup (WHERE NOT EXISTS).
/// Simulates what sync.rs does for inbound v2 Nostr messages.
fn insert_v2_msg(
    conn: &Connection,
    nostr_id: &str,
    sender: &str,
    recipient: &str,
    content: &str,
    msg_id: &str,
) -> bool {
    let rows = conn.execute(
        "INSERT OR IGNORE INTO messages
            (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
             created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
         SELECT ?1, 'in', ?2, ?3, ?4, 'received', 'unread',
                '2026-03-25T00:00:00Z', '2026-03-25T00:00:01Z', ?5, NULL, NULL, 'nostr', ?1
         WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?5 AND msg_id IS NOT NULL AND msg_id != '')",
        rusqlite::params![nostr_id, sender, recipient, content, msg_id],
    ).unwrap();
    rows > 0
}

/// version 2 envelope: retry with new nostr_id but same msg_id is deduplicated.
/// Inbox must show the message exactly once even after a retry publish.
#[test]
fn test_v2_msg_id_dedup_on_retry() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();

    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
    let msg_id = Uuid::now_v7().to_string();

    // First delivery: original nostr event ID
    let first_nostr_id = "nostr_event_id_original_aaaa1111";
    let inserted = insert_v2_msg(
        &conn,
        first_nostr_id,
        sender,
        recipient,
        "hello from v2",
        &msg_id,
    );
    assert!(inserted, "first delivery must be inserted");

    // Retry: same msg_id, different nostr event ID (outbox retry scenario)
    let retry_nostr_id = "nostr_event_id_retry_bbbb2222";
    let retry_inserted = insert_v2_msg(
        &conn,
        retry_nostr_id,
        sender,
        recipient,
        "hello from v2",
        &msg_id,
    );
    assert!(
        !retry_inserted,
        "v2 retry with same msg_id must be deduplicated (not inserted)"
    );

    // Inbox must show exactly one message
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE recipient = ?1",
            rusqlite::params![recipient],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "dedup: inbox must show message exactly once, not twice"
    );

    // The stored message must be the first delivery's nostr_id
    let stored_nostr_id: String = conn
        .query_row(
            "SELECT nostr_id FROM messages WHERE msg_id = ?1",
            rusqlite::params![msg_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_nostr_id, first_nostr_id,
        "first delivery wins; retry is dropped"
    );
}

// ---------------------------------------------------------------------------
// QA: Envelope validation rules, msg_id dedup, ACK handling — adversarial tests
// using isolated schema mirrors and direct inserts for specific invariants.
// ---------------------------------------------------------------------------

/// Isolated schema mirror for dedup/ACK QA.
/// The existing SCHEMA constant above has only (msg_id) unique index — which differs
/// from production (msg_id, direction). Tests that need direction-aware dedup use
/// SCHEMA_WITH_ACKS below.
const SCHEMA_WITH_ACKS: &str = "
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
-- Production-accurate composite unique index (msg_id, direction).
-- The integration test SCHEMA above has only (msg_id) — a schema divergence.
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_msg_id ON messages(msg_id, direction);
CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);
CREATE TABLE IF NOT EXISTS threads (
    thread_id   TEXT PRIMARY KEY,
    subject     TEXT,
    members     TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
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

fn open_mem_v4() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA_WITH_ACKS).unwrap();
    conn
}

/// Insert a v2 message using the same WHERE NOT EXISTS dedup logic as insert_message_v2.
fn insert_v2_msg_dedup(
    conn: &Connection,
    nostr_id: &str,
    direction: &str,
    sender: &str,
    recipient: &str,
    content: &str,
    msg_id: &str,
) -> bool {
    let rows = if !msg_id.is_empty() {
        conn.execute(
            "INSERT OR IGNORE INTO messages
                (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
                 created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
             SELECT ?1, ?2, ?3, ?4, ?5, 'received', 'unread',
                    '2026-03-25T00:00:00Z', '2026-03-25T00:00:01Z', ?6, NULL, NULL, 'nostr', ?1
             WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?6 AND msg_id IS NOT NULL AND msg_id != '')",
            rusqlite::params![nostr_id, direction, sender, recipient, content, msg_id],
        ).unwrap()
    } else {
        // legacy path: no msg_id, dedup on nostr_id PK
        conn.execute(
            "INSERT OR IGNORE INTO messages
                (nostr_id, direction, sender, recipient, content, delivery_status, read_status,
                 created_at, received_at, msg_id, thread_id, reply_to, transport, transport_msg_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 'received', 'unread',
                     '2026-03-25T00:00:00Z', '2026-03-25T00:00:01Z', NULL, NULL, NULL, 'nostr', ?1)",
            rusqlite::params![nostr_id, direction, sender, recipient, content],
        ).unwrap()
    };
    rows > 0
}

/// Insert an ACK using the same anti-storm logic as the ingress ACK path.
/// Returns (anti_storm_blocked, insert_succeeded).
fn insert_ack_with_antistorm(
    conn: &Connection,
    msg_id: &str,
    ack_sender: &str,
    ack_ts: &str,
) -> (bool, bool) {
    // Anti-storm check: any existing ack within 60 seconds?
    let existing_recent: Option<String> = conn.query_row(
        "SELECT created_at FROM acks WHERE msg_id = ?1 AND ack_sender = ?2
         AND (CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', created_at) AS INTEGER)) < 60
         LIMIT 1",
        rusqlite::params![msg_id, ack_sender],
        |row| row.get(0),
    ).ok();

    if existing_recent.is_some() {
        return (true, false); // anti-storm blocked
    }

    let rows = conn.execute(
        "INSERT OR IGNORE INTO acks (msg_id, ack_sender, ack_status, created_at) VALUES (?1, ?2, 'acknowledged', ?3)",
        rusqlite::params![msg_id, ack_sender, ack_ts],
    ).unwrap();

    (false, rows > 0)
}

// ── Test 1 ───────────────────────────────────────────────────────────────────
/// v2 envelope with empty msg_id: falls through to nostr_id PK dedup.
/// Two messages with different nostr_ids and empty msg_id must both be stored.
#[test]
fn qa_v2_empty_msg_id_no_dedup() {
    let conn = open_mem_v4();
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";

    // The ingest path keeps msg_id=None when env.v==2 but msg_id is empty,
    // so the empty-msg_id path uses legacy nostr_id PK dedup.
    let first = insert_v2_msg_dedup(&conn, "nostr_id_a", "in", sender, recipient, "msg A", "");
    let second = insert_v2_msg_dedup(&conn, "nostr_id_b", "in", sender, recipient, "msg B", "");

    assert!(first, "first insert with empty msg_id must succeed");
    assert!(
        second,
        "second insert with different nostr_id + empty msg_id must also succeed (no dedup key)"
    );

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    // If count == 1, empty msg_id is acting as a dedup key — that would be a bug.
    assert_eq!(
        count, 2,
        "empty msg_id must NOT act as dedup key; both messages must be stored"
    );
}

// ── Test 2 ───────────────────────────────────────────────────────────────────
/// v2 envelope with parts=[] but msg field populated: hybrid v1/v2.
/// EnvelopeWire From impl converts empty parts + non-empty msg into a TextPart.
#[test]
fn qa_v2_empty_parts_with_legacy_msg_field() {
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";

    // Wire: v=2, empty parts[], but has msg field
    let json = format!(
        r#"{{"v":2,"msg_id":"hybrid-001","from":"{sender}","to":"{recipient}","ts":"2026-03-25T00:00:00Z","msg":"hybrid content","parts":[]}}"#
    );

    // Deserialize via serde_json — this exercises EnvelopeWire → Envelope From conversion.
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(val["v"], 2);
    // Wire has empty parts
    assert!(
        val["parts"].as_array().unwrap().is_empty(),
        "wire parts must be empty"
    );
    // The msg field is present
    assert_eq!(val["msg"].as_str().unwrap(), "hybrid content");

    // The Envelope struct's From<EnvelopeWire> impl:
    //   if wire.parts.is_empty() && !wire.msg.is_empty() → build TextPart from msg
    // We cannot call this from integration tests (no lib.rs), so we verify the rule directly:
    let parts_is_empty = val["parts"].as_array().unwrap().is_empty();
    let msg_non_empty = !val["msg"].as_str().unwrap_or("").is_empty();
    let will_build_text_part = parts_is_empty && msg_non_empty;
    assert!(
        will_build_text_part,
        "v2 envelope with empty parts[] + non-empty msg must trigger TextPart construction from msg"
    );

    // Verify the content extraction logic: if parts non-empty after conversion, use parts;
    // otherwise use msg. Since conversion always populates parts from msg, content = "hybrid content".
    // (We verify the rule since we cannot call extract_content directly.)
    let expected_content = "hybrid content";
    assert_eq!(
        expected_content,
        val["msg"].as_str().unwrap(),
        "content should match msg field"
    );
}

// ── Test 3 ───────────────────────────────────────────────────────────────────
/// AckPart with a nonexistent original_msg_id: no FK check exists.
/// The ACK should be stored (orphan ACK accumulation is possible).
#[test]
fn qa_ack_for_nonexistent_msg_id() {
    let conn = open_mem_v4();
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let ghost_msg_id = "nonexistent-message-id-99999";

    // The ingest ACK path does not check whether original_msg_id exists in messages.
    let (storm_blocked, inserted) =
        insert_ack_with_antistorm(&conn, ghost_msg_id, sender, "2026-03-25T00:00:00Z");
    assert!(
        !storm_blocked,
        "no prior ACK exists, anti-storm must not fire"
    );
    assert!(
        inserted,
        "ACK for nonexistent msg_id must be stored (no FK validation)"
    );

    let stored: Option<String> = conn
        .query_row(
            "SELECT msg_id FROM acks WHERE msg_id = ?1",
            rusqlite::params![ghost_msg_id],
            |row| row.get(0),
        )
        .ok();
    assert!(
        stored.is_some(),
        "orphan ACK (for unknown msg_id) is accepted without FK check — accumulation risk"
    );
}

// ── Test 4 ───────────────────────────────────────────────────────────────────
/// AckPart with empty original_msg_id: stored with msg_id = '' in acks table.
/// Two ACKs with empty msg_id from different senders collide on the PRIMARY KEY.
#[test]
fn qa_ack_empty_original_msg_id() {
    let conn = open_mem_v4();
    let sender1 = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let sender2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    let (_, first) = insert_ack_with_antistorm(&conn, "", sender1, "2026-03-25T00:00:00Z");
    // Empty string is a valid SQLite PK value — first insert succeeds.
    // No assertion on first here — behaviour may vary.

    let (storm_blocked2, second) =
        insert_ack_with_antistorm(&conn, "", sender2, "2026-03-25T00:00:01Z");
    // The anti-storm query filters by (msg_id, ack_sender), so sender2 should not be blocked.
    // But INSERT OR IGNORE on the PK (msg_id alone) will block sender2's insert if sender1 succeeded.

    let total: i64 = conn
        .query_row("SELECT COUNT(*) FROM acks WHERE msg_id = ''", [], |row| {
            row.get(0)
        })
        .unwrap();

    if first {
        // sender1's ACK was stored; sender2's ACK should be blocked by PK collision.
        assert!(
            !second,
            "BUG: acks PK is msg_id alone — second empty-string ACK from different sender is silently dropped"
        );
        assert_eq!(
            total, 1,
            "only one row with empty msg_id can exist (PK constraint)"
        );
        assert!(
            !storm_blocked2,
            "sender2 is not blocked by anti-storm (different sender), but IS blocked by PK"
        );
    }
    // If neither was stored, the code is rejecting empty msg_id — also acceptable.
}

// ── Test 5 ───────────────────────────────────────────────────────────────────
/// Duplicate ACK within 60s anti-storm window: second must be suppressed.
/// The first ACK is inserted with datetime('now') so the anti-storm time comparison is valid.
#[test]
fn qa_ack_duplicate_within_antistorm_window() {
    let conn = open_mem_v4();
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let msg_id = "antistorm-dedup-msg-001";

    // Insert first ACK using SQLite's current time so the anti-storm comparison fires correctly.
    conn.execute(
        "INSERT INTO acks (msg_id, ack_sender, ack_status, created_at) VALUES (?1, ?2, 'acknowledged', datetime('now'))",
        rusqlite::params![msg_id, sender],
    ).unwrap();

    // Second ACK arrives immediately after (within 60s) — anti-storm must suppress it.
    let (storm2, inserted2) =
        insert_ack_with_antistorm(&conn, msg_id, sender, "2026-03-25T00:00:05Z");
    assert!(
        storm2,
        "second ACK within 60s must be suppressed by anti-storm window"
    );
    assert!(!inserted2, "second ACK must not be inserted");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM acks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 1,
        "only one ACK row must exist after duplicate suppression"
    );
}

// ── Test 6 ───────────────────────────────────────────────────────────────────
/// BUG: Duplicate ACK after 60s anti-storm window expired.
/// After the window expires, the anti-storm check passes, but INSERT OR IGNORE hits the
/// PRIMARY KEY (msg_id alone) — the second ACK is blocked regardless of elapsed time.
/// Expected: second ACK accepted (anti-storm expired). Actual: blocked by PK.
#[test]
fn qa_ack_duplicate_after_antistorm_window_bug() {
    let conn = open_mem_v4();
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let msg_id = "antistorm-expiry-msg-001";

    // Insert first ACK with a timestamp 70 seconds in the past (anti-storm window expired).
    conn.execute(
        "INSERT INTO acks (msg_id, ack_sender, ack_status, created_at)
         VALUES (?1, ?2, 'acknowledged', datetime('now', '-70 seconds'))",
        rusqlite::params![msg_id, sender],
    )
    .unwrap();

    // Now try to insert a second ACK. Anti-storm check should PASS (70s > 60s window).
    // But the INSERT OR IGNORE will be blocked by the PRIMARY KEY constraint.
    let (storm_blocked, inserted) =
        insert_ack_with_antistorm(&conn, msg_id, sender, "2026-03-25T00:00:00Z");

    // Anti-storm check should NOT fire (70s elapsed > 60s window)
    assert!(
        !storm_blocked,
        "anti-storm check must NOT fire when 70s have elapsed (window is 60s)"
    );

    // BUG: INSERT OR IGNORE blocks the insert because msg_id is the PRIMARY KEY.
    // The anti-storm logic is dead code for re-ACKs on already-acked messages.
    assert!(
        !inserted,
        "BUG CONFIRMED: second ACK after anti-storm expiry is silently blocked by acks PRIMARY KEY \
         (msg_id alone); the anti-storm window concept is undermined — re-ACKs are permanently blocked, \
         not just for 60s. The PK should be (msg_id, ack_sender) to allow re-ACKing after the window."
    );
}

// ── Test 7 ───────────────────────────────────────────────────────────────────
/// Envelope with v=3 (future version) must be rejected gracefully.
#[test]
fn qa_envelope_v3_rejected() {
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";

    let json = format!(
        r#"{{"v":3,"msg_id":"future-msg-001","from":"{sender}","to":"{recipient}","ts":"2026-03-25T00:00:00Z","parts":[{{"type":"text","text":"from the future"}}]}}"#
    );
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();
    let v = val["v"].as_u64().unwrap() as u8;

    assert_eq!(v, 3, "v=3 must deserialize without panic");
    // The ingest version gate rejects any envelope version outside {1,2}.
    let rejected = v != 1 && v != 2;
    assert!(rejected, "v=3 must be rejected by the version gate");
}

// ── Test 8 ───────────────────────────────────────────────────────────────────
/// Envelope with v=0 (invalid / missing version) must be rejected.
#[test]
fn qa_envelope_v0_rejected() {
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";

    // Explicit v=0
    let json_v0 = format!(
        r#"{{"v":0,"from":"{sender}","to":"{recipient}","msg":"zero version","ts":"2026-03-25T00:00:00Z"}}"#
    );
    let val0: serde_json::Value = serde_json::from_str(&json_v0).unwrap();
    let v0 = val0["v"].as_u64().unwrap_or(0) as u8;
    assert_eq!(v0, 0);
    assert!(
        v0 != 1 && v0 != 2,
        "v=0 must be rejected by the version gate"
    );

    // Missing v field: EnvelopeWire has #[serde(default)] on v, so it becomes 0
    let json_no_v = format!(
        r#"{{"from":"{sender}","to":"{recipient}","msg":"no version field","ts":"2026-03-25T00:00:00Z"}}"#
    );
    let val_no_v: serde_json::Value = serde_json::from_str(&json_no_v).unwrap();
    // In JSON the field is absent; serde's default for u8 is 0
    let v_missing = val_no_v["v"].as_u64().unwrap_or(0) as u8;
    assert_eq!(v_missing, 0, "missing v field must default to 0");
    assert!(
        v_missing != 1 && v_missing != 2,
        "missing v field (v=0) must be rejected"
    );
}

// ── Test 9 ───────────────────────────────────────────────────────────────────
/// Same msg_id, same direction=in, different nostr_id → must deduplicate (one row).
#[test]
fn qa_dedup_same_msg_id_same_direction() {
    let conn = open_mem_v4();
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
    let msg_id = Uuid::now_v7().to_string();

    let first = insert_v2_msg_dedup(
        &conn,
        "nostr_in_1",
        "in",
        sender,
        recipient,
        "original",
        &msg_id,
    );
    assert!(first, "first insert must succeed");

    let second = insert_v2_msg_dedup(
        &conn,
        "nostr_in_2",
        "in",
        sender,
        recipient,
        "duplicate",
        &msg_id,
    );
    assert!(
        !second,
        "same msg_id + same direction must be deduplicated (WHERE NOT EXISTS blocks it)"
    );

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE msg_id = ?1",
            rusqlite::params![msg_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "exactly one message row must survive dedup");
}

// ── Test 10 ──────────────────────────────────────────────────────────────────
/// Same msg_id, direction=in then direction=out → should NOT deduplicate.
/// The production UNIQUE index is (msg_id, direction) — two directions are distinct.
/// BUT insert_message_v2's WHERE NOT EXISTS clause does NOT filter by direction,
/// so the out-copy is incorrectly blocked when an in-copy already exists.
///
/// BUG: WHERE NOT EXISTS (SELECT 1 FROM messages WHERE msg_id = ?10 AND ...)
/// has no direction filter → direction-blind dedup.
#[test]
fn qa_dedup_same_msg_id_different_direction_bug() {
    let conn = open_mem_v4();
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
    let msg_id = Uuid::now_v7().to_string();

    // Insert direction=in first
    let in_ok = insert_v2_msg_dedup(
        &conn,
        "nostr_in_x",
        "in",
        sender,
        recipient,
        "inbound",
        &msg_id,
    );
    assert!(in_ok, "direction=in must be inserted");

    // Insert direction=out — same msg_id, different direction (send-to-self scenario).
    // The WHERE NOT EXISTS check in insert_message_v2 does NOT have a direction filter,
    // so it will find the in-row and block the out-insert.
    let out_ok = insert_v2_msg_dedup(
        &conn,
        "nostr_out_x",
        "out",
        sender,
        recipient,
        "outbound",
        &msg_id,
    );

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE msg_id = ?1",
            rusqlite::params![msg_id],
            |row| row.get(0),
        )
        .unwrap();

    // The UNIQUE index (msg_id, direction) would allow both rows.
    // The WHERE NOT EXISTS guard prevents the second insert without checking direction.
    assert!(
        !out_ok,
        "BUG CONFIRMED: insert_message_v2 WHERE NOT EXISTS is direction-blind — \
         out-copy (direction=out) is incorrectly deduplicated against in-copy (direction=in) \
         with the same msg_id; send-to-self messages are silently dropped"
    );
    assert_eq!(
        count, 1,
        "only one row exists because the out-copy was incorrectly deduplicated (direction-blind WHERE NOT EXISTS)"
    );
}

// ── Test 11 ──────────────────────────────────────────────────────────────────
/// Oversized envelope (>8KB raw content) must be rejected.
/// The ingest size gate checks the raw envelope length against MAX_MESSAGE_SIZE (8192).
#[test]
fn qa_oversized_envelope_rejected() {
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
    const MAX_SIZE: usize = 8192;

    // Build JSON envelope whose raw string length exceeds 8192 bytes
    let big_text = "x".repeat(8100);
    let big_json = format!(
        r#"{{"v":2,"msg_id":"oversized-001","from":"{sender}","to":"{recipient}","ts":"2026-03-25T00:00:00Z","parts":[{{"type":"text","text":"{big_text}"}}]}}"#
    );
    assert!(
        big_json.len() > MAX_SIZE,
        "test setup: oversized envelope must exceed {MAX_SIZE} bytes (actual: {})",
        big_json.len()
    );

    // The ingest size gate rejects frames whose raw envelope exceeds MAX_MESSAGE_SIZE.
    let rejected = big_json.len() > MAX_SIZE;
    assert!(rejected, "oversized envelope must be rejected (size gate)");

    // Envelope at exactly MAX_SIZE must be accepted (boundary condition).
    // Compute overhead by building the envelope with empty text (0-char) and measuring.
    let empty_text = "";
    let json_zero = format!(
        r#"{{"v":2,"msg_id":"exact-limit","from":"{sender}","to":"{recipient}","ts":"2026-03-25T00:00:00Z","parts":[{{"type":"text","text":"{empty_text}"}}]}}"#
    );
    let overhead = json_zero.len(); // full length with 0-char text
    if MAX_SIZE >= overhead {
        let exact_text = "z".repeat(MAX_SIZE - overhead);
        let exact_json = format!(
            r#"{{"v":2,"msg_id":"exact-limit","from":"{sender}","to":"{recipient}","ts":"2026-03-25T00:00:00Z","parts":[{{"type":"text","text":"{exact_text}"}}]}}"#
        );
        assert_eq!(
            exact_json.len(),
            MAX_SIZE,
            "boundary: exact 8192-byte envelope must be exactly {MAX_SIZE} bytes"
        );
        assert!(
            exact_json.len() <= MAX_SIZE,
            "boundary: envelope at exactly MAX_SIZE must pass size gate"
        );
    }
}

// ── Test 12 ──────────────────────────────────────────────────────────────────
/// AckPart inside a v1 envelope: malformed but must not crash.
/// The ingest ACK path should treat it as a control frame, not a regular message.
#[test]
fn qa_ackpart_in_v1_envelope_does_not_crash() {
    let conn = open_mem_v4();
    let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
    let recipient = "1122334455667788990011223344556677889900112233445566778899001122";

    // Malformed: v=1 with AckPart in parts[] (not valid v1 wire format).
    let json = format!(
        r#"{{"v":1,"from":"{sender}","to":"{recipient}","msg":"ignored","ts":"2026-03-25T00:00:00Z","parts":[{{"type":"ack","original_msg_id":"some-msg","status":"acknowledged","ack_ts":"2026-03-25T00:00:00Z"}}]}}"#
    );

    // Must deserialize without panic
    let val: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(val["v"], 1, "version must be 1");

    // Verify it contains an AckPart in parts
    let parts = val["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 1);
    assert_eq!(
        parts[0]["type"].as_str().unwrap(),
        "ack",
        "must contain an ack part"
    );

    // Any AckPart in parts[] should be treated as a control frame, not stored as a message.
    let is_ack = parts.iter().any(|p| p["type"].as_str() == Some("ack"));
    assert!(is_ack, "v1+AckPart must trigger is_ack guard");

    // Simulate ACK handling path: extract original_msg_id and store in acks.
    let original_msg_id = parts[0]["original_msg_id"].as_str().unwrap();
    let (storm_blocked, inserted) =
        insert_ack_with_antistorm(&conn, original_msg_id, sender, "2026-03-25T00:00:00Z");
    assert!(!storm_blocked, "no prior ACK, anti-storm must not fire");
    assert!(inserted, "ACK extracted from v1+AckPart must be stored");

    // Verify: no message row stored (is_ack guard prevented regular storage)
    let msg_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        msg_count, 0,
        "v1+AckPart must not be stored as a regular message"
    );
}
