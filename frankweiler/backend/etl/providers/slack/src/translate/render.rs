//! Per-thread Markdown render + sidecar emission.
//!
//! For each Slack thread we emit two co-located files under
//! `<out>/rendered_md/slack/<team>/<channel>/threads/<thread_uuid>/`:
//!
//!   * `index.md` — human-readable + grid `qmd_path` target. YAML
//!     frontmatter carries provider metadata + the `thread_uuid` as
//!     `source_fingerprint`.
//!   * `index.grid_rows.json` — structured rows for the downstream
//!     loader.
//!
//! Incrementality is driven upstream of render: `parse` consulted
//! `dolt_diff_<table>` against the render cursor and only loaded
//! threads that actually changed. Once we reach this module, every
//! thread bucket in `parsed.threads` should be (re)rendered. The
//! cursor is advanced on success.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl::section::msg_div_open;
use frankweiler_etl::title::Title;
use frankweiler_index_lib::emit_sidecar;
use frankweiler_schema::grid_rows::GridRow;

use super::mrkdwn::{emojize_shortcodes, resolve_user_mentions, to_commonmark};
use super::{slack_link, Message, ParsedSlack, SlackThreadBucket};

/// Bump when the on-disk render layout changes in a way that must
/// invalidate stale `.md` files.
pub const RENDER_VERSION: u32 = 2;

#[derive(Debug, Default)]
pub struct RenderSummary {
    pub threads_total: usize,
    pub threads_rendered: usize,
    pub threads_skipped: usize,
}

/// Render every thread bucket in `parsed` under `out_dir`. Idempotent
/// at the dolt_diff level — buckets the scan reported as unchanged
/// don't appear in `parsed.threads` in the first place.
pub fn render_all(
    parsed: &ParsedSlack,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    // Log how long the dolt_diff scan took (matches chatgpt/anthropic).
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_name,
        scan_elapsed_ms = elapsed_ms,
        changed_threads = parsed
            .scan
            .changed_threads
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        cold_start = parsed.scan.changed_threads.is_none(),
        "[translate] slack dolt_diff scan"
    );

    let user_labels: BTreeMap<String, String> = parsed
        .users
        .iter()
        .map(|(id, u)| (id.clone(), u.label()))
        .collect();

    let mut summary = RenderSummary {
        threads_total: parsed.threads.len() + parsed.docs_skipped,
        threads_skipped: parsed.docs_skipped,
        ..Default::default()
    };
    progress.set_length(Some(summary.threads_total as u64));
    progress.inc(parsed.docs_skipped as u64);

    for bucket in &parsed.threads {
        let root: &Message = bucket
            .messages
            .iter()
            .find(|m| m.is_thread_root)
            .unwrap_or_else(|| bucket.messages.first().expect("non-empty thread bucket"));
        let channel = parsed.channels.get(&root.channel_id);
        let cname = channel
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| root.channel_id.clone());

        let (md_path, json_path) = output_paths(
            out_dir,
            &root.team_id,
            &root.channel_id,
            &bucket.thread_uuid,
        );

        // The per-doc `source_fingerprint` is the thread_uuid itself
        // — stable across re-renders, distinct across buckets. The
        // skip decision now happens in `parse` via dolt_diff.
        let fingerprint = bucket.thread_uuid.clone();
        let md_rel = md_path
            .strip_prefix(out_dir)
            .unwrap_or(&md_path)
            .to_path_buf();
        let rows = build_thread_rows(parsed, bucket, root, &cname, &user_labels);
        let md = render_thread_md(
            &bucket.thread_uuid,
            &fingerprint,
            root,
            &cname,
            &bucket.messages,
            &user_labels,
            source_name,
            &md_rel,
            &bucket.blobs,
        );

        let page_dir = md_path
            .parent()
            .expect("output_paths always produces <page_dir>/index.md");
        fs::create_dir_all(page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

        // Order: blobs → md → sidecar → callback. The callback is the
        // commit point.
        bucket
            .blobs
            .materialize_to_dir(&page_dir.join("blobs"))
            .with_context(|| format!("materialize blobs for {}", bucket.thread_uuid))?;

        fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;
        emit_sidecar(
            &json_path,
            &bucket.thread_uuid,
            &fingerprint,
            RENDER_VERSION,
            &rows,
            &[],
        )?;

        on_doc_complete(RenderedMarkdown {
            markdown_uuid: bucket.thread_uuid.clone(),
            source_name: source_name.to_string(),
            source_fingerprint: fingerprint,
            upstream_cursor: None,
            md_path: md_path.clone(),
            render_version: RENDER_VERSION,
            rows,
            edges: Vec::new(),
        })
        .with_context(|| format!("on_doc_complete {}", bucket.thread_uuid))?;

        summary.threads_rendered += 1;
        progress.inc(1);
    }

    // Advance the render cursor only when everything succeeded AND
    // we managed to read HEAD at scan time. Without HEAD, the next
    // run is another cold start (the right behavior — we have no way
    // to anchor the diff).
    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(out_dir, "slack", source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write slack render cursor {}", cursor_path.display()))?;
    }
    Ok(summary)
}

fn output_paths(
    out_dir: &Path,
    team_id: &str,
    channel_id: &str,
    thread_uuid: &str,
) -> (PathBuf, PathBuf) {
    let dir = out_dir
        .join("rendered_md")
        .join("slack")
        .join(team_id)
        .join(channel_id)
        .join("threads")
        .join(thread_uuid);
    let md = dir.join("index.md");
    let json = dir.join("index.grid_rows.json");
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

#[allow(clippy::too_many_arguments)]
fn render_thread_md(
    thread_uuid: &str,
    fingerprint: &str,
    root: &Message,
    channel_name: &str,
    msgs: &[Message],
    user_labels: &BTreeMap<String, String>,
    source_name: &str,
    md_rel_path: &Path,
    blobs: &BlobBundle,
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
    let title_text = format!("#{channel_name}: {title}");
    let permalink = slack_link(team_id, channel_id, &root.ts, None);
    p.push(
        Title {
            text: &title_text,
            markdown_uuid: Some(thread_uuid),
            source_url: Some(&permalink),
        }
        .render()
        .trim_end()
        .to_string(),
    );
    p.push(String::new());

    for m in msgs.iter() {
        let author = m
            .user_id
            .as_deref()
            .and_then(|u| user_labels.get(u).cloned())
            .unwrap_or_else(|| m.user_id.clone().unwrap_or_else(|| "unknown".into()));
        let link = slack_link(team_id, channel_id, &m.ts, Some(&root.ts));
        p.push(msg_div_open(&m.uuid(), "slack"));
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

        for f in files(&m.raw_json) {
            let alt = f
                .title
                .clone()
                .or_else(|| f.name.clone())
                .unwrap_or_else(|| "file".into())
                .replace(']', "");
            let link = file_link(blobs, &f)
                .unwrap_or_else(|| f.url.clone().unwrap_or_else(|| "about:blank".to_string()));
            let _ = (source_name, md_rel_path);
            if f.is_image {
                p.push(format!("![{alt}]({link})"));
            } else {
                p.push(format!("[\\[file\\] {alt}]({link})"));
            }
            p.push(String::new());
        }

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
    parsed: &ParsedSlack,
    bucket: &SlackThreadBucket,
    root: &Message,
    channel_name: &str,
    user_labels: &BTreeMap<String, String>,
) -> Vec<GridRow> {
    let qmd = super::slack_qmd_path(&root.team_id, &root.channel_id, &bucket.thread_uuid);
    let author = root
        .user_id
        .as_deref()
        .and_then(|u| user_labels.get(u).cloned())
        .or_else(|| root.user_id.clone());

    let mut out = Vec::with_capacity(bucket.messages.len() + 1);
    out.push(GridRow {
        uuid: bucket.thread_uuid.clone(),
        provider: "slack".into(),
        kind: "Slack Thread".into(),
        source_label: "Slack".into(),
        when_ts: Some(root.ts_iso.clone()),
        author: author.clone(),
        account: Some(root.team_id.clone()),
        org_uuid: None,
        org_name: None,
        project: None,
        channel: Some(channel_name.to_string()),
        conversation_name: Some(format!("#{channel_name}")),
        conversation_uuid: bucket.thread_uuid.clone(),
        message_index: None,
        entire_chat: format!("/slack/{}", bucket.thread_uuid),
        text: resolve_user_mentions(&root.text, user_labels),
        slack_link: Some(slack_link(&root.team_id, &root.channel_id, &root.ts, None)),
        qmd_path: Some(qmd.clone()),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(bucket.thread_uuid.clone()),
    });
    let _ = parsed;
    for (idx, m) in bucket.messages.iter().enumerate() {
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
            when_ts: Some(m.ts_iso.clone()),
            author: mauthor,
            account: Some(m.team_id.clone()),
            org_uuid: None,
            org_name: None,
            project: None,
            channel: Some(channel_name.to_string()),
            conversation_name: Some(format!("#{channel_name}")),
            conversation_uuid: bucket.thread_uuid.clone(),
            message_index: Some(idx as i64),
            entire_chat: format!("/slack/{}", bucket.thread_uuid),
            text: resolve_user_mentions(&m.text, user_labels),
            slack_link: Some(slack_link(&m.team_id, &m.channel_id, &m.ts, Some(&root.ts))),
            qmd_path: Some(qmd.clone()),
            source_url: None,
            git_sha: None,
            external_id: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(bucket.thread_uuid.clone()),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// File / reaction extraction from raw_json.
// ---------------------------------------------------------------------------

struct FileRef {
    id: Option<String>,
    name: Option<String>,
    title: Option<String>,
    url: Option<String>,
    is_image: bool,
    external: bool,
}

fn files(raw: &Value) -> Vec<FileRef> {
    raw.get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|f| {
                    let mimetype = f.get("mimetype").and_then(|v| v.as_str()).unwrap_or("");
                    let filetype = f.get("filetype").and_then(|v| v.as_str()).unwrap_or("");
                    let is_image = mimetype.starts_with("image/")
                        || matches!(filetype, "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg");
                    let external = f
                        .get("is_external")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || f.get("mode").and_then(|v| v.as_str()) == Some("tombstone");
                    FileRef {
                        id: f.get("id").and_then(|v| v.as_str()).map(str::to_string),
                        name: f.get("name").and_then(|v| v.as_str()).map(str::to_string),
                        title: f.get("title").and_then(|v| v.as_str()).map(str::to_string),
                        url: f
                            .get("url_private")
                            .or_else(|| f.get("permalink"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                        is_image,
                        external,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Relative link from a thread's `index.md` to its locally-staged
/// copy of `f`. Returns `None` for externals or files whose bytes
/// aren't in the bundle, so the caller can fall back to the upstream
/// URL.
fn file_link(blobs: &BlobBundle, f: &FileRef) -> Option<String> {
    if f.external {
        return None;
    }
    let id = f.id.as_deref()?;
    Some(blobs.markdown_link(id, f.name.as_deref(), f.is_image))
}

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
                    Some((name, users))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn yaml_scalar(v: &str) -> String {
    if v.is_empty() {
        return "\"\"".into();
    }
    let needs_quote = v.chars().any(|c| {
        matches!(
            c,
            ':' | '#' | '\n' | '"' | '\'' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>'
        )
    }) || v != v.trim();
    if needs_quote {
        serde_json::to_string(v).unwrap_or_else(|_| format!("\"{v}\""))
    } else {
        v.into()
    }
}

// Suppress unused-import warning if the unused-set arg is removed.
#[allow(dead_code)]
fn _used(_h: HashSet<String>) {}
