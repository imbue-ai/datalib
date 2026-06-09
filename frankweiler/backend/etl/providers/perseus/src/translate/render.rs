//! Render one `.md` per book (`index.md`, a chapter cross-link
//! table) + one `.md` per (chapter, language) under
//! `<out_dir>/rendered_md/perseus/thucydides/histories/`, plus a
//! sibling `.grid_rows.json` sidecar carrying ALL rows for that doc.
//!
//! Per chapter doc, we emit:
//!   * one chapter-level row (kind = "Chapter ({lang})") whose `uuid`
//!     equals the chapter uuid;
//!   * one section-level row per non-empty section (kind =
//!     "Section ({lang})") whose `uuid` equals the section uuid and
//!     `markdown_uuid` equals the chapter uuid.
//!
//! Each section in the chapter md is wrapped in
//! `<div data-section-uuid="…">` so the SPA's preview pane
//! (`frankweiler/ui/src/components/ChatBody.vue::applySelection`)
//! highlights + scrolls to the matching section on row click. This
//! mirrors the per-message wrappers ChatGPT / Anthropic chats use.

use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, TimeZone, Utc};

use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_schema::edges::EdgeRow;
use frankweiler_schema::grid_rows::GridRow;

use super::super::{
    book_uuid, chapter_uuid, edge_uuid, paragraph_sentence_uuid, paragraph_uuid, TLG0003_TLG001,
    WORK_SHORT, WORK_TITLE,
};
use super::align::{PerseusAlignments, Sentence};
use super::parse::{Book, Chapter, ParsedPerseus};
use super::RENDER_VERSION;

/// Synthetic `when_ts` base. Drives the grid's global sort so default
/// ordering yields reading order (Book 1 Chapter 1 first).
fn ts_base() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub markdowns_total: usize,
    pub markdowns_rendered: usize,
    pub markdowns_skipped: usize,
    /// Total grid rows emitted (one per book, one per chapter ×
    /// language, one per non-empty section × language). Surfaced
    /// for the sync orchestrator's summary log.
    pub rows_emitted: usize,
}

/// Translate entry point. Mirrors the shape of `contacts::translate::render::render_all`
/// so the sync orchestrator's match arm wires up the same way.
///
/// `alignments` carries within-section sentence alignments produced
/// upstream by `align::align_all()` (async). The renderer is
/// synchronous; the orchestrator awaits the alignment phase before
/// calling here. Sections without a precomputed alignment fall back
/// to the trivial 1:1 split, which keeps the existing fixture-based
/// tests working without forcing the test harness to load the model.
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
    let total: usize = parsed.books.iter().map(|b| 1 + 2 * b.chapters.len()).sum();
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
            for lang in ["grc", "eng"] {
                render_chapter(
                    book,
                    chapter,
                    lang,
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
    let book_dir = out_dir.join(book_dir_rel(&book.n));
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

    let row = book_grid_row(book, &m_uuid);
    let rows = vec![row];
    let edges = book_edges(book, &m_uuid);
    write_sidecar(&sidecar_path, &m_uuid, &fingerprint, &rows, &edges)?;

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
    lang: &str,
    alignments: &PerseusAlignments,
    out_dir: &Path,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
    summary: &mut RenderSummary,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let m_uuid = chapter_uuid(&book.n, &chapter.n, lang);
    let fingerprint = compute_chapter_fingerprint(chapter, lang);
    let rel = chapter_md_rel(&book.n, &chapter.n, lang);
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

    let md = render_chapter_md(chapter, lang, alignments);
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let mut rows: Vec<GridRow> = Vec::with_capacity(1 + chapter.sections.len());
    rows.push(chapter_grid_row(book, chapter, lang, &m_uuid, &rel));
    let mut idx = 0i64;
    for sec in &chapter.sections {
        let text = if lang == "grc" { &sec.grc } else { &sec.eng };
        if text.is_empty() {
            continue;
        }
        let s_uuid = paragraph_uuid(&book.n, &chapter.n, &sec.n, lang);
        rows.push(section_grid_row(
            book, chapter, sec, lang, &s_uuid, &m_uuid, &rel, text, idx,
        ));
        idx += 1;
    }
    let edges = chapter_edges(book, chapter, lang, &m_uuid, alignments);
    write_sidecar(&sidecar_path, &m_uuid, &fingerprint, &rows, &edges)?;

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
    .with_context(|| format!("on_doc_complete chapter {}.{} ({lang})", book.n, chapter.n))?;

    summary.markdowns_rendered += 1;
    Ok(())
}

fn book_dir_rel(book_n: &str) -> PathBuf {
    let bn: u32 = book_n.parse().unwrap_or(0);
    PathBuf::from(format!(
        "rendered_md/perseus/thucydides/histories/book_{bn:02}"
    ))
}

fn chapter_md_rel(book_n: &str, ch_n: &str, lang: &str) -> String {
    let ci: u32 = ch_n.parse().unwrap_or(0);
    // `_lang.md`, not `.lang.md`: qmd's internal docid normalization
    // collapses `.eng.md` to `-eng.md`, but our `norm_path` keeps the
    // dot, so a hit-to-row path lookup misses on the dotted form.
    format!(
        "{}/chapter_{ci:03}_{lang}.md",
        book_dir_rel(book_n).display()
    )
}

fn chapter_title(book_n: &str, ch_n: &str) -> String {
    let bn: u32 = book_n.parse().unwrap_or(0);
    let ci: u32 = ch_n.parse().unwrap_or(0);
    format!("{WORK_SHORT} {bn}.{ci}")
}

/// Per-language chapter title used everywhere a single string has to
/// disambiguate the grc vs. eng rendering (grid `Conversation Name`
/// column, the rendered `.md` H1, and downstream `markdowns.title`
/// which flows into the UI's outgoing-destinations link text).
fn chapter_title_localized(book_n: &str, ch_n: &str, lang: &str) -> String {
    format!("{} ({})", chapter_title(book_n, ch_n), lang_label(lang))
}

fn book_title(book_n: &str) -> String {
    let bn: u32 = book_n.parse().unwrap_or(0);
    format!("{WORK_SHORT} Book {bn}")
}

fn lang_label(lang: &str) -> &'static str {
    match lang {
        "grc" => "Greek",
        "eng" => "English",
        _ => "?",
    }
}

fn synth_when_ts(book_n: &str, ch_n: i64) -> String {
    let bi: i64 = book_n.parse().unwrap_or(0);
    let offset = bi * 10_000 + ch_n;
    let ts = ts_base() + Duration::seconds(offset);
    ts.to_rfc3339()
}

/// Bake one section into the chapter md as an HTML-wrapped div the
/// SPA can find via `[data-section-uuid="…"]`. Same shape as
/// `chatgpt::render::msg_div_open` so a single ChatBody.vue selector
/// handles both providers without branching.
fn section_div_open(section_uuid: &str) -> String {
    format!(
        "<div id=\"m-{section_uuid}\" data-section-uuid=\"{section_uuid}\" class=\"msg msg--perseus\">"
    )
}

const SECTION_DIV_CLOSE: &str = "</div>";

fn render_book_md(book: &Book) -> String {
    // The book doc carries only its frontmatter + H1 now. Every
    // chapter cross-link that used to live inline as a markdown
    // table is expressed instead as one `edges` row per (chapter,
    // language) pair — see `book_edges` — and rendered in the UI's
    // outgoing-destinations list. Keeping the body empty makes the
    // book a pure navigation entry rather than a synthetic
    // table-of-contents doc that the user has to scroll through.
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

fn render_chapter_md(chapter: &Chapter, lang: &str, alignments: &PerseusAlignments) -> String {
    let title = chapter_title(&chapter.book_n, &chapter.n);
    let mut out = format!(
        "---\n\
         provider: perseus\n\
         work: {WORK_TITLE}\n\
         edition: {TLG0003_TLG001}\n\
         book: {book_n}\n\
         chapter: {ch_n}\n\
         title: {title} ({lang_label})\n\
         language: {lang}\n\
         ---\n\
         \n\
         # {title} ({lang_label})\n\
         \n",
        book_n = chapter.book_n,
        ch_n = chapter.n,
        lang_label = lang_label(lang),
    );
    // The cross-language jump that used to live as an inline
    // `*Other:* [...](/#/chat/…)` here is now expressed as an
    // outgoing `edges` row (label = "cross-language"). The UI
    // renders it as part of the doc-level destinations list at the
    // top of ChatBody, no markdown footprint needed.
    for sec in &chapter.sections {
        let text = if lang == "grc" { &sec.grc } else { &sec.eng };
        if text.is_empty() {
            continue;
        }
        let s_uuid = paragraph_uuid(&chapter.book_n, &chapter.n, &sec.n, lang);
        let alignment = alignments.get_or_trivial(&chapter.book_n, &chapter.n, &sec.n, sec);
        let sentences: &[Sentence] = if lang == "grc" {
            &alignment.grc_sentences
        } else {
            &alignment.eng_sentences
        };
        let body = wrap_sentences(text, sentences, |i| {
            paragraph_sentence_uuid(&chapter.book_n, &chapter.n, &sec.n, lang, i)
        });
        // Blank lines around the `<div>` / `</div>` so the markdown
        // parser inside doesn't get confused — common-mark allows
        // raw HTML blocks but needs the surrounding blank lines to
        // treat the inner content as markdown again.
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
/// `<span data-section-uuid="…">…</span>` so the UI can highlight any
/// individual sentence when a bilingual-alignment edge points at it.
///
/// `sentences` carries the byte ranges produced by the alignment
/// splitter; `anchor_for(i)` returns the per-sentence UUID for the
/// i-th sentence. Whitespace between sentences (and any leading /
/// trailing) is preserved outside the spans, so the rendered body
/// concatenates back to the original section text modulo what the
/// splitter trimmed (only outer whitespace on individual sentences,
/// which never changes the user-visible result after the surrounding
/// markdown collapses runs anyway).
///
/// If `sentences` is empty (no detectable sentence boundaries — rare,
/// would mean the splitter rejected the whole section), the original
/// text is returned unwrapped: the section-level `<div data-section-
/// uuid="…">` from `section_div_open` still gives the UI a fallback
/// anchor for click-to-scroll.
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
        // Anything before this sentence's start (leading or inter-
        // sentence whitespace) gets copied verbatim.
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
    // Trailing whitespace after the last sentence.
    if cursor < text.len() {
        out.push_str(&text[cursor..]);
    }
    out
}

fn chapter_text_for_grid(chapter: &Chapter, lang: &str) -> String {
    let title = chapter_title(&chapter.book_n, &chapter.n);
    let mut out = format!("{title} ({})", lang_label(lang));
    for sec in &chapter.sections {
        let text = if lang == "grc" { &sec.grc } else { &sec.eng };
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
    // Used as the row's `text` (search body + grid Contents
    // snippet). The book is a navigation-only entry now; its
    // chapters live in the `edges` table, not in any free-text
    // body. Keep this to the book title alone — enough for the
    // grid row to read sensibly but no synthetic prose for search
    // to match against.
    book_title(&book.n)
}

fn book_grid_row(book: &Book, bk_uuid: &str) -> GridRow {
    GridRow {
        uuid: bk_uuid.to_string(),
        provider: "perseus".to_string(),
        kind: "Book".to_string(),
        source_label: "Perseus".to_string(),
        when_ts: synth_when_ts(&book.n, 0),
        author: Some("Thucydides".to_string()),
        account: Some("Perseus Digital Library".to_string()),
        org_uuid: None,
        org_name: None,
        project: Some(WORK_TITLE.to_string()),
        channel: None,
        conversation_name: Some(book_title(&book.n)),
        conversation_uuid: bk_uuid.to_string(),
        message_index: None,
        entire_chat: format!("/chat/{bk_uuid}"),
        text: book_text_for_grid(book),
        slack_link: None,
        qmd_path: Some(format!("{}/index.md", book_dir_rel(&book.n).display())),
        source_url: Some(format!(
            "https://scaife.perseus.org/reader/{TLG0003_TLG001}:{}/",
            book.n
        )),
        git_sha: None,
        external_id: Some(book.n.clone()),
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(bk_uuid.to_string()),
    }
}

fn chapter_grid_row(
    book: &Book,
    chapter: &Chapter,
    lang: &str,
    ch_uuid: &str,
    md_rel: &str,
) -> GridRow {
    let ci: i64 = chapter.n.parse().unwrap_or(0);
    let bi: u32 = book.n.parse().unwrap_or(0);
    let ci_u: u32 = ci as u32;
    GridRow {
        uuid: ch_uuid.to_string(),
        provider: "perseus".to_string(),
        kind: format!("Chapter ({lang})"),
        source_label: "Perseus".to_string(),
        when_ts: synth_when_ts(&book.n, ci),
        author: Some("Thucydides".to_string()),
        account: Some("Perseus Digital Library".to_string()),
        org_uuid: None,
        org_name: None,
        project: Some(WORK_TITLE.to_string()),
        channel: None,
        conversation_name: Some(chapter_title_localized(&chapter.book_n, &chapter.n, lang)),
        conversation_uuid: ch_uuid.to_string(),
        message_index: None,
        entire_chat: format!("/chat/{ch_uuid}"),
        text: chapter_text_for_grid(chapter, lang),
        slack_link: None,
        qmd_path: Some(md_rel.to_string()),
        source_url: Some(format!(
            "https://scaife.perseus.org/reader/{TLG0003_TLG001}:{bi}.{ci_u}/"
        )),
        git_sha: None,
        external_id: Some(format!("{bi}.{ci_u}")),
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(ch_uuid.to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
fn section_grid_row(
    book: &Book,
    chapter: &Chapter,
    sec: &super::parse::Section,
    lang: &str,
    sec_uuid: &str,
    ch_uuid: &str,
    md_rel: &str,
    text: &str,
    idx: i64,
) -> GridRow {
    let bi: u32 = book.n.parse().unwrap_or(0);
    let ci: u32 = chapter.n.parse().unwrap_or(0);
    let si: u32 = sec.n.parse().unwrap_or(0);
    let when_ts = {
        // Stagger sections within a chapter by 1-second increments so
        // the grid's `when_ts` sort still walks reading order:
        // synth_when_ts(book, chapter) gives the chapter slot;
        // sections sit just after it.
        let ci_i64: i64 = ci as i64;
        let chapter_secs = bi as i64 * 10_000 + ci_i64;
        let ts = ts_base() + Duration::seconds(chapter_secs) + Duration::milliseconds(idx + 1);
        ts.to_rfc3339()
    };
    GridRow {
        uuid: sec_uuid.to_string(),
        provider: "perseus".to_string(),
        kind: format!("Section ({lang})"),
        source_label: "Perseus".to_string(),
        when_ts,
        author: Some("Thucydides".to_string()),
        account: Some("Perseus Digital Library".to_string()),
        org_uuid: None,
        org_name: None,
        project: Some(WORK_TITLE.to_string()),
        channel: None,
        conversation_name: Some(chapter_title_localized(&chapter.book_n, &chapter.n, lang)),
        // conversation_uuid points at the chapter — clicking the row's
        // conversation column groups the rows by chapter, same as
        // message rows group by conversation in the chatgpt/anthropic
        // providers.
        conversation_uuid: ch_uuid.to_string(),
        message_index: Some(idx),
        entire_chat: format!("/chat/{ch_uuid}"),
        text: text.to_string(),
        slack_link: None,
        // Shared `qmd_path` with the chapter row — the SPA opens the
        // chapter doc and the per-row `uuid` (== sec_uuid) lights up
        // the right `<div data-section-uuid="…">` inside it.
        qmd_path: Some(md_rel.to_string()),
        source_url: Some(format!(
            "https://scaife.perseus.org/reader/{TLG0003_TLG001}:{bi}.{ci}.{si}/"
        )),
        git_sha: None,
        external_id: Some(format!("{bi}.{ci}.{si}")),
        notion_page_uuid: None,
        notion_block_uuid: None,
        // markdown_uuid points at the chapter doc — that's the file
        // backing `/api/chat/{markdown_uuid}`. The SPA's URL fragment
        // `#m{message_index}` plus the row's `uuid` together drive
        // scroll-and-highlight; see SearchView.vue::openRow.
        markdown_uuid: Some(ch_uuid.to_string()),
    }
}

fn write_sidecar(
    path: &Path,
    markdown_uuid: &str,
    fingerprint: &str,
    rows: &[GridRow],
    edges: &[EdgeRow],
) -> Result<()> {
    let mut payload = serde_json::json!({
        "header": {
            "markdown_uuid": markdown_uuid,
            "source_fingerprint": fingerprint,
            "render_version": RENDER_VERSION,
        },
        "rows": rows,
    });
    // Match `Sidecar`'s `skip_serializing_if = "Vec::is_empty"` so docs
    // that have no edges (e.g. the per-book index.md) produce
    // byte-identical sidecars to the pre-edges era. Only emit the
    // field when at least one edge originates from this markdown.
    if !edges.is_empty() {
        payload["edges"] = serde_json::to_value(edges)?;
    }
    fs::write(path, serde_json::to_string_pretty(&payload)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Edges originating from one book's index doc. The book is a
/// pure navigation entry — its body is empty — so each chapter
/// cross-link the old `## Chapters` table used to carry is
/// expressed instead as one whole-doc edge per (chapter, language)
/// pair. Label is the literal "chapter"; the destination markdown's
/// title (e.g. "Thucydides 1.1 (Greek)") supplies the
/// disambiguation, rendered in parens after the label by the UI.
fn book_edges(book: &Book, bk_uuid: &str) -> Vec<EdgeRow> {
    let label = Some("chapter");
    let mut edges: Vec<EdgeRow> = Vec::with_capacity(book.chapters.len() * 2);
    for chapter in &book.chapters {
        for lang in ["grc", "eng"] {
            let dst_md = chapter_uuid(&book.n, &chapter.n, lang);
            edges.push(EdgeRow {
                edge_uuid: edge_uuid(bk_uuid, None, &dst_md, None, label),
                src_markdown_uuid: bk_uuid.to_string(),
                src_anchor_uuid: None,
                dst_markdown_uuid: dst_md,
                dst_anchor_uuid: None,
                label: label.map(str::to_string),
            });
        }
    }
    edges
}

/// Edges originating from one chapter doc (`m_uuid`, in `lang`).
/// Emits:
///   * one cross-language edge whose dst is the matching chapter
///     doc in the other language; the label is the destination's
///     language name ("Greek" / "English") so the UI's
///     outgoing-destinations list can render it directly without a
///     join against the dst row's metadata;
///   * one `bilingual-alignment` edge per (src-sentence, dst-sentence)
///     pair from the within-section sentence alignment. For each
///     `SentenceGroup` we emit the full cross-product of indices on
///     both sides — a 1:1 group is one edge, a 1:2 group is two
///     edges (the single src sentence pointing at each of the two
///     dst sentences), and so on. The UI looks edges up by
///     `(src_markdown_uuid, src_anchor_uuid)` so multiple
///     destinations for one source span surface as multiple links.
fn chapter_edges(
    book: &Book,
    chapter: &Chapter,
    lang: &str,
    m_uuid: &str,
    alignments: &PerseusAlignments,
) -> Vec<EdgeRow> {
    let other = if lang == "grc" { "eng" } else { "grc" };
    let other_md = chapter_uuid(&chapter.book_n, &chapter.n, other);
    let mut edges: Vec<EdgeRow> = Vec::new();

    // Cross-language doc-level edge. Label = destination language so
    // `DocColumn.vue` can use it verbatim as the link text — that's
    // the field the user actually wants to see ("→ Greek"), not the
    // edge taxonomy ("cross-language") it replaces.
    let cross_label = Some(lang_label(other));
    edges.push(EdgeRow {
        edge_uuid: edge_uuid(m_uuid, None, &other_md, None, cross_label),
        src_markdown_uuid: m_uuid.to_string(),
        src_anchor_uuid: None,
        dst_markdown_uuid: other_md.clone(),
        dst_anchor_uuid: None,
        label: cross_label.map(str::to_string),
    });

    // Per-sentence-pair alignment edges.
    let label = Some("bilingual-alignment");
    for sec in &chapter.sections {
        if sec.grc.is_empty() || sec.eng.is_empty() {
            continue;
        }
        let alignment = alignments.get_or_trivial(&book.n, &chapter.n, &sec.n, sec);
        for group in &alignment.groups {
            let (src_idxs, dst_idxs) = if lang == "grc" {
                (&group.grc_indices, &group.eng_indices)
            } else {
                (&group.eng_indices, &group.grc_indices)
            };
            for &si in src_idxs {
                let src_anchor = paragraph_sentence_uuid(&book.n, &chapter.n, &sec.n, lang, si);
                for &di in dst_idxs {
                    let dst_anchor =
                        paragraph_sentence_uuid(&book.n, &chapter.n, &sec.n, other, di);
                    edges.push(EdgeRow {
                        edge_uuid: edge_uuid(
                            m_uuid,
                            Some(&src_anchor),
                            &other_md,
                            Some(&dst_anchor),
                            label,
                        ),
                        src_markdown_uuid: m_uuid.to_string(),
                        src_anchor_uuid: Some(src_anchor.clone()),
                        dst_markdown_uuid: other_md.clone(),
                        dst_anchor_uuid: Some(dst_anchor),
                        label: label.map(str::to_string),
                    });
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

fn compute_chapter_fingerprint(chapter: &Chapter, lang: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    "chapter".hash(&mut h);
    chapter.book_n.hash(&mut h);
    chapter.n.hash(&mut h);
    lang.hash(&mut h);
    for sec in &chapter.sections {
        sec.n.hash(&mut h);
        if lang == "grc" {
            sec.grc.hash(&mut h);
        } else {
            sec.eng.hash(&mut h);
        }
    }
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translate::parse::{Book, Chapter, Section};

    fn tiny() -> ParsedPerseus {
        ParsedPerseus {
            books: vec![Book {
                n: "1".to_string(),
                chapters: vec![Chapter {
                    book_n: "1".to_string(),
                    n: "1".to_string(),
                    sections: vec![
                        Section {
                            n: "1".to_string(),
                            grc: "Θουκυδίδης Ἀθηναῖος.".to_string(),
                            eng: "Thucydides the Athenian.".to_string(),
                        },
                        Section {
                            n: "2".to_string(),
                            grc: "δεύτερον τμῆμα.".to_string(),
                            eng: "Second section.".to_string(),
                        },
                    ],
                }],
            }],
        }
    }

    #[test]
    fn chapter_doc_emits_chapter_row_plus_one_row_per_section() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitted: Vec<RenderedMarkdown> = Vec::new();
        let mut on_doc = |r: RenderedMarkdown| {
            emitted.push(r);
            Ok(())
        };
        let summary = render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut on_doc,
        )
        .unwrap();
        assert_eq!(summary.markdowns_total, 3); // book + 2 chapter langs
        assert_eq!(summary.markdowns_rendered, 3);
        // Rows: 1 book + (1 chapter + 2 sections) × 2 langs = 7.
        assert_eq!(summary.rows_emitted, 7);

        // Per chapter doc: 1 chapter row + 2 section rows.
        let ch_docs: Vec<_> = emitted
            .iter()
            .filter(|d| d.markdown_uuid != book_uuid("1"))
            .collect();
        assert_eq!(ch_docs.len(), 2);
        for d in &ch_docs {
            assert_eq!(d.rows.len(), 3);
            assert!(d.rows[0].kind.starts_with("Chapter ("));
            assert!(d.rows[1].kind.starts_with("Section ("));
            // All rows for a chapter doc share markdown_uuid.
            for r in &d.rows {
                assert_eq!(r.markdown_uuid.as_deref(), Some(d.markdown_uuid.as_str()));
            }
        }
    }

    #[test]
    fn chapter_md_wraps_each_section_in_data_section_uuid_div() {
        let dir = tempfile::tempdir().unwrap();
        render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut |_| Ok(()),
        )
        .unwrap();
        let ch_grc = std::fs::read_to_string(
            dir.path()
                .join("rendered_md/perseus/thucydides/histories/book_01/chapter_001_grc.md"),
        )
        .unwrap();
        let s1 = paragraph_uuid("1", "1", "1", "grc");
        let s2 = paragraph_uuid("1", "1", "2", "grc");
        assert!(
            ch_grc.contains(&format!("data-section-uuid=\"{s1}\"")),
            "section 1 div not found:\n{ch_grc}"
        );
        assert!(ch_grc.contains(&format!("data-section-uuid=\"{s2}\"")));
        assert!(ch_grc.contains("class=\"msg msg--perseus\""));
    }

    #[test]
    fn book_index_md_has_empty_body_and_chapter_edges() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitted: Vec<RenderedMarkdown> = Vec::new();
        render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut |r| {
                emitted.push(r);
                Ok(())
            },
        )
        .unwrap();
        // Body: empty navigation entry (frontmatter + H1, nothing else).
        let idx = std::fs::read_to_string(
            dir.path()
                .join("rendered_md/perseus/thucydides/histories/book_01/index.md"),
        )
        .unwrap();
        assert!(!idx.contains("## Chapters"));
        assert!(!idx.contains("/#/chat/"));

        // Edges: one per (chapter, language). `tiny()` has 1 chapter,
        // so 2 edges (grc + eng), both labeled "chapter".
        let bk = emitted
            .iter()
            .find(|d| d.markdown_uuid == book_uuid("1"))
            .expect("book doc emitted");
        assert_eq!(bk.edges.len(), 2);
        for e in &bk.edges {
            assert_eq!(e.label.as_deref(), Some("chapter"));
            assert!(e.src_anchor_uuid.is_none());
            assert!(e.dst_anchor_uuid.is_none());
            assert_eq!(e.src_markdown_uuid, bk.markdown_uuid);
        }
        let dst_set: std::collections::HashSet<&str> = bk
            .edges
            .iter()
            .map(|e| e.dst_markdown_uuid.as_str())
            .collect();
        assert!(dst_set.contains(chapter_uuid("1", "1", "grc").as_str()));
        assert!(dst_set.contains(chapter_uuid("1", "1", "eng").as_str()));
    }

    #[test]
    fn section_rows_have_per_section_text_and_anchor_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitted: Vec<RenderedMarkdown> = Vec::new();
        render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut |r| {
                emitted.push(r);
                Ok(())
            },
        )
        .unwrap();
        let grc_doc = emitted
            .iter()
            .find(|d| d.markdown_uuid == chapter_uuid("1", "1", "grc"))
            .unwrap();
        let sec_row = &grc_doc.rows[1];
        assert_eq!(sec_row.uuid, paragraph_uuid("1", "1", "1", "grc"));
        assert_eq!(sec_row.kind, "Section (grc)");
        // text holds JUST the section, not the whole chapter.
        assert!(sec_row.text.contains("Θουκυδίδης Ἀθηναῖος"));
        assert!(!sec_row.text.contains("δεύτερον"));
        // qmd_path / markdown_uuid point at the chapter doc — the
        // shared file the deep-link opens.
        assert_eq!(
            sec_row.markdown_uuid.as_deref(),
            Some(grc_doc.markdown_uuid.as_str())
        );
        assert!(sec_row
            .qmd_path
            .as_deref()
            .unwrap()
            .ends_with("/chapter_001_grc.md"));
        assert_eq!(sec_row.message_index, Some(0));
    }

    #[test]
    fn chapter_emits_cross_language_and_per_sentence_edges() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitted: Vec<RenderedMarkdown> = Vec::new();
        render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut |r| {
                emitted.push(r);
                Ok(())
            },
        )
        .unwrap();

        let grc_doc = emitted
            .iter()
            .find(|d| d.markdown_uuid == chapter_uuid("1", "1", "grc"))
            .unwrap();
        let eng_md = chapter_uuid("1", "1", "eng");

        // One cross-language edge (whole-doc) + one bilingual-alignment edge per
        // non-empty bilingual section. tiny() has 2 sections, both bilingual, so
        // we expect 1 + 2 = 3 edges.
        assert_eq!(grc_doc.edges.len(), 3);

        // Doc-level edge from grc → eng carries the *destination's*
        // language as the label, not a generic "cross-language" tag —
        // the UI uses it verbatim as the outgoing-destinations link
        // text.
        let cross = grc_doc
            .edges
            .iter()
            .find(|e| e.label.as_deref() == Some("English"))
            .expect("cross-language edge to English present");
        assert_eq!(cross.src_markdown_uuid, grc_doc.markdown_uuid);
        assert!(cross.src_anchor_uuid.is_none());
        assert_eq!(cross.dst_markdown_uuid, eng_md);
        assert!(cross.dst_anchor_uuid.is_none());

        let aligns: Vec<_> = grc_doc
            .edges
            .iter()
            .filter(|e| e.label.as_deref() == Some("bilingual-alignment"))
            .collect();
        assert_eq!(aligns.len(), 2);
        for e in &aligns {
            assert_eq!(e.src_markdown_uuid, grc_doc.markdown_uuid);
            assert_eq!(e.dst_markdown_uuid, eng_md);
            assert!(e.src_anchor_uuid.is_some());
            assert!(e.dst_anchor_uuid.is_some());
        }
        // First section's sentence-0 anchors on both sides are the
        // anchors the bilingual-alignment edge MUST reference. tiny()
        // sections are single-sentence so sent_idx is always 0.
        let s0_grc = paragraph_sentence_uuid("1", "1", "1", "grc", 0);
        let s0_eng = paragraph_sentence_uuid("1", "1", "1", "eng", 0);
        assert!(aligns
            .iter()
            .any(|e| e.src_anchor_uuid.as_deref() == Some(&s0_grc)
                && e.dst_anchor_uuid.as_deref() == Some(&s0_eng)));
    }

    #[test]
    fn chapter_md_wraps_each_sentence() {
        let dir = tempfile::tempdir().unwrap();
        render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut |_| Ok(()),
        )
        .unwrap();
        let ch_grc = std::fs::read_to_string(
            dir.path()
                .join("rendered_md/perseus/thucydides/histories/book_01/chapter_001_grc.md"),
        )
        .unwrap();
        // tiny()'s sections are single-sentence — each section's
        // whole text is wrapped in one sentence-anchor span (sent
        // index 0). The outer <div data-section-uuid="…"> from
        // section_div_open is the section-level anchor, distinct.
        let s0 = paragraph_sentence_uuid("1", "1", "1", "grc", 0);
        assert!(
            ch_grc.contains(&format!(
                "<span data-section-uuid=\"{s0}\">Θουκυδίδης Ἀθηναῖος.</span>"
            )),
            "sentence span not found in:\n{ch_grc}"
        );
        // The old inline `*Other:* [Thucydides 1.1](/#/chat/…)` line is
        // gone — replaced by an `edges` row at the data layer.
        assert!(
            !ch_grc.contains("*English:*"),
            "old inline cross-language link survived"
        );
    }

    #[test]
    fn second_run_skips_unchanged_docs() {
        let dir = tempfile::tempdir().unwrap();
        let mut on_doc = |_r: RenderedMarkdown| Ok(());
        let first = render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut on_doc,
        )
        .unwrap();
        let mut prior = HashMap::new();
        prior.insert(book_uuid("1"), compute_book_fingerprint(&tiny().books[0]));
        prior.insert(
            chapter_uuid("1", "1", "grc"),
            compute_chapter_fingerprint(&tiny().books[0].chapters[0], "grc"),
        );
        prior.insert(
            chapter_uuid("1", "1", "eng"),
            compute_chapter_fingerprint(&tiny().books[0].chapters[0], "eng"),
        );

        let mut on_doc2 = |_r: RenderedMarkdown| Ok(());
        let second = render_all(
            &tiny(),
            &PerseusAlignments::default(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &prior,
            &mut on_doc2,
        )
        .unwrap();
        assert_eq!(first.markdowns_rendered, 3);
        assert_eq!(second.markdowns_rendered, 0);
        assert_eq!(second.markdowns_skipped, 3);
    }

    /// Exercise the multi-sentence path of `wrap_sentences` +
    /// `chapter_edges` without needing the embedder. Builds a
    /// PerseusAlignments by hand: a 1-grc-sentence section paired
    /// with 2 eng sentences (a 1:2 group), which the Python ref data
    /// shows happens in ~30% of Thucydides sections.
    #[test]
    fn multi_sentence_alignment_emits_cross_product_edges_and_wraps_each_sentence() {
        use crate::translate::align::{SectionAlignment, SentenceGroup};

        // One section: 1 grc sentence, 2 eng sentences. Build the
        // ParsedPerseus and the matching alignment side by side.
        let parsed = ParsedPerseus {
            books: vec![Book {
                n: "1".to_string(),
                chapters: vec![Chapter {
                    book_n: "1".to_string(),
                    n: "1".to_string(),
                    sections: vec![Section {
                        n: "1".to_string(),
                        grc: "Πρώτη φράσις τοῦ τμήματος.".to_string(),
                        eng: "First sentence. Second sentence.".to_string(),
                    }],
                }],
            }],
        };

        let mut by_section = HashMap::new();
        by_section.insert(
            ("1".to_string(), "1".to_string(), "1".to_string()),
            SectionAlignment {
                grc_sentences: super::super::align::split::split_grc(
                    &parsed.books[0].chapters[0].sections[0].grc,
                ),
                eng_sentences: super::super::align::split::split_eng(
                    &parsed.books[0].chapters[0].sections[0].eng,
                ),
                groups: vec![SentenceGroup {
                    grc_indices: vec![0],
                    eng_indices: vec![0, 1],
                }],
            },
        );
        let alignments = PerseusAlignments::from_map(by_section);

        let dir = tempfile::tempdir().unwrap();
        let mut emitted: Vec<RenderedMarkdown> = Vec::new();
        render_all(
            &parsed,
            &alignments,
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut |r| {
                emitted.push(r);
                Ok(())
            },
        )
        .unwrap();

        // Edges on the grc side: 1 cross-language doc edge + the 1:2
        // cross-product = 1*2 = 2 alignment edges, for 3 total.
        let grc_doc = emitted
            .iter()
            .find(|d| d.markdown_uuid == chapter_uuid("1", "1", "grc"))
            .unwrap();
        let aligns: Vec<_> = grc_doc
            .edges
            .iter()
            .filter(|e| e.label.as_deref() == Some("bilingual-alignment"))
            .collect();
        assert_eq!(aligns.len(), 2, "expected 1×2 cross-product = 2 edges");
        let g0 = paragraph_sentence_uuid("1", "1", "1", "grc", 0);
        let e0 = paragraph_sentence_uuid("1", "1", "1", "eng", 0);
        let e1 = paragraph_sentence_uuid("1", "1", "1", "eng", 1);
        assert!(aligns
            .iter()
            .all(|e| e.src_anchor_uuid.as_deref() == Some(&g0)));
        let dst_set: std::collections::HashSet<&str> = aligns
            .iter()
            .filter_map(|e| e.dst_anchor_uuid.as_deref())
            .collect();
        assert!(dst_set.contains(e0.as_str()), "missing edge to eng sent 0");
        assert!(dst_set.contains(e1.as_str()), "missing edge to eng sent 1");

        // The eng-side rendered markdown should wrap each of its two
        // sentences in its own span with distinct anchor UUIDs.
        let eng_md = std::fs::read_to_string(
            dir.path()
                .join("rendered_md/perseus/thucydides/histories/book_01/chapter_001_eng.md"),
        )
        .unwrap();
        assert!(
            eng_md.contains(&format!(
                "<span data-section-uuid=\"{e0}\">First sentence.</span>"
            )),
            "eng sent 0 span missing in:\n{eng_md}"
        );
        assert!(
            eng_md.contains(&format!(
                "<span data-section-uuid=\"{e1}\">Second sentence.</span>"
            )),
            "eng sent 1 span missing in:\n{eng_md}"
        );

        // And the eng-side edges should mirror the cross-product
        // (each of the 2 eng sentences pointing back at the 1 grc
        // sentence) — total 2 bilingual-alignment edges.
        let eng_doc = emitted
            .iter()
            .find(|d| d.markdown_uuid == chapter_uuid("1", "1", "eng"))
            .unwrap();
        let eng_aligns: Vec<_> = eng_doc
            .edges
            .iter()
            .filter(|e| e.label.as_deref() == Some("bilingual-alignment"))
            .collect();
        assert_eq!(eng_aligns.len(), 2, "expected 2×1 cross-product = 2 edges");
        assert!(eng_aligns
            .iter()
            .all(|e| e.dst_anchor_uuid.as_deref() == Some(&g0)));
        let src_set: std::collections::HashSet<&str> = eng_aligns
            .iter()
            .filter_map(|e| e.src_anchor_uuid.as_deref())
            .collect();
        assert!(src_set.contains(e0.as_str()));
        assert!(src_set.contains(e1.as_str()));
    }
}
