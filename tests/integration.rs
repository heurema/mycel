// Integration tests — send + receive roundtrip via a real public relay.
//
// These tests require network access and are marked #[ignore] by default.
// Run with: cargo test --test integration -- --ignored
//
// The integration test verifies end-to-end:
//   1. Two key pairs (sender + receiver)
//   2. Sender publishes a gift-wrap to a public relay
//   3. Receiver fetches and unwraps it
//   4. Content matches

use nostr_sdk::prelude::*;
use std::time::Duration;

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
