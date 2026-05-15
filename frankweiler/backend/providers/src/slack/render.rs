//! Per-thread Markdown render + sidecar emission.
//!
//! For each Slack thread we emit two co-located files under
//! `<out>/rendered_md/slack/<team>/<channel>/threads/`:
//!
//!   * `<thread_uuid>__<slug>.md` — human-readable + grid `qmd_path`
//!     target. YAML frontmatter carries provider metadata and a
//!     `source_fingerprint` that hashes the canonical Slack payload of
//!     every message in the thread.
//!   * `<thread_uuid>__<slug>.grid_rows.json` — structured rows for the
//!     downstream loader. The loader reads this, not the markdown, so
//!     it doesn't have to re-parse mrkdwn.
//!
//! Incrementality: if a thread's `source_fingerprint` matches the one
//! recorded in the existing `.md`, we skip the write entirely. The
//! fingerprint is derived from the raw Slack JSON, so render-code
//! changes alone do not invalidate prior outputs — bump the
//! [`RENDER_VERSION`] constant when you need a forced rebake.

use std::collections::BTreeMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use frankweiler_schema::grid_rows::GridRow;

use super::mrkdwn::{emojize_shortcodes, resolve_user_mentions, to_commonmark};
use super::translate::{slack_link, Message, TranslatedSlack};

/// Bump when the on-disk render layout changes in a way that must
/// invalidate stale `.md` files even though their `source_fingerprint`
/// would otherwise still match.
pub const RENDER_VERSION: u32 = 1;

#[derive(Debug, Default)]
pub struct RenderSummary {
    pub threads_total: usize,
    pub threads_rendered: usize,
    pub threads_skipped: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SidecarHeader {
    pub thread_uuid: String,
    pub source_fingerprint: String,
    pub render_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub header: SidecarHeader,
    pub rows: Vec<GridRow>,
}

/// Render every thread in `t` under `out_dir`. Idempotent: threads
/// whose fingerprint already matches the on-disk `.md` are left alone.
pub fn render_all(
    t: &TranslatedSlack,
    out_dir: &Path,
    progress: impl Fn(&str),
) -> Result<RenderSummary> {
    let user_labels: BTreeMap<String, String> = t
        .users
        .iter()
        .map(|(id, u)| (id.clone(), u.label()))
        .collect();

    // Group messages by thread_uuid.
    let mut by_thread: BTreeMap<String, Vec<&Message>> = BTreeMap::new();
    for m in t.messages.values() {
        by_thread.entry(m.thread_uuid()).or_default().push(m);
    }

    let mut summary = RenderSummary {
        threads_total: by_thread.len(),
        ..Default::default()
    };

    for (thread_uuid, mut msgs) in by_thread {
        msgs.sort_by(|a, b| {
            (a.ts_iso.as_str(), a.ts.as_str()).cmp(&(b.ts_iso.as_str(), b.ts.as_str()))
        });
        let root: &Message = msgs
            .iter()
            .copied()
            .find(|m| m.is_thread_root)
            .unwrap_or(msgs[0]);
        let channel = t.channels.get(&root.channel_id);
        let cname = channel
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| root.channel_id.clone());

        let fingerprint = compute_fingerprint(&msgs);
        let (md_path, json_path) = output_paths(
            out_dir,
            &root.team_id,
            &cname,
            &thread_uuid,
            root,
            &user_labels,
        );

        if existing_fingerprint(&md_path).as_deref() == Some(fingerprint.as_str()) {
            summary.threads_skipped += 1;
            continue;
        }

        let rows = build_thread_rows(t, &thread_uuid, &msgs, root, &cname, &user_labels);
        let md = render_thread_md(
            &thread_uuid,
            &fingerprint,
            root,
            &cname,
            &msgs,
            &user_labels,
        );

        if let Some(parent) = md_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        // Clean up any earlier `<uuid>__*.md` from before the slug changed.
        if let Some(dir) = md_path.parent() {
            let prefix = format!("{thread_uuid}__");
            if let Ok(rd) = fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p == md_path || p == json_path {
                        continue;
                    }
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if name.starts_with(&prefix)
                        && (name.ends_with(".md") || name.ends_with(".grid_rows.json"))
                    {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }

        fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;
        let sidecar = Sidecar {
            header: SidecarHeader {
                thread_uuid: thread_uuid.clone(),
                source_fingerprint: fingerprint.clone(),
                render_version: RENDER_VERSION,
            },
            rows,
        };
        let sj = serde_json::to_string_pretty(&sidecar)?;
        fs::write(&json_path, sj).with_context(|| format!("write {}", json_path.display()))?;

        summary.threads_rendered += 1;
        progress(&format!(
            "rendered {}/{}",
            summary.threads_rendered + summary.threads_skipped,
            summary.threads_total
        ));
    }
    Ok(summary)
}

/// Hash of `(ts, canonical_json(raw_json))` pairs for every message in
/// the thread, plus the render-version stamp. Stable across runs: the
/// same source payload yields the same fingerprint.
fn compute_fingerprint(msgs: &[&Message]) -> String {
    let mut pairs: Vec<(String, String)> = msgs
        .iter()
        .map(|m| {
            (
                m.ts.clone(),
                serde_json::to_string(&m.raw_json).unwrap_or_default(),
            )
        })
        .collect();
    pairs.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    for (ts, raw) in &pairs {
        ts.hash(&mut h);
        raw.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn output_paths(
    out_dir: &Path,
    team_id: &str,
    channel_name: &str,
    thread_uuid: &str,
    root: &Message,
    user_labels: &BTreeMap<String, String>,
) -> (PathBuf, PathBuf) {
    let title = thread_title(&root.text, user_labels);
    let slug = slugify(&title);
    let dir = out_dir
        .join("rendered_md")
        .join("slack")
        .join(team_id)
        .join(channel_name)
        .join("threads");
    let md = dir.join(format!("{thread_uuid}__{slug}.md"));
    let json = dir.join(format!("{thread_uuid}__{slug}.grid_rows.json"));
    (md, json)
}

fn thread_title(root_text: &str, user_labels: &BTreeMap<String, String>) -> String {
    let resolved = resolve_user_mentions(root_text, user_labels);
    let first = resolved
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(empty thread)")
        .to_string();
    first.chars().take(80).collect()
}

fn existing_fingerprint(md_path: &Path) -> Option<String> {
    let text = fs::read_to_string(md_path).ok()?;
    for line in text.lines().take(40) {
        if let Some(rest) = line.strip_prefix("source_fingerprint:") {
            let v = rest.trim().trim_matches('"').to_string();
            return Some(v);
        }
        if line == "---" && !text.starts_with(line) {
            // End of frontmatter.
            return None;
        }
    }
    None
}

fn render_thread_md(
    thread_uuid: &str,
    fingerprint: &str,
    root: &Message,
    channel_name: &str,
    msgs: &[&Message],
    user_labels: &BTreeMap<String, String>,
) -> String {
    let title = thread_title(&root.text, user_labels);
    let team_id = &root.team_id;
    let channel_id = &root.channel_id;

    let mut p: Vec<String> = Vec::new();
    p.push("---".into());
    p.push("provider: slack".into());
    p.push(format!("thread_uuid: {}", yaml_scalar(thread_uuid)));
    p.push(format!("team_id: {}", yaml_scalar(team_id)));
    p.push(format!("channel_id: {}", yaml_scalar(channel_id)));
    p.push(format!("channel_name: {}", yaml_scalar(channel_name)));
    p.push(format!("root_ts: {}", yaml_scalar(&root.ts)));
    p.push(format!("root_ts_iso: {}", yaml_scalar(&root.ts_iso)));
    p.push(format!(
        "slack_link: {}",
        yaml_scalar(&slack_link(team_id, channel_id, &root.ts, None))
    ));
    p.push(format!("source_fingerprint: {}", yaml_scalar(fingerprint)));
    p.push(format!("render_version: {RENDER_VERSION}"));
    p.push("---".into());
    p.push(String::new());
    p.push(format!("# #{channel_name}: {title}"));
    p.push(String::new());

    for (idx, m) in msgs.iter().enumerate() {
        let author = m
            .user_id
            .as_deref()
            .and_then(|u| user_labels.get(u).cloned())
            .unwrap_or_else(|| m.user_id.clone().unwrap_or_else(|| "unknown".into()));
        let link = slack_link(team_id, channel_id, &m.ts, Some(&root.ts));
        p.push(format!(
            r#"<div class="msg" data-msg-uuid="{}" data-msg-index="{}" data-provider="slack">"#,
            m.uuid(),
            idx
        ));
        p.push(String::new());
        p.push(format!("## {author}"));
        p.push(String::new());
        p.push(format!(
            r#"<div class="msg-meta"><em>{}</em> · <a href="{}" target="_blank" rel="noopener noreferrer" title="View in Slack">↗</a></div>"#,
            m.ts_iso, link
        ));
        p.push(String::new());
        p.push(to_commonmark(m.text.trim_end(), user_labels));
        p.push(String::new());

        // Files: include verbatim url_private references. The downloader
        // captures them locally, but the renderer is intentionally
        // decoupled — we don't depend on a parallel media directory.
        for f in files(&m.raw_json) {
            let alt = f
                .title
                .or(f.name)
                .unwrap_or_else(|| "file".into())
                .replace(']', "");
            let url = f.url.unwrap_or_default();
            if !url.is_empty() {
                p.push(format!("![{alt}]({url})"));
                p.push(String::new());
            }
        }

        // Reactions: collapse to `:name: ×N` lines per emoji.
        let rxs = reactions(&m.raw_json);
        if !rxs.is_empty() {
            let parts: Vec<String> = rxs
                .into_iter()
                .map(|(name, count)| {
                    let rendered = emojize_shortcodes(&format!(":{name}:"));
                    if count > 1 {
                        format!("{rendered} ×{count}")
                    } else {
                        rendered
                    }
                })
                .collect();
            p.push(format!("> Reactions: {}", parts.join(" ")));
            p.push(String::new());
        }

        p.push("</div>".into());
        p.push(String::new());
    }

    let mut body = p.join("\n");
    while body.ends_with('\n') {
        body.pop();
    }
    body.push('\n');
    body
}

fn build_thread_rows(
    t: &TranslatedSlack,
    thread_uuid: &str,
    msgs: &[&Message],
    root: &Message,
    channel_name: &str,
    user_labels: &BTreeMap<String, String>,
) -> Vec<GridRow> {
    let qmd = super::translate::slack_qmd_path(
        &root.team_id,
        channel_name,
        thread_uuid,
        &root.text,
        user_labels,
    );
    let author = root
        .user_id
        .as_deref()
        .and_then(|u| user_labels.get(u).cloned())
        .or_else(|| root.user_id.clone());

    let mut out = Vec::with_capacity(msgs.len() + 1);
    out.push(GridRow {
        uuid: thread_uuid.to_string(),
        provider: "slack".into(),
        kind: "Slack Thread".into(),
        source_label: "Slack".into(),
        when_ts: root.ts_iso.clone(),
        author: author.clone(),
        account: Some(root.team_id.clone()),
        project: None,
        channel: Some(channel_name.to_string()),
        conversation_name: Some(format!("#{channel_name}")),
        conversation_uuid: thread_uuid.to_string(),
        message_index: None,
        entire_chat: format!("/slack/{thread_uuid}"),
        text: resolve_user_mentions(&root.text, user_labels),
        slack_link: Some(slack_link(&root.team_id, &root.channel_id, &root.ts, None)),
        qmd_path: Some(qmd.clone()),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        document_uuid: Some(thread_uuid.to_string()),
    });
    let _ = t; // future: thread-level project, etc.
    for (idx, m) in msgs.iter().enumerate() {
        let mauthor = m
            .user_id
            .as_deref()
            .and_then(|u| user_labels.get(u).cloned())
            .or_else(|| m.user_id.clone());
        out.push(GridRow {
            uuid: m.uuid(),
            provider: "slack".into(),
            kind: "Slack Message".into(),
            source_label: "Slack".into(),
            when_ts: m.ts_iso.clone(),
            author: mauthor,
            account: Some(m.team_id.clone()),
            project: None,
            channel: Some(channel_name.to_string()),
            conversation_name: Some(format!("#{channel_name}")),
            conversation_uuid: thread_uuid.to_string(),
            message_index: Some(idx as i64),
            entire_chat: format!("/slack/{thread_uuid}"),
            text: resolve_user_mentions(&m.text, user_labels),
            slack_link: Some(slack_link(&m.team_id, &m.channel_id, &m.ts, Some(&root.ts))),
            qmd_path: Some(qmd.clone()),
            source_url: None,
            git_sha: None,
            external_id: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            document_uuid: Some(thread_uuid.to_string()),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// File / reaction extraction from raw_json. These shapes match the
// Slack API response verbatim; the downloader's `raw_json` capture
// preserves them.
// ---------------------------------------------------------------------------

struct FileRef {
    name: Option<String>,
    title: Option<String>,
    url: Option<String>,
}

fn files(raw: &Value) -> Vec<FileRef> {
    raw.get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|f| FileRef {
                    name: f.get("name").and_then(|v| v.as_str()).map(str::to_string),
                    title: f.get("title").and_then(|v| v.as_str()).map(str::to_string),
                    url: f
                        .get("url_private")
                        .or_else(|| f.get("permalink"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `[(name, count), ...]` in source order, summing user lists.
fn reactions(raw: &Value) -> Vec<(String, usize)> {
    raw.get("reactions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let name = r.get("name").and_then(|v| v.as_str())?.to_string();
                    let users = r
                        .get("users")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let count = r
                        .get("count")
                        .and_then(|v| v.as_u64())
                        .map(|n| n as usize)
                        .unwrap_or(users);
                    Some((name, count))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tiny helpers: slug, yaml-scalar.
// ---------------------------------------------------------------------------

const SLUG_MAX_LEN: usize = 60;

fn slugify(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    let mut prev_dash = true;
    for ch in name.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            s.push(c);
            prev_dash = false;
        } else if !prev_dash {
            s.push('-');
            prev_dash = true;
        }
    }
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        return "untitled".to_string();
    }
    if s.len() > SLUG_MAX_LEN {
        s.truncate(SLUG_MAX_LEN);
        while s.ends_with('-') {
            s.pop();
        }
        if s.is_empty() {
            return "untitled".to_string();
        }
    }
    s
}

fn yaml_scalar(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    let needs_quote = s.contains(':')
        || s.contains('#')
        || s.contains('\n')
        || s.contains('"')
        || s.contains('\'')
        || s != s.trim();
    if needs_quote {
        serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
    } else {
        s.to_string()
    }
}
