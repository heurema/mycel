use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

macro_rules! str_enum {
    ($(#[$meta:meta])* $vis:vis enum $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        $vis enum $name {
            $($variant),+
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant => f.write_str($s)),+
                }
            }
        }

        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok(Self::$variant)),+,
                    other => Err(format!(concat!("invalid ", stringify!($name), ": {}"), other)),
                }
            }
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $s),+
                }
            }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::Borrowed(
                    rusqlite::types::ValueRef::Text(self.as_str().as_bytes()),
                ))
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                let s = value.as_str()?;
                s.parse::<Self>().map_err(|e| rusqlite::types::FromSqlError::Other(e.into()))
            }
        }
    }
}

str_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Direction {
        In => "in",
        Out => "out",
    }
}

str_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TrustTier {
        Known => "known",
        Unknown => "unknown",
        Blocked => "blocked",
    }
}

str_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum DeliveryStatus {
        Pending => "pending",
        Received => "received",
        Delivered => "delivered",
        Failed => "failed",
        Blocked => "blocked",
        Confirmed => "confirmed",
    }
}

str_enum! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ReadStatus {
        Unread => "unread",
        Read => "read",
        Blocked => "blocked",
    }
}

/// V2 message identity metadata — the transport-independent fields added in schema v2.
/// Used alongside `MessageRow` when inserting messages with full v2 envelope data.
#[derive(Debug, Clone, Default)]
pub struct MessageMeta {
    /// Logical message ID (UUIDv7 for new messages, "legacy:<nostr_id>" for backfilled v1).
    pub msg_id: Option<String>,
    /// Thread identifier this message belongs to (sha256 of topic or UUID).
    pub thread_id: Option<String>,
    /// Parent `msg_id` for threaded replies (always references msg_id, not transport_msg_id).
    pub reply_to: Option<String>,
    /// Transport used to deliver the message ("nostr" or "local").
    pub transport: Option<String>,
    /// Per-transport copy identifier (Nostr event ID or local row ID).
    pub transport_msg_id: Option<String>,
}

/// A single content part of an Envelope v2 message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Part {
    /// Plain text content part.
    #[serde(rename = "text")]
    TextPart { text: String },
    /// Binary/file content part (skeleton for v0.4).
    #[serde(rename = "data")]
    DataPart { mime_type: String, data: String },
}

/// Role of the agent or user sending the message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Used in Envelope.role deserialization and thread message routing
pub enum AgentRole {
    User,         // human user
    Agent,        // general AI agent
    Coordinator,  // orchestration/routing agent
    Reviewer,     // code/output review agent
    Implementer,  // code/task implementation agent
}

/// A member of a thread with their join timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadMember {
    /// Hex-encoded public key.
    pub pubkey: String,
    /// ISO 8601 UTC timestamp when the member joined.
    pub joined_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_roundtrip() {
        assert_eq!(Direction::In.to_string(), "in");
        assert_eq!(Direction::Out.to_string(), "out");
        assert_eq!("in".parse::<Direction>().unwrap(), Direction::In);
        assert_eq!("out".parse::<Direction>().unwrap(), Direction::Out);
        assert!("invalid".parse::<Direction>().is_err());
    }

    #[test]
    fn trust_tier_roundtrip() {
        for tier in [TrustTier::Known, TrustTier::Unknown, TrustTier::Blocked] {
            let s = tier.to_string();
            assert_eq!(s.parse::<TrustTier>().unwrap(), tier);
        }
    }

    #[test]
    fn delivery_status_roundtrip() {
        for status in [
            DeliveryStatus::Pending,
            DeliveryStatus::Received,
            DeliveryStatus::Delivered,
            DeliveryStatus::Failed,
            DeliveryStatus::Blocked,
            DeliveryStatus::Confirmed,
        ] {
            let s = status.to_string();
            assert_eq!(s.parse::<DeliveryStatus>().unwrap(), status);
        }
    }

    #[test]
    fn fn_delivery_status_confirmed() {
        assert_eq!(DeliveryStatus::Confirmed.to_string(), "confirmed");
        assert_eq!("confirmed".parse::<DeliveryStatus>().unwrap(), DeliveryStatus::Confirmed);
    }

    #[test]
    fn read_status_roundtrip() {
        for status in [ReadStatus::Unread, ReadStatus::Read, ReadStatus::Blocked] {
            let s = status.to_string();
            assert_eq!(s.parse::<ReadStatus>().unwrap(), status);
        }
    }
}
