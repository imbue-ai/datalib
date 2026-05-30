//! Markdown + grid_rows rendering for Beeper documents.
//!
//! One `.md` per `(room, period)` bucket. Reactions render inline
//! under the message they target, even when the reaction itself
//! landed in a later period. Blobs that were ingested with bytes
//! get materialized to a sibling `blobs/` directory and linked
//! relatively from the markdown.

use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use frankweiler_etl::load::RenderedDoc;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};
use frankweiler_schema::grid_rows::GridRow;

use super::parse::{DocBucket, Event, ParsedBeeper, Room};
use super::{beeper_document_uuid, beeper_event_uuid};

/// Bump when the rendered markdown / grid_rows layout changes
/// enough that we need every existing doc rebuilt.
pub const RENDER_VERSION: u32 = 1;

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub docs_total: usize,
    pub docs_rendered: usize,
    pub docs_skipped: usize,
    pub blobs_materialized: usize,
}

/// Entry point. Iterates every `(room, period)` doc the parser
/// produced and renders it. Calls `on_doc_complete` exactly once
/// per rendered (or skipped) document so the sync orchestrator's
/// per-doc index callback fires reliably.
pub fn render_all(
    parsed: &ParsedBeeper,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedDoc) -> Result<()>,
    raw_db_path: &Path,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        docs_total: parsed.docs.len(),
        ..Default::default()
    };
    progress.set_length(Some(summary.docs_total as u64));

    for doc in &parsed.docs {
        let Some(room) = parsed.rooms.get(&doc.room_uuid) else {
            // Should never happen — the parser populates the room
            // map from the same db. Log and skip rather than abort
            // the whole translate pass.
            tracing::warn!(
                event = "beeper_render_missing_room",
                room_uuid = %doc.room_uuid
            );
            progress.inc(1);
            continue;
        };
        let res = render_one(
            room,
            doc,
            out_dir,
            source_name,
            prior_fingerprints,
            on_doc_complete,
            raw_db_path,
        )?;
        match res {
            RenderOutcome::Rendered { blobs } => {
                summary.docs_rendered += 1;
                summary.blobs_materialized += blobs;
            }
            RenderOutcome::Skipped => summary.docs_skipped += 1,
        }
        progress.inc(1);
    }
    Ok(summary)
}

enum RenderOutcome {
    Rendered { blobs: usize },
    Skipped,
}

fn render_one(
    room: &Room,
    doc: &DocBucket,
    out_dir: &Path,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedDoc) -> Result<()>,
    raw_db_path: &Path,
) -> Result<RenderOutcome> {
    let document_uuid = beeper_document_uuid(&room.room_uuid, &doc.period_key);
    let fingerprint = compute_fingerprint(doc);
    let (md_path, json_path, page_dir) = output_paths(out_dir, room, &doc.period_key);

    if prior_fingerprints.get(&document_uuid).map(String::as_str) == Some(fingerprint.as_str())
        && md_path.exists()
    {
        return Ok(RenderOutcome::Skipped);
    }

    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Blobs first — if a later step fails we never stamp the
    // fingerprint, so a re-run will redo the whole doc cleanly.
    let blobs_dir = page_dir.join("blobs");
    let blob_count = materialize_blobs(raw_db_path, doc, &blobs_dir)
        .with_context(|| format!("materialize blobs for {document_uuid}"))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_path_buf();
    let md = render_markdown(
        room,
        doc,
        &document_uuid,
        &fingerprint,
        source_name,
        &md_rel,
    );
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let rows = build_grid_rows(room, doc, &document_uuid, &md_rel);
    let sidecar = Sidecar {
        header: SidecarHeader {
            document_uuid: document_uuid.clone(),
            source_fingerprint: fingerprint.clone(),
            render_version: RENDER_VERSION,
        },
        rows: rows.clone(),
    };
    let sj = serde_json::to_string_pretty(&sidecar).context("serialize beeper sidecar")?;
    fs::write(&json_path, sj).with_context(|| format!("write {}", json_path.display()))?;

    on_doc_complete(RenderedDoc {
        document_uuid: document_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        // No provider-side cheap-probe signal today. The
        // doltlite extract phase rewrites every doc's `events`
        // on each run, so we always re-render and trust the
        // `source_fingerprint` skip. A future enhancement could
        // stamp `MAX(events.fetched_at)` per room here so the
        // next run can skip whole rooms before parsing.
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
    })
    .with_context(|| format!("on_doc_complete {document_uuid}"))?;

    Ok(RenderOutcome::Rendered { blobs: blob_count })
}

/// Where this doc's `.md`, `.grid_rows.json`, and `blobs/` directory
/// live: `<out>/rendered_md/beeper/<network>/<room_uuid>/<period>.md`.
/// Mirrors slack / chatgpt / anthropic — every provider rendered
/// document lives under `rendered_md/`. Blobs live alongside at the
/// room level (`<room_uuid>/blobs/`) rather than per-period, since a
/// single image can be referenced by multiple period files via its
/// reactions.
fn output_paths(out_dir: &Path, room: &Room, period_key: &str) -> (PathBuf, PathBuf, PathBuf) {
    let page_dir = out_dir
        .join("rendered_md")
        .join("beeper")
        .join(&room.network)
        .join(&room.room_uuid);
    let md_path = page_dir.join(format!("{period_key}.md"));
    let json_path = page_dir.join(format!("{period_key}.grid_rows.json"));
    (md_path, json_path, page_dir)
}

// ─────────────────────────────────────────────────────────────────────
// Fingerprint
// ─────────────────────────────────────────────────────────────────────

/// Stable hash of every message + attached reaction in the doc, plus
/// the render-version stamp. Re-renders of unchanged docs collapse
/// to a no-op via `prior_fingerprints`.
fn compute_fingerprint(doc: &DocBucket) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    doc.room_uuid.hash(&mut h);
    doc.period_key.hash(&mut h);
    for m in &doc.messages {
        m.event_uuid.hash(&mut h);
        m.timestamp_ms.hash(&mut h);
        m.text_content.hash(&mut h);
        m.external_event_id.hash(&mut h);
        m.edit_of_native_event_id.hash(&mut h);
    }
    // Reactions iterate in target order (BTreeMap), then by their
    // own native_event_id sorted, so the hash is order-stable.
    for (target, rs) in &doc.reactions_by_target {
        target.hash(&mut h);
        let mut sorted: Vec<&Event> = rs.iter().collect();
        sorted.sort_by(|a, b| a.native_event_id.cmp(&b.native_event_id));
        for r in sorted {
            r.event_uuid.hash(&mut h);
            r.sender_uuid.hash(&mut h);
            r.reaction_emoji.hash(&mut h);
            r.external_event_id.hash(&mut h);
        }
    }
    format!("{:016x}", h.finish())
}

// ─────────────────────────────────────────────────────────────────────
// Markdown
// ─────────────────────────────────────────────────────────────────────

fn render_markdown(
    room: &Room,
    doc: &DocBucket,
    document_uuid: &str,
    fingerprint: &str,
    source_name: &str,
    md_rel: &Path,
) -> String {
    let _ = md_rel;
    let mut out = String::with_capacity(8 * 1024);

    // Frontmatter — minimal but searchable.
    out.push_str("---\n");
    out.push_str(&format!("document_uuid: {document_uuid}\n"));
    out.push_str(&format!("source_fingerprint: {fingerprint}\n"));
    out.push_str(&format!("source_name: {source_name}\n"));
    out.push_str("provider: beeper\n");
    out.push_str(&format!("network: {}\n", room.network));
    out.push_str(&format!("room_uuid: {}\n", room.room_uuid));
    if let Some(ext) = &room.external_room_id {
        out.push_str(&format!("external_room_id: {ext}\n"));
    }
    if let Some(ws) = &room.external_workspace_id {
        out.push_str(&format!("external_workspace_id: {ws}\n"));
    }
    if let Some(t) = &room.title {
        out.push_str(&format!("title: {}\n", yaml_safe(t)));
    }
    if let Some(acc) = &room.account_id {
        out.push_str(&format!("account_id: {acc}\n"));
    }
    out.push_str(&format!("period: {}\n", doc.period_key));
    out.push_str(&format!("is_dm: {}\n", room.is_dm));
    out.push_str(&format!("event_count: {}\n", doc.messages.len()));
    out.push_str(&format!("first_ts: {}\n", iso_from_ms(doc.first_ms)));
    out.push_str(&format!("last_ts: {}\n", iso_from_ms(doc.last_ms)));
    out.push_str("---\n\n");

    out.push_str(&format!(
        "# {} · {}\n\n",
        room.title.as_deref().unwrap_or(&room.network),
        doc.period_key
    ));

    for m in &doc.messages {
        // Every grid row this provider emits for an event must have a
        // matching `data-section-uuid="<event_uuid>"` node in the
        // rendered markdown — otherwise the chat-preview pane can't
        // highlight (or even scroll to) the message a clicked row
        // points at, and the row looks like data loss. The grid row
        // uuid is `m.event_uuid` (see `rows_for_doc`); same here.
        out.push_str(&format!(
            "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--beeper\">\n\n",
            uuid = m.event_uuid,
        ));

        // HIDDEN events are surfaced as a single italic line — the
        // desktop app suppresses them, but they're real history
        // (membership changes, encryption setup, transcript-
        // exclude marks…) and consumers may want to know they
        // happened. Translators can drop them by filtering on
        // `event_type == "HIDDEN"` if they want a cleaner view.
        if m.is_hidden() {
            out.push_str(&format!(
                "*<small>{} — hidden: {}</small>*\n\n",
                display_ts(m.timestamp_ms),
                hidden_summary(m)
            ));
            out.push_str("</div>\n\n");
            continue;
        }
        out.push_str("## ");
        out.push_str(&display_ts(m.timestamp_ms));
        out.push_str(" — ");
        out.push_str(m.sender_label.as_deref().unwrap_or("unknown"));
        out.push('\n');

        if let Some(reply_to) = &m.reply_to_native_event_id {
            // We don't currently link the reply target to its own
            // markdown anchor — the bridge of native↔matrix ids
            // makes that fiddly when the target lives in a
            // different period file. For now surface the bridge
            // id so a translator-aware reader can chase it.
            out.push_str(&format!("> ↪ in reply to `{reply_to}`\n"));
        }

        match m.event_type.as_str() {
            "TEXT" | "NOTICE" => {
                if let Some(text) = m.text_content.as_deref().filter(|s| !s.is_empty()) {
                    out.push('\n');
                    out.push_str(text);
                    out.push('\n');
                }
            }
            "IMAGE" | "FILE" | "VIDEO" | "AUDIO" | "VOICE" => {
                render_attachment_body(&mut out, m);
            }
            "MEMBERSHIP" => {
                out.push_str("\n*(membership event)*\n");
            }
            other => {
                out.push_str(&format!("\n*({} event)*\n", other.to_lowercase()));
                if let Some(text) = m.text_content.as_deref().filter(|s| !s.is_empty()) {
                    out.push_str(text);
                    out.push('\n');
                }
            }
        }

        // Reactions attached to this message. We wrap each one in its
        // own `data-section-uuid` span so clicking a reaction grid row
        // (whose uuid is the reaction's own `event_uuid`) highlights
        // exactly that bullet rather than the target message wholesale.
        if let Some(rs) = doc.reactions_by_target.get(&m.native_event_id) {
            let mut by_emoji: HashMap<&str, Vec<&Event>> = HashMap::new();
            for r in rs {
                let key = r.reaction_emoji.as_deref().unwrap_or("?");
                by_emoji.entry(key).or_default().push(r);
            }
            let mut groups: Vec<(&&str, &Vec<&Event>)> = by_emoji.iter().collect();
            groups.sort_by_key(|(emoji, _)| *emoji);
            out.push('\n');
            for (emoji, reactors) in groups {
                for r in reactors {
                    let who = r.sender_label.as_deref().unwrap_or("?");
                    out.push_str(&format!(
                        "- <span data-section-uuid=\"{uuid}\">{emoji} {who}</span>\n",
                        uuid = r.event_uuid,
                    ));
                }
            }
        }
        out.push_str("\n</div>\n\n");
    }

    // Reactions whose target wasn't in this doc's messages (DM
    // hidden, race condition, or imported from a network where
    // the target wasn't ingested). Surface them so they're not
    // silently dropped.
    let known_targets: std::collections::HashSet<&str> = doc
        .messages
        .iter()
        .map(|m| m.native_event_id.as_str())
        .collect();
    let orphans: Vec<(&String, &Vec<Event>)> = doc
        .reactions_by_target
        .iter()
        .filter(|(t, _)| !known_targets.contains(t.as_str()))
        .collect();
    if !orphans.is_empty() {
        out.push_str("---\n\n## Reactions to messages outside this period\n\n");
        for (target, rs) in orphans {
            out.push_str(&format!("- target `{target}`:\n"));
            for r in rs {
                let emoji = r.reaction_emoji.as_deref().unwrap_or("?");
                let who = r.sender_label.as_deref().unwrap_or("?");
                out.push_str(&format!(
                    "  - {emoji} {who} ({})\n",
                    display_ts(r.timestamp_ms)
                ));
            }
        }
    }

    out
}

fn render_attachment_body(out: &mut String, m: &Event) {
    if let Some(caption) = m.text_content.as_deref().filter(|s| !s.is_empty()) {
        out.push('\n');
        out.push_str(caption);
        out.push('\n');
    }
    if m.blobs.is_empty() {
        out.push_str(&format!(
            "\n*[{}: no blob ingested]*\n",
            m.event_type.to_lowercase()
        ));
        return;
    }
    for b in &m.blobs {
        let rel = blob_relpath(&b.blob_id, b.content_type.as_deref(), &b.slot);
        let size = b
            .byte_len
            .map(human_bytes)
            .unwrap_or_else(|| "size unknown".to_string());
        let kind_marker = match m.event_type.as_str() {
            "IMAGE" => "🖼",
            "VIDEO" => "🎞",
            "AUDIO" | "VOICE" => "🔊",
            _ => "📎",
        };
        out.push('\n');
        if m.event_type == "IMAGE" && b.has_bytes {
            // Inline image syntax — the markdown previewer will
            // render it when bytes are on disk.
            out.push_str(&format!("![{}]({})\n", b.slot, rel));
        } else {
            out.push_str(&format!("{kind_marker} [{}]({}) — {}\n", b.slot, rel, size));
        }
        if !b.has_bytes {
            out.push_str(&format!(
                "*(blob bytes missing; source url: {})*\n",
                b.source_url.as_deref().unwrap_or("?")
            ));
        }
    }
}

/// Pull a short human label out of a HIDDEN event so the
/// one-liner renderer can say something more useful than just
/// "hidden". Beeper-internal types live under `extra.eventType`
/// in the message JSON, but we don't have that handy at translate
/// time — fall back to the sender or text_content when available.
fn hidden_summary(m: &Event) -> String {
    if let Some(text) = m.text_content.as_deref().filter(|s| !s.is_empty()) {
        let preview: String = text.chars().take(60).collect();
        return preview;
    }
    if let Some(label) = m.sender_label.as_deref() {
        return format!("from {label}");
    }
    "(no body)".to_string()
}

fn yaml_safe(s: &str) -> String {
    // Quote when ambiguity-prone characters appear. Not a full
    // YAML escape, but covers the common chat-title cases.
    if s.chars()
        .any(|c| matches!(c, ':' | '#' | '@' | '"' | '\'' | '\n'))
    {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn human_bytes(n: i64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{} B", n as i64)
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KiB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MiB", n / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n / (1024.0 * 1024.0 * 1024.0))
    }
}

/// RFC-3339 timestamp with millisecond precision. Used for
/// machine-facing surfaces (`grid_rows.when_ts`, frontmatter
/// `first_ts` / `last_ts`) so cross-provider sorts / range
/// queries behave consistently with the other providers' index
/// rows.
fn iso_from_ms(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_else(|| format!("@{ms}ms"))
}

/// Human-friendly timestamp for rendering inside the markdown
/// body (section headers, hidden-event one-liners). Easier to
/// skim than a full RFC-3339 string when a person is reading the
/// transcript.
fn display_ts(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("@{ms}ms"))
}

// ─────────────────────────────────────────────────────────────────────
// Blob materialization
// ─────────────────────────────────────────────────────────────────────

fn blob_relpath(blob_id: &str, content_type: Option<&str>, slot: &str) -> String {
    let ext = content_type
        .and_then(content_type_to_ext)
        .or_else(|| {
            slot.rsplit('.')
                .next()
                .filter(|s| !s.contains(' '))
                .map(str::to_string)
        })
        .unwrap_or_else(|| "bin".to_string());
    let safe_blob = blob_id.replace(['/', ':'], "_");
    format!("blobs/{safe_blob}.{ext}")
}

fn content_type_to_ext(ct: &str) -> Option<String> {
    let ct = ct.split(';').next().unwrap_or(ct).trim();
    Some(
        match ct {
            "image/jpeg" => "jpg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "video/mp4" => "mp4",
            "video/webm" => "webm",
            "audio/mpeg" => "mp3",
            "audio/mp4" => "m4a",
            "audio/wav" => "wav",
            "audio/ogg" => "ogg",
            _ => return None,
        }
        .to_string(),
    )
}

/// Stream each blob's bytes from the raw doltlite into a file
/// under `blobs/<blob-id>.<ext>`. Blobs without bytes (extract
/// failed to fetch them, or `--no-media` was set) are skipped —
/// the markdown links them but the file just won't exist.
fn materialize_blobs(raw_db_path: &Path, doc: &DocBucket, blobs_dir: &Path) -> Result<usize> {
    // Collect the blob ids referenced by this doc's messages. We
    // pull every event's blobs through `parse.rs`, so the doc
    // already has them in memory — but the BYTES are still in
    // doltlite. Re-open the raw db and SELECT bytes by id.
    let mut needed: Vec<(String, String)> = Vec::new(); // (id, slot)
    for m in &doc.messages {
        for b in &m.blobs {
            if b.has_bytes {
                needed.push((b.blob_id.clone(), b.slot.clone()));
            }
        }
    }
    if needed.is_empty() {
        return Ok(0);
    }
    fs::create_dir_all(blobs_dir).with_context(|| format!("mkdir -p {}", blobs_dir.display()))?;

    // Same rationale as parse_async: bridge sync-Rust into async
    // sqlx by borrowing the existing runtime rather than spawning
    // a new one (which would panic inside the sync orchestrator's
    // tokio context).
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let opts =
                SqliteConnectOptions::from_str(&format!("sqlite://{}", raw_db_path.display()))?
                    .read_only(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(opts)
                .await
                .with_context(|| format!("open doltlite for blobs at {}", raw_db_path.display()))?;
            let mut written = 0usize;
            for (id, slot) in &needed {
                let row = sqlx::query("SELECT bytes, content_type FROM blobs WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&pool)
                    .await?;
                let Some(r) = row else { continue };
                let bytes: Vec<u8> = match r.try_get::<Option<Vec<u8>>, _>("bytes")? {
                    Some(b) => b,
                    None => continue,
                };
                let ct: Option<String> = r.try_get("content_type")?;
                let rel = blob_relpath(id, ct.as_deref(), slot);
                // rel is "blobs/<safe>.<ext>" — write under page_dir's
                // parent of blobs_dir. blobs_dir's parent is the page
                // dir, and rel starts with "blobs/" so we strip that.
                let filename = rel.strip_prefix("blobs/").unwrap_or(&rel);
                let path = blobs_dir.join(filename);
                fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
                written += 1;
            }
            Ok::<usize, anyhow::Error>(written)
        })
    })
    .map_err(|e| anyhow::anyhow!("{e:#}"))
}

// ─────────────────────────────────────────────────────────────────────
// GridRow
// ─────────────────────────────────────────────────────────────────────

fn build_grid_rows(
    room: &Room,
    doc: &DocBucket,
    document_uuid: &str,
    md_rel: &Path,
) -> Vec<GridRow> {
    let qmd_path = Some(md_rel.display().to_string());
    let entire_chat = format!("/beeper/{}/{}", room.network, room.room_uuid);
    let conversation_name = room.title.clone().or_else(|| room.external_room_id.clone());
    // Composite `source_label` carries both routing layers:
    //   "Beeper:Signal", "Beeper:Google Chat", "Beeper:WhatsApp", …
    // Downstream queries can do `LIKE 'Beeper:%'` to pull
    // everything that came through this provider, or `LIKE
    // '%:Signal'` to grab Signal regardless of which extractor
    // delivered it (e.g. once a future direct-megabridge reader
    // lands, its rows would carry `source_label = "Signal"` and
    // sort cleanly alongside ours).
    let source_label = format!("Beeper:{}", network_label(&room.network));

    let mut rows: Vec<GridRow> = Vec::with_capacity(doc.messages.len() + 1);

    // One "conversation" header row per doc.
    rows.push(GridRow {
        uuid: document_uuid.to_string(),
        provider: "beeper".into(),
        kind: kind_for_conversation(&room.network),
        source_label: source_label.clone(),
        when_ts: iso_from_ms(doc.first_ms),
        author: None,
        account: room.account_id.clone(),
        project: room.external_workspace_id.clone(),
        channel: conversation_name.clone(),
        conversation_name: conversation_name.clone(),
        conversation_uuid: room.room_uuid.clone(),
        message_index: None,
        entire_chat: entire_chat.clone(),
        text: doc
            .messages
            .iter()
            // The chat-header row's `text` field is what
            // full-text-search indexes. HIDDEN events (encryption
            // setup, membership churn) carry no human signal, so
            // we keep them OUT of the concatenated search text
            // even though they DO get their own `… Hidden` row.
            .filter(|m| !m.is_hidden())
            .filter_map(|m| m.text_content.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        slack_link: None,
        qmd_path: qmd_path.clone(),
        source_url: None,
        git_sha: None,
        external_id: room.external_room_id.clone(),
        notion_page_uuid: None,
        notion_block_uuid: None,
        document_uuid: Some(document_uuid.to_string()),
    });

    for (idx, m) in doc.messages.iter().enumerate() {
        rows.push(GridRow {
            uuid: m.event_uuid.clone(),
            provider: "beeper".into(),
            kind: kind_for_message(&room.network, &m.event_type),
            source_label: source_label.clone(),
            when_ts: iso_from_ms(m.timestamp_ms),
            author: m.sender_label.clone(),
            account: room.account_id.clone(),
            project: room.external_workspace_id.clone(),
            channel: conversation_name.clone(),
            conversation_name: conversation_name.clone(),
            conversation_uuid: room.room_uuid.clone(),
            message_index: Some(idx as i64),
            entire_chat: entire_chat.clone(),
            text: m.text_content.clone().unwrap_or_default(),
            slack_link: None,
            qmd_path: qmd_path.clone(),
            source_url: m.blobs.first().and_then(|b| b.source_url.clone()),
            git_sha: None,
            external_id: m.external_event_id.clone(),
            notion_page_uuid: None,
            notion_block_uuid: None,
            document_uuid: Some(document_uuid.to_string()),
        });
    }

    // Reactions get their own rows so search can find them.
    for (target, rs) in &doc.reactions_by_target {
        for r in rs {
            // Stable rowuuid for reactions: their own event_uuid
            // already collapses sender+target+emoji on the source
            // side (see megabridge enrichment), so we re-use it.
            let _ = target;
            let _ = beeper_event_uuid; // imported for future use
            rows.push(GridRow {
                uuid: r.event_uuid.clone(),
                provider: "beeper".into(),
                kind: format!("{} Reaction", network_label(&room.network)),
                source_label: source_label.clone(),
                when_ts: iso_from_ms(r.timestamp_ms),
                author: r.sender_label.clone(),
                account: room.account_id.clone(),
                project: room.external_workspace_id.clone(),
                channel: conversation_name.clone(),
                conversation_name: conversation_name.clone(),
                conversation_uuid: room.room_uuid.clone(),
                message_index: None,
                entire_chat: entire_chat.clone(),
                text: r.reaction_emoji.clone().unwrap_or_default(),
                slack_link: None,
                qmd_path: qmd_path.clone(),
                source_url: None,
                git_sha: None,
                external_id: r.external_event_id.clone(),
                notion_page_uuid: None,
                notion_block_uuid: None,
                document_uuid: Some(document_uuid.to_string()),
            });
        }
    }

    rows
}

fn kind_for_conversation(network: &str) -> String {
    format!("{} Chat", network_label(network))
}

fn kind_for_message(network: &str, ev_type: &str) -> String {
    let label = network_label(network);
    match ev_type {
        "TEXT" | "NOTICE" => format!("{label} Message"),
        "IMAGE" => format!("{label} Image"),
        "VIDEO" => format!("{label} Video"),
        "FILE" => format!("{label} File"),
        "AUDIO" | "VOICE" => format!("{label} Audio"),
        "MEMBERSHIP" => format!("{label} Membership"),
        // Distinct `Hidden` kind so consumers can filter cheaply
        // (downstream sees `kind = "Signal Hidden"` etc.). Same
        // pattern as MEMBERSHIP — taxonomy parity matters for
        // search facets.
        "HIDDEN" => format!("{label} Hidden"),
        other => format!("{label} {other}"),
    }
}

fn network_label(network: &str) -> &str {
    match network {
        "signal" => "Signal",
        "googlechat" => "Google Chat",
        "slack" => "Slack",
        "whatsapp" => "WhatsApp",
        "imessage" => "iMessage",
        "telegram" => "Telegram",
        "discord" => "Discord",
        "linkedin" => "LinkedIn",
        "twitter" => "Twitter",
        "instagram" => "Instagram",
        "facebook" => "Facebook",
        "sms" => "SMS",
        other => other,
    }
}
