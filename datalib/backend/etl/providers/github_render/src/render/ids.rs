//! GitHub entity ids. A PR number is unique within its repository and
//! a comment's numeric id within its API namespace — issue comments,
//! reviews and review comments are three sequences that overlap — so
//! the scope is the repository and the kind is the API namespace.

use datalib_id::{IdNamespace, Identity, Scope};
use datalib_time::record_stamp_ms;

pub const ID_NAMESPACE: IdNamespace = IdNamespace::Github;

pub const KIND_PR: &str = "pull_request";
pub const KIND_ISSUE_COMMENT: &str = "issue_comment";
pub const KIND_PR_REVIEW: &str = "pr_review";
pub const KIND_PR_REVIEW_COMMENT: &str = "pr_review_comment";

/// `created_at` is the stamp GitHub wrote on the record, as the row
/// stores it — a PR's or a comment's own, never derived — so the id
/// carries it.
fn identity(
    source_id: &str,
    repo: &str,
    entity_kind: &'static str,
    natural_key: String,
    created_at: Option<&str>,
) -> Identity {
    Identity::mint(
        ID_NAMESPACE,
        source_id,
        Scope::Upstream(repo),
        entity_kind,
        natural_key,
        created_at.and_then(record_stamp_ms),
    )
}

pub fn pull_request(
    source_id: &str,
    repo: &str,
    number: u32,
    created_at: Option<&str>,
) -> Identity {
    identity(source_id, repo, KIND_PR, number.to_string(), created_at)
}

pub fn comment(
    source_id: &str,
    repo: &str,
    kind: &'static str,
    id: i64,
    created_at: Option<&str>,
) -> Identity {
    identity(source_id, repo, kind, id.to_string(), created_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_id::{entity_id_str, stamp_of};

    const AT: Option<&str> = Some("2023-11-14T22:13:20Z");

    #[test]
    fn natural_key_regenerates_the_uuid() {
        for got in [
            pull_request("gh", "o/r", 42, AT),
            comment("gh", "o/r", KIND_ISSUE_COMMENT, 7, AT),
            comment("gh", "o/r", KIND_PR_REVIEW, 7, None),
        ] {
            assert_eq!(
                got.uuid,
                entity_id_str(
                    ID_NAMESPACE,
                    "gh",
                    Scope::Upstream("o/r"),
                    got.entity_kind,
                    &got.natural_key,
                    got.at,
                ),
            );
        }
    }

    /// The three comment namespaces overlap on numeric id; a PR and a
    /// comment may share a number too.
    #[test]
    fn kinds_and_repos_separate_the_same_number() {
        assert_ne!(
            comment("gh", "o/r", KIND_ISSUE_COMMENT, 7, AT).uuid,
            comment("gh", "o/r", KIND_PR_REVIEW_COMMENT, 7, AT).uuid
        );
        assert_ne!(
            pull_request("gh", "o/r", 7, AT).uuid,
            comment("gh", "o/r", KIND_ISSUE_COMMENT, 7, AT).uuid
        );
        assert_ne!(
            pull_request("gh", "o/r", 7, AT).uuid,
            pull_request("gh", "o/other", 7, AT).uuid
        );
    }

    #[test]
    fn the_stamp_is_githubs_created_at() {
        assert_eq!(
            stamp_of(&pull_request("gh", "o/r", 1, AT).uuid),
            Some(1_700_000_000_000)
        );
        assert_eq!(stamp_of(&pull_request("gh", "o/r", 1, Some("")).uuid), None);
        assert_eq!(stamp_of(&pull_request("gh", "o/r", 1, None).uuid), None);
    }
}
