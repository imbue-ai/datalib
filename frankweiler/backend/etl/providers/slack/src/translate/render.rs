//! Slack render: convert parsed thread buckets into the shared
//! `chat-common` normalized model and delegate the markdown / grid-row /
//! sidecar plumbing to [`frankweiler_etl_chat_common::render::render_all`].
//!
//! One Slack thread → one [`NormalizedChat`] with a single `"all"`
//! bucket; `chat_uuid` and the bucket's `markdown_uuid` are the existing
//! `slack_thread_uuid`, so page identities / links stay stable across
//! the migration. Each thread carries its own per-thread `BlobBundle`
//! (keyed by `chat.id`) so chat-common materializes attachment bytes the
//! same way the bespoke renderer used to.
//!
//! What chat-common gives us: image/audio/video attachments render as
//! inline `<img>` / `<audio controls>` / `<video controls>` widgets;
//! reactions become first-class per-reaction grid rows; the thread
//! permalink is the chat-level `↗` linkout and each message carries its
//! own permalink `↗` + grid `source_url`.
//!
//! Incrementality is unchanged and still dolt-diff driven: `parse`
//! consulted `dolt_diff_<table>` against the render cursor and only
//! loaded threads that actually changed, so we pass an empty
//! `prior_fingerprints` map (chat-common's fingerprint-skip is a no-op
//! here) and advance the cursor on success.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl_chat_common::render::{render_all as cc_render_all, RenderProfile};
use frankweiler_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};

use crate::extract::schema_raw::slack_reaction_uuid;

use super::mrkdwn::{emojize_shortcodes, resolve_user_mentions, to_commonmark};
use super::{slack_link, Message, ParsedSlack};

/// Bump when the on-disk render layout changes in a way that must
/// invalidate stale docs. v3: render via chat-common.
pub const RENDER_VERSION: u32 = 3;

#[derive(Debug, Default)]
pub struct RenderSummary {
    pub threads_total: usize,
    pub threads_rendered: usize,
    pub threads_skipped: usize,
}

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "slack",
        source_label: "Slack".to_string(),
        chat_kind: "Slack Thread".to_string(),
        message_kind: "Slack Message".to_string(),
        reaction_kind: "Slack Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render every thread bucket in `parsed` under `out_dir` via the shared
/// chat renderer. Idempotent at the dolt_diff level — buckets the scan
/// reported as unchanged never appear in `parsed.threads`.
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

    let (chats, blobs_by_chat) = build_chats(parsed, &user_labels);

    // Incremental skip is driven upstream by dolt_diff, so the
    // fingerprint map is intentionally empty: every changed thread that
    // reached us is (re)rendered.
    let no_priors: HashMap<String, String> = HashMap::new();
    let cc = cc_render_all(
        &profile(),
        &chats,
        out_dir,
        source_name,
        &blobs_by_chat,
        progress,
        &no_priors,
        on_doc_complete,
    )
    .context("slack chat-common render")?;

    // Advance the render cursor only when everything succeeded AND we
    // managed to read HEAD at scan time. Without HEAD the next run is
    // another cold start (the right behavior — nothing to anchor on).
    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(out_dir, "slack", source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write slack render cursor {}", cursor_path.display()))?;
    }

    Ok(RenderSummary {
        threads_total: parsed.threads.len() + parsed.docs_skipped,
        threads_rendered: cc.docs_rendered,
        threads_skipped: parsed.docs_skipped,
    })
}

/// One [`NormalizedChat`] per thread bucket, plus the per-thread
/// [`BlobBundle`] keyed by `chat.id` for chat-common to materialize.
fn build_chats(
    parsed: &ParsedSlack,
    user_labels: &BTreeMap<String, String>,
) -> (Vec<NormalizedChat>, HashMap<String, BlobBundle>) {
    let mut chats = Vec::with_capacity(parsed.threads.len());
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();

    for bucket in &parsed.threads {
        let root: &Message = bucket
            .messages
            .iter()
            .find(|m| m.is_thread_root)
            .unwrap_or_else(|| bucket.messages.first().expect("non-empty thread bucket"));
        let cname = parsed
            .channels
            .get(&root.channel_id)
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| root.channel_id.clone());
        let thread_uuid = bucket.thread_uuid.clone();

        let items: Vec<NormalizedChatItem> = bucket
            .messages
            .iter()
            .map(|m| build_item(m, root, user_labels))
            .collect();

        // "#channel: <root snippet>" preserves the old scannable H1; the
        // bare "#channel" remains the conversation_name (via `display`).
        let title = format!("#{cname}: {}", thread_title(&root.text, user_labels));

        chats.push(NormalizedChat {
            id: thread_uuid.clone(),
            chat_uuid: thread_uuid.clone(),
            display: format!("#{cname}"),
            title: Some(title),
            account: Some(root.team_id.clone()),
            project: None,
            external_id: None,
            // Thread permalink → chat-level `↗` + chat grid source_url.
            source_url: Some(slack_link(&root.team_id, &root.channel_id, &root.ts, None)),
            buckets: vec![NormalizedDoc {
                period_key: "all".to_string(),
                markdown_uuid: thread_uuid.clone(),
                items,
            }],
        });
        blobs_by_chat.insert(thread_uuid, bucket.blobs.clone());
    }
    (chats, blobs_by_chat)
}

fn build_item(
    m: &Message,
    root: &Message,
    user_labels: &BTreeMap<String, String>,
) -> NormalizedChatItem {
    let author_display = m
        .user_id
        .as_deref()
        .and_then(|u| user_labels.get(u).cloned())
        .unwrap_or_else(|| m.user_id.clone().unwrap_or_else(|| "unknown".into()));
    let body = to_commonmark(m.text.trim_end(), user_labels);
    let attachments = build_attachments(&m.raw_json);
    let reactions = build_reactions(&m.raw_json, m, user_labels);
    let kind = if attachments.is_empty() {
        ItemKind::Text
    } else {
        ItemKind::Attachment
    };
    NormalizedChatItem {
        message_uuid: m.uuid(),
        author_id: m.user_id.clone().unwrap_or_else(|| "unknown".into()),
        author_display,
        date_ms: ts_to_ms(&m.ts),
        text: (!body.trim().is_empty()).then_some(body),
        kind,
        attachments,
        reactions,
        system_note: None,
        // Per-message permalink (with thread_ts for replies).
        source_url: Some(slack_link(&m.team_id, &m.channel_id, &m.ts, Some(&root.ts))),
    }
}

/// First non-empty line of the (mention-resolved) root text, truncated —
/// the scannable bit of the thread title.
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

/// Slack `ts` ("secs.frac", UTC) → unix milliseconds.
fn ts_to_ms(ts: &str) -> i64 {
    let (secs_str, frac_str) = ts.split_once('.').unwrap_or((ts, ""));
    let secs: i64 = secs_str.parse().unwrap_or(0);
    let mut frac = frac_str.to_string();
    while frac.len() < 3 {
        frac.push('0');
    }
    frac.truncate(3);
    let millis: i64 = frac.parse().unwrap_or(0);
    secs * 1000 + millis
}

// ---------------------------------------------------------------------------
// File / reaction extraction from raw_json → normalized model.
// ---------------------------------------------------------------------------

/// Map a Slack message's `files[]` into [`NormalizedAttachment`]s. The
/// real `mimetype` flows through so chat-common picks `<img>` / `<audio>`
/// / `<video>` / generic per attachment. Local bytes resolve via
/// `ref_id` (the Slack `file_id`) against the thread's bundle; externals
/// / tombstones carry no `ref_id` and fall back to the upstream URL.
fn build_attachments(raw: &Value) -> Vec<NormalizedAttachment> {
    raw.get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|f| {
                    let filetype = f.get("filetype").and_then(|v| v.as_str()).unwrap_or("");
                    let mime_type = f
                        .get("mimetype")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or_else(|| image_mime_for(filetype));
                    let file_name = f
                        .get("title")
                        .and_then(|v| v.as_str())
                        .or_else(|| f.get("name").and_then(|v| v.as_str()))
                        .map(str::to_string);
                    let external = f
                        .get("is_external")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || f.get("mode").and_then(|v| v.as_str()) == Some("tombstone");
                    let source_url = f
                        .get("url_private")
                        .or_else(|| f.get("permalink"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let ref_id = if external {
                        None
                    } else {
                        f.get("id").and_then(|v| v.as_str()).map(str::to_string)
                    };
                    NormalizedAttachment {
                        rel_path: None,
                        file_name,
                        mime_type,
                        byte_len: f.get("size").and_then(|v| v.as_i64()),
                        source_url,
                        ref_id,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Backfill an image mime for files where Slack omitted `mimetype` but
/// gave a recognizable `filetype`, so they still render inline.
fn image_mime_for(filetype: &str) -> Option<String> {
    let m = match filetype {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    Some(m.to_string())
}

/// Expand a message's `reactions[]` into one [`NormalizedReaction`] per
/// reacting user (resolved to a display label), so each is its own
/// searchable grid row. A count-only reaction (no `users` list) yields a
/// single row labelled with the count.
fn build_reactions(
    raw: &Value,
    m: &Message,
    user_labels: &BTreeMap<String, String>,
) -> Vec<NormalizedReaction> {
    let date_ms = ts_to_ms(&m.ts);
    let mut out = Vec::new();
    let Some(arr) = raw.get("reactions").and_then(|v| v.as_array()) else {
        return out;
    };
    for r in arr {
        let Some(name) = r.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let emoji = emojize_shortcodes(&format!(":{name}:"));
        let users: Vec<&str> = r
            .get("users")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|u| u.as_str()).collect())
            .unwrap_or_default();
        if users.is_empty() {
            let count = r.get("count").and_then(|v| v.as_u64()).unwrap_or(1);
            out.push(NormalizedReaction {
                reaction_uuid: slack_reaction_uuid(&m.team_id, &m.channel_id, &m.ts, name, ""),
                reactor_display: format!("{count}"),
                emoji,
                date_ms,
            });
        } else {
            for u in users {
                out.push(NormalizedReaction {
                    reaction_uuid: slack_reaction_uuid(&m.team_id, &m.channel_id, &m.ts, name, u),
                    reactor_display: user_labels.get(u).cloned().unwrap_or_else(|| u.to_string()),
                    emoji: emoji.clone(),
                    date_ms,
                });
            }
        }
    }
    out
}
