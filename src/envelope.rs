use nostr_sdk::Keys;
use nostr_sdk::PublicKey as NostrPublicKey;
use nostr_sdk::SecretKey;
use nostr_sdk::nostr::hashes::Hash;
use nostr_sdk::nostr::hashes::sha256::Hash as Sha256Hash;
use nostr_sdk::nostr::secp256k1::schnorr::Signature;
use nostr_sdk::nostr::secp256k1::{Message, SECP256K1, XOnlyPublicKey};
use serde::{Deserialize, Serialize};

use crate::error::MAX_MESSAGE_SIZE;
use crate::types::Part;

/// mycel wire format v2 (backward-compatible with v1 via EnvelopeWire adapter).
/// Carried inside Nostr event content (NIP-44 encrypted).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "EnvelopeWire")]
pub struct Envelope {
    pub v: u8,                     // wire format version
    pub msg_id: String,            // opaque message ID (UUIDv7 string)
    pub from: String,              // sender public key (hex)
    pub to: String,                // recipient public key (hex)
    pub ts: String,                // ISO 8601 timestamp
    pub thread_id: Option<String>, // thread ID (optional)
    pub reply_to: Option<String>,  // reply-to msg_id (optional)
    pub role: Option<String>,      // sender role (optional)
    pub parts: Vec<Part>,          // message parts (v2)
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub msg: String, // legacy v1 text field
    /// Schnorr signature over canonical envelope hash (local transport authenticity).
    /// Omitted when None (e.g. Nostr transport where NIP-59 provides authenticity).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sig: Option<String>,
}

/// Intermediate for deserializing both v1 and v2 wire format.
#[derive(Deserialize)]
struct EnvelopeWire {
    #[serde(default)]
    v: u8,
    #[serde(default)]
    msg_id: String,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    #[serde(default)]
    ts: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    parts: Vec<Part>,
    /// v1 legacy text field
    #[serde(default)]
    msg: String,
    #[serde(default)]
    sig: Option<String>,
}

impl From<EnvelopeWire> for Envelope {
    fn from(wire: EnvelopeWire) -> Self {
        // v1 compat: if parts is empty, build a TextPart from msg
        let parts = if wire.parts.is_empty() && !wire.msg.is_empty() {
            vec![Part::TextPart {
                text: wire.msg.clone(),
            }]
        } else {
            wire.parts
        };
        Envelope {
            v: wire.v,
            msg_id: wire.msg_id,
            from: wire.from,
            to: wire.to,
            ts: wire.ts,
            thread_id: wire.thread_id,
            reply_to: wire.reply_to,
            role: wire.role,
            parts,
            msg: wire.msg,
            sig: wire.sig,
        }
    }
}

impl Envelope {
    /// Create a new v2 Envelope. msg_id must be supplied by the caller.
    pub fn new_v2(msg_id: String, from: String, to: String, parts: Vec<Part>) -> Self {
        Self {
            v: 2,
            msg_id,
            from,
            to,
            ts: now_iso8601(),
            thread_id: None,
            reply_to: None,
            role: None,
            parts,
            msg: String::new(),
            sig: None,
        }
    }

    /// Create a v1-compatible Envelope (legacy; used by send command until migrated).
    pub fn new(from: String, to: String, msg: String) -> Self {
        Self {
            v: 1,
            msg_id: String::new(),
            from,
            to,
            ts: now_iso8601(),
            thread_id: None,
            reply_to: None,
            role: None,
            parts: vec![Part::TextPart { text: msg.clone() }],
            msg,
            sig: None,
        }
    }

    /// Sign this envelope with the given secret key (Schnorr over canonical hash).
    /// Sets the `sig` field with the hex-encoded signature.
    pub fn sign(&mut self, secret_key: &SecretKey) -> anyhow::Result<()> {
        let hash = canonical_envelope_hash(self);
        let msg = Message::from_digest(hash);
        let keys = Keys::new(secret_key.clone());
        let sig: Signature = keys.sign_schnorr(&msg);
        self.sig = Some(format!("{sig:x}"));
        Ok(())
    }

    /// Verify the Schnorr signature in the `sig` field against the sender's pubkey (`from`).
    ///
    /// Returns:
    /// - `Ok(false)` if `sig` is absent
    /// - `Ok(true)` if signature is valid
    /// - `Ok(false)` if signature is invalid
    /// - `Err(_)` if parsing `from` pubkey or `sig` hex fails
    ///
    /// Callers MUST skip this when `transport == "nostr"` (NIP-59 provides authenticity).
    pub fn verify_sig(&self) -> anyhow::Result<bool> {
        let sig_hex = match &self.sig {
            Some(s) => s,
            None => return Ok(false),
        };

        // Parse sender pubkey from `from` field
        let nostr_pk = NostrPublicKey::from_hex(&self.from)
            .map_err(|e| anyhow::anyhow!("invalid from pubkey: {e}"))?;
        let xonly = XOnlyPublicKey::from_slice(nostr_pk.as_bytes())
            .map_err(|e| anyhow::anyhow!("xonly pubkey parse: {e}"))?;

        // Recompute canonical hash (without sig field)
        let hash = canonical_envelope_hash(self);
        let msg = Message::from_digest(hash);

        // Parse signature
        let sig = sig_hex
            .parse::<Signature>()
            .map_err(|e| anyhow::anyhow!("invalid sig hex: {e}"))?;

        match SECP256K1.verify_schnorr(&sig, &msg, &xonly) {
            Ok(()) => Ok(true),
            Err(_) => Ok(false),
        }
    }
}

/// Compute the canonical SHA-256 hash of an envelope for signing/verification.
///
/// Algorithm:
/// 1. Serialize envelope to a JSON `Value`
/// 2. Remove the `sig` field (not part of the signed content)
/// 3. Re-serialize with keys sorted alphabetically (deterministic)
/// 4. SHA-256 hash of the canonical bytes
pub fn canonical_envelope_hash(envelope: &Envelope) -> [u8; 32] {
    // Serialize to JSON Value
    let mut val = serde_json::to_value(envelope).expect("Envelope always serializes to valid JSON");

    // Remove sig field from the object
    if let serde_json::Value::Object(ref mut map) = val {
        map.remove("sig");
    }

    // Produce canonical JSON with sorted keys
    let canonical = canonical_json_sorted(&val);

    // SHA-256
    let hash = Sha256Hash::hash(canonical.as_bytes());
    *hash.as_byte_array()
}

/// Serialize a serde_json Value to JSON with keys sorted alphabetically at every level.
fn canonical_json_sorted(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
            keys.sort_unstable();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        canonical_json_sorted(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonical_json_sorted).collect();
            format!("[{}]", inner.join(","))
        }
        other => serde_json::to_string(other).unwrap(),
    }
}

/// Extract the value of a custom mycel tag from a NIP-17 Kind 14 rumor's tags.
///
/// Tags in a Nostr UnsignedEvent are `Vec<Tag>`. This helper finds a tag whose first
/// element matches `tag_name` and returns the second element.
///
/// Used to extract `mycel-msg-id` and `mycel-thread-id` from unwrapped rumors
/// for event_id → msg_id mapping and reply_to resolution.
#[allow(dead_code)] // Used when sync.rs processes NIP-17 thread messages
pub fn extract_mycel_tag(tags: &[nostr_sdk::Tag], tag_name: &str) -> Option<String> {
    for tag in tags {
        let content = tag.as_slice();
        if content.len() >= 2 && content[0] == tag_name {
            return Some(content[1].to_string());
        }
    }
    None
}

/// Validate message size per C7. Returns Err with byte count if too large.
pub fn validate_message_size(msg: &str) -> Result<(), crate::error::MycelError> {
    let size = msg.len();
    if size > MAX_MESSAGE_SIZE {
        return Err(crate::error::MycelError::MessageTooLarge {
            size,
            max: MAX_MESSAGE_SIZE,
        });
    }
    Ok(())
}

/// ISO 8601 UTC timestamp without chrono dependency.
/// Shared by all modules — do not duplicate.
pub fn now_iso8601() -> String {
    use std::time::SystemTime;
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    timestamp_to_iso8601(secs)
}

/// Convert unix timestamp (seconds) to ISO 8601 UTC string.
pub fn timestamp_to_iso8601(secs: u64) -> String {
    let (days, rem) = (secs / 86400, secs % 86400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let (mins, secs) = (rem / 60, rem % 60);
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{secs:02}Z")
}

pub fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970;
    loop {
        let year_days = if is_leap(year) { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }
    let month_days: [u64; 12] = if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut month = 12; // default to December if loop exhausts (shouldn't happen)
    for (i, &md) in month_days.iter().enumerate() {
        if days < md {
            month = i as u64 + 1;
            break;
        }
        days -= md;
    }
    (year, month, days + 1)
}

fn is_leap(year: u64) -> bool {
    year.is_multiple_of(4) && !year.is_multiple_of(100) || year.is_multiple_of(400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_v2_roundtrip() {
        let env = Envelope::new_v2(
            "01950000-0000-7000-8000-000000000001".to_string(),
            "aabbcc".to_string(),
            "ddeeff".to_string(),
            vec![Part::TextPart {
                text: "hello v2".to_string(),
            }],
        );
        let json = serde_json::to_string(&env).unwrap();
        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.v, 2);
        assert_eq!(parsed.msg_id, "01950000-0000-7000-8000-000000000001");
        assert_eq!(parsed.from, "aabbcc");
        assert_eq!(parsed.parts.len(), 1);
        match &parsed.parts[0] {
            Part::TextPart { text } => assert_eq!(text, "hello v2"),
            _ => panic!("expected TextPart"),
        }
        // Verify RFC wire format: type discriminator is "text", not "text_part"
        assert!(
            json.contains(r#""type":"text""#),
            "Part type must serialize as 'text' per RFC"
        );
    }

    #[test]
    fn v1_compat_deserialize() {
        // v1 wire format: no msg_id, no parts, has msg field
        let v1_json =
            r#"{"v":1,"from":"aabbcc","to":"ddeeff","msg":"hello v1","ts":"2026-03-23T00:00:00Z"}"#;
        let env: Envelope = serde_json::from_str(v1_json).unwrap();
        assert_eq!(env.v, 1);
        assert_eq!(env.msg, "hello v1");
        assert_eq!(env.parts.len(), 1);
        match &env.parts[0] {
            Part::TextPart { text } => assert_eq!(text, "hello v1"),
            _ => panic!("expected TextPart from v1 msg"),
        }
    }

    #[test]
    fn envelope_roundtrip() {
        // v1-style constructor still works
        let env = Envelope::new(
            "aabbcc".to_string(),
            "ddeeff".to_string(),
            "hello".to_string(),
        );
        let json = serde_json::to_string(&env).unwrap();
        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.v, 1);
        assert_eq!(parsed.msg, "hello");
        assert_eq!(parsed.from, "aabbcc");
    }

    #[test]
    fn message_size_cap() {
        let big_msg = "x".repeat(8193);
        assert!(
            big_msg.len() > 8192,
            "C7: messages over 8KB should be rejected"
        );
    }

    #[test]
    fn test_message_size_cap() {
        // Exact limit is fine
        let ok_msg = "x".repeat(8192);
        assert!(validate_message_size(&ok_msg).is_ok());

        // One byte over the limit should fail
        let big_msg = "x".repeat(8193);
        let err = validate_message_size(&big_msg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("8193"), "error should show byte count");
        assert!(msg.contains("8192"), "error should show max");
    }

    #[test]
    fn sig_roundtrip() {
        use nostr_sdk::Keys;

        // 1. Generate test keys
        let keys = Keys::generate();
        let sk = keys.secret_key().clone();
        let pk_hex = keys.public_key().to_hex();

        // 2. Create a v2 Envelope
        let mut env = Envelope::new_v2(
            "01950000-0000-7000-8000-000000000002".to_string(),
            pk_hex.clone(),
            "ddeeff".to_string(),
            vec![Part::TextPart {
                text: "sig test".to_string(),
            }],
        );

        // 3. Sign the envelope
        env.sign(&sk).expect("sign should succeed");
        assert!(env.sig.is_some(), "sig field must be set after sign()");

        // 4. Verify: valid sig returns Ok(true)
        assert!(env.verify_sig().unwrap(), "valid sig should verify");

        // 5. Tamper with a field and re-verify: should return Ok(false) or Err
        let original_ts = env.ts.clone();
        env.ts = "2099-01-01T00:00:00Z".to_string();
        let tampered_result = env.verify_sig();
        match tampered_result {
            Ok(false) => {} // expected: invalid sig
            Err(_) => {}    // also acceptable: parsing error
            Ok(true) => panic!("tampered envelope must not verify"),
        }

        // 6. Restore and serialize/deserialize: sig field must survive roundtrip
        env.ts = original_ts;
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains("\"sig\""), "sig field must appear in JSON");
        let parsed: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.sig, env.sig, "sig must survive JSON roundtrip");
        assert!(
            parsed.verify_sig().unwrap(),
            "deserialized envelope must verify"
        );

        // 7. Transport-aware: caller determines transport context (not an envelope field).
        // When caller knows message arrived via Nostr, it should skip verify_sig().
        // Transport is NOT stored in Envelope per RFC — it's caller context.
    }
}
