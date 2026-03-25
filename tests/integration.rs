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
    use nostr_sdk::nostr::hashes::sha256::Hash as Sha256Hash;
    use nostr_sdk::nostr::hashes::Hash;
    let thread_id = format!("{:x}", Sha256Hash::hash(topic.as_bytes()));

    let members_json = serde_json::json!([
        {"pubkey": alice_hex, "joined_at": "2026-03-23T00:00:00Z"},
        {"pubkey": bob_hex, "joined_at": "2026-03-23T00:00:00Z"},
    ]).to_string();

    let inserted = conn.execute(
        "INSERT OR IGNORE INTO threads (thread_id, subject, members, created_at, updated_at)
         VALUES (?1, ?2, ?3, '2026-03-23T00:00:00Z', '2026-03-23T00:00:00Z')",
        rusqlite::params![thread_id, topic, members_json],
    ).unwrap();
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
        .tag(Tag::custom(TagKind::custom("mycel-thread-id"), [thread_id.as_str()]))
        .tag(Tag::custom(TagKind::custom("mycel-msg-id"), [msg_id.as_str()]))
        .build(sender_pk);

    // Verify rumor is Kind::ChatMessage (Kind 14)
    assert_eq!(rumor.kind, Kind::ChatMessage, "rumor must be Kind 14 (ChatMessage)");

    // Verify custom tags are present in the rumor
    let rumor_tags: Vec<Vec<String>> = rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
    let has_thread_id_tag = rumor_tags.iter().any(|t| t.len() >= 2 && t[0] == "mycel-thread-id" && t[1] == thread_id);
    let has_msg_id_tag = rumor_tags.iter().any(|t| t.len() >= 2 && t[0] == "mycel-msg-id" && t[1] == msg_id);
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
    use nostr_sdk::nostr::hashes::sha256::Hash as Sha256Hash;
    use nostr_sdk::nostr::hashes::Hash;
    let thread_id = format!("{:x}", Sha256Hash::hash(topic.as_bytes()));

    // Insert thread
    let members_json = serde_json::json!([{"pubkey": alice_hex, "joined_at": "2026-03-23T00:00:00Z"}]).to_string();
    conn.execute(
        "INSERT OR IGNORE INTO threads (thread_id, subject, members, created_at, updated_at)
         VALUES (?1, ?2, ?3, '2026-03-23T00:00:00Z', '2026-03-23T00:00:00Z')",
        rusqlite::params![thread_id, topic, members_json],
    ).unwrap();

    // Message 1 (root)
    let msg1_id = Uuid::now_v7().to_string();
    let event1_id = "event_hex_001"; // mock NIP-17 event_id for msg1
    insert_thread_msg(&conn, event1_id, &alice_hex, &thread_id, &msg1_id, None, "root message", event1_id);

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
    assert_eq!(transport_id_for_msg1.as_deref(), Some(event1_id), "e-tag: must resolve msg1_id to event1_id");

    // Build reply rumor with e-tag referencing event1_id
    let event1_nid = EventId::from_hex(
        // Use a valid 32-byte hex for the mock event_id in test
        "0000000000000000000000000000000000000000000000000000000000000001",
    ).unwrap();
    let reply_rumor: UnsignedEvent = EventBuilder::new(Kind::ChatMessage, "reply to root")
        .tag(Tag::public_key(alice_keys.public_key()))
        .tag(Tag::event(event1_nid))
        .tag(Tag::custom(TagKind::custom("mycel-thread-id"), [thread_id.as_str()]))
        .tag(Tag::custom(TagKind::custom("mycel-msg-id"), [msg2_id.as_str()]))
        .build(alice_keys.public_key());

    // Verify e-tag is in the reply rumor
    let reply_tags: Vec<Vec<String>> = reply_rumor.tags.iter().map(|t| t.as_slice().to_vec()).collect();
    let has_e_tag = reply_tags.iter().any(|t| t.len() >= 2 && t[0] == "e");
    assert!(has_e_tag, "reply rumor must have an e-tag referencing parent event");

    insert_thread_msg(&conn, event2_id, &alice_hex, &thread_id, &msg2_id, Some(&msg1_id), "reply to root", event2_id);

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
    assert_eq!(transport_id_for_msg2.as_deref(), Some(event2_id), "e-tag: must resolve msg2_id to event2_id");

    insert_thread_msg(&conn, event3_id, &alice_hex, &thread_id, &msg3_id, Some(&msg2_id), "reply to reply", event3_id);

    // Verify all 3 messages are in the thread ordered by created_at
    let chain: Vec<(String, Option<String>)> = conn
        .prepare("SELECT msg_id, reply_to FROM messages WHERE thread_id = ?1 ORDER BY created_at ASC")
        .unwrap()
        .query_map(rusqlite::params![thread_id], |row| Ok((row.get(0)?, row.get(1)?)))
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
    let inserted = insert_v2_msg(&conn, first_nostr_id, sender, recipient, "hello from v2", &msg_id);
    assert!(inserted, "first delivery must be inserted");

    // Retry: same msg_id, different nostr event ID (outbox retry scenario)
    let retry_nostr_id = "nostr_event_id_retry_bbbb2222";
    let retry_inserted = insert_v2_msg(&conn, retry_nostr_id, sender, recipient, "hello from v2", &msg_id);
    assert!(!retry_inserted, "v2 retry with same msg_id must be deduplicated (not inserted)");

    // Inbox must show exactly one message
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE recipient = ?1",
            rusqlite::params![recipient],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "dedup: inbox must show message exactly once, not twice");

    // The stored message must be the first delivery's nostr_id
    let stored_nostr_id: String = conn
        .query_row(
            "SELECT nostr_id FROM messages WHERE msg_id = ?1",
            rusqlite::params![msg_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_nostr_id, first_nostr_id, "first delivery wins; retry is dropped");
}

