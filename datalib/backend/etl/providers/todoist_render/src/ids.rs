//! Todoist entity ids. Todoist's ids are unique across the account for
//! each kind of object, so each natural key is Todoist's id alone, as
//! the raw store keys it.

use datalib_id::{IdNamespace, Identity, Minter};
use datalib_time::{record_stamp_ms, RecordStampPrecision};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Todoist;

pub const KIND_PROJECT: &str = "project";
pub const KIND_TASK: &str = "task";
pub const KIND_COMMENT: &str = "comment";
pub const KIND_PROJECT_COMMENT: &str = "project_comment";

const IDS: Minter = Minter::new(ID_NAMESPACE, RecordStampPrecision::Millis);

/// No stamp: a fetch problem names the project by its id alone, and has
/// to find the same document; and a project's document row takes its
/// stamps from what is in it.
pub fn project(source_id: &str, project_id: &str) -> Identity {
    IDS.mint(source_id, KIND_PROJECT, project_id.to_string(), None)
}

/// `added_at` is the task's own.
pub fn task(source_id: &str, task_id: &str, added_at: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_TASK,
        task_id.to_string(),
        added_at.and_then(record_stamp_ms),
    )
}

/// A comment on a task (`notes`), stamped with its `posted_at`.
pub fn comment(source_id: &str, note_id: &str, posted_at: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_COMMENT,
        note_id.to_string(),
        posted_at.and_then(record_stamp_ms),
    )
}

/// A comment on a project (`project_notes`).
pub fn project_comment(source_id: &str, note_id: &str, posted_at: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_PROJECT_COMMENT,
        note_id.to_string(),
        posted_at.and_then(record_stamp_ms),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::entity_id_str;

    #[test]
    fn natural_key_regenerates_the_uuid() {
        let at = Some("2026-09-20T08:00:00.000000Z");
        for got in [
            project("src", "6Refit"),
            task("src", "6Coils", at),
            comment("src", "6Note", at),
            project_comment("src", "6Note", at),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "src",
                    None,
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
        assert_ne!(
            comment("src", "6Note", at).uuid,
            project_comment("src", "6Note", at).uuid,
            "the kind separates one key"
        );
    }
}
