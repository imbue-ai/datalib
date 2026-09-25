//! What the forge providers' render crates (github, gitlab) share. A
//! pull request and a merge request are one thing — a change request —
//! and their comments, reviews and discussion notes one stream of
//! [`Comment`]s. A provider's parse fills in [`Parsed`] from its raw
//! store; [`render_all`] writes one document per change request and its
//! grid rows. What differs between forges is a [`ForgeProfile`].

mod grid_rows;
mod markdown;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Bucket, Buckets, Input, RawRange};
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

/// How one forge names and lays out what is otherwise the same document.
#[derive(Debug, Clone, Copy)]
pub struct ForgeProfile {
    pub provider: Provider,
    /// The `provider:` line of the front matter.
    pub tag: &'static str,
    pub source_label: &'static str,
    /// `grid_rows.kind` of the change request's own row.
    pub doc_kind: &'static str,
    /// `upstream_entity_kind` of the change request's own row.
    pub doc_entity_kind: &'static str,
    /// The raw table the change requests are read from.
    pub table: &'static str,
    /// Front matter keys: the repository or project, the number, and
    /// the two refs the change goes from and to.
    pub container_key: &'static str,
    pub number_key: &'static str,
    pub from_ref_key: &'static str,
    pub to_ref_key: &'static str,
    /// What goes before the number in the title: `#` or `!`.
    pub number_sigil: char,
    /// The document's directory under its container: `pr-7`, `mr-7`.
    pub dir_prefix: &'static str,
    /// Whether a container with no owner segment is filed under
    /// `unknown/`, as a GitHub `owner/repo` is expected to have one.
    pub container_needs_owner: bool,
    /// Whether the document has a Reviews section.
    pub reviews: bool,
    pub render_version: u32,
}

impl ForgeProfile {
    /// The document's path under the data root.
    pub fn qmd_path_rel(&self, stanza: &str, container: &str, number: u32) -> String {
        let container = if self.container_needs_owner && !container.contains('/') {
            format!("unknown/{container}")
        } else {
            container.to_string()
        };
        format!(
            "{stanza}/{}/{container}/{}{number}/index.md",
            datalib_etl::layout::RENDER_MARKDOWN_DIR,
            self.dir_prefix,
        )
    }
}

/// A pull request or a merge request.
#[derive(Debug, Clone)]
pub struct ChangeRequest {
    pub uuid: String,
    /// Its raw row's id: the bucket key every row of the document shares.
    pub row_id: String,
    /// The repository or project it belongs to.
    pub container: String,
    pub number: u32,
    pub title: String,
    pub body: String,
    pub state: Option<String>,
    pub url: Option<String>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub from_ref: Option<String>,
    pub to_ref: Option<String>,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
}

/// Where a comment goes in its change request's document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// A review, with its state.
    Review,
    /// The conversation on the change request as a whole.
    General,
    /// Anchored to a line of the diff.
    Inline,
}

/// A comment, review or note on a change request.
#[derive(Debug, Clone)]
pub struct Comment {
    pub uuid: String,
    /// The raw row it came from, which its document declares.
    pub table: &'static str,
    pub row_id: String,
    /// The `row_id` of the change request it belongs to.
    pub parent_row_id: String,
    pub kind: &'static str,
    /// `upstream_entity_kind`: what the forge's id is an id of.
    pub entity_kind: &'static str,
    pub section: Section,
    pub external_id: i64,
    /// The comment this one replies to; `None` for one that starts a
    /// thread.
    pub in_reply_to_id: Option<i64>,
    pub author: Option<String>,
    pub body: String,
    pub url: Option<String>,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub commit_sha: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    /// A review's state (`APPROVED`, `CHANGES_REQUESTED`, …).
    pub state: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Parsed {
    /// The change requests to render this pass — narrowed to the
    /// buckets the diff named, or every one on a cold start.
    pub change_requests: Vec<ChangeRequest>,
    pub comments: Vec<Comment>,
    /// The commit everything was read at.
    pub head: Option<String>,
    /// The buckets to render; `None` renders everything.
    pub render: Option<HashSet<String>>,
}

impl Parsed {
    /// Narrow to what changed since the cursor. A change request's row
    /// id is its bucket key, so a changed one names itself — gone or
    /// not; a changed comment names its change request, and a comment
    /// that went is the driver's to name.
    pub fn narrow(
        &mut self,
        table: &str,
        changed: Option<HashMap<String, HashSet<String>>>,
        range: RawRange<'_>,
    ) {
        let forward = changed.map(|changed| {
            let mut out: HashSet<String> = changed.get(table).cloned().unwrap_or_default();
            for c in &self.comments {
                if changed
                    .get(c.table)
                    .is_some_and(|ids| ids.contains(&c.row_id))
                {
                    out.insert(c.parent_row_id.clone());
                }
            }
            out
        });
        self.render = range.narrow(forward.as_ref());
        if let Some(render) = self.render.as_ref() {
            self.change_requests
                .retain(|cr| render.contains(&cr.row_id));
            // Comments follow their change request: one left attached to
            // one this pass is not rendering would be grouped into a
            // document nobody emits.
            self.comments.retain(|c| render.contains(&c.parent_row_id));
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub rendered: usize,
    /// Every change request rendered, with the rows it read, for the
    /// caller to declare.
    pub buckets: Buckets,
}

pub fn render_all(
    profile: &ForgeProfile,
    parsed: &Parsed,
    root: &Path,
    stanza: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();
    tracing::info!(
        source = stanza,
        provider = profile.tag,
        change_requests = parsed.change_requests.len(),
        cold_start = parsed.render.is_none(),
        "[render] forge scan"
    );
    let mut by_parent: HashMap<&str, Vec<&Comment>> = HashMap::new();
    for c in &parsed.comments {
        by_parent.entry(&c.parent_row_id).or_default().push(c);
    }
    progress.set_length(Some(parsed.change_requests.len() as u64));
    for cr in &parsed.change_requests {
        let comments = by_parent.remove(cr.row_id.as_str()).unwrap_or_default();
        let md_rel = profile.qmd_path_rel(stanza, &cr.container, cr.number);
        let md_path = root.join(&md_rel);
        markdown::write(profile, cr, &comments, &md_path)?;
        let mut problems: Vec<ProblemRow> = Vec::new();
        let rows = grid_rows::rows_for(profile, cr, &comments, stanza, &md_rel, &mut problems);
        on_doc_complete(RenderedMarkdown {
            markdown_uuid: cr.uuid.clone(),
            source_id: String::new(),
            upstream_cursor: None,
            bucket_key: Some(cr.row_id.clone()),
            md_path,
            render_version: profile.render_version,
            rows,
            sections: Vec::new(),
            edges: Vec::new(),
            problems,
        })?;
        summary.buckets.push(Bucket {
            key: cr.row_id.clone(),
            inputs: inputs_of(profile, cr, &comments),
        });
        summary.rendered += 1;
        progress.inc(1);
    }
    Ok(summary)
}

/// The change request's row and every raw row a comment came from, each
/// once.
fn inputs_of(profile: &ForgeProfile, cr: &ChangeRequest, comments: &[&Comment]) -> Vec<Input> {
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    let mut inputs = vec![Input::new(profile.table, &cr.row_id)];
    for c in comments {
        if seen.insert((c.table, &c.row_id)) {
            inputs.push(Input::new(c.table, &c.row_id));
        }
    }
    inputs
}

/// Comments in the order the document shows them: reviews, then the
/// general conversation, then the inline threads by `(path, line)`,
/// each chronological. A reply sits in its thread's anchor, whatever
/// position it carries itself.
fn ordered<'a>(comments: &[&'a Comment]) -> Ordered<'a> {
    let by_time = |a: &&Comment, b: &&Comment| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.external_id.cmp(&b.external_id))
    };
    let of = |section: Section| -> Vec<&'a Comment> {
        let mut v: Vec<&'a Comment> = comments
            .iter()
            .copied()
            .filter(|c| c.section == section)
            .collect();
        v.sort_by(by_time);
        v
    };
    let own_anchor = |c: &Comment| {
        (
            c.path.clone().unwrap_or_else(|| "unknown".into()),
            c.line.unwrap_or(0),
        )
    };
    let inline: Vec<&'a Comment> = comments
        .iter()
        .copied()
        .filter(|c| c.section == Section::Inline)
        .collect();
    let anchor_of_thread: HashMap<i64, (String, i64)> = inline
        .iter()
        .filter(|c| c.in_reply_to_id.is_none())
        .map(|c| (c.external_id, own_anchor(c)))
        .collect();
    let mut threads: std::collections::BTreeMap<(String, i64), Vec<&'a Comment>> =
        Default::default();
    for c in inline {
        let anchor = c
            .in_reply_to_id
            .and_then(|p| anchor_of_thread.get(&p).cloned())
            .unwrap_or_else(|| own_anchor(c));
        threads.entry(anchor).or_default().push(c);
    }
    for thread in threads.values_mut() {
        thread.sort_by(by_time);
    }
    Ordered {
        reviews: of(Section::Review),
        general: of(Section::General),
        inline: threads,
    }
}

struct Ordered<'a> {
    reviews: Vec<&'a Comment>,
    general: Vec<&'a Comment>,
    inline: std::collections::BTreeMap<(String, i64), Vec<&'a Comment>>,
}

impl<'a> Ordered<'a> {
    fn flat(self) -> impl Iterator<Item = &'a Comment> {
        self.reviews
            .into_iter()
            .chain(self.general)
            .chain(self.inline.into_values().flatten())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(id: i64, section: Section, reply_to: Option<i64>, at: &str) -> Comment {
        Comment {
            uuid: format!("u{id}"),
            table: "t",
            row_id: format!("r{id}"),
            parent_row_id: "pr".into(),
            kind: "k",
            entity_kind: "e",
            section,
            external_id: id,
            in_reply_to_id: reply_to,
            author: None,
            body: String::new(),
            url: None,
            path: Some(format!("f{id}.rs")),
            line: Some(id),
            commit_sha: None,
            created_at: at.into(),
            updated_at: None,
            state: None,
        }
    }

    /// A reply is shown in its thread, under the anchor the thread
    /// started at, even when its own position names another line.
    #[test]
    fn a_reply_sits_in_its_threads_anchor() {
        let c = [
            comment(2, Section::Inline, Some(1), "2024-01-02"),
            comment(1, Section::Inline, None, "2024-01-01"),
            comment(3, Section::General, None, "2024-01-03"),
            comment(4, Section::Review, None, "2024-01-04"),
        ];
        let refs: Vec<&Comment> = c.iter().collect();
        let ids: Vec<i64> = ordered(&refs).flat().map(|c| c.external_id).collect();
        assert_eq!(ids, vec![4, 3, 1, 2]);
        assert_eq!(ordered(&refs).inline.len(), 1, "one thread, not two");
    }

    #[test]
    fn a_container_with_no_owner_is_filed_under_unknown_only_where_it_should_have_one() {
        let mut p = ForgeProfile {
            provider: Provider::Github,
            tag: "github",
            source_label: "GitHub",
            doc_kind: "GitHub PR",
            doc_entity_kind: "pull_request",
            table: "pull_requests",
            container_key: "repo",
            number_key: "pr_number",
            from_ref_key: "head_ref",
            to_ref_key: "base_ref",
            number_sigil: '#',
            dir_prefix: "pr-",
            container_needs_owner: true,
            reviews: true,
            render_version: 1,
        };
        assert!(p
            .qmd_path_rel("s", "repo", 7)
            .ends_with("/unknown/repo/pr-7/index.md"));
        assert!(p
            .qmd_path_rel("s", "o/repo", 7)
            .ends_with("/o/repo/pr-7/index.md"));
        p.container_needs_owner = false;
        assert!(p
            .qmd_path_rel("s", "proj", 7)
            .ends_with("/proj/pr-7/index.md"));
    }
}
