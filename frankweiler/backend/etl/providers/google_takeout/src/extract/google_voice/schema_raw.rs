//! Raw-store schema for the Google Voice feed.
//!
//! Declarations-only, mirroring the crate's top-level `schema_raw`. The
//! tables here are merged into the crate's `full_ddl()` /
//! `DATA_TABLES` / `EDGE_TABLES` so `RawDb::open` creates them and
//! `reset()` truncates them.
//!
//! ## Identity (idempotent)
//!
//! Google Voice gives us no per-record IDs, so every row's PK is a
//! uuidv5 over a recipe of the most stable fields. Re-ingesting the same
//! export reproduces identical IDs (the recipes are pure functions of
//! the parsed content):
//!
//!   - message: `voice:msg:{folder}:{conversation}:{rfc3339_millis}:{sender}:{sha8(body)}`
//!     — the ms-precision timestamp + sender + body hash is effectively
//!     unique and stable.
//!   - call/voicemail event: `voice:{kind}:{folder}:{party}:{published}`.
//!   - bill: `voice:bill:{sha8(row cells)}`.
//!   - greeting: `voice:greeting:{filename}`.

use frankweiler_etl::doltlite_raw::{WirePayload, WirePayloadRow};
use frankweiler_etl_macros::{CasEdgeRow, WirePayloadRow};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Entity (wire-payload) tables — each gets a `_bookkeeping` sidecar via
/// the crate's `full_ddl()` loop.
pub const VOICE_DATA_TABLES: &[&str] = &["voice_messages", "voice_bills", "voice_greetings"];

/// CAS edge tables (image / audio / recording / greeting blobs).
pub const VOICE_EDGE_TABLES: &[&str] = &["voice_attachments"];

/// Per-feed uuidv5 namespace.
fn voice_ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"google-voice.frankweiler")
}

/// uuidv5 of a recipe under the Voice namespace.
pub fn ns_id(recipe: &str) -> String {
    Uuid::new_v5(&voice_ns(), recipe.as_bytes())
        .as_hyphenated()
        .to_string()
}

/// First 8 hex chars of the sha256 of `s` — a short, stable content tag
/// for identity recipes.
pub fn sha8(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    hex8(&digest)
}

fn hex8(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(8);
    for b in bytes.iter().take(4) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `voice_messages` — one row per text message OR call/voicemail event.
/// Promoted columns drive the render-side grouping/sorting without
/// cracking the JSON.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "voice_messages")]
pub struct VoiceMessageRow {
    pub id_and_payload: WirePayload,
    /// Channel key — the other party (E.164 or contact label).
    pub conversation_key: Option<String>,
    /// RFC3339 (millis) event time, for sorting + month bucketing.
    pub when_ts: Option<String>,
    /// `text|voicemail|missed|placed|received|recorded`.
    pub kind: Option<String>,
    /// `calls|spam` — which folder it came from.
    pub folder: Option<String>,
}

/// `voice_bills` — one row per `Bills.html` table row.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "voice_bills")]
pub struct VoiceBillRow {
    pub id_and_payload: WirePayload,
}

/// `voice_greetings` — one row per voicemail greeting; `blake3` is the
/// CAS key of the audio bytes.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "voice_greetings")]
pub struct VoiceGreetingRow {
    pub id_and_payload: WirePayload,
    pub blake3: Option<String>,
}

/// `voice_attachments` — per-provider CAS edge for blobs referenced by
/// voice messages (MMS images, voicemail/recording audio). Owning
/// entity: the message id. Ref: the attachment's filename.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "voice_attachments")]
pub struct VoiceAttachmentRow {
    pub id: String,
    pub message_id: String,
    pub ref_name: String,
    pub blake3: Option<String>,
}

/// Entity-table DDL for the Voice feed (the crate's `full_ddl()` appends
/// the `_bookkeeping` sidecars + CAS edge DDL via the shared loop).
pub fn voice_table_ddl() -> Vec<String> {
    use frankweiler_etl::blob_cas::CasEdgeRow as _;
    let mut out = vec![
        VoiceMessageRow::ddl(),
        VoiceBillRow::ddl(),
        VoiceGreetingRow::ddl(),
    ];
    out.extend(VoiceAttachmentRow::all_ddl());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ns_id_is_stable_and_distinct() {
        let a = ns_id("voice:msg:calls:+1:2024-01-01T00:00:00.000Z:Me:abcd1234");
        assert_eq!(
            a,
            ns_id("voice:msg:calls:+1:2024-01-01T00:00:00.000Z:Me:abcd1234")
        );
        assert_eq!(a.len(), 36);
        assert_ne!(
            a,
            ns_id("voice:msg:calls:+1:2024-01-01T00:00:01.000Z:Me:abcd1234")
        );
    }

    #[test]
    fn sha8_is_8_hex() {
        let h = sha8("hello");
        assert_eq!(h.len(), 8);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
