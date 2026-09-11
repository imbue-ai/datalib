//! Signal's render stage: normalize, then hand off to `chat-common`.
//!
//! Everything about *how* a Signal message looks now lives in
//! `datalib_etl_chat_common`. What is left here is the stage plumbing —
//! the progress accounting parse's skip-load needs, and the render
//! cursor.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::periodize::Period;
use datalib_etl::progress::Progress;
use datalib_etl::render_cursor;
use datalib_etl_chat_common::render::ENTITY_KIND_CONVERSATION;
use datalib_etl_chat_common::{RenderProfile, RenderSummary as ChatSummary};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_schema::providers::Provider;

use super::normalize::to_chats;
use super::parse::ParsedSignal;

/// Bump when Signal's own contribution to the rendered output changes.
/// The shared layout has its own number — see
/// `datalib_etl_chat_common::LAYOUT_VERSION`, which is folded into every
/// fingerprint alongside this one.
pub const RENDER_VERSION: u32 = 5;

const SOURCE_LABEL: &str = "Signal";
const PROVIDER: Provider = Provider::Signal;

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub docs_total: usize,
    pub docs_rendered: usize,
    pub docs_skipped: usize,
    pub messages_rendered: usize,
}

/// The render params recorded alongside the cursor. `period` decides
/// how messages bucket into documents, so changing it invalidates every
/// document the previous run wrote — see
/// [`datalib_etl::render_cursor::read_for_params`].
pub fn render_params(period: Period) -> serde_json::Value {
    serde_json::json!({ "period": period.as_config_str() })
}

pub fn profile() -> RenderProfile {
    RenderProfile {
        when_ts_precision: datalib_etl_chat_common::WhenTsPrecision::Seconds,
        provider: PROVIDER,
        source_label: SOURCE_LABEL.to_string(),
        chat_kind: "Signal Chat".to_string(),
        message_kind: "Signal Message".to_string(),
        // Signal Android backups carry reactions, but this provider's
        // parse does not read them yet, so nothing is ever tagged with
        // this. Named for when it does.
        reaction_kind: "Signal Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
    }
}

pub fn render_all(
    parsed: &ParsedSignal,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    render_params: &serde_json::Value,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    // Log how long the dolt_diff scan took. Logged on every render
    // (including cold start with `None`) so the timing shows up in
    // sync output without the user having to crack the cursor open.
    tracing::info!(
        source = source_id,
        scan_elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64),
        changed_chats = parsed
            .scan
            .changed_chats
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        cold_start = parsed.scan.changed_chats.is_none(),
        "[render] signal dolt_diff scan"
    );

    // The parse-side skip-load has already filtered out unchanged
    // buckets; report them all up front so the progress bar accounts
    // for them too.
    progress.set_length(Some((parsed.docs.len() + parsed.docs_skipped) as u64));
    progress.inc(parsed.docs_skipped as u64);

    let (chats, blobs_by_chat) = to_chats(parsed, source_id);
    // Empty on purpose. chat-common skips a document whose fingerprint
    // is unchanged, and parse has *already* made that decision from
    // `dolt_diff` — a bucket reaching here is one we have committed to
    // writing. Handing over a real map would skip it a second time on
    // the wrong evidence.
    let prior_fingerprints = HashMap::new();

    let ChatSummary {
        docs_rendered,
        items_rendered,
        ..
    } = datalib_etl_chat_common::render_all(
        &profile(),
        &chats,
        out_dir,
        source_id,
        &blobs_by_chat,
        progress,
        &prior_fingerprints,
        on_doc_complete,
    )?;

    // Advance the render cursor only when:
    //   * every doc rendered without error (we're here, so true), AND
    //   * we managed to read HEAD at scan time.
    // A missing HEAD (stock libsqlite3 / non-doltlite db) leaves the
    // cursor unwritten — next run is another cold start, which is the
    // right behavior since we have no way to anchor the diff.
    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(out_dir, source_id);
        render_cursor::write(&cursor_path, head, render_params)
            .with_context(|| format!("write signal render cursor {}", cursor_path.display()))?;
    }

    Ok(RenderSummary {
        docs_total: parsed.docs.len() + parsed.docs_skipped,
        docs_rendered,
        docs_skipped: parsed.docs_skipped,
        messages_rendered: items_rendered,
    })
}
