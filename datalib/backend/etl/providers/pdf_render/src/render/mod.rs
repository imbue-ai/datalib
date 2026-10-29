//! The `pdf` render side: convert each identified document to markdown
//! and emit it with its `grid_rows`.

pub mod convert;
pub mod grid_rows;

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use datalib_etl::blob_cas::blake3_hex;
use datalib_etl::progress::Progress;
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::grid_index::{doc_md_path, RenderedMarkdown};
use datalib_etl_render::html::{escape_html_outside_code, escape_md_inline};
use datalib_etl_render::inputs::{Bucket, Buckets, Input, RawRange};
use datalib_etl_render::section::{msg_div_open, MSG_DIV_CLOSE};
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};

pub use convert::RENDER_VERSION;
use datalib_etl_pdf::ingest::{RawDb, RenderTarget};

pub struct RenderSummary {
    pub converted: usize,
    pub failed: usize,
    /// Documents whose file no longer holds the bytes they are named by.
    /// Each is emitted as a stand-in: its document row, a page saying why
    /// its pages are missing, and a problem row.
    pub changed: usize,
    /// The documents whose conversion failed, by blake3. Their pages are
    /// stale rather than gone.
    pub failed_blake3s: std::collections::HashSet<String>,
    /// Each failure, for the processor to record as the document's
    /// problem and its bucket's failure.
    pub failures: Vec<Failure>,
}

pub struct Failure {
    pub blake3: String,
    pub doc_uuid: String,
    pub error: String,
}

/// Load the work list. Split from [`render_targets`] so the async
/// database work finishes before the non-`Send` document sink enters
/// scope — otherwise the whole render future is non-`Send` and cannot
/// be driven by the `#[async_trait]` processor.
/// **`None` means the store could not be read**, which is not the same as
/// a corpus with nothing in it.
pub async fn load_targets(raw_dir: &Path) -> Result<Option<Vec<RenderTarget>>> {
    Ok(load(raw_dir, RawRange::cold()).await?.map(|l| l.targets))
}

/// The corpus at one commit, with what the diff since the cursor named.
pub struct Loaded {
    pub targets: Vec<RenderTarget>,
    /// The `pdf_scan_meta` row every document's absolute path comes from.
    pub scan_meta_id: Option<String>,
    pub scan: PdfScan,
}

/// One open of the store for the whole pass: the documents to convert
/// and the diff, at one commit — the driver's when it pinned one.
/// `None` means the store could not be read, which is not the same as
/// a corpus with nothing in it.
pub async fn load(raw_dir: &Path, range: RawRange<'_>) -> Result<Option<Loaded>> {
    let db_path = datalib_etl_pdf::ingest::db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(None);
    }
    let Some(db) = RawDb::open_reader(&db_path, range.pin).await? else {
        return Ok(None);
    };
    let loaded = async {
        let (scan_meta_id, targets) = match db.scan_root().await? {
            Some((id, root)) => (Some(id), db.convertible_documents(&root).await?),
            // No scan has run against this store yet.
            None => (None, Vec::new()),
        };
        let scan = scan_changed(&db, range).await?;
        Ok::<_, anyhow::Error>(Loaded {
            targets,
            scan_meta_id,
            scan,
        })
    }
    .await;
    // Closed before returning, on the error path too: the next open of
    // this store is a second connection until this one is gone.
    db.close().await;
    loaded.map(Some)
}

/// What a document reads: its own row, every path that holds its bytes,
/// and the scan row its absolute path is rooted at.
pub fn inputs_of(t: &RenderTarget, scan_meta_id: Option<&str>) -> Vec<Input> {
    let mut inputs = vec![Input::new("pdf_documents", &t.blake3)];
    inputs.extend(t.path_ids.iter().map(|id| Input::new("pdf_paths", id)));
    if let Some(id) = scan_meta_id {
        inputs.push(Input::new("pdf_scan_meta", id));
    }
    inputs
}

/// Every bucket a pass over `to_render` produces, for the processor to
/// declare: the ones looked at, with nothing, then the ones converted,
/// with what they read.
pub fn buckets_of(
    looked_at: Option<&std::collections::HashSet<String>>,
    converted: &[RenderTarget],
    scan_meta_id: Option<&str>,
) -> Buckets {
    looked_at
        .into_iter()
        .flatten()
        .map(|blake3| Bucket {
            key: blake3.clone(),
            inputs: Vec::new(),
        })
        .chain(converted.iter().map(|t| Bucket {
            key: t.blake3.clone(),
            inputs: inputs_of(t, scan_meta_id),
        }))
        .collect()
}

pub fn render_targets(
    targets: &[RenderTarget],
    data_root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        converted: 0,
        failed: 0,
        changed: 0,
        failed_blake3s: Default::default(),
        failures: Vec::new(),
    };
    if targets.is_empty() {
        return Ok(summary);
    }
    progress.set_length(Some(targets.len() as u64));
    let render_root = datalib_etl::layout::render_markdown_root(data_root, source_id);

    for t in targets {
        progress.inc(1);
        let doc_uuid = grid_rows::document(
            source_id,
            &t.blake3,
            grid_rows::document_stamp(t.doc_created_at.as_deref(), t.doc_modified_at.as_deref()),
        )
        .uuid;
        let md_path = doc_md_path(&render_root, &doc_uuid);
        let content = match read_document(t) {
            Ok(Some(bytes)) => convert::convert(&bytes, &t.abs_path).map(Content::Pages),
            Ok(None) => Ok(Content::FileChanged),
            Err(e) => Err(e),
        };
        let changed = matches!(content, Ok(Content::FileChanged));
        match content.and_then(|c| render_one(t, c, data_root, &md_path, source_id, &doc_uuid)) {
            Ok(rendered) => {
                if changed {
                    summary.changed += 1;
                } else {
                    summary.converted += 1;
                }
                on_doc_complete(rendered)?;
            }
            Err(e) => {
                // One malformed document must not abort a corpus scan;
                // the failure is the document's problem, recorded by
                // the processor, and a log line beside it.
                summary.failed += 1;
                summary.failed_blake3s.insert(t.blake3.clone());
                summary.failures.push(Failure {
                    blake3: t.blake3.clone(),
                    doc_uuid: doc_uuid.clone(),
                    error: format!("{e:#}"),
                });
                tracing::warn!(
                    path = %t.rel_path, blake3 = %t.blake3, error = %e,
                    "pdf_render_failed"
                );
            }
        }
    }
    Ok(summary)
}

/// Convenience wrapper: load then render. Used by tests and by any
/// caller that does not need the two phases apart.
pub async fn render(
    raw_dir: &Path,
    data_root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    // An unreadable store renders nothing. This wrapper does no deleting,
    // so unlike the processor's path it can treat the two alike.
    let targets = load_targets(raw_dir).await?.unwrap_or_default();
    render_targets(&targets, data_root, source_id, progress, on_doc_complete)
}

/// The document's bytes, or `None` when its file no longer holds them. The
/// document is its hash, so the markdown filed under it must come from
/// those bytes and no others.
fn read_document(t: &RenderTarget) -> Result<Option<Vec<u8>>> {
    let bytes = fs::read(&t.abs_path).with_context(|| format!("read {}", t.abs_path.display()))?;
    Ok((blake3_hex(&bytes) == t.blake3).then_some(bytes))
}

/// What a document's page holds.
enum Content {
    Pages(Vec<convert::Page>),
    /// Its file no longer holds the bytes it is named by, so there is
    /// nothing to convert. The page says so, and so does a problem row,
    /// until the next sync reads the file as it is now.
    FileChanged,
}

const FILE_CHANGED: &str = "This file has changed since the last sync read it, so its pages \
     are not shown. The next sync reads it as it is now.";

fn render_one(
    t: &RenderTarget,
    content: Content,
    data_root: &Path,
    md_path: &Path,
    source_id: &str,
    doc_uuid: &str,
) -> Result<RenderedMarkdown> {
    let title = grid_rows::display_title(t.title.as_deref(), &t.rel_path);

    let qmd_rel = md_path
        .strip_prefix(data_root)
        .unwrap_or(md_path)
        .to_string_lossy()
        .into_owned();

    let mut body = String::new();
    body.push_str("---\n");
    body.push_str(&format!("provider: {}\n", grid_rows::PROVIDER));
    body.push_str(&format!("blake3: {}\n", yaml_scalar(&t.blake3)));
    body.push_str(&format!("title: {}\n", yaml_scalar(&title)));
    if let Some(a) = &t.author {
        body.push_str(&format!("author: {}\n", yaml_scalar(a)));
    }
    body.push_str(&format!("page_count: {}\n", t.page_count));
    body.push_str(&format!("pdf_type: {}\n", yaml_scalar(&t.pdf_type)));
    body.push_str(&format!("source_path: {}\n", yaml_scalar(&t.rel_path)));
    if t.copy_count > 1 {
        body.push_str(&format!("copies: {}\n", t.copy_count));
    }
    if let Some(c) = &t.doc_created_at {
        body.push_str(&format!("created_at: {}\n", yaml_scalar(c)));
    }
    if let Some(m) = &t.doc_modified_at {
        body.push_str(&format!("modified_at: {}\n", yaml_scalar(m)));
    }
    body.push_str("---\n\n");
    let doc_stamp =
        grid_rows::document_stamp(t.doc_created_at.as_deref(), t.doc_modified_at.as_deref());
    let page_uuid = |number| grid_rows::page(source_id, &t.blake3, number, doc_stamp).uuid;
    let (page_rows, problems) = match content {
        Content::Pages(pages) => {
            let (pages_md, page_rows) = pages_markdown(&title, &pages, page_uuid);
            body.push_str(&pages_md);
            (page_rows, Vec::new())
        }
        Content::FileChanged => {
            body.push_str(&format!(
                "# {}

*{FILE_CHANGED}*
",
                escape_md_inline(&title)
            ));
            let problem = ProblemRow::new(
                source_id,
                Stage::Render,
                Scope::Markdown(doc_uuid),
                Some(doc_uuid),
                Outcome::Nulled,
                Problem::record(
                    Reason::RenderFailed,
                    &format!("{}: {FILE_CHANGED}", t.rel_path),
                ),
                Some(RENDER_VERSION),
            );
            (Vec::new(), vec![problem])
        }
    };

    if let Some(parent) = md_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(md_path, &body).with_context(|| format!("write {}", md_path.display()))?;

    let meta = grid_rows::DocumentMeta {
        blake3: &t.blake3,
        abs_path: &t.abs_path,
        title: t.title.as_deref(),
        author: t.author.as_deref(),
        rel_path: &t.rel_path,
        copy_count: t.copy_count,
        created_at: t.doc_created_at.as_deref(),
        modified_at: t.doc_modified_at.as_deref(),
        qmd_path: Some(&qmd_rel),
    };
    let rows = grid_rows::rows_for_document(source_id, &meta, &page_rows);

    Ok(RenderedMarkdown {
        markdown_uuid: doc_uuid.to_string(),
        source_id: source_id.to_string(),
        upstream_cursor: None,
        // The bucket is the raw row, not the document: the id carries a
        // stamp the diff cannot see.
        bucket_key: Some(t.blake3.clone()),
        md_path: md_path.to_path_buf(),
        render_version: RENDER_VERSION,
        rows,
        sections: Vec::new(),
        edges: Vec::new(),
        contacts: Vec::new(),
        problems,
    })
}

/// The document under its title: one section per readable page, and the
/// note for a page that could not be read. Returns the markdown and each
/// readable page's number and text, for its grid row.
fn pages_markdown(
    title: &str,
    pages: &[convert::Page],
    page_uuid: impl Fn(u32) -> String,
) -> (String, Vec<(u32, String)>) {
    let mut body = format!("# {}\n\n", escape_md_inline(title));
    let mut page_rows: Vec<(u32, String)> = Vec::with_capacity(pages.len());
    for p in pages {
        if p.non_textual {
            // A page we could not read. It goes in the markdown so the
            // gap is visible to whoever opens the document, but carries
            // no section wrapper and no grid row: there is no content to
            // navigate to, and the note is the same sentence on every
            // such page in every document. See `convert::note_for_page`.
            body.push_str(&p.text);
            body.push_str("\n\n");
            continue;
        }
        // Per-page section wrapper. The `data-section-uuid` must be
        // byte-equal to the page grid row's `uuid` or row→preview
        // navigation silently fails (see `etl::section` docs).
        body.push_str(&msg_div_open(&page_uuid(p.number), grid_rows::PROVIDER));
        body.push('\n');
        // pdf-inspector writes the page as markdown but leaves the
        // words in it as the PDF had them.
        body.push_str(&escape_html_outside_code(&p.text));
        body.push('\n');
        body.push_str(MSG_DIV_CLOSE);
        body.push_str("\n\n");
        page_rows.push((p.number, p.text.clone()));
    }
    (body, page_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PDF's title and its words are text: a `<` in a technical manual
    /// shows as one, and only code keeps its characters as they are.
    #[test]
    fn a_document_in_markup_renders_escaped() {
        let pages = [convert::Page {
            number: 1,
            text: "<script>x</script> & co\n\n`<kbd>`".to_string(),
            non_textual: false,
        }];
        let (md, rows) = pages_markdown("<script>x</script> & co", &pages, |n| format!("p{n}"));
        assert!(
            md.starts_with("# &lt;script&gt;x&lt;/script&gt; &amp; co\n\n"),
            "{md}"
        );
        assert!(
            md.contains("\n&lt;script&gt;x&lt;/script&gt; &amp; co\n\n`<kbd>`\n"),
            "{md}"
        );
        assert!(!md.contains("<script>"), "{md}");
        assert_eq!(
            rows,
            [(1, pages[0].text.clone())],
            "the grid keeps the words"
        );
    }
}

/// What the `dolt_diff` scan found, for a caller that will narrow its
/// target list with it.
#[derive(Debug, Clone, Default)]
pub struct PdfScan {
    /// `Some(set)` → convert only documents whose blake3 is in it: the
    /// ones the driver found stale through their declared inputs, plus
    /// the ones the diff named. `None` → convert everything the corpus
    /// holds.
    pub render: Option<std::collections::HashSet<String>>,
    pub new_head: Option<String>,
    pub elapsed: Option<std::time::Duration>,
}

/// Ask the raw store which documents moved since the cursor.
///
/// The bucket is the document's blake3, which is also its identity — for
/// this provider "changed" and "different document" are the same
/// statement. `pdf_paths` joins the union because a file appearing at a
/// new path is how a document enters the corpus, even when its bytes
/// were already known. A `pdf_scan_meta` change reaches every document
/// through the row each declared.
async fn scan_changed(db: &RawDb, range: RawRange<'_>) -> Result<PdfScan> {
    let pin = db
        .pin()
        .expect("open_reader returns a pinned handle")
        .clone();
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        db.pool(),
        range.cursor,
        &pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT blake3 FROM (
                    SELECT coalesce(to_blake3, from_blake3) AS blake3
                      FROM dolt_diff_pdf_documents
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_blake3, from_blake3)
                      FROM dolt_diff_pdf_paths
                     WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                )
                WHERE blake3 IS NOT NULL
            ",
        },
    )
    .await?;
    // The driver names stale buckets by blake3, as the diff does.
    Ok(PdfScan {
        render: range.narrow(scan.render.as_ref()),
        new_head: scan.new_head,
        elapsed: scan.scan_elapsed,
    })
}
