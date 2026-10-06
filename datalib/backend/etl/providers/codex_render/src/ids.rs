//! Codex entity ids. A thread id is one Codex minted (a UUIDv7) and a
//! tool call's id one the model API minted; a line has no id of its own
//! and is keyed by its number within the thread. No account: upstream's
//! `session_meta` declares `creator_account_id` optional and none of
//! the rollouts measured here carries one, so it fails the
//! present-or-never rule (`docs/dev/entity_ids.md`).

use datalib_id::{composite_key, IdNamespace, Identity, Minter};
use datalib_time::RecordStampPrecision;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Codex;
pub const STAMP_PRECISION: RecordStampPrecision = RecordStampPrecision::Seconds;

// Entity kinds — the `entity_kind` recipe component, and the value
// stamped into `grid_rows.upstream_entity_kind`.
pub const KIND_THREAD: &str = "thread";
pub const KIND_RECORD: &str = "record";
pub const KIND_TOOL_USE: &str = "tool_use";
pub const KIND_TOOL_RESULT: &str = "tool_result";

/// `date_ms` is the item's own `date_ms`, so the stamp in the id is the
/// row's; a thread's id carries none, its row's stamp being derived
/// from its items.
const IDS: Minter = Minter::new(ID_NAMESPACE, STAMP_PRECISION);

/// A thread's document, keyed by the thread id — the raw store's
/// transcript id.
pub fn thread(source_id: &str, thread_id: &str) -> Identity {
    IDS.mint(source_id, KIND_THREAD, thread_id.to_string(), None)
}

/// One line of a rollout, by its number within the thread.
///
/// Through `composite_key`, not `format!`, so a thread id that ever
/// carried a `#` could not make two lines share a key — the raw
/// store's own spelling of this key is its business, and
/// `the_natural_key_is_the_raw_stores_spelling` holds the two together.
pub fn record(source_id: &str, thread_id: &str, line_no: i64, date_ms: Option<i64>) -> Identity {
    IDS.mint(
        source_id,
        KIND_RECORD,
        composite_key(&[thread_id, &line_no.to_string()]),
        date_ms,
    )
}

pub fn tool_use(source_id: &str, thread_id: &str, call_id: &str, date_ms: Option<i64>) -> Identity {
    IDS.mint(
        source_id,
        KIND_TOOL_USE,
        composite_key(&[thread_id, call_id]),
        date_ms,
    )
}

pub fn tool_result(
    source_id: &str,
    thread_id: &str,
    call_id: &str,
    date_ms: Option<i64>,
) -> Identity {
    IDS.mint(
        source_id,
        KIND_TOOL_RESULT,
        composite_key(&[thread_id, call_id]),
        date_ms,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_and_its_lines_get_different_ids() {
        let t = thread("src", "t1");
        let l = record("src", "t1", 1, None);
        assert_ne!(t.uuid, l.uuid);
        assert_eq!(l.natural_key, "t1#1");
        assert_ne!(
            record("src", "t1", 1, None).uuid,
            record("src", "t1", 2, None).uuid
        );
    }

    #[test]
    fn a_tool_use_and_its_result_get_different_ids() {
        assert_ne!(
            tool_use("src", "t1", "c1", None).uuid,
            tool_result("src", "t1", "c1", None).uuid
        );
        assert_eq!(
            tool_use("src", "t1", "c1", None).uuid,
            tool_use("src", "t1", "c1", None).uuid
        );
        assert_ne!(
            tool_use("src", "t1", "c1", None).uuid,
            tool_use("src", "t2", "c1", None).uuid
        );
    }

    /// Two sources mirroring one Codex home render two sets of rows.
    #[test]
    fn the_source_is_part_of_every_id() {
        assert_ne!(thread("a", "t1").uuid, thread("b", "t1").uuid);
    }

    /// `upstream_id` is what a person takes back to the rollout, so it
    /// has to be the key the raw store used. Nothing forces the two
    /// spellings to agree — the raw key is built in the ingest crate,
    /// which cannot see this one — so assert it.
    #[test]
    fn the_natural_key_is_the_raw_stores_spelling() {
        let raw = datalib_etl_codex::ingest::parse::record_id("t1", 7);
        assert_eq!(record("src", "t1", 7, None).natural_key, raw);
    }

    #[test]
    fn an_item_carries_its_stamp_to_the_second_and_a_thread_none() {
        use datalib_id::stamp_of;
        assert_eq!(
            stamp_of(&record("src", "t1", 3, Some(1_700_000_000_999)).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&thread("src", "t1").uuid), None);
    }
}
