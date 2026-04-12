use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::envelope;
use crate::error::MAX_MESSAGE_SIZE;
use crate::store::{self, IngressFrameRow, MessageRow};
use crate::types::{DeliveryStatus, Direction, MessageMeta, Part, ReadStatus, TrustTier};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    pub processed: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub errors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NostrAuthMeta {
    event_ts: u64,
}

enum FrameDecision {
    Message(Box<MessageRow>, MessageMeta),
    AckHandled,
    Reject(String),
}

pub async fn ingest_pending(db: &store::Db) -> Result<IngestReport> {
    db.run(ingest_pending_conn).await
}

pub fn ingest_pending_conn(conn: &Connection) -> Result<IngestReport> {
    let frames = store::get_pending_ingress_frames(conn)?;
    let mut report = IngestReport::default();

    for frame in frames {
        report.processed += 1;
        match ingest_one_frame(conn, &frame) {
            Ok(FrameDecision::Message(row, meta)) => {
                let inserted = store::insert_message_v2(conn, &row, &meta)?;
                store::update_ingress_frame_result(conn, &frame.frame_id, "accepted", None)?;
                if inserted {
                    report.accepted += 1;
                }
            }
            Ok(FrameDecision::AckHandled) => {
                store::update_ingress_frame_result(conn, &frame.frame_id, "accepted", None)?;
            }
            Ok(FrameDecision::Reject(reason)) => {
                store::update_ingress_frame_result(
                    conn,
                    &frame.frame_id,
                    "rejected",
                    Some(&reason),
                )?;
                report.rejected += 1;
            }
            Err(e) => {
                store::update_ingress_frame_result(
                    conn,
                    &frame.frame_id,
                    "error",
                    Some(&e.to_string()),
                )?;
                report.errors += 1;
            }
        }
    }

    Ok(report)
}

fn ingest_one_frame(conn: &Connection, frame: &IngressFrameRow) -> Result<FrameDecision> {
    let env: envelope::Envelope = match serde_json::from_str(&frame.envelope_json) {
        Ok(env) => env,
        Err(_) => return Ok(FrameDecision::Reject("invalid envelope json".to_string())),
    };

    if env.v != 1 && env.v != 2 {
        return Ok(FrameDecision::Reject(format!(
            "unsupported envelope version: {}",
            env.v
        )));
    }

    if frame.envelope_json.len() > MAX_MESSAGE_SIZE {
        return Ok(FrameDecision::Reject(
            "envelope exceeds size limit".to_string(),
        ));
    }

    match frame.transport.as_str() {
        "nostr" => {
            if let Some(sender_hint) = &frame.sender_hint
                && env.from != *sender_hint
            {
                return Ok(FrameDecision::Reject("nostr sender mismatch".to_string()));
            }
        }
        "local" | "local_direct" => {
            if let Some(sender_hint) = &frame.sender_hint
                && env.from != *sender_hint
            {
                return Ok(FrameDecision::Reject("local sender mismatch".to_string()));
            }
            if let Some(recipient_hint) = &frame.recipient_hint
                && env.to != *recipient_hint
            {
                return Ok(FrameDecision::Reject(
                    "local recipient mismatch".to_string(),
                ));
            }
            match env.verify_sig() {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    return Ok(FrameDecision::Reject(
                        "invalid or missing local signature".to_string(),
                    ));
                }
            }
        }
        other => {
            return Ok(FrameDecision::Reject(format!(
                "unsupported transport: {other}"
            )));
        }
    }

    if let Some(decision) = maybe_handle_ack(conn, &env, &env.from)? {
        return Ok(decision);
    }

    let trust_tier = match store::get_contact_by_pubkey(conn, &env.from)? {
        Some(c) => c.trust_tier,
        None => TrustTier::Unknown,
    };

    if trust_tier == TrustTier::Blocked {
        return Ok(FrameDecision::Reject("blocked sender".to_string()));
    }

    let msg_id = if env.v == 2 && !env.msg_id.is_empty() {
        Some(env.msg_id.clone())
    } else {
        None
    };

    let (thread_id, reply_to) = if trust_tier == TrustTier::Known {
        (env.thread_id.clone(), env.reply_to.clone())
    } else {
        (None, None)
    };

    let created_at = match frame.transport.as_str() {
        "nostr" => frame
            .auth_meta_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<NostrAuthMeta>(json).ok())
            .map(|meta| envelope::timestamp_to_iso8601(meta.event_ts))
            .unwrap_or_else(|| env.ts.clone()),
        _ => env.ts.clone(),
    };

    let nostr_id = match frame.transport.as_str() {
        "local" | "local_direct" => frame.frame_id.clone(),
        _ => frame
            .transport_msg_id
            .clone()
            .unwrap_or_else(|| frame.frame_id.clone()),
    };

    let row = MessageRow {
        nostr_id,
        direction: Direction::In,
        sender: env.from.clone(),
        recipient: frame
            .recipient_hint
            .clone()
            .unwrap_or_else(|| env.to.clone()),
        content: extract_content(&env),
        delivery_status: DeliveryStatus::Received,
        read_status: ReadStatus::Unread,
        created_at,
        received_at: frame.received_at.clone(),
        sender_alias: None,
    };

    let meta = MessageMeta {
        msg_id,
        thread_id,
        reply_to,
        transport: Some(frame.transport.clone()),
        transport_msg_id: frame.transport_msg_id.clone(),
        source_frame_id: Some(frame.frame_id.clone()),
    };

    Ok(FrameDecision::Message(Box::new(row), meta))
}

fn maybe_handle_ack(
    conn: &Connection,
    env: &envelope::Envelope,
    sender_hex: &str,
) -> Result<Option<FrameDecision>> {
    let ack_part = env.parts.iter().find_map(|p| {
        if let Part::AckPart {
            original_msg_id,
            status,
            ack_ts,
        } = p
        {
            Some((original_msg_id.clone(), *status, ack_ts.clone()))
        } else {
            None
        }
    });

    let Some((original_msg_id, status, ack_ts)) = ack_part else {
        return Ok(None);
    };

    const ANTI_STORM_SECS: u64 = 60;
    let existing_recent: Option<String> = conn
        .query_row(
            "SELECT created_at FROM acks WHERE msg_id = ?1 AND ack_sender = ?2
             AND (CAST(strftime('%s', 'now') AS INTEGER) - CAST(strftime('%s', created_at) AS INTEGER)) < ?3
             LIMIT 1",
            rusqlite::params![original_msg_id, sender_hex, ANTI_STORM_SECS as i64],
            |row| row.get(0),
        )
        .ok();

    if existing_recent.is_some() {
        return Ok(Some(FrameDecision::AckHandled));
    }

    let ack_row = store::AckRow {
        msg_id: original_msg_id,
        ack_sender: sender_hex.to_string(),
        ack_status: status,
        created_at: ack_ts,
        sent_at: None,
    };

    let _ = store::insert_ack(conn, &ack_row)?;
    Ok(Some(FrameDecision::AckHandled))
}

fn extract_content(env: &envelope::Envelope) -> String {
    if !env.parts.is_empty() {
        env.parts
            .iter()
            .filter_map(|p| match p {
                Part::TextPart { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        env.msg.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store;

    fn open_mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(store::SCHEMA).unwrap();
        conn
    }

    fn make_nostr_frame(
        sender: &str,
        recipient: &str,
        msg_id: &str,
        text: &str,
    ) -> IngressFrameRow {
        let env = serde_json::json!({
            "v": 2,
            "msg_id": msg_id,
            "from": sender,
            "to": recipient,
            "ts": "2026-03-25T00:00:00Z",
            "parts": [{"type":"text","text":text}]
        });
        IngressFrameRow {
            frame_id: format!("nostr:{msg_id}"),
            transport: "nostr".to_string(),
            endpoint_id: None,
            agent_ref: None,
            transport_msg_id: Some(format!("event:{msg_id}")),
            sender_hint: Some(sender.to_string()),
            recipient_hint: Some(recipient.to_string()),
            envelope_json: env.to_string(),
            auth_meta_json: Some(
                serde_json::to_string(&NostrAuthMeta {
                    event_ts: 1742860800,
                })
                .unwrap(),
            ),
            received_at: "2026-03-25T00:00:01Z".to_string(),
            processed_at: None,
            status: "pending".to_string(),
            error: None,
        }
    }

    fn make_nostr_frame_with_envelope(
        sender: &str,
        recipient: &str,
        transport_msg_id: &str,
        envelope_json: serde_json::Value,
    ) -> IngressFrameRow {
        IngressFrameRow {
            frame_id: format!("nostr:{transport_msg_id}"),
            transport: "nostr".to_string(),
            endpoint_id: None,
            agent_ref: None,
            transport_msg_id: Some(transport_msg_id.to_string()),
            sender_hint: Some(sender.to_string()),
            recipient_hint: Some(recipient.to_string()),
            envelope_json: envelope_json.to_string(),
            auth_meta_json: Some(
                serde_json::to_string(&NostrAuthMeta {
                    event_ts: 1742860800,
                })
                .unwrap(),
            ),
            received_at: "2026-03-25T00:00:01Z".to_string(),
            processed_at: None,
            status: "pending".to_string(),
            error: None,
        }
    }

    #[test]
    fn test_ingest_pending_nostr_frame() {
        let conn = open_mem();
        let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
        let frame = make_nostr_frame(
            sender,
            recipient,
            "01950000-0000-7000-8000-000000000042",
            "hello",
        );
        store::insert_ingress_frame(&conn, &frame).unwrap();

        let report = ingest_pending_conn(&conn).unwrap();
        assert_eq!(report.accepted, 1);

        let stored: String = conn
            .query_row(
                "SELECT content FROM messages WHERE source_frame_id = ?1",
                rusqlite::params![frame.frame_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "hello");
    }

    #[test]
    fn test_ingest_accepts_v1_nostr_frame() {
        let conn = open_mem();
        let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
        let frame = make_nostr_frame_with_envelope(
            sender,
            recipient,
            "event:v1",
            serde_json::json!({
                "v": 1,
                "from": sender,
                "to": recipient,
                "msg": "hello v1",
                "ts": "2026-03-25T00:00:00Z"
            }),
        );
        store::insert_ingress_frame(&conn, &frame).unwrap();

        let report = ingest_pending_conn(&conn).unwrap();
        assert_eq!(report.accepted, 1);

        let row = conn
            .query_row(
                "SELECT content, msg_id FROM messages WHERE source_frame_id = ?1",
                rusqlite::params![frame.frame_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "hello v1");
        assert_eq!(row.1, None, "v1 frames must not synthesize msg_id");
    }

    #[test]
    fn test_ingest_rejects_unknown_envelope_version() {
        let conn = open_mem();
        let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
        let frame = make_nostr_frame_with_envelope(
            sender,
            recipient,
            "event:v3",
            serde_json::json!({
                "v": 3,
                "msg_id": "01950000-0000-7000-8000-000000000777",
                "from": sender,
                "to": recipient,
                "ts": "2026-03-25T00:00:00Z",
                "parts": []
            }),
        );
        store::insert_ingress_frame(&conn, &frame).unwrap();

        let report = ingest_pending_conn(&conn).unwrap();
        assert_eq!(report.rejected, 1);

        let status: String = conn
            .query_row(
                "SELECT status FROM ingress_frames WHERE frame_id = ?1",
                rusqlite::params![frame.frame_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "rejected");
    }

    #[test]
    fn test_ingest_rejects_invalid_local_sig() {
        let conn = open_mem();
        let sender = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let recipient = "1122334455667788990011223344556677889900112233445566778899001122";
        let env = serde_json::json!({
            "v": 2,
            "msg_id": "01950000-0000-7000-8000-000000000099",
            "from": sender,
            "to": recipient,
            "ts": "2026-03-25T00:00:00Z",
            "parts": [{"type":"text","text":"hello"}],
            "sig": "deadbeef"
        });
        let frame = IngressFrameRow {
            frame_id: "local:1".to_string(),
            transport: "local".to_string(),
            endpoint_id: None,
            agent_ref: None,
            transport_msg_id: Some("01950000-0000-7000-8000-000000000099".to_string()),
            sender_hint: Some(sender.to_string()),
            recipient_hint: Some(recipient.to_string()),
            envelope_json: env.to_string(),
            auth_meta_json: None,
            received_at: "2026-03-25T00:00:01Z".to_string(),
            processed_at: None,
            status: "pending".to_string(),
            error: None,
        };
        store::insert_ingress_frame(&conn, &frame).unwrap();

        let report = ingest_pending_conn(&conn).unwrap();
        assert_eq!(report.rejected, 1);

        let status: String = conn
            .query_row(
                "SELECT status FROM ingress_frames WHERE frame_id = ?1",
                rusqlite::params![frame.frame_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "rejected");
    }
}
