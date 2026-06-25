//! Render one `index.md` per book plus one `.md` per (chapter,
//! edition) under `<out_dir>/<stanza>/rendered_md/thucydides/histories/`,
//! each with a sibling `.grid_rows.json` sidecar carrying all rows for
//! that doc.
//!
//! Per chapter/edition doc, we emit:
//!   * one chapter-level row (kind = "Chapter (<edition-id>)") whose
//!     `uuid` equals the (chapter, edition) uuid;
//!   * one section-level row per section the edition covers (kind =
//!     "Section (<edition-id>)") whose `uuid` equals the (section,
//!     edition) uuid and `markdown_uuid` equals the chapter uuid.
//!
//! `conversation_name` carries `"<b>.<c> <edition-title>"`, where the
//! title is CTS-derived; the leading locator lets the UI's control
//! panel (`perseusView`) recover the edition title by stripping it.
//!
//! Each section is wrapped in `<div data-section-uuid="…">` so the SPA
//! scrolls/highlights it on a row click. Editions that participate in a
//! configured `alignment_pairs` entry additionally get one
//! `<span data-section-uuid="…">` per sentence plus `bilingual-alignment`
//! edges to the paired edition's sentences.

use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};

use frankweiler_etl::layout::rendered_md_root;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_index_lib::emit_sidecar;
use frankweiler_schema::edges::EdgeRow;
use frankweiler_schema::grid_rows::GridRow;

use super::super::{
    book_uuid, chapter_uuid, edge_uuid, paragraph_sentence_uuid, paragraph_uuid, TLG0003_TLG001,
    WORK_SHORT, WORK_TITLE, WORK_URN,
};
use super::align::{split, PerseusAlignments, Sentence};
use super::parse::{Book, Chapter, Edition, ParsedPerseus, Section};
use super::RENDER_VERSION;

/// Synthetic `when_ts` base. Drives the grid's global sort so default
/// ordering yields reading order (Book 1 Chapter 1 first).
///
/// **Known violation of "no fabricated timestamps"**: Perseus is an
/// immutable upstream corpus with no per-section timestamps, so we
/// synthesize a deterministic ordering stamp. See
/// `data_architecture_ingestion.md` "Entities without a time-shape".
fn ts_base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

fn render_synth_ts(ts: DateTime<Utc>) -> String {
    frankweiler_time::IsoOffsetTimestamp::from(ts).to_rfc3339()
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub markdowns_total: usize,
    pub markdowns_rendered: usize,
    pub markdowns_skipped: usize,
    pub rows_emitted: usize,
}

/// Translate entry point. Mirrors `contacts::render_and_index_md::render::render_all`
/// so the sync orchestrator's match arm wires up the same way.
///
/// `alignments` carries the per-section sentence alignments for the
/// configured edition pairs (empty when none configured). Editions not
/// in any pair render with section-level anchors only.
#[allow(clippy::too_many_arguments)]
pub fn render_all(
    parsed: &ParsedPerseus,
    alignments: &PerseusAlignments,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();

    // One book doc each, plus one doc per (chapter, edition) the
    // edition actually covers.
    let total: usize = parsed
        .books
        .iter()
        .map(|b| {
            1 + b
                .chapters
                .iter()
                .map(|c| {
                    parsed
                        .editions
                        .iter()
                        .filter(|e| chapter_covers(c, &e.id))
                        .count()
                })
                .sum::<usize>()
        })
        .sum();
    summary.markdowns_total = total;
    progress.set_length(Some(total as u64));

    for book in &parsed.books {
        render_book(
            book,
            out_dir,
            source_name,
            prior_fingerprints,
            &mut summary,
            on_doc_complete,
        )?;
        progress.inc(1);

        for chapter in &book.chapters {
            for edition in &parsed.editions {
                if !chapter_covers(chapter, &edition.id) {
                    continue;
                }
                render_chapter(
                    book,
                    chapter,
                    edition,
                    alignments,
                    out_dir,
                    source_name,
                    prior_fingerprints,
                    &mut summary,
                    on_doc_complete,
                )?;
                progress.inc(1);
            }
        }
    }
    Ok(summary)
}

/// Whether `edition_id` has any non-empty section text in `chapter`.
fn chapter_covers(chapter: &Chapter, edition_id: &str) -> bool {
    chapter
        .sections
        .iter()
        .any(|s| !s.text(edition_id).is_empty())
}

fn render_book(
    book: &Book,
    out_dir: &Path,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
    summary: &mut RenderSummary,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let m_uuid = book_uuid(&book.n);
    let fingerprint = compute_book_fingerprint(book);
    let book_dir = rendered_md_root(out_dir, source_name).join(book_content_rel(&book.n));
    fs::create_dir_all(&book_dir).with_context(|| format!("mkdir -p {}", book_dir.display()))?;
    let md_path = book_dir.join("index.md");
    let sidecar_path = book_dir.join("index.grid_rows.json");

    if prior_fingerprints.get(&m_uuid).map(String::as_str) == Some(fingerprint.as_str())
        && md_path.exists()
    {
        summary.markdowns_skipped += 1;
        return Ok(());
    }

    let md = render_book_md(book);
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let rows = vec![book_grid_row(source_name, book, &m_uuid)?];
    let edges: Vec<EdgeRow> = Vec::new();
    emit_sidecar(
        &sidecar_path,
        &m_uuid,
        &fingerprint,
        RENDER_VERSION,
        &rows,
        &edges,
    )?;

    summary.rows_emitted += rows.len();
    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
        edges,
    })
    .with_context(|| format!("on_doc_complete book {}", book.n))?;

    summary.markdowns_rendered += 1;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_chapter(
    book: &Book,
    chapter: &Chapter,
    edition: &Edition,
    alignments: &PerseusAlignments,
    out_dir: &Path,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
    summary: &mut RenderSummary,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let m_uuid = chapter_uuid(&book.n, &chapter.n, &edition.id);
    let fingerprint = compute_chapter_fingerprint(book, chapter, edition, alignments);
    let rel = chapter_md_rel(source_name, &book.n, &chapter.n, &edition.id);
    let md_path = out_dir.join(&rel);
    let sidecar_path = md_path.with_file_name(format!(
        "{}.grid_rows.json",
        md_path.file_stem().unwrap().to_string_lossy()
    ));

    if prior_fingerprints.get(&m_uuid).map(String::as_str) == Some(fingerprint.as_str())
        && md_path.exists()
    {
        summary.markdowns_skipped += 1;
        return Ok(());
    }

    if let Some(parent) = md_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
    }

    let md = render_chapter_md(chapter, edition, alignments);
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let mut rows: Vec<GridRow> = Vec::with_capacity(1 + chapter.sections.len());
    rows.push(chapter_grid_row(book, chapter, edition, &m_uuid, &rel)?);
    let mut idx = 0i64;
    for sec in &chapter.sections {
        let text = sec.text(&edition.id);
        if text.is_empty() {
            continue;
        }
        let s_uuid = paragraph_uuid(&book.n, &chapter.n, &sec.n, &edition.id);
        rows.push(section_grid_row(
            book, chapter, sec, edition, &s_uuid, &m_uuid, &rel, text, idx,
        )?);
        idx += 1;
    }
    let edges = chapter_edges(book, chapter, edition, &m_uuid, alignments);
    emit_sidecar(
        &sidecar_path,
        &m_uuid,
        &fingerprint,
        RENDER_VERSION,
        &rows,
        &edges,
    )?;

    summary.rows_emitted += rows.len();
    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
        edges,
    })
    .with_context(|| {
        format!(
            "on_doc_complete chapter {}.{} ({})",
            book.n, chapter.n, edition.id
        )
    })?;

    summary.markdowns_rendered += 1;
    Ok(())
}

/// Book directory path under `rendered_md/`, relative to the stanza's
/// `rendered_md` root: `thucydides/histories/book_{NN}`. Content-only —
/// the `<stanza>/rendered_md/` prefix is added by the callers (absolute
/// via `layout::rendered_md_root`, rel-string via `book_dir_rel`).
fn book_content_rel(book_n: &str) -> PathBuf {
    let bn: u32 = book_n.parse().unwrap_or(0);
    PathBuf::from(format!("thucydides/histories/book_{bn:02}"))
}

/// Book directory as a `<data_root>`-relative path string:
/// `<stanza>/rendered_md/thucydides/histories/book_{NN}`.
fn book_dir_rel(stanza: &str, book_n: &str) -> PathBuf {
    PathBuf::from(stanza)
        .join("rendered_md")
        .join(book_content_rel(book_n))
}

fn chapter_md_rel(stanza: &str, book_n: &str, ch_n: &str, edition_id: &str) -> String {
    let ci: u32 = ch_n.parse().unwrap_or(0);
    // `_<edition>.md`, not `.<edition>.md`: qmd's docid normalization
    // collapses `.x.md` to `-x.md` but our `norm_path` keeps the dot,
    // so a dotted form would miss the hit-to-row path lookup. Edition
    // ids contain no dots (the work prefix is stripped at parse time).
    format!(
        "{}/chapter_{ci:03}_{edition_id}.md",
        book_dir_rel(stanza, book_n).display()
    )
}

fn chapter_locator(book_n: &str, ch_n: &str) -> String {
    let bn: u32 = book_n.parse().unwrap_or(0);
    let ci: u32 = ch_n.parse().unwrap_or(0);
    format!("{bn}.{ci}")
}

/// Grid `conversation_name` / markdown title for a (chapter, edition):
/// `"<b>.<c> <edition-title>"`. The leading locator lets `perseusView`
/// recover the bare edition title (it strips the `^[\d.]+\s` prefix).
fn conversation_name(book_n: &str, ch_n: &str, edition: &Edition) -> String {
    format!("{} {}", chapter_locator(book_n, ch_n), edition.title)
}

fn book_title(book_n: &str) -> String {
    let bn: u32 = book_n.parse().unwrap_or(0);
    format!("{WORK_SHORT} Book {bn}")
}

fn section_div_open(section_uuid: &str) -> String {
    format!(
        "<div id=\"m-{section_uuid}\" data-section-uuid=\"{section_uuid}\" class=\"msg msg--perseus\">"
    )
}

const SECTION_DIV_CLOSE: &str = "</div>";

fn render_book_md(book: &Book) -> String {
    let title = book_title(&book.n);
    format!(
        "---\n\
         provider: perseus\n\
         work: {WORK_TITLE}\n\
         edition: {TLG0003_TLG001}\n\
         book: {book_n}\n\
         title: {title}\n\
         ---\n\
         \n\
         # {title}\n\
         \n",
        book_n = book.n,
    )
}

fn render_chapter_md(
    chapter: &Chapter,
    edition: &Edition,
    alignments: &PerseusAlignments,
) -> String {
    let locator = chapter_locator(&chapter.book_n, &chapter.n);
    let title = format!("{WORK_SHORT} {locator} — {}", edition.title);
    let mut out = format!(
        "---\n\
         provider: perseus\n\
         work: {WORK_TITLE}\n\
         edition: {edition_id}\n\
         book: {book_n}\n\
         chapter: {ch_n}\n\
         title: {title}\n\
         language: {lang}\n\
         ---\n\
         \n\
         # {title}\n\
         \n",
        edition_id = edition.id,
        book_n = chapter.book_n,
        ch_n = chapter.n,
        lang = edition.lang,
    );
    let aligned = alignments.is_aligned(&edition.id);
    for sec in &chapter.sections {
        let text = sec.text(&edition.id);
        if text.is_empty() {
            continue;
        }
        let s_uuid = paragraph_uuid(&chapter.book_n, &chapter.n, &sec.n, &edition.id);
        let body = if aligned {
            let sentences = split::split_for(&edition.lang, text);
            wrap_sentences(text, &sentences, |i| {
                paragraph_sentence_uuid(&chapter.book_n, &chapter.n, &sec.n, &edition.id, i)
            })
        } else {
            text.to_string()
        };
        out.push_str(&section_div_open(&s_uuid));
        out.push_str("\n\n");
        out.push_str(&format!(
            "### {}.{}.{}\n\n{}\n\n",
            chapter.book_n, chapter.n, sec.n, body
        ));
        out.push_str(SECTION_DIV_CLOSE);
        out.push_str("\n\n");
    }
    out
}

/// Wrap each sentence of `text` in its own inline
/// `<span data-section-uuid="…">…</span>`. Whitespace between sentences
/// is preserved outside the spans. Empty `sentences` → text unchanged.
fn wrap_sentences(
    text: &str,
    sentences: &[Sentence],
    anchor_for: impl Fn(usize) -> String,
) -> String {
    if sentences.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + sentences.len() * 64);
    let mut cursor = 0usize;
    for (i, sent) in sentences.iter().enumerate() {
        if sent.start > cursor {
            out.push_str(&text[cursor..sent.start]);
        }
        let uuid = anchor_for(i);
        out.push_str("<span data-section-uuid=\"");
        out.push_str(&uuid);
        out.push_str("\">");
        out.push_str(&text[sent.start..sent.end]);
        out.push_str("</span>");
        cursor = sent.end;
    }
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

fn chapter_text_for_grid(chapter: &Chapter, edition: &Edition) -> String {
    let mut out = conversation_name(&chapter.book_n, &chapter.n, edition);
    for sec in &chapter.sections {
        let text = sec.text(&edition.id);
        if text.is_empty() {
            continue;
        }
        out.push('\n');
        out.push_str(&format!(
            "[{}.{}.{}] {text}",
            chapter.book_n, chapter.n, sec.n
        ));
    }
    out
}

fn book_text_for_grid(book: &Book) -> String {
    book_title(&book.n)
}

fn synth_when_ts(book_n: &str, ch_n: i64) -> String {
    let bi: i64 = book_n.parse().unwrap_or(0);
    let offset = bi * 10_000 + ch_n;
    render_synth_ts(ts_base() + Duration::seconds(offset))
}

fn book_grid_row(stanza: &str, book: &Book, bk_uuid: &str) -> Result<GridRow> {
    GridRow::builder()
        .uuid(bk_uuid.to_string())
        .provider("perseus")
        .kind("Book")
        .source_label("Perseus")
        .when_ts(Some(synth_when_ts(&book.n, 0)))
        .author(Some("Thucydides".to_string()))
        .account(Some("Perseus Digital Library".to_string()))
        .project(Some(WORK_TITLE.to_string()))
        .conversation_name(Some(book_title(&book.n)))
        .conversation_uuid(bk_uuid.to_string())
        .entire_chat(format!("/chat/{bk_uuid}"))
        .text(book_text_for_grid(book))
        .qmd_path(Some(format!(
            "{}/index.md",
            book_dir_rel(stanza, &book.n).display()
        )))
        .source_url(Some(format!(
            "https://scaife.perseus.org/reader/{TLG0003_TLG001}:{}/",
            book.n
        )))
        .external_id(Some(book.n.clone()))
        .markdown_uuid(Some(bk_uuid.to_string()))
        .build()
        .map_err(anyhow::Error::from)
}

fn chapter_grid_row(
    book: &Book,
    chapter: &Chapter,
    edition: &Edition,
    ch_uuid: &str,
    md_rel: &str,
) -> Result<GridRow> {
    let ci: i64 = chapter.n.parse().unwrap_or(0);
    let bi: u32 = book.n.parse().unwrap_or(0);
    let ci_u: u32 = ci as u32;
    GridRow::builder()
        .uuid(ch_uuid.to_string())
        .provider("perseus")
        .kind(format!("Chapter ({})", edition.id))
        .source_label("Perseus")
        .when_ts(Some(synth_when_ts(&book.n, ci)))
        .author(Some("Thucydides".to_string()))
        .account(Some("Perseus Digital Library".to_string()))
        .project(Some(WORK_TITLE.to_string()))
        .conversation_name(Some(conversation_name(&book.n, &chapter.n, edition)))
        .conversation_uuid(ch_uuid.to_string())
        .entire_chat(format!("/chat/{ch_uuid}"))
        .text(chapter_text_for_grid(chapter, edition))
        .qmd_path(Some(md_rel.to_string()))
        .source_url(Some(format!(
            "https://scaife.perseus.org/reader/{WORK_URN}.{}:{bi}.{ci_u}/",
            edition.id
        )))
        .external_id(Some(format!("{bi}.{ci_u}")))
        .markdown_uuid(Some(ch_uuid.to_string()))
        .build()
        .map_err(anyhow::Error::from)
}

#[allow(clippy::too_many_arguments)]
fn section_grid_row(
    book: &Book,
    chapter: &Chapter,
    sec: &Section,
    edition: &Edition,
    sec_uuid: &str,
    ch_uuid: &str,
    md_rel: &str,
    text: &str,
    idx: i64,
) -> Result<GridRow> {
    let bi: u32 = book.n.parse().unwrap_or(0);
    let ci: u32 = chapter.n.parse().unwrap_or(0);
    let si: u32 = sec.n.parse().unwrap_or(0);
    let when_ts = {
        let ci_i64: i64 = ci as i64;
        let chapter_secs = bi as i64 * 10_000 + ci_i64;
        let ts = ts_base() + Duration::seconds(chapter_secs) + Duration::milliseconds(idx + 1);
        render_synth_ts(ts)
    };
    GridRow::builder()
        .uuid(sec_uuid.to_string())
        .provider("perseus")
        .kind(format!("Section ({})", edition.id))
        .source_label("Perseus")
        .when_ts(Some(when_ts))
        .author(Some("Thucydides".to_string()))
        .account(Some("Perseus Digital Library".to_string()))
        .project(Some(WORK_TITLE.to_string()))
        .conversation_name(Some(conversation_name(&book.n, &chapter.n, edition)))
        .conversation_uuid(ch_uuid.to_string())
        .message_index(Some(idx))
        .entire_chat(format!("/chat/{ch_uuid}"))
        .text(text.to_string())
        .qmd_path(Some(md_rel.to_string()))
        .source_url(Some(format!(
            "https://scaife.perseus.org/reader/{WORK_URN}.{}:{bi}.{ci}.{si}/",
            edition.id
        )))
        .external_id(Some(format!("{bi}.{ci}.{si}")))
        .markdown_uuid(Some(ch_uuid.to_string()))
        .build()
        .map_err(anyhow::Error::from)
}

/// Edges from this edition's chapter doc to its configured alignment
/// counterparts: one doc-level edge per counterpart edition (label =
/// counterpart short id) plus one `bilingual-alignment` edge per
/// aligned sentence pair. Empty unless `edition` is in an
/// `alignment_pairs` entry.
fn chapter_edges(
    book: &Book,
    chapter: &Chapter,
    edition: &Edition,
    m_uuid: &str,
    alignments: &PerseusAlignments,
) -> Vec<EdgeRow> {
    let mut edges: Vec<EdgeRow> = Vec::new();
    let mut doc_level_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let bilingual = Some("bilingual-alignment");

    for sec in &chapter.sections {
        for pa in alignments.for_section(&book.n, &chapter.n, &sec.n) {
            // Orient the pair so `this` is `edition`: `this_is_a` picks
            // which side of each group's index lists belongs to us.
            let (other_id, this_is_a) = if pa.a_id == edition.id {
                (pa.b_id.as_str(), true)
            } else if pa.b_id == edition.id {
                (pa.a_id.as_str(), false)
            } else {
                continue;
            };
            let other_md = chapter_uuid(&book.n, &chapter.n, other_id);

            // One doc-level edge per counterpart, label = its short id.
            if doc_level_seen.insert(other_id.to_string()) {
                let label = Some(other_id);
                edges.push(EdgeRow {
                    edge_uuid: edge_uuid(m_uuid, None, &other_md, None, label),
                    src_markdown_uuid: m_uuid.to_string(),
                    src_anchor_uuid: None,
                    dst_markdown_uuid: other_md.clone(),
                    dst_anchor_uuid: None,
                    label: label.map(str::to_string),
                });
            }

            for g in &pa.groups {
                let (this_idxs, other_idxs) = if this_is_a {
                    (&g.a, &g.b)
                } else {
                    (&g.b, &g.a)
                };
                for &si in this_idxs {
                    let src_anchor =
                        paragraph_sentence_uuid(&book.n, &chapter.n, &sec.n, &edition.id, si);
                    for &di in other_idxs {
                        let dst_anchor =
                            paragraph_sentence_uuid(&book.n, &chapter.n, &sec.n, other_id, di);
                        edges.push(EdgeRow {
                            edge_uuid: edge_uuid(
                                m_uuid,
                                Some(&src_anchor),
                                &other_md,
                                Some(&dst_anchor),
                                bilingual,
                            ),
                            src_markdown_uuid: m_uuid.to_string(),
                            src_anchor_uuid: Some(src_anchor.clone()),
                            dst_markdown_uuid: other_md.clone(),
                            dst_anchor_uuid: Some(dst_anchor),
                            label: bilingual.map(str::to_string),
                        });
                    }
                }
            }
        }
    }
    edges
}

fn compute_book_fingerprint(book: &Book) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    "book".hash(&mut h);
    book.n.hash(&mut h);
    book.chapters.len().hash(&mut h);
    for c in &book.chapters {
        c.n.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn compute_chapter_fingerprint(
    book: &Book,
    chapter: &Chapter,
    edition: &Edition,
    alignments: &PerseusAlignments,
) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    "chapter".hash(&mut h);
    chapter.book_n.hash(&mut h);
    chapter.n.hash(&mut h);
    edition.id.hash(&mut h);
    // Title flows into conversation_name / markdown title, so a CTS
    // change must re-render.
    edition.title.hash(&mut h);
    alignments.is_aligned(&edition.id).hash(&mut h);
    for sec in &chapter.sections {
        let text = sec.text(&edition.id);
        if text.is_empty() {
            continue;
        }
        sec.n.hash(&mut h);
        text.hash(&mut h);
        // Fold in the alignment groups touching this edition so a
        // change to `alignment_pairs` re-renders the affected docs.
        for pa in alignments.for_section(&book.n, &chapter.n, &sec.n) {
            if pa.a_id != edition.id && pa.b_id != edition.id {
                continue;
            }
            pa.a_id.hash(&mut h);
            pa.b_id.hash(&mut h);
            for g in &pa.groups {
                g.a.hash(&mut h);
                g.b.hash(&mut h);
            }
        }
    }
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_and_index_md::parse::{Book, Chapter, Edition, Section};
    use std::collections::BTreeMap;

    fn edition(id: &str, lang: &str, title: &str) -> Edition {
        Edition {
            id: id.to_string(),
            short: id.rsplit('-').next().unwrap_or(id).to_string(),
            lang: lang.to_string(),
            title: title.to_string(),
        }
    }

    fn section(n: &str, texts: &[(&str, &str)]) -> Section {
        Section {
            n: n.to_string(),
            texts: texts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn chapter_md_carries_edition_title_and_section_anchor() {
        let ed = edition("perseus-grc2", "grc", "Ἱστορίαι — Jones");
        let chapter = Chapter {
            book_n: "1".into(),
            n: "1".into(),
            sections: vec![section("1", &[("perseus-grc2", "Θουκυδίδης ξυνέγραψε.")])],
        };
        let md = render_chapter_md(&chapter, &ed, &PerseusAlignments::default());
        assert!(md.contains("# Thucydides 1.1 — Ἱστορίαι — Jones"));
        let s_uuid = paragraph_uuid("1", "1", "1", "perseus-grc2");
        assert!(md.contains(&format!("data-section-uuid=\"{s_uuid}\"")));
        // No per-sentence spans when the edition isn't in a pair.
        assert!(!md.contains("<span data-section-uuid"));
    }

    #[test]
    fn unaligned_edition_emits_no_edges() {
        let ed = edition("1st1K-eng1", "eng", "History — Smith");
        let book = Book {
            n: "1".into(),
            chapters: vec![],
        };
        let chapter = Chapter {
            book_n: "1".into(),
            n: "1".into(),
            sections: vec![section("1", &[("1st1K-eng1", "First. Second.")])],
        };
        let edges = chapter_edges(
            &book,
            &chapter,
            &ed,
            &chapter_uuid("1", "1", "1st1K-eng1"),
            &PerseusAlignments::default(),
        );
        assert!(edges.is_empty());
    }

    #[test]
    fn conversation_name_has_locator_prefix() {
        let ed = edition(
            "perseus-eng6",
            "eng",
            "History of the Peloponnesian War — Crawley",
        );
        assert_eq!(
            conversation_name("6", "98", &ed),
            "6.98 History of the Peloponnesian War — Crawley"
        );
    }
}
