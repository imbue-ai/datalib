//! Render captured GitLab MRs to **one** markdown document per MR.
//!
//! Layout:
//! ```text
//! <root>/rendered_md/gitlab/<namespace>/<project>/mr-<iid>__<slug>/index.md
//! <root>/rendered_md/gitlab/<namespace>/<project>/mr-<iid>__<slug>/index.grid_rows.json
//! ```
//!
//! Section order in the doc:
//! 1. Front matter + title + MR meta (state, source/target, author)
//! 2. **Description** — `merge_request.description`
//! 3. **General discussion** — individual_note + non-positioned discussions
//! 4. **Inline comments** — positioned discussions, grouped by (`new_path`,
//!    `new_line`), then within each group chronologically.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::load::RenderedDoc;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};
use once_cell::sync::Lazy;
use regex::Regex;

use super::grid_rows::{fingerprint_for_mr, rows_for_mr, RENDER_VERSION};
use super::parse::{MergeRequestRow, NoteRow, NoteSection, ParsedGitlabApi};

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

pub fn mr_qmd_path_rel(project_full_path: &str, iid: u32) -> String {
    format!("rendered_md/gitlab/{project_full_path}/mr-{iid}/index.md")
}

fn mr_dir(root: &Path, project: &str, iid: u32) -> PathBuf {
    let mut p = root.join("rendered_md").join("gitlab");
    for part in project.split('/') {
        p = p.join(part);
    }
    p.join(format!("mr-{iid}"))
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

fn note_header(n: &NoteRow) -> String {
    let who = n.author_username.as_deref().unwrap_or("unknown");
    let when = n.created_at.as_str();
    let link = n
        .web_url
        .as_deref()
        .map(|u| format!(" — [link]({u})"))
        .unwrap_or_default();
    let reply = if n.in_reply_to_id.is_some() {
        " *(reply)*".to_string()
    } else {
        String::new()
    };
    format!("**@{who}**{reply} @ {when}{link}")
}

fn render_one_mr(mr: &MergeRequestRow, notes: &[NoteRow], root: &Path) -> Result<PathBuf> {
    let dir = mr_dir(root, &mr.project_full_path, mr.mr_iid);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let md_path = dir.join("index.md");

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("provider: gitlab\n");
    out.push_str(&format!(
        "project: {}\n",
        yaml_scalar(&mr.project_full_path)
    ));
    out.push_str(&format!("mr_iid: {}\n", mr.mr_iid));
    out.push_str(&format!("title: {}\n", yaml_scalar(&mr.title)));
    out.push_str(&format!("state: {}\n", yaml_opt(mr.state.as_deref())));
    out.push_str(&format!(
        "author: {}\n",
        yaml_opt(mr.author_username.as_deref())
    ));
    out.push_str(&format!(
        "created_at: {}\n",
        yaml_opt(mr.created_at.as_deref())
    ));
    out.push_str(&format!(
        "updated_at: {}\n",
        yaml_opt(mr.updated_at.as_deref())
    ));
    out.push_str(&format!(
        "merged_at: {}\n",
        yaml_opt(mr.merged_at.as_deref())
    ));
    out.push_str(&format!("head_sha: {}\n", yaml_opt(mr.head_sha.as_deref())));
    out.push_str(&format!("base_sha: {}\n", yaml_opt(mr.base_sha.as_deref())));
    out.push_str(&format!(
        "source_branch: {}\n",
        yaml_opt(mr.source_branch.as_deref())
    ));
    out.push_str(&format!(
        "target_branch: {}\n",
        yaml_opt(mr.target_branch.as_deref())
    ));
    out.push_str("---\n\n");

    out.push_str(&format!("# {} (!{})\n\n", mr.title, mr.mr_iid));
    if let Some(url) = &mr.web_url {
        out.push_str(&format!("[View on GitLab ↗]({url})\n\n"));
    }
    let state = mr.state.as_deref().unwrap_or("unknown");
    let author = mr.author_username.as_deref().unwrap_or("unknown");
    let src = mr.source_branch.as_deref().unwrap_or("?");
    let tgt = mr.target_branch.as_deref().unwrap_or("?");
    out.push_str(&format!("*{state}* — @{author} — `{src}` → `{tgt}`\n\n"));

    out.push_str("## Description\n\n");
    if mr.body.trim().is_empty() {
        out.push_str("*(no description)*\n\n");
    } else {
        out.push_str(&mr.body);
        if !mr.body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }

    // -- general --
    let mut general: Vec<&NoteRow> = notes
        .iter()
        .filter(|n| n.section == NoteSection::General)
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
        for n in general {
            out.push_str(&note_header(n));
            out.push_str("\n\n");
            out.push_str(&quote_body(&n.body));
            out.push_str("\n\n");
        }
    }

    // -- inline grouped by (path, line) --
    let inline: Vec<&NoteRow> = notes
        .iter()
        .filter(|n| n.section == NoteSection::Inline)
        .collect();
    out.push_str("## Inline comments\n\n");
    if inline.is_empty() {
        out.push_str("*(no inline comments)*\n\n");
    } else {
        // GitLab discussions already carry path/line on every note in the
        // thread, so we don't need an anchor_for_id table — group directly.
        let mut groups: BTreeMap<(String, i64), Vec<&NoteRow>> = BTreeMap::new();
        let mut group_keys: BTreeSet<(String, i64)> = BTreeSet::new();
        for n in &inline {
            let key = (
                n.path.clone().unwrap_or_else(|| "unknown".into()),
                n.line.unwrap_or(0),
            );
            group_keys.insert(key.clone());
            groups.entry(key).or_default().push(n);
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
            for n in bucket {
                out.push_str(&note_header(n));
                out.push_str("\n\n");
                out.push_str(&quote_body(&n.body));
                out.push_str("\n\n");
            }
        }
    }

    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    fs::write(&md_path, &out).with_context(|| format!("write {}", md_path.display()))?;

    let rows = rows_for_mr(mr, notes);
    let sidecar = Sidecar {
        header: SidecarHeader {
            document_uuid: mr.uuid.clone(),
            source_fingerprint: fingerprint_for_mr(mr, notes),
            render_version: RENDER_VERSION,
        },
        rows,
    };
    let sidecar_path = md_path.with_extension("grid_rows.json");
    fs::write(&sidecar_path, serde_json::to_string_pretty(&sidecar)?)
        .with_context(|| format!("write {}", sidecar_path.display()))?;

    Ok(md_path)
}

pub fn render_gitlab(
    parsed: &ParsedGitlabApi,
    root: &Path,
    progress: &Progress,
    prior_fingerprints: &std::collections::HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedDoc) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();
    let mut by_mr: std::collections::HashMap<(String, u32), Vec<NoteRow>> = Default::default();
    for n in &parsed.notes {
        by_mr
            .entry((n.project_full_path.clone(), n.mr_iid))
            .or_default()
            .push(n.clone());
    }
    progress.set_length(Some(parsed.merge_requests.len() as u64));
    for mr in &parsed.merge_requests {
        let key = (mr.project_full_path.clone(), mr.mr_iid);
        let notes = by_mr.remove(&key).unwrap_or_default();
        let fingerprint = fingerprint_for_mr(mr, &notes);
        let md_rel = mr_qmd_path_rel(&mr.project_full_path, mr.mr_iid);
        let md_path = root.join(&md_rel);

        if prior_fingerprints.get(&mr.uuid).map(String::as_str) == Some(fingerprint.as_str())
            && md_path.exists()
        {
            summary.skipped += 1;
            progress.inc(1);
            continue;
        }

        render_one_mr(mr, &notes, root)?;
        let rows = rows_for_mr(mr, &notes);
        on_doc_complete(RenderedDoc {
            document_uuid: mr.uuid.clone(),
            source_name: String::new(),
            source_fingerprint: fingerprint,
            md_path: md_path.clone(),
            render_version: RENDER_VERSION,
            rows,
        })?;
        summary.rendered += 1;
        progress.inc(1);
    }
    Ok(summary)
}
