//! `My Activity/Gemini Apps/MyActivity.html` walker.

use datalib_etl::prune;
use datalib_etl_files::fsscan;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{blake3_hex, CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::download_problems::SkippedRecord;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::file_checkpoint;
use datalib_problems::{Problem, Reason};
use datalib_time::IsoOffsetTimestamp;
use serde_json::json;
use url::Url;

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

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<GeminiSummary> {
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
    let mut skipped: Vec<SkippedRecord> = Vec::new();
    let mut n_attachments: usize = 0;

    let cells: Vec<&str> = mdl_html::iter_cells(&html).collect();
    if cells.is_empty() {
        return Err(super::unknown_layout(FILE_REL, "holds no activity cells"));
    }
    for &raw in &cells {
        let Some(cell) = parse_cell(raw) else {
            skipped.push(SkippedRecord {
                entry: raw.to_string(),
                problem: Problem::record(
                    Reason::FetchFailed,
                    &format!(
                        "an activity entry with no date line: {}",
                        mdl_html::strip_tags(raw)
                    ),
                ),
            });
            continue;
        };
        let id_seed = format!("{}\0{}", cell.prompt, cell.when);
        let id = ns_id(&format!("gemini:{}", blake3_hex(id_seed.as_bytes())));
        for file in cell.files() {
            n_attachments += 1;
            attach(&mut acc, &cell_dir, &id, file);
        }
        let payload = json!({
            "promptText": cell.prompt,
            "responseHtml": cell.response_html,
            "attachedFiles": cell
                .attached
                .iter()
                .map(|a| json!({"file": a.file, "name": a.name}))
                .collect::<Vec<_>>(),
            "generatedImages": cell.generated_images,
            "whenStr": cell.when,
        });
        rows.push(GeminiActivityRow {
            id_and_payload: WirePayload {
                id,
                payload: payload.to_string(),
            },
            when_ts: time_parser::parse_mdl_grid(&cell.when),
        });
    }
    super::require_some_read(FILE_REL, cells.len(), rows.len())?;
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
    found.skipped("gemini_apps", skipped);

    Ok(GeminiSummary {
        activity: n_activity,
        attachments: n_attachments,
        blobs_stored,
        removed: gone.len(),
    })
}

/// One activity entry as Google lays it out. The left body cell is the
/// prompt's lines (`Prompted …`), an optional `N generated image.` line,
/// an optional `Attached N file.` line with a `-  <a href>` line per file,
/// the date, and then the response's HTML. The right cell holds a
/// preview `<img>` of an attached image.
#[derive(Debug, PartialEq)]
struct Cell {
    prompt: String,
    when: String,
    response_html: String,
    attached: Vec<Attached>,
    generated_images: Vec<String>,
    previews: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct Attached {
    /// The file's name in the export, as the link names it.
    file: String,
    /// The name it was uploaded under, as the link reads.
    name: String,
}

impl Cell {
    /// Every file in the export the entry names, each once.
    fn files(&self) -> Vec<&str> {
        let mut seen = HashSet::new();
        self.attached
            .iter()
            .map(|a| a.file.as_str())
            .chain(self.generated_images.iter().map(String::as_str))
            .chain(self.previews.iter().map(String::as_str))
            .filter(|f| seen.insert(*f))
            .collect()
    }
}

const BODY_CELL: &str = "mdl-typography--body-1\">";
const PREVIEW_CELL: &str = "mdl-typography--text-right\">";

/// `None` for an entry with no date line, the one line every entry has.
fn parse_cell(cell: &str) -> Option<Cell> {
    let body = mdl_html::div_contents(cell, BODY_CELL)?;
    let mut head: Vec<&str> = Vec::new();
    let mut rest = body;
    let (when, response) = loop {
        let (line, after) = rest.split_once("<br>")?;
        let text = mdl_html::strip_tags(line);
        if !head.is_empty() && time_parser::parse_mdl_grid(&text).is_some() {
            break (text, after);
        }
        head.push(line);
        rest = after;
    };

    let mut prompt_lines: Vec<String> = Vec::new();
    let mut attached = Vec::new();
    for line in head {
        let text = mdl_html::strip_tags(line);
        if text.starts_with('-') && line.contains("<a ") {
            attached.extend(
                mdl_html::iter_anchors(line)
                    .into_iter()
                    .filter_map(|(href, name)| {
                        Some(Attached {
                            file: local_file(&href)?,
                            name,
                        })
                    }),
            );
        } else if prompt_lines.is_empty() || !is_count_line(&text) {
            prompt_lines.push(text);
        }
    }
    if let Some(first) = prompt_lines.first_mut() {
        if let Some(prompt) = first.strip_prefix("Prompted ") {
            *first = prompt.to_string();
        }
    }

    let response_html = response.trim().trim_end_matches("<br>").trim().to_string();
    let generated_images = img_files(&response_html);
    let previews = mdl_html::div_contents(cell, PREVIEW_CELL)
        .map(img_files)
        .unwrap_or_default();
    Some(Cell {
        prompt: prompt_lines.join("\n"),
        when,
        response_html,
        attached,
        generated_images,
        previews,
    })
}

/// `1 generated image.`, `Attached 2 files.`: what follows the prompt,
/// counted.
fn is_count_line(text: &str) -> bool {
    let counted = |rest: &str, singular: &str, plural: &str| {
        rest.split_once(' ').is_some_and(|(n, what)| {
            n.parse::<u32>().is_ok() && (what == singular || what == plural)
        })
    };
    counted(text, "generated image.", "generated images.")
        || text
            .strip_prefix("Attached ")
            .is_some_and(|rest| counted(rest, "file.", "files."))
}

fn img_files(html: &str) -> Vec<String> {
    mdl_html::iter_img_srcs(html)
        .iter()
        .filter_map(|src| local_file(src))
        .collect()
}

/// The export file a link points at, its percent-escapes decoded —
/// Google writes `Prime%20Directive.pdf` for `Prime Directive.pdf` — or `None`
/// for a link off the page.
fn local_file(href: &str) -> Option<String> {
    if href.starts_with('#') {
        return None;
    }
    let base = Url::parse("file:///export/").expect("a valid base");
    let url = base.join(href).ok()?;
    if url.scheme() != "file" || url.path().ends_with('/') {
        return None;
    }
    let path = url.to_file_path().ok()?;
    Some(path.file_name()?.to_str()?.to_string())
}

fn attach(acc: &mut CasEdgeAccumulator, cell_dir: &Path, owning: &str, file_name: &str) {
    let Some(path) = on_disk(cell_dir, file_name) else {
        acc.add_skipped(
            owning,
            file_name,
            Reason::NotFound,
            format!("{file_name} is not in the export"),
        );
        return;
    };
    match std::fs::read(&path) {
        Ok(bytes) => {
            let ct = guess_content_type(Path::new(file_name));
            acc.add_fetched(owning, file_name, bytes, ct);
        }
        Err(e) => acc.add_failed(owning, file_name, format!("read {}: {e}", path.display())),
    }
}

/// Where the file a page names is. Google has written a generated image
/// under another extension than the one its page names it by (`….jpeg`
/// on the page, `….png` on disk, JPEG inside), so a name with no file of
/// its own takes the one file that shares its stem.
fn on_disk(dir: &Path, file_name: &str) -> Option<PathBuf> {
    let exact = dir.join(file_name);
    if exact.is_file() {
        return Some(exact);
    }
    let stem = Path::new(file_name).file_stem()?;
    let mut same_stem = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_stem() == Some(stem) && p.is_file());
    let only = same_stem.next()?;
    if same_stem.next().is_some() {
        return None;
    }
    tracing::info!(
        named = file_name,
        found = %only.display(),
        "a Gemini file is on disk under another extension than its page names",
    );
    Some(only)
}

/// Returns how many blobs it stored.
async fn flush(db: &RawDb, acc: &mut CasEdgeAccumulator) -> Result<usize> {
    let blobs_stored = acc.fetched_len();
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

fn guess_content_type(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let ct = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => return None,
    };
    Some(ct.to_string())
}
