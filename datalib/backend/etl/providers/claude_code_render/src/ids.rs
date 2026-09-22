//! Claude Code entity ids. Every natural key is one Claude Code minted —
//! a session id, a record uuid, a tool-use id — and Claude Code mints
//! them unique across every machine, so the scope is provider-global.

use datalib_id::{composite_key, IdNamespace, Identity, Scope};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::ClaudeCode;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

// Entity kinds — the `entity_kind` recipe component, and the value
// stamped into `grid_rows.upstream_entity_kind`.
pub const KIND_SESSION: &str = "session";
pub const KIND_AGENT_TRANSCRIPT: &str = "agent_transcript";
pub const KIND_RECORD: &str = "record";
pub const KIND_THINKING: &str = "thinking_block";
pub const KIND_TOOL_USE: &str = "tool_use";
pub const KIND_TOOL_RESULT: &str = "tool_result";
pub const KIND_BLOCK: &str = "content_block";

/// `date_ms` is the item's `date_ms`, so the stamp in the id is the
/// row's; a transcript's id carries none, its row's stamp being
/// derived from its items.
fn identity(entity_kind: &'static str, natural_key: String, date_ms: Option<i64>) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        Scope::ProviderGlobal,
        entity_kind,
        natural_key,
        STAMP_PRECISION.stored_ms(date_ms),
    )
}

/// A transcript's document: the session, or one subagent's transcript
/// within it (`<session_id>#<agent_id>`, the raw store's transcript id).
pub fn transcript(session_id: &str, agent_id: Option<&str>) -> Identity {
    match agent_id {
        None => identity(KIND_SESSION, session_id.to_string(), None),
        Some(a) => identity(KIND_AGENT_TRANSCRIPT, composite_key(&[session_id, a]), None),
    }
}

pub fn record(record_uuid: &str, date_ms: Option<i64>) -> Identity {
    identity(KIND_RECORD, record_uuid.to_string(), date_ms)
}

pub fn thinking_block(record_uuid: &str, block_index: usize, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_THINKING,
        composite_key(&[record_uuid, &block_index.to_string()]),
        date_ms,
    )
}

pub fn tool_use(record_uuid: &str, tool_use_id: &str, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_TOOL_USE,
        composite_key(&[record_uuid, tool_use_id]),
        date_ms,
    )
}

pub fn tool_result(record_uuid: &str, tool_use_id: &str, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_TOOL_RESULT,
        composite_key(&[record_uuid, tool_use_id]),
        date_ms,
    )
}

pub fn block_fallback(record_uuid: &str, block_index: usize, date_ms: Option<i64>) -> Identity {
    identity(
        KIND_BLOCK,
        composite_key(&[record_uuid, &block_index.to_string()]),
        date_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_and_its_subagent_get_different_ids() {
        let s = transcript("s1", None);
        let a = transcript("s1", Some("a9"));
        assert_ne!(s.uuid, a.uuid);
        assert_eq!(a.natural_key, "s1#a9");
        assert_eq!(s.natural_key, "s1");
    }

    #[test]
    fn a_tool_use_and_its_result_get_different_ids() {
        assert_ne!(
            tool_use("r1", "t1", None).uuid,
            tool_result("r2", "t1", None).uuid
        );
        assert_eq!(
            tool_use("r1", "t1", None).uuid,
            tool_use("r1", "t1", None).uuid
        );
    }

    #[test]
    fn an_item_carries_its_stamp_to_the_second_and_a_transcript_none() {
        use datalib_id::stamp_of;
        assert_eq!(
            stamp_of(&record("r1", Some(1_700_000_000_999)).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&transcript("s1", None).uuid), None);
    }
}
