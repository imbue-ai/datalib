//! Render captured GitHub PRs to **one** markdown document per PR.
//!
//! Layout:
//! ```text
//! <root>/<stanza>/rendered_md/<owner>/<repo>/pr-<num>/index.md
//! <root>/<stanza>/rendered_md/<owner>/<repo>/pr-<num>/index.grid_rows.json
//! ```
//!
//! Section order in the doc:
//! 1. Front matter + title + PR meta (state, head/base, author)
//! 2. **Description** — `pull_request.body`
//! 3. **Reviews** — one block per `pr_review` summary, oldest first
//! 4. **General discussion** — `issue_comments`, oldest first
//! 5. **Inline comments** — grouped by (`path`, `line`), then within each
//!    group chronologically (parent then replies)
//!
//! Every individual comment block carries its `html_url` as `[link]` so
//! a reader can pop the original conversation on github.com.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::title::Title;
use frankweiler_index_lib::emit_sidecar;
use once_cell::sync::Lazy;
use regex::Regex;

use super::grid_rows::{fingerprint_for_pr, rows_for_pr, RENDER_VERSION};
use super::parse::{CommentRow, CommentSection, ParsedGithubApi, PullRequestRow};

pub const SLUG_MAX_LEN: usize = 60;

static SLUG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^a-z0-9]+").unwrap());

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub rendered: usize,
    pub skipped: usize,
}

pub fn slugify(name: &str) -> String {
    if name.is_empty() {
        return "untitled".into();
    }
    let lower = name.to_lowercase();
    let s = SLUG_RE
        .replace_all(&lower, "-")
        .trim_matches('-')
        .to_string();
    if s.is_empty() {
        return "untitled".into();
    }
    let mut s: String = s.chars().take(SLUG_MAX_LEN).collect();
    s = s.trim_end_matches('-').to_string();
    if s.is_empty() {
        "untitled".into()
    } else {
        s
    }
}

/// Relative path from the data root to a PR's `index.md`.
pub fn pr_qmd_path_rel(stanza: &str, repo_full_name: &str, pr_number: u32) -> String {
    let (owner, repo) = repo_full_name
        .split_once('/')
        .unwrap_or(("unknown", repo_full_name));
    format!("{stanza}/rendered_md/{owner}/{repo}/pr-{pr_number}/index.md")
}

fn pr_dir(root: &Path, stanza: &str, repo: &str, num: u32) -> PathBuf {
    let (owner, name) = repo.split_once('/').unwrap_or(("unknown", repo));
    frankweiler_etl::layout::rendered_md_root(root, stanza)
        .join(owner)
        .join(name)
        .join(format!("pr-{num}"))
}

fn yaml_scalar(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    let needs_quote = s
        .chars()
        .any(|c| matches!(c, ':' | '#' | '\n' | '"' | '\''))
        || s != s.trim();
    if needs_quote {
        serde_json::to_string(s).unwrap_or_else(|_| s.into())
    } else {
        s.into()
    }
}
fn yaml_opt(s: Option<&str>) -> String {
    s.map(yaml_scalar).unwrap_or_else(|| "null".into())
}

fn quote_body(body: &str) -> String {
    // Each line gets `> ` so the comment renders as a blockquote.
    if body.is_empty() {
        return "> *(empty)*".into();
    }
    body.lines()
        .map(|l| {
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn comment_header(c: &CommentRow) -> String {
    let who = c.user_login.as_deref().unwrap_or("unknown");
    let when = c.created_at.as_str();
    let link = c
        .html_url
        .as_deref()
        .map(|u| format!(" — [link]({u})"))
        .unwrap_or_default();
    let state = c
        .state
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" *({s})*"))
        .unwrap_or_default();
    let reply = if c.in_reply_to_id.is_some() {
        " *(reply)*".to_string()
    } else {
        String::new()
    };
    format!("**@{who}**{state}{reply} @ {when}{link}")
}

fn render_one_pr(
    pr: &PullRequestRow,
    comments: &[CommentRow],
    root: &Path,
    stanza: &str,
) -> Result<PathBuf> {
    let dir = pr_dir(root, stanza, &pr.repo_full_name, pr.pr_number);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let md_path = dir.join("index.md");

    let mut out = String::new();
    // -- front matter --
    out.push_str("---\n");
    out.push_str("provider: github\n");
    out.push_str(&format!("repo: {}\n", yaml_scalar(&pr.repo_full_name)));
    out.push_str(&format!("pr_number: {}\n", pr.pr_number));
    out.push_str(&format!("title: {}\n", yaml_scalar(&pr.title)));
    out.push_str(&format!("state: {}\n", yaml_opt(pr.state.as_deref())));
    out.push_str(&format!("author: {}\n", yaml_opt(pr.user_login.as_deref())));
    out.push_str(&format!(
        "created_at: {}\n",
        yaml_opt(pr.created_at.as_deref())
    ));
    out.push_str(&format!(
        "updated_at: {}\n",
        yaml_opt(pr.updated_at.as_deref())
    ));
    out.push_str(&format!(
        "merged_at: {}\n",
        yaml_opt(pr.merged_at.as_deref())
    ));
    out.push_str(&format!("head_sha: {}\n", yaml_opt(pr.head_sha.as_deref())));
    out.push_str(&format!("base_sha: {}\n", yaml_opt(pr.base_sha.as_deref())));
    out.push_str(&format!("head_ref: {}\n", yaml_opt(pr.head_ref.as_deref())));
    out.push_str(&format!("base_ref: {}\n", yaml_opt(pr.base_ref.as_deref())));
    out.push_str("---\n\n");

    // -- title --
    let title_text = format!("{} (#{})", pr.title, pr.pr_number);
    out.push_str(
        &Title {
            text: &title_text,
            markdown_uuid: Some(&pr.uuid),
            source_url: pr.html_url.as_deref(),
        }
        .render(),
    );
    let state = pr.state.as_deref().unwrap_or("unknown");
    let author = pr.user_login.as_deref().unwrap_or("unknown");
    let head_ref = pr.head_ref.as_deref().unwrap_or("?");
    let base_ref = pr.base_ref.as_deref().unwrap_or("?");
    out.push_str(&format!(
        "*{state}* — @{author} — `{head_ref}` → `{base_ref}`\n\n"
    ));

    // -- description --
    out.push_str("## Description\n\n");
    if pr.body.trim().is_empty() {
        out.push_str("*(no description)*\n\n");
    } else {
        out.push_str(&pr.body);
        if !pr.body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }

    // -- reviews --
    let mut reviews: Vec<&CommentRow> = comments
        .iter()
        .filter(|c| c.section == CommentSection::Review)
        .collect();
    reviews.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.external_id.cmp(&b.external_id))
    });
    out.push_str("## Reviews\n\n");
    if reviews.is_empty() {
        out.push_str("*(no reviews)*\n\n");
    } else {
        for r in reviews {
            out.push_str(&comment_header(r));
            out.push_str("\n\n");
            if !r.body.trim().is_empty() {
                out.push_str(&quote_body(&r.body));
                out.push_str("\n\n");
            }
        }
    }

    // -- general --
    let mut general: Vec<&CommentRow> = comments
        .iter()
        .filter(|c| c.section == CommentSection::General)
        .collect();
    general.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.external_id.cmp(&b.external_id))
    });
    out.push_str("## General discussion\n\n");
    if general.is_empty() {
        out.push_str("*(no general comments)*\n\n");
    } else {
        for c in general {
            out.push_str(&comment_header(c));
            out.push_str("\n\n");
            out.push_str(&quote_body(&c.body));
            out.push_str("\n\n");
        }
    }

    // -- inline grouped by (path, line) --
    let inline: Vec<&CommentRow> = comments
        .iter()
        .filter(|c| c.section == CommentSection::Inline)
        .collect();
    out.push_str("## Inline comments\n\n");
    if inline.is_empty() {
        out.push_str("*(no inline comments)*\n\n");
    } else {
        // Resolve thread anchors. A top-level inline comment (in_reply_to_id
        // is None) owns its (path, line). Replies inherit their parent's
        // anchor. Group by anchor, then sort groups by (path, line).
        let mut anchor_for_id: std::collections::HashMap<i64, (String, i64)> = Default::default();
        for c in &inline {
            if c.in_reply_to_id.is_none() {
                let path = c.path.clone().unwrap_or_else(|| "unknown".into());
                let line = c.line.unwrap_or(0);
                anchor_for_id.insert(c.external_id, (path, line));
            }
        }
        let mut groups: BTreeMap<(String, i64), Vec<&CommentRow>> = BTreeMap::new();
        let mut group_keys: BTreeSet<(String, i64)> = BTreeSet::new();
        for c in &inline {
            let anchor = c
                .in_reply_to_id
                .and_then(|p| anchor_for_id.get(&p).cloned())
                .unwrap_or_else(|| {
                    (
                        c.path.clone().unwrap_or_else(|| "unknown".into()),
                        c.line.unwrap_or(0),
                    )
                });
            group_keys.insert(anchor.clone());
            groups.entry(anchor).or_default().push(c);
        }
        for key in group_keys {
            let (path, line) = &key;
            out.push_str(&format!("### `{path}:{line}`\n\n"));
            let mut bucket = groups.remove(&key).unwrap_or_default();
            bucket.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then(a.external_id.cmp(&b.external_id))
            });
            for c in bucket {
                out.push_str(&comment_header(c));
                out.push_str("\n\n");
                out.push_str(&quote_body(&c.body));
                out.push_str("\n\n");
            }
        }
    }

    // trim trailing blank lines
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    fs::write(&md_path, &out).with_context(|| format!("write {}", md_path.display()))?;

    // sidecar
    let rows = rows_for_pr(pr, comments, stanza)?;
    let sidecar_path = md_path.with_extension("grid_rows.json");
    emit_sidecar(
        &sidecar_path,
        &pr.uuid,
        &fingerprint_for_pr(pr, comments),
        RENDER_VERSION,
        &rows,
        &[],
    )?;

    Ok(md_path)
}

pub fn render_github(
    parsed: &ParsedGithubApi,
    root: &Path,
    stanza: &str,
    progress: &Progress,
    prior_fingerprints: &std::collections::HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();
    // Group comments by PR.
    let mut by_pr: std::collections::HashMap<(String, u32), Vec<CommentRow>> = Default::default();
    for c in &parsed.comments {
        by_pr
            .entry((c.repo_full_name.clone(), c.pr_number))
            .or_default()
            .push(c.clone());
    }
    progress.set_length(Some(parsed.pull_requests.len() as u64));
    for pr in &parsed.pull_requests {
        let key = (pr.repo_full_name.clone(), pr.pr_number);
        let comments = by_pr.remove(&key).unwrap_or_default();
        let fingerprint = fingerprint_for_pr(pr, &comments);
        let md_rel = pr_qmd_path_rel(stanza, &pr.repo_full_name, pr.pr_number);
        let md_path = root.join(&md_rel);

        if prior_fingerprints.get(&pr.uuid).map(String::as_str) == Some(fingerprint.as_str())
            && md_path.exists()
        {
            summary.skipped += 1;
            progress.inc(1);
            continue;
        }

        render_one_pr(pr, &comments, root, stanza)?;
        let rows = rows_for_pr(pr, &comments, stanza)?;
        on_doc_complete(RenderedMarkdown {
            markdown_uuid: pr.uuid.clone(),
            source_name: String::new(),
            source_fingerprint: fingerprint,
            upstream_cursor: None,
            md_path: md_path.clone(),
            render_version: RENDER_VERSION,
            rows,
            edges: Vec::new(),
        })?;
        summary.rendered += 1;
        progress.inc(1);
    }
    Ok(summary)
}
