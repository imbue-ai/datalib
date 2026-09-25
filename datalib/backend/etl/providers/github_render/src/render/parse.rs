//! Parse the GitHub doltlite database written by [`datalib_etl_github::ingest`] into
//! in-memory rows for the renderer + grid_rows pass. Each PR's
//! `issue_comments`, `pr_reviews`, and `pr_review_comments` collapse
//! into one `Comment` stream sorted (per render) by section, then by
//! file/line, then chronologically within a thread.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use serde_json::Value;

use datalib_etl_github::ingest::db::{db_path_for, LoadedChild, LoadedRaw, RawDb};

use datalib_etl_forge_render_common::{ChangeRequest, Comment, Parsed, Section};

use super::ids::{KIND_ISSUE_COMMENT, KIND_PR_REVIEW, KIND_PR_REVIEW_COMMENT};

/// Every table a PR's document reads; the forward scan diffs each.
const TABLES: [&str; 4] = [
    "pull_requests",
    "issue_comments",
    "pr_reviews",
    "pr_review_comments",
];

/// Read raw payloads out of the doltlite DB. `path` may be either a
/// `.doltlite_db` file or the per-source directory (whose entity db is
/// `entities.doltlite_db`) — both resolve to the same sqlite file via
/// [`db_path_for`].
pub fn parse_api_dir(path: &Path, source_id: &str, range: RawRange<'_>) -> Result<Parsed> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        // No store: this source has never been downloaded. That is
        // the normal state of every source in a freshly scaffolded
        // config, not an error — render nothing and succeed. A store
        // that exists but can't be read still fails, below. See
        // docs/dev/step_protocol.md, "Rendering a source with no data".
        return Ok(Parsed::default());
    }
    let (raw, head, changed) = tokio::task::block_in_place(|| {
        let path = db_path.clone();
        tokio::runtime::Handle::current().block_on(async move {
            let Some(db) = RawDb::open_reader(&path, range.pin).await? else {
                return Ok((LoadedRaw::default(), None, None));
            };
            let out = read_everything(&db, range).await;
            // Closed before returning, on the error path too.
            db.close().await;
            out
        })
    })
    .with_context(|| format!("load github db {}", db_path.display()))?;

    let mut parsed = parse_loaded(source_id, raw);
    parsed.head = head;
    parsed.narrow(TABLES[0], changed, range);
    Ok(parsed)
}

async fn read_everything(
    db: &RawDb,
    range: RawRange<'_>,
) -> Result<(
    LoadedRaw,
    Option<String>,
    Option<HashMap<String, HashSet<String>>>,
)> {
    let pin = db.pin().expect("open_reader returns a pinned handle");
    let raw = LoadedRaw {
        self_identity: db.load_self_identity().await?,
        pull_requests: db.load_pull_requests().await?,
        issue_comments: db.load_children("issue_comments").await?,
        pr_reviews: db.load_children("pr_reviews").await?,
        pr_review_comments: db.load_children("pr_review_comments").await?,
    };
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    Ok((raw, Some(pin.commit().to_string()), changed))
}

/// The bucket key a PR's rows share: `pull_requests.id`, and the same
/// string composed from any child row's `(repo_full_name, pr_number)`.
fn pr_pk(repo: &str, num: u32) -> String {
    format!("{repo}#{num}")
}

pub fn parse_loaded(source_id: &str, raw: LoadedRaw) -> Parsed {
    let mut out = Parsed::default();

    for pr in raw.pull_requests {
        let repo = pr.repo_full_name;
        let num = pr.pr_number;
        if repo.is_empty() || num == 0 {
            continue;
        }
        let p = &pr.payload;
        let created_at = opt_str(p, "created_at");
        let head = p.get("head");
        let base = p.get("base");
        out.change_requests.push(ChangeRequest {
            uuid: super::ids::pull_request(source_id, &repo, num, created_at.as_deref()).uuid,
            row_id: pr.id,
            container: repo,
            number: num,
            title: str_field(p, "title"),
            body: str_field(p, "body"),
            state: opt_str(p, "state"),
            url: opt_str(p, "html_url"),
            head_sha: head.and_then(|h| opt_str(h, "sha")),
            base_sha: base.and_then(|b| opt_str(b, "sha")),
            from_ref: head.and_then(|h| opt_str(h, "ref")),
            to_ref: base.and_then(|b| opt_str(b, "ref")),
            author: login(p),
            created_at,
            updated_at: opt_str(p, "updated_at"),
            merged_at: opt_str(p, "merged_at"),
        });
    }

    push_children(
        source_id,
        &mut out.comments,
        raw.issue_comments,
        &ISSUE_COMMENTS,
    );
    push_children(source_id, &mut out.comments, raw.pr_reviews, &PR_REVIEWS);
    push_children(
        source_id,
        &mut out.comments,
        raw.pr_review_comments,
        &PR_REVIEW_COMMENTS,
    );

    out
}

/// One of the three child tables a PR's comments come from, and what
/// sets its rows apart.
struct ChildKind {
    table: &'static str,
    entity: &'static str,
    kind: &'static str,
    section: Section,
    /// The payload field the row's `created_at` is read from.
    created_at: &'static str,
}

const ISSUE_COMMENTS: ChildKind = ChildKind {
    table: "issue_comments",
    entity: KIND_ISSUE_COMMENT,
    kind: "GitHub PR Comment",
    section: Section::General,
    created_at: "created_at",
};

const PR_REVIEWS: ChildKind = ChildKind {
    table: "pr_reviews",
    entity: KIND_PR_REVIEW,
    kind: "GitHub Review",
    section: Section::Review,
    created_at: "submitted_at",
};

const PR_REVIEW_COMMENTS: ChildKind = ChildKind {
    table: "pr_review_comments",
    entity: KIND_PR_REVIEW_COMMENT,
    kind: "GitHub Review Comment",
    section: Section::Inline,
    created_at: "created_at",
};

fn push_children(
    source_id: &str,
    out: &mut Vec<Comment>,
    rows: Vec<LoadedChild>,
    child: &ChildKind,
) {
    for c in rows {
        let id = c.payload.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if c.repo_full_name.is_empty() || c.pr_number == 0 || id == 0 {
            continue;
        }
        let p = &c.payload;
        let created_at = str_field(p, child.created_at);
        let inline = child.section == Section::Inline;
        let review = child.section == Section::Review;
        out.push(Comment {
            uuid: super::ids::comment(
                source_id,
                &c.repo_full_name,
                child.entity,
                id,
                Some(&created_at),
            )
            .uuid,
            table: child.table,
            row_id: c.id,
            parent_row_id: pr_pk(&c.repo_full_name, c.pr_number),
            kind: child.kind,
            entity_kind: child.entity,
            section: child.section,
            external_id: id,
            in_reply_to_id: inline
                .then(|| p.get("in_reply_to_id").and_then(|v| v.as_i64()))
                .flatten(),
            author: login(p),
            body: str_field(p, "body"),
            url: opt_str(p, "html_url"),
            path: inline.then(|| opt_str(p, "path")).flatten(),
            line: inline
                .then(|| {
                    p.get("line")
                        .and_then(|v| v.as_i64())
                        .or_else(|| p.get("original_line").and_then(|v| v.as_i64()))
                })
                .flatten(),
            commit_sha: match child.section {
                Section::Inline => {
                    opt_str(p, "commit_id").or_else(|| opt_str(p, "original_commit_id"))
                }
                Section::Review => opt_str(p, "commit_id"),
                Section::General => None,
            },
            created_at,
            // A review has no `updated_at` of its own.
            updated_at: (!review).then(|| opt_str(p, "updated_at")).flatten(),
            state: review.then(|| opt_str(p, "state")).flatten(),
        });
    }
}

fn str_field(p: &Value, key: &str) -> String {
    p.get(key).and_then(|v| v.as_str()).unwrap_or("").into()
}

fn opt_str(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn login(p: &Value) -> Option<String> {
    p.get("user").and_then(|u| opt_str(u, "login"))
}

#[cfg(test)]
mod no_data_tests {
    use super::*;

    /// A source that has never been downloaded renders as empty, not
    /// as a failure: that is the normal state of every source in a
    /// freshly scaffolded config. See docs/dev/step_protocol.md,
    /// "Rendering a source with no data".
    #[test]
    fn parse_missing_source_returns_empty_silently() {
        let parsed =
            parse_api_dir(Path::new("/this/does/not/exist"), "src", RawRange::cold()).unwrap();
        assert!(parsed.change_requests.is_empty());
        assert!(parsed.comments.is_empty());
    }
}
