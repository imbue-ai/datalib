//! Claude Code entity ids. Every natural key is one Claude Code minted —
//! a session id, a record uuid, a tool-use id — and Claude Code mints
//! them unique across every machine, so the scope is provider-global.

use datalib_id::{composite_key, entity_id_str, IdNamespace, Scope};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::ClaudeCode;

// Entity kinds — the `entity_kind` recipe component, and the value
// stamped into `grid_rows.upstream_entity_kind`.
pub const KIND_SESSION: &str = "session";
pub const KIND_AGENT_TRANSCRIPT: &str = "agent_transcript";
pub const KIND_RECORD: &str = "record";
pub const KIND_THINKING: &str = "thinking_block";
pub const KIND_TOOL_USE: &str = "tool_use";
pub const KIND_TOOL_RESULT: &str = "tool_result";
pub const KIND_BLOCK: &str = "content_block";

#[derive(Debug, Clone)]
pub struct Identity {
    pub uuid: String,
    pub natural_key: String,
    pub entity_kind: &'static str,
}

fn identity(entity_kind: &'static str, natural_key: String) -> Identity {
    Identity {
        uuid: entity_id_str(
            ID_NAMESPACE,
            Scope::ProviderGlobal,
            entity_kind,
            &natural_key,
        ),
        natural_key,
        entity_kind,
    }
}

/// A transcript's document: the session, or one subagent's transcript
/// within it (`<session_id>#<agent_id>`, the raw store's transcript id).
pub fn transcript(session_id: &str, agent_id: Option<&str>) -> Identity {
    match agent_id {
        None => identity(KIND_SESSION, session_id.to_string()),
        Some(a) => identity(KIND_AGENT_TRANSCRIPT, composite_key(&[session_id, a])),
    }
}

pub fn record(record_uuid: &str) -> Identity {
    identity(KIND_RECORD, record_uuid.to_string())
}

pub fn thinking_block(record_uuid: &str, block_index: usize) -> Identity {
    identity(
        KIND_THINKING,
        composite_key(&[record_uuid, &block_index.to_string()]),
    )
}

pub fn tool_use(record_uuid: &str, tool_use_id: &str) -> Identity {
    identity(KIND_TOOL_USE, composite_key(&[record_uuid, tool_use_id]))
}

pub fn tool_result(record_uuid: &str, tool_use_id: &str) -> Identity {
    identity(KIND_TOOL_RESULT, composite_key(&[record_uuid, tool_use_id]))
}

pub fn block_fallback(record_uuid: &str, block_index: usize) -> Identity {
    identity(
        KIND_BLOCK,
        composite_key(&[record_uuid, &block_index.to_string()]),
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
        assert_ne!(tool_use("r1", "t1").uuid, tool_result("r2", "t1").uuid);
        assert_eq!(tool_use("r1", "t1").uuid, tool_use("r1", "t1").uuid);
    }
}
