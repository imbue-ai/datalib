//! `My Activity/Gemini Apps/MyActivity.html` walker.
//!
//! Same MDL `outer-cell` shape as YouTube watch-history, but the
//! cell carries `prompt_text`, `response_html`, and `attached_files`
//! references to sibling files in the same directory. PK recipe:
//! `uuidv5(NS, "gemini:" + blake3_hex(prompt + "\0" + when_str))`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{blake3_hex, CasEdgeAccumulator, CasEdgeRow as _};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::json;
use tracing::warn;

use super::db::RawDb;
use super::mdl_html;
use super::schema_raw::{ns_id, GeminiActivityRow, GeminiAttachmentRow};
use super::time as time_parser;
use frankweiler_etl::doltlite_raw::WirePayload;

const FILE_REL: &str = "My Activity/Gemini Apps/MyActivity.html";
const SCOPE: &str = "google_takeout/gemini_apps";

#[derive(Debug, Default, Clone)]
pub struct GeminiSummary {
    pub activity: usize,
    pub attachments: usize,
    pub blobs_stored: usize,
}

pub async fn ingest(db: &RawDb, root: &Path, progress: &Progress) -> Result<GeminiSummary> {
    let path = root.join(FILE_REL);
    if !path.exists() {
        return Ok(GeminiSummary::default());
    }
    let fp = FileFingerprint::of(&path)?;
    let stamped = file_checkpoint::load(db.pool(), SCOPE).await?;
    if file_checkpoint::should_skip(&stamped, &fp) {
        return Ok(GeminiSummary::default());
    }
    let html =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let cell_dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut acc = CasEdgeAccumulator::new();
    let mut rows: Vec<GeminiActivityRow> = Vec::new();
    let mut n_attachments: usize = 0;

    for cell in mdl_html::iter_cells(&html) {
        let text = mdl_html::strip_tags(cell);
        let when_str = mdl_html::last_timestamp_chunk(cell);
        let when_ts = when_str.as_deref().and_then(time_parser::parse_mdl_grid);
        // Prompt text: heuristic — the first paragraph of the body
        // cell. Fall back to a stripped-tag prefix.
        let prompt_text = extract_prompt_text(cell).unwrap_or_else(|| text.clone());
        let response_html = extract_response_html(cell).unwrap_or_default();
        let anchors = mdl_html::iter_anchors(cell);
        let attached: Vec<(String, String)> = anchors
            .iter()
            .filter(|(href, _)| is_local_attachment(href))
            .cloned()
            .collect();
        let id_seed = format!("{prompt_text}\0{}", when_str.clone().unwrap_or_default());
        let id = ns_id(&format!("gemini:{}", blake3_hex(id_seed.as_bytes())));
        let payload = json!({
            "promptText": prompt_text,
            "responseHtml": response_html,
            "attachedFiles": attached
                .iter()
                .map(|(href, name)| json!({"href": href, "name": name}))
                .collect::<Vec<_>>(),
            "whenStr": when_str,
        });
        // Attachments: try to read each referenced sibling file.
        for (href, _name) in &attached {
            let file_name = href
                .rsplit('/')
                .next()
                .unwrap_or(href)
                .split('?')
                .next()
                .unwrap_or(href)
                .to_string();
            if file_name.is_empty() {
                continue;
            }
            n_attachments += 1;
            let sibling = cell_dir.join(&file_name);
            match std::fs::read(&sibling) {
                Ok(bytes) => {
                    let ct = guess_content_type(&sibling);
                    acc.add_fetched(&id, &file_name, bytes, ct, Some(file_name.clone()));
                }
                Err(e) => {
                    warn!(
                        event = "gemini_attachment_missing",
                        activity_id = %id,
                        file_name = %file_name,
                        error = %e,
                    );
                    acc.add_failed(&id, &file_name, "attachment file missing on disk");
                }
            }
        }
        rows.push(GeminiActivityRow {
            id_and_payload: WirePayload {
                id,
                payload: payload.to_string(),
            },
            when_ts,
        });
    }
    let n_activity = rows.len();
    progress.set_message(&format!("gemini: {n_activity} entries"));

    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await.context("begin gemini_apps tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    file_checkpoint::record_finished(&mut tx, SCOPE, &fp).await?;
    tx.commit().await.context("commit gemini_apps tx")?;

    let blobs_stored = acc.bundle_mut().cas_inserts().len();
    acc.flush(db.pool(), db.cas(), |owning, ref_id, blake3| {
        GeminiAttachmentRow {
            id: GeminiAttachmentRow::pk_recipe(owning, ref_id),
            activity_id: owning.to_string(),
            filename: ref_id.to_string(),
            blake3: blake3.map(str::to_string),
        }
    })
    .await?;

    Ok(GeminiSummary {
        activity: n_activity,
        attachments: n_attachments,
        blobs_stored,
    })
}

fn extract_prompt_text(cell: &str) -> Option<String> {
    // Look for the first `Prompted ` / `You said ` style preamble
    // (Google has flip-flopped on phrasing) — fall back to the
    // first stripped paragraph.
    let stripped = mdl_html::strip_tags(cell);
    let candidate = stripped.trim();
    // Heuristic: chop at " Response" if present so we don't fold
    // the response text into the prompt.
    let cut = candidate
        .find(" Response")
        .or_else(|| candidate.find(" Jun "))
        .or_else(|| candidate.find(" Jan "));
    let trimmed = match cut {
        Some(i) => &candidate[..i],
        None => candidate,
    };
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn extract_response_html(cell: &str) -> Option<String> {
    // The response body sits inside a `<p>` (or larger) block after
    // the prompt. Picking the second `<p>` onward is good enough for
    // a first pass; translate gets the verbatim cell either way via
    // the payload's `responseHtml`.
    let lower = cell;
    let key = "<p>";
    let mut found = lower.match_indices(key);
    let _first = found.next()?;
    let second = found.next()?;
    let start = second.0;
    let end_key = "</div>";
    let end = lower[start..]
        .find(end_key)
        .map(|i| start + i)
        .unwrap_or(lower.len());
    Some(lower[start..end].trim().to_string())
}

fn is_local_attachment(href: &str) -> bool {
    !href.starts_with("http://") && !href.starts_with("https://") && !href.starts_with('#')
}

fn guess_content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let ct = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => return None,
    };
    Some(ct.to_string())
}
