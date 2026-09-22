//! Codex entity ids. A thread id is one Codex minted (a UUIDv7, unique
//! across machines) and a tool call's id one the model API minted, so
//! the scope is provider-global. A line has no id of its own and is
//! keyed by its number within the thread.

use datalib_id::{composite_key, entity_id_str, IdNamespace, Scope};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Codex;

// Entity kinds — the `entity_kind` recipe component, and the value
// stamped into `grid_rows.upstream_entity_kind`.
pub const KIND_THREAD: &str = "thread";
pub const KIND_RECORD: &str = "record";
pub const KIND_TOOL_USE: &str = "tool_use";
pub const KIND_TOOL_RESULT: &str = "tool_result";

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

/// A thread's document, keyed by the thread id — the raw store's
/// transcript id.
pub fn thread(thread_id: &str) -> Identity {
    identity(KIND_THREAD, thread_id.to_string())
}

/// One line of a rollout, by its number within the thread; the raw
/// store's record id.
pub fn record(thread_id: &str, line_no: i64) -> Identity {
    identity(
        KIND_RECORD,
        datalib_etl_codex::ingest::parse::record_id(thread_id, line_no),
    )
}

pub fn tool_use(thread_id: &str, call_id: &str) -> Identity {
    identity(KIND_TOOL_USE, composite_key(&[thread_id, call_id]))
}

pub fn tool_result(thread_id: &str, call_id: &str) -> Identity {
    identity(KIND_TOOL_RESULT, composite_key(&[thread_id, call_id]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_and_its_lines_get_different_ids() {
        let t = thread("t1");
        let l = record("t1", 1);
        assert_ne!(t.uuid, l.uuid);
        assert_eq!(l.natural_key, "t1#1");
        assert_ne!(record("t1", 1).uuid, record("t1", 2).uuid);
    }

    #[test]
    fn a_tool_use_and_its_result_get_different_ids() {
        assert_ne!(tool_use("t1", "c1").uuid, tool_result("t1", "c1").uuid);
        assert_eq!(tool_use("t1", "c1").uuid, tool_use("t1", "c1").uuid);
        assert_ne!(tool_use("t1", "c1").uuid, tool_use("t2", "c1").uuid);
    }
}
