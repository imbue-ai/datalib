//! GitLab entity ids. An MR's `iid` is unique within its project and a
//! note's id across the instance, so both scope to the project.

use datalib_id::{IdNamespace, Identity, Scope};
use datalib_time::record_stamp_ms;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Gitlab;

pub const KIND_MR: &str = "merge_request";
pub const KIND_NOTE: &str = "note";

/// `created_at` is the stamp GitLab wrote on the record, as the row
/// stores it, so the id carries it.
fn identity(
    source_id: &str,
    project: &str,
    entity_kind: &'static str,
    natural_key: String,
    created_at: Option<&str>,
) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        source_id,
        Scope::Upstream(project),
        entity_kind,
        natural_key,
        created_at.and_then(record_stamp_ms),
    )
}

pub fn merge_request(
    source_id: &str,
    project: &str,
    iid: u32,
    created_at: Option<&str>,
) -> Identity {
    identity(source_id, project, KIND_MR, iid.to_string(), created_at)
}

pub fn note(source_id: &str, project: &str, id: i64, created_at: Option<&str>) -> Identity {
    identity(source_id, project, KIND_NOTE, id.to_string(), created_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const AT: Option<&str> = Some("2023-11-14T22:13:20.000Z");

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            merge_request("gl", "g/p", 17, AT),
            note("gl", "g/p", 17, None),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "gl",
                    Scope::Upstream("g/p"),
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    #[test]
    fn kinds_and_projects_separate_the_same_number() {
        assert_ne!(
            merge_request("gl", "g/p", 7, AT).uuid,
            note("gl", "g/p", 7, AT).uuid
        );
        assert_ne!(
            merge_request("gl", "g/p", 7, AT).uuid,
            merge_request("gl", "g/q", 7, AT).uuid
        );
    }

    #[test]
    fn the_stamp_is_gitlabs_created_at() {
        assert_eq!(
            stamp_of(&note("gl", "g/p", 1, AT).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&note("gl", "g/p", 1, Some("")).uuid), None);
    }
}
