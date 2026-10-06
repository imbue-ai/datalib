//! Notion entity ids. Notion's page, discussion and comment ids are
//! UUIDs it mints, unique across the service, so the scope is
//! provider-global. They used to pass straight through as
//! `grid_rows.uuid`; now they are the backpointer.

use datalib_id::{IdNamespace, Identity, Minter};
use datalib_time::{record_stamp_ms, RecordStampPrecision};

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Notion;

pub const KIND_PAGE: &str = "page";
pub const KIND_DISCUSSION: &str = "discussion";
pub const KIND_COMMENT: &str = "comment";

/// Millis: a stamp here is the record's own string, read back whole
/// by `record_stamp_ms`, so nothing is rounded away.
const IDS: Minter = Minter::new(ID_NAMESPACE, RecordStampPrecision::Millis);

fn stamp(created_time: Option<&str>) -> Option<i64> {
    created_time.and_then(record_stamp_ms)
}

/// `created_time` is the page's own, as the row stores it.
pub fn page(source_id: &str, page_id: &str, created_time: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_PAGE,
        page_id.to_string(),
        stamp(created_time),
    )
}

/// No stamp: a thread's `created_at` is its first comment's.
pub fn discussion(source_id: &str, discussion_id: &str) -> Identity {
    IDS.mint(source_id, KIND_DISCUSSION, discussion_id.to_string(), None)
}

pub fn comment(source_id: &str, comment_id: &str, created_time: Option<&str>) -> Identity {
    IDS.mint(
        source_id,
        KIND_COMMENT,
        comment_id.to_string(),
        stamp(created_time),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const AT: Option<&str> = Some("2023-11-14T22:13:20.000Z");

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            page("src", "p", AT),
            discussion("src", "d"),
            comment("src", "c", AT),
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
    }

    #[test]
    fn kinds_separate_the_same_notion_id() {
        assert_ne!(page("src", "x", AT).uuid, comment("src", "x", AT).uuid);
        assert_ne!(page("src", "x", None).uuid, discussion("src", "x").uuid);
    }

    #[test]
    fn pages_and_comments_carry_their_stamp_and_threads_none() {
        assert_eq!(
            stamp_of(&page("src", "p", AT).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            stamp_of(&comment("src", "c", AT).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&discussion("src", "d").uuid), None);
    }
}
