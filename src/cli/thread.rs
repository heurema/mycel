use anyhow::Result;
use clap::Subcommand;
use nostr_sdk::prelude::*;
use std::time::Duration;
use uuid::Uuid;

use crate::config;
use crate::core::router;
use crate::crypto;
use crate::envelope;
use crate::error::MycelError;
use crate::nostr as mycel_nostr;
use crate::store;
use crate::types::{DeliveryStatus, Direction, MessageMeta, Part, ReadStatus, ThreadMember};

/// Subcommands for `mycel thread`.
#[derive(Subcommand)]
pub enum ThreadCommand {
    /// Create a new thread with a topic and members
    Create {
        /// Thread topic / subject
        topic: String,
        /// Member pubkeys (hex or npub) — up to 10
        #[arg(required = true)]
        members: Vec<String>,
    },
    /// Send a message to an existing thread
    Send {
        /// Thread ID
        thread_id: String,
        /// Message text
        message: String,
        /// Reply to a specific msg_id (optional)
        #[arg(long)]
        reply_to: Option<String>,
    },
    /// Show messages in a thread
    Log {
        /// Thread ID
        thread_id: String,
        /// Output as JSONL (one message per line)
        #[arg(long)]
        json: bool,
    },
}

/// Entry point for `mycel thread <subcommand>`.
pub async fn run(cmd: ThreadCommand) -> Result<()> {
    match cmd {
        ThreadCommand::Create { topic, members } => create_thread(&topic, &members).await,
        ThreadCommand::Send {
            thread_id,
            message,
            reply_to,
        } => send_thread_message(&thread_id, &message, reply_to.as_deref()).await,
        ThreadCommand::Log { thread_id, json } => log_thread(&thread_id, json).await,
    }
}

/// `mycel thread create <topic> <member1> [member2 ...]`
///
/// Creates a new thread. Validates member count <= 10. Generates thread_id as
/// SHA-256 of the topic string. Inserts into threads table. Prints thread_id to stdout.
pub async fn create_thread(topic: &str, members: &[String]) -> Result<()> {
    // Reject empty topic (SHA-256 of "" is deterministic → thread_id collision)
    if topic.trim().is_empty() {
        return Err(anyhow::anyhow!("topic cannot be empty"));
    }

    // Reject > 10 members (fan-out budget per RFC Contract 6)
    if members.len() > 10 {
        return Err(MycelError::ThreadMemberLimitExceeded.into());
    }

    // Load config + keys
    let cfg = config::load()?;
    let enc_path = config::config_dir()?.join("key.enc");
    let keys = crypto::load_keys(&enc_path, cfg.identity.storage)?;
    let sender_hex = keys.public_key().to_hex();

    // Generate thread_id as hex-encoded SHA-256 of topic
    let thread_id = sha256_hex(topic.as_bytes());

    // Resolve member pubkeys (hex). Include sender as a member (self-copy).
    let mut member_pubkeys: Vec<String> = vec![sender_hex.clone()];
    for m in members {
        let hex = resolve_member_pubkey(m)?;
        if !member_pubkeys.contains(&hex) {
            member_pubkeys.push(hex);
        }
    }

    // Re-check after including self
    if member_pubkeys.len() > 10 {
        return Err(MycelError::ThreadMemberLimitExceeded.into());
    }

    let now = envelope::now_iso8601();
    let thread_members: Vec<ThreadMember> = member_pubkeys
        .iter()
        .map(|pk| ThreadMember {
            pubkey: pk.clone(),
            joined_at: now.clone(),
        })
        .collect();
    let members_json = serde_json::to_string(&thread_members)?;

    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;
    let thread_id_clone = thread_id.clone();
    let topic_clone = topic.to_string();
    let now_clone = now.clone();
    let inserted = db
        .run(move |conn| {
            store::insert_thread(
                conn,
                &store::ThreadRow {
                    thread_id: thread_id_clone,
                    subject: Some(topic_clone),
                    members: members_json,
                    created_at: now_clone.clone(),
                    updated_at: now_clone,
                },
            )
        })
        .await?;

    if inserted {
        println!("{thread_id}");
    } else {
        // Thread already exists — idempotent, just print the id
        println!("{thread_id}");
    }

    Ok(())
}

/// `mycel thread send <thread_id> <message> [--reply-to <msg_id>]`
///
/// Sends a message to all thread members via NIP-17 multi_recipient_gift_wrap.
/// Inserts message into DB with transport='nostr', transport_msg_id = first event_id.
/// Prints msg_id to stdout.
pub async fn send_thread_message(
    thread_id: &str,
    message: &str,
    reply_to: Option<&str>,
) -> Result<()> {
    if message.trim().is_empty() {
        return Err(MycelError::EmptyMessage.into());
    }
    envelope::validate_message_size(message)?;

    let cfg = config::load()?;
    let enc_path = config::config_dir()?.join("key.enc");
    let keys = crypto::load_keys(&enc_path, cfg.identity.storage)?;
    let sender_hex = keys.public_key().to_hex();

    // Load thread from DB
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;
    let tid = thread_id.to_string();
    let thread = db
        .run(move |conn| store::get_thread(conn, &tid))
        .await?
        .ok_or_else(|| MycelError::ThreadNotFound {
            thread_id: thread_id.to_string(),
        })?;

    let members: Vec<ThreadMember> = serde_json::from_str(&thread.members).unwrap_or_default();
    let member_pubkeys: Vec<String> = members.iter().map(|m| m.pubkey.clone()).collect();
    let member_routes = router::resolve_thread_member_routes(&member_pubkeys)?;
    let member_pubkeys: Vec<String> = member_routes
        .iter()
        .map(|route| route.pubkey_hex.clone())
        .collect();

    // Generate UUIDv7 msg_id for this message
    let msg_id = Uuid::now_v7().to_string();

    // Resolve reply_to msg_id → NIP-17 event_id for e-tag
    let reply_event_id: Option<String> = if let Some(parent_msg_id) = reply_to {
        let pmid = parent_msg_id.to_string();
        db.run(move |conn| store::get_transport_msg_id_by_msg_id(conn, &pmid))
            .await?
    } else {
        None
    };

    // Determine subject tag: include on first message (no prior messages in thread)
    let tid2 = thread_id.to_string();
    let has_messages = db
        .run(move |conn| {
            let msgs = store::get_thread_messages(conn, &tid2)?;
            Ok(msgs.len())
        })
        .await?;
    let subject = if has_messages == 0 {
        thread.subject.as_deref()
    } else {
        None
    };

    let relay_urls = cfg.relays.urls.clone();
    let timeout = Duration::from_secs(cfg.relays.timeout_secs);

    // Build envelope v2 for the message content
    let env = envelope::Envelope::new_v2(
        msg_id.clone(),
        sender_hex.clone(),
        thread_id.to_string(),
        vec![Part::TextPart {
            text: message.to_string(),
        }],
    );
    let env_json = serde_json::to_string(&env)?;

    // Fan-out via NIP-17 multi_recipient_gift_wrap
    let event_ids = mycel_nostr::multi_recipient_gift_wrap(
        &keys,
        &member_pubkeys,
        &env_json,
        &relay_urls,
        thread_id,
        &msg_id,
        subject,
        reply_event_id.as_deref(),
        timeout,
    )
    .await?;

    // transport_msg_id = first event_id from fan-out (sender's own copy preferred)
    let transport_msg_id = event_ids
        .get(&sender_hex)
        .or_else(|| event_ids.values().next())
        .cloned()
        .unwrap_or_else(|| msg_id.clone());

    let now = envelope::now_iso8601();

    // Store outbound copy in DB
    let msg_row = store::MessageRow {
        nostr_id: transport_msg_id.clone(),
        direction: Direction::Out,
        sender: sender_hex.clone(),
        recipient: thread_id.to_string(),
        content: message.to_string(),
        delivery_status: if event_ids.is_empty() {
            DeliveryStatus::Failed
        } else {
            DeliveryStatus::Delivered
        },
        read_status: ReadStatus::Read,
        created_at: env.ts.clone(),
        received_at: now.clone(),
        sender_alias: None,
    };
    let meta = MessageMeta {
        msg_id: Some(msg_id.clone()),
        thread_id: Some(thread_id.to_string()),
        reply_to: reply_to.map(|s| s.to_string()),
        transport: Some("nostr".to_string()),
        transport_msg_id: Some(transport_msg_id.clone()),
        source_frame_id: None,
    };

    db.run(move |conn| store::insert_message_v2(conn, &msg_row, &meta).map(|_| ()))
        .await?;

    println!("{msg_id}");
    Ok(())
}

/// `mycel thread log <thread_id> [--json]`
///
/// Fetches all messages in thread_id from DB ordered by created_at ASC.
/// Displays in human-readable format or JSONL (--json).
pub async fn log_thread(thread_id: &str, json: bool) -> Result<()> {
    let db = store::Db::open(&config::data_dir()?.join("mycel.db"))?;

    // Verify thread exists
    let tid = thread_id.to_string();
    let thread = db
        .run(move |conn| store::get_thread(conn, &tid))
        .await?
        .ok_or_else(|| MycelError::ThreadNotFound {
            thread_id: thread_id.to_string(),
        })?;

    let tid2 = thread_id.to_string();
    // Use full message query with WHERE thread_id = ?
    let rows = db
        .run(move |conn| get_thread_messages_with_meta(conn, &tid2))
        .await?;

    if rows.is_empty() {
        if json {
            // empty JSONL — just print nothing
        } else {
            println!(
                "Thread: {} ({})",
                thread.subject.as_deref().unwrap_or("(no subject)"),
                thread_id
            );
            println!("No messages.");
        }
        return Ok(());
    }

    if json {
        for (msg, meta) in &rows {
            let obj = serde_json::json!({
                "msg_id": meta.msg_id,
                "thread_id": meta.thread_id,
                "reply_to": meta.reply_to,
                "sender": msg.sender,
                "content": msg.content,
                "created_at": msg.created_at,
                "transport_msg_id": meta.transport_msg_id,
            });
            println!("{}", serde_json::to_string(&obj)?);
        }
    } else {
        println!(
            "Thread: {} ({})",
            thread.subject.as_deref().unwrap_or("(no subject)"),
            thread_id
        );
        println!("{}", "─".repeat(60));
        for (msg, meta) in &rows {
            let msg_id_display = meta.msg_id.as_deref().unwrap_or("?");
            println!(
                "[{}] {} | msg_id: {}",
                msg.created_at, msg.sender, msg_id_display
            );
            println!("  {}", msg.content);
            if let Some(reply_to) = &meta.reply_to {
                println!("  (reply to: {})", reply_to);
            }
            println!();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Query thread messages with full v2 metadata using WHERE thread_id = ?
fn get_thread_messages_with_meta(
    conn: &rusqlite::Connection,
    thread_id: &str,
) -> Result<Vec<(store::MessageRow, MessageMeta)>> {
    // WHERE thread_id = ? ensures we only get messages for this thread
    store::get_thread_messages_full(conn, thread_id)
}

/// Resolve a member string (hex pubkey or npub bech32) to hex pubkey.
fn resolve_member_pubkey(member: &str) -> Result<String> {
    // Try parsing as hex pubkey first
    if let Ok(pk) = PublicKey::from_hex(member) {
        return Ok(pk.to_hex());
    }
    // Try npub bech32
    if let Ok(pk) = PublicKey::from_bech32(member) {
        return Ok(pk.to_hex());
    }
    Err(anyhow::anyhow!(
        "invalid member pubkey: '{}' — expected hex or npub bech32",
        member
    ))
}

/// Compute SHA-256 hex of bytes (for thread_id generation from topic string).
fn sha256_hex(input: &[u8]) -> String {
    use nostr_sdk::nostr::hashes::Hash;
    use nostr_sdk::nostr::hashes::sha256::Hash as Sha256Hash;
    let hash = Sha256Hash::hash(input);
    format!("{hash:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256_hex_deterministic() {
        let h1 = sha256_hex(b"test-topic");
        let h2 = sha256_hex(b"test-topic");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64, "SHA-256 hex must be 64 chars");
    }

    #[test]
    fn test_resolve_member_pubkey_hex() {
        let keys = nostr_sdk::Keys::generate();
        let hex = keys.public_key().to_hex();
        let resolved = resolve_member_pubkey(&hex).unwrap();
        assert_eq!(resolved, hex);
    }

    #[test]
    fn test_resolve_member_pubkey_invalid() {
        let err = resolve_member_pubkey("not-a-pubkey");
        assert!(err.is_err());
    }
}
