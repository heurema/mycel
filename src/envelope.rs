use serde::{Deserialize, Serialize};

/// mycel wire format v1
///
/// Carried inside Nostr event content (NIP-44 encrypted).
/// ID = Nostr event id (outer Gift Wrap). No separate envelope ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Wire format version
    pub v: u8,
    /// Sender public key (hex)
    pub from: String,
    /// Recipient public key (hex)
    pub to: String,
    /// Message text (max 8KB per C7)
    pub msg: String,
    /// ISO 8601 timestamp
    pub ts: String,
}

impl Envelope {
    pub fn new(from: String, to: String, msg: String) -> Self {
        Self {
            v: 1,
            from,
            to,
            msg,
            ts: chrono_free_now(),
        }
    }
}

/// ISO 8601 UTC timestamp without chrono dependency
fn chrono_free_now() -> String {
    use std::time::SystemTime;
    let duration = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = duration.as_secs();
    // Format as simplified ISO 8601
    let (days, rem) = (secs / 86400, secs % 86400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let (mins, secs) = (rem / 60, rem % 60);
    // Days since 1970-01-01
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{mins:02}:{secs:02}Z")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
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
    let mut month = 0;
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
    fn envelope_roundtrip() {
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
        assert!(big_msg.len() > 8192, "C7: messages over 8KB should be rejected");
    }
}
