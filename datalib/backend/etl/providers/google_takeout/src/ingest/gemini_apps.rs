//! `My Activity/Gemini Apps/MyActivity.html` walker.

use datalib_etl::fsscan;
use datalib_etl::prune;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{blake3_hex, CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::file_checkpoint;
use datalib_etl::progress::Progress;
use datalib_problems::Reason;
use datalib_time::IsoOffsetTimestamp;
use serde_json::json;

use super::db::RawDb;
use super::mdl_html;
use super::schema_raw::{ns_id, GeminiActivityRow, GeminiAttachmentRow};
use super::time as time_parser;
use datalib_etl::doltlite_raw::WirePayload;

const FILE_REL: &str = "My Activity/Gemini Apps/MyActivity.html";
const SCOPE: &str = "google_takeout/gemini_apps";

#[derive(Debug, Default, Clone)]
pub struct GeminiSummary {
    pub activity: usize,
    pub attachments: usize,
    pub blobs_stored: usize,
    /// Activity the file no longer lists, deleted with its attachment edges.
    pub removed: usize,
}

pub async fn ingest(db: &RawDb, scan: &fsscan::Scan, progress: &Progress) -> Result<GeminiSummary> {
    let Some(f) = scan.file(FILE_REL) else {
        return Ok(GeminiSummary::default());
    };
    let cell_dir = f
        .path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let prev = file_checkpoint::load_cursor(db.pool(), SCOPE).await?;
    if prev.get(&f.rel) == Some(&f.blake3) {
        let blobs_stored = retry_unfetched_attachments(db, &cell_dir).await?;
        return Ok(GeminiSummary {
            blobs_stored,
            ..GeminiSummary::default()
        });
    }
    let html =
        std::fs::read_to_string(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    let mut acc = CasEdgeAccumulator::new();
    let mut rows: Vec<GeminiActivityRow> = Vec::new();
    let mut n_attachments: usize = 0;

    let cells: Vec<&str> = mdl_html::iter_cells(&html).collect();
    if cells.is_empty() {
        return Err(super::unknown_layout(FILE_REL, "holds no activity cells"));
    }
    for cell in cells {
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
            // Exact join, deliberately — NOT Chat's truncation-tolerant
            // `attachment_path::resolve`. This `file_name` comes from an
            // `href` Google wrote into the export HTML, pointing at the
            // file it actually wrote, so there is no full-vs-truncated
            // mismatch to bridge. Unverified against an export with a
            // filename long enough to trigger the cap: if missing Gemini
            // attachments show up, re-examine this line first.
            attach(&mut acc, &cell_dir, &id, &file_name);
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

    // The attachments land before the file is stamped: a flush that fails
    // leaves the file to be read again.
    let blobs_stored = flush(db, &mut acc).await?;

    // The file is the whole activity log, so what it no longer lists is gone.
    let keep: HashSet<String> = rows.iter().map(|r| r.id_and_payload.id.clone()).collect();
    let now = IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.context("begin gemini_apps tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    let gone = prune::prune_scope_in_tx(&mut tx, "gemini_activity", &[], &keep).await?;
    prune::delete_owned_in_tx(&mut tx, "gemini_attachments", "activity_id", &gone).await?;
    file_checkpoint::record_file(&mut tx, SCOPE, f).await?;
    tx.commit().await.context("commit gemini_apps tx")?;
    prune::record("gemini_activity", keep.len() + gone.len(), gone.len());

    Ok(GeminiSummary {
        activity: n_activity,
        attachments: n_attachments,
        blobs_stored,
        removed: gone.len(),
    })
}

fn attach(acc: &mut CasEdgeAccumulator, cell_dir: &Path, owning: &str, file_name: &str) {
    let sibling = cell_dir.join(file_name);
    match std::fs::read(&sibling) {
        Ok(bytes) => {
            let ct = guess_content_type(&sibling);
            acc.add_fetched(owning, file_name, bytes, ct, Some(file_name.to_string()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => acc.add_skipped(
            owning,
            file_name,
            Reason::NotFound,
            format!("{file_name} is not in the export"),
        ),
        Err(e) => acc.add_failed(
            owning,
            file_name,
            format!("read {}: {e}", sibling.display()),
        ),
    }
}

/// Returns how many blobs it stored.
async fn flush(db: &RawDb, acc: &mut CasEdgeAccumulator) -> Result<usize> {
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
    Ok(blobs_stored)
}

/// An unchanged activity file is not read again, so this is the only
/// retry an attachment that did not read gets.
async fn retry_unfetched_attachments(db: &RawDb, cell_dir: &Path) -> Result<usize> {
    let unfetched: Vec<(String, String)> =
        sqlx::query_as("SELECT activity_id, filename FROM gemini_attachments WHERE blake3 IS NULL")
            .fetch_all(db.pool())
            .await
            .context("list gemini attachments with no bytes")?;
    if unfetched.is_empty() {
        return Ok(0);
    }
    let mut acc = CasEdgeAccumulator::new();
    for (activity_id, file_name) in &unfetched {
        attach(&mut acc, cell_dir, activity_id, file_name);
    }
    flush(db, &mut acc).await
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
    // a first pass; render gets the verbatim cell either way via
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
