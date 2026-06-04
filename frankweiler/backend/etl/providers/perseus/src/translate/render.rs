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
use frankweiler_schema::grid_rows::GridRow;

use super::super::{
    book_uuid, chapter_uuid, paragraph_uuid, TLG0003_TLG001, WORK_SHORT, WORK_TITLE,
};
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
pub fn render_all(
    parsed: &ParsedPerseus,
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
    write_sidecar(&sidecar_path, &m_uuid, &fingerprint, &rows)?;

    summary.rows_emitted += rows.len();
    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
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

    let md = render_chapter_md(chapter, lang);
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
    write_sidecar(&sidecar_path, &m_uuid, &fingerprint, &rows)?;

    summary.rows_emitted += rows.len();
    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
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
    let title = book_title(&book.n);
    let mut out = format!(
        "---\n\
         provider: perseus\n\
         work: {WORK_TITLE}\n\
         edition: {TLG0003_TLG001}\n\
         book: {book_n}\n\
         title: {title}\n\
         ---\n\
         \n\
         # {title}\n\
         \n\
         {n_chapters} chapters.\n\
         \n\
         ## Chapters\n\
         \n\
         | # | Greek | English |\n\
         |---|-------|---------|\n",
        book_n = book.n,
        n_chapters = book.chapters.len(),
    );
    for chapter in &book.chapters {
        let bi: u32 = book.n.parse().unwrap_or(0);
        let ci: u32 = chapter.n.parse().unwrap_or(0);
        let grc = chapter_uuid(&book.n, &chapter.n, "grc");
        let eng = chapter_uuid(&book.n, &chapter.n, "eng");
        // SPA uses createWebHashHistory(); routes live under /#/<path>.
        out.push_str(&format!(
            "| {bi}.{ci} | [{WORK_SHORT} {bi}.{ci}](/#/chat/{grc}) | [{WORK_SHORT} {bi}.{ci}](/#/chat/{eng}) |\n"
        ));
    }
    out.push('\n');
    out
}

fn render_chapter_md(chapter: &Chapter, lang: &str) -> String {
    let title = chapter_title(&chapter.book_n, &chapter.n);
    let other = if lang == "grc" { "eng" } else { "grc" };
    let other_uuid = chapter_uuid(&chapter.book_n, &chapter.n, other);
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
         \n\
         *{other_label}:* [{title}](/#/chat/{other_uuid})\n\
         \n",
        book_n = chapter.book_n,
        ch_n = chapter.n,
        lang_label = lang_label(lang),
        other_label = lang_label(other),
    );
    for sec in &chapter.sections {
        let text = if lang == "grc" { &sec.grc } else { &sec.eng };
        if text.is_empty() {
            continue;
        }
        let s_uuid = paragraph_uuid(&chapter.book_n, &chapter.n, &sec.n, lang);
        // Blank lines around the `<div>` / `</div>` so the markdown
        // parser inside doesn't get confused — common-mark allows
        // raw HTML blocks but needs the surrounding blank lines to
        // treat the inner content as markdown again.
        out.push_str(&section_div_open(&s_uuid));
        out.push_str("\n\n");
        out.push_str(&format!(
            "### {}.{}.{}\n\n{}\n\n",
            chapter.book_n, chapter.n, sec.n, text
        ));
        out.push_str(SECTION_DIV_CLOSE);
        out.push_str("\n\n");
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
    let mut out = format!(
        "{} — {} chapters.",
        book_title(&book.n),
        book.chapters.len()
    );
    for chapter in &book.chapters {
        out.push_str(&format!("\n{}", chapter_title(&book.n, &chapter.n)));
    }
    out
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
        project: Some(WORK_TITLE.to_string()),
        channel: None,
        conversation_name: Some(chapter_title(&chapter.book_n, &chapter.n)),
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
        project: Some(WORK_TITLE.to_string()),
        channel: None,
        conversation_name: Some(chapter_title(&chapter.book_n, &chapter.n)),
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
) -> Result<()> {
    let payload = serde_json::json!({
        "header": {
            "markdown_uuid": markdown_uuid,
            "source_fingerprint": fingerprint,
            "render_version": RENDER_VERSION,
        },
        "rows": rows,
    });
    fs::write(path, serde_json::to_string_pretty(&payload)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
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
    fn book_index_md_has_chapter_table() {
        let dir = tempfile::tempdir().unwrap();
        render_all(
            &tiny(),
            dir.path(),
            "perseus",
            &Progress::noop(),
            &HashMap::new(),
            &mut |_| Ok(()),
        )
        .unwrap();
        let idx = std::fs::read_to_string(
            dir.path()
                .join("rendered_md/perseus/thucydides/histories/book_01/index.md"),
        )
        .unwrap();
        assert!(idx.contains("## Chapters"));
        assert!(idx.contains("| Greek | English |"));
        // One row per chapter with deep-links into both languages.
        let grc = chapter_uuid("1", "1", "grc");
        let eng = chapter_uuid("1", "1", "eng");
        assert!(idx.contains(&format!("/#/chat/{grc}")));
        assert!(idx.contains(&format!("/#/chat/{eng}")));
    }

    #[test]
    fn section_rows_have_per_section_text_and_anchor_uuid() {
        let dir = tempfile::tempdir().unwrap();
        let mut emitted: Vec<RenderedMarkdown> = Vec::new();
        render_all(
            &tiny(),
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
    fn second_run_skips_unchanged_docs() {
        let dir = tempfile::tempdir().unwrap();
        let mut on_doc = |_r: RenderedMarkdown| Ok(());
        let first = render_all(
            &tiny(),
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
}
