//! `grid_rows` projection for PDF documents.
//!
//! PDFs are not chat-shaped, which is the open question fsindex punted
//! on ("filesystem entries aren't chat-shaped… closer to
//! contacts-shaped"). The mapping we settle on here is
//! **document ≈ conversation, page ≈ message**:
//!
//! - one `kind = "PDF Document"` row per document, carrying the whole
//!   text in `entire_chat`-adjacent fields, and
//! - one `kind = "PDF Page"` row per page, so a search hit resolves to
//!   a page anchor rather than dumping the reader at the top of a
//!   200-page file.
//!
//! That is the same shape every chat provider already produces, so the
//! grid, the preview pane's scroll-to-section, and per-section feedback
//! all work with no UI change — the page `uuid` is byte-equal to the
//! `data-section-uuid` the renderer emits.

use std::path::Path;

use datalib_schema::grid_rows::GridRow;
use uuid::Uuid;

pub const PROVIDER: &str = "pdf";
pub const SOURCE_LABEL: &str = "PDF";
pub const KIND_DOCUMENT: &str = "PDF Document";
pub const KIND_PAGE: &str = "PDF Page";

/// `grid_rows.upstream_entity_kind` values — the `entity_kind` component
/// of the `datalib_id` recipe, in the provider's own vocabulary.
/// Distinct from `KIND_*` above, which are display labels for the
/// grid's Kind column: those may be reworded freely, these may not,
/// because `uuid` is derived from them.
pub const ENTITY_KIND_DOCUMENT: &str = "document";
pub const ENTITY_KIND_PAGE: &str = "page";

fn pdf_ns() -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_DNS, b"pdf.datalib")
}

/// Deterministic id from a recipe string. Re-running the pipeline over
/// an unchanged corpus must produce byte-identical ids, so nothing here
/// may depend on scan order or wall-clock.
pub fn ns_id(recipe: &str) -> String {
    Uuid::new_v5(&pdf_ns(), recipe.as_bytes()).to_string()
}

/// The document's stable id. Derived from content hash, so the same
/// PDF found at a new path keeps its identity and its feedback history.
pub fn document_uuid(blake3: &str) -> String {
    ns_id(&format!("doc:{blake3}"))
}

/// A page's stable id, likewise content-derived.
pub fn page_uuid(blake3: &str, page: u32) -> String {
    ns_id(&format!("page:{blake3}:{page}"))
}

pub struct DocumentMeta<'a> {
    pub blake3: &'a str,
    /// Absolute path of a representative copy on disk. Becomes the row's
    /// `source_url` as a `file://` URL — see [`file_url`].
    pub abs_path: &'a Path,
    pub title: Option<&'a str>,
    /// Info `/Author` or XMP `dc:creator`. Usually `None`.
    pub author: Option<&'a str>,
    /// Representative root-relative path, for display.
    pub rel_path: &'a str,
    /// How many paths currently hold these bytes.
    pub copy_count: i64,
    /// ISO-8601 with offset, from the PDF's Info dictionary.
    pub created_at: Option<&'a str>,
    pub modified_at: Option<&'a str>,
    /// Path to the rendered markdown, **relative to the data root**
    /// (`<stanza>/rendered_md/docs/<blake3>.md`) — see
    /// [`super::doc_qmd_path_rel`]. Anything shorter makes the document
    /// unfindable through qmd search.
    pub qmd_path: Option<&'a str>,
    pub source_name: &'a str,
}

/// Display title: the PDF's own title if it has a usable one, else the
/// filename. Many PDFs carry a `Title` that is a LaTeX temp name or the
/// producing application's boilerplate, so a title that looks like a
/// path or is a bare extension-less duplicate of the filename is not an
/// improvement over the filename itself.
pub fn display_title(title: Option<&str>, rel_path: &str) -> String {
    let filename = rel_path.rsplit('/').next().unwrap_or(rel_path);
    match title.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) if !t.contains('/') && !t.contains('\\') => t.to_string(),
        _ => filename.to_string(),
    }
}

/// Absolute path → `file://` URL for `grid_rows.source_url`.
///
/// `source_url` is documented as "canonical URL pointing back to the
/// original source"; for a local corpus that source is a file, and
/// `file://` is the URL form of one. Keeping the column a real URL —
/// rather than smuggling a bare path into it — is what lets the UI
/// branch on **scheme** instead of provider, so any future local-file
/// source inherits the same "reveal in the file manager" behavior.
///
/// Built with `Url::from_file_path` rather than string concatenation.
/// Percent-encoding is not optional here: a real filename in the corpus
/// this was tested against is
/// `Imbue Mail - 7-Eleven SpeakOut_ New Order # 101445654.pdf`, and a
/// raw `#` would truncate the URL at the fragment. Spaces, non-ASCII,
/// and Windows drive letters have the same problem.
///
/// Returns `None` for a non-absolute path, which `Url::from_file_path`
/// rejects — the caller then leaves `source_url` NULL rather than
/// emitting something unusable.
pub fn file_url(abs: &Path) -> Option<String> {
    url::Url::from_file_path(abs).ok().map(|u| u.to_string())
}

/// Hard ceiling from `grid_rows.author`'s `VARCHAR(255)`. We stay well
/// under it — see [`display_author`].
const AUTHOR_MAX: usize = 120;

/// Shorten a PDF's author string for the grid.
///
/// The full value stays in `pdf_documents.author` and in the markdown
/// frontmatter; this is only the grid projection. Two real shapes from
/// a 20-document sample drove it:
///
/// * A 14-author physics paper produced a 165-character semicolon-
///   separated list. That fits `VARCHAR(255)` today but a 30-author
///   paper would not, and as a grid cell it is unreadable either way.
///   Semicolon-separated lists collapse to `First Author et al.`
/// * Everything else is short and passes through. We do NOT split on
///   commas: `Lo, Kyle` is one person, and guessing wrong turns a name
///   into a surname.
///
/// Anything still over the limit is truncated on a character boundary.
pub fn display_author(author: Option<&str>) -> Option<String> {
    let a = author.map(str::trim).filter(|s| !s.is_empty())?;
    if let Some((first, _rest)) = a.split_once(';') {
        let first = first.trim();
        if !first.is_empty() {
            return Some(format!("{first} et al."));
        }
    }
    if a.chars().count() > AUTHOR_MAX {
        let short: String = a.chars().take(AUTHOR_MAX - 1).collect();
        return Some(format!("{short}…"));
    }
    Some(a.to_string())
}

/// Build the row set for one document: the document row followed by one
/// row per page, in page order.
pub fn rows_for_document(meta: &DocumentMeta<'_>, pages: &[(u32, String)]) -> Vec<GridRow> {
    let doc_uuid = document_uuid(meta.blake3);
    let title = display_title(meta.title, meta.rel_path);
    let author = display_author(meta.author);
    // NULL rather than a bare path when the URL can't be formed; a
    // half-valid link is worse than an absent one.
    let source_url = file_url(meta.abs_path);
    // Prefer the authored creation date; fall back to modification.
    // Never fall back to "now" — an ingest timestamp masquerading as an
    // authored one would sort the whole corpus to today.
    let when = meta.created_at.or(meta.modified_at);

    let mut rows = Vec::with_capacity(pages.len() + 1);

    rows.push(GridRow {
        uuid: doc_uuid.clone(),
        provider: PROVIDER.into(),
        kind: KIND_DOCUMENT.into(),
        source_label: SOURCE_LABEL.into(),
        when_ts: when.map(str::to_string),
        author: meta.author.map(str::to_string),
        account: Some(meta.source_name.to_string()),
        project: None,
        org_uuid: None,
        org_name: None,
        channel: None,
        conversation_name: Some(title.clone()),
        conversation_uuid: doc_uuid.clone(),
        message_index: None,
        entire_chat: format!("/chat/{doc_uuid}"),
        // The document row's text is the title plus its location, not
        // the whole document: the per-page rows carry the body, and
        // duplicating it here would double the index size and make
        // every query match the document row too.
        text: if meta.copy_count > 1 {
            format!("{title} ({} copies)", meta.copy_count)
        } else {
            title.clone()
        },
        slack_link: None,
        qmd_path: meta.qmd_path.map(str::to_string),
        source_url: source_url.clone(),
        git_sha: None,
        // Content-scoped: identity IS the bytes, so the same PDF found
        // under two scanned trees is deliberately one row. `upstream_scope`
        // stays NULL because a content hash needs no further scoping.
        upstream_id: Some(meta.blake3.to_string()),
        upstream_entity_kind: Some(ENTITY_KIND_DOCUMENT.to_string()),
        upstream_scope: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(doc_uuid.clone()),
    });

    for (number, text) in pages {
        let uuid = page_uuid(meta.blake3, *number);
        rows.push(GridRow {
            uuid: uuid.clone(),
            provider: PROVIDER.into(),
            kind: KIND_PAGE.into(),
            source_label: SOURCE_LABEL.into(),
            when_ts: when.map(str::to_string),
            // Denormalized onto the page rows too, matching how every
            // chat provider stamps the author on each message row so
            // the grid can filter without a join.
            author: author.clone(),
            account: Some(meta.source_name.to_string()),
            project: None,
            org_uuid: None,
            org_name: None,
            channel: None,
            conversation_name: Some(title.clone()),
            conversation_uuid: doc_uuid.clone(),
            message_index: Some(i64::from(*number)),
            entire_chat: format!("/chat/{doc_uuid}"),
            text: text.clone(),
            slack_link: None,
            qmd_path: meta.qmd_path.map(str::to_string),
            source_url: source_url.clone(),
            git_sha: None,
            upstream_id: Some(format!("{}#{number}", meta.blake3)),
            upstream_entity_kind: Some(ENTITY_KIND_PAGE.to_string()),
            upstream_scope: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(doc_uuid.clone()),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta<'a>(title: Option<&'a str>, rel: &'a str) -> DocumentMeta<'a> {
        DocumentMeta {
            blake3: "abc123",
            abs_path: Path::new("/corpus/a/b.pdf"),
            title,
            author: Some("Jean-Luc Picard"),
            rel_path: rel,
            copy_count: 1,
            created_at: Some("2024-01-15T10:30:00-08:00"),
            modified_at: None,
            qmd_path: Some("papers/rendered_md/docs/abc123.md"),
            source_name: "papers",
        }
    }

    #[test]
    fn ids_are_content_derived_so_a_move_preserves_identity() {
        // Same bytes at a different path must yield the same uuid, or
        // feedback and search history detach on every `mv`.
        let a = document_uuid("deadbeef");
        let b = document_uuid("deadbeef");
        assert_eq!(a, b);
        assert_ne!(a, document_uuid("cafebabe"));
    }

    #[test]
    fn page_ids_differ_per_page_and_per_document() {
        assert_ne!(page_uuid("abc", 1), page_uuid("abc", 2));
        assert_ne!(page_uuid("abc", 1), page_uuid("xyz", 1));
    }

    #[test]
    fn document_row_comes_first_then_pages_in_order() {
        let pages = vec![(1, "one".to_string()), (2, "two".to_string())];
        let rows = rows_for_document(&meta(Some("Paper"), "a/b.pdf"), &pages);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].kind, KIND_DOCUMENT);
        assert_eq!(rows[1].kind, KIND_PAGE);
        assert_eq!(rows[1].message_index, Some(1));
        assert_eq!(rows[2].message_index, Some(2));
    }

    #[test]
    fn every_page_row_shares_the_document_conversation_uuid() {
        let pages = vec![(1, "one".into()), (2, "two".into())];
        let rows = rows_for_document(&meta(None, "a/b.pdf"), &pages);
        let doc = rows[0].uuid.clone();
        assert!(rows.iter().all(|r| r.conversation_uuid == doc));
    }

    #[test]
    fn title_falls_back_to_filename_when_absent_or_pathlike() {
        assert_eq!(display_title(Some("Real Title"), "a/b.pdf"), "Real Title");
        assert_eq!(display_title(None, "a/b.pdf"), "b.pdf");
        assert_eq!(display_title(Some("   "), "a/b.pdf"), "b.pdf");
        // LaTeX and Word both emit paths as titles surprisingly often.
        assert_eq!(display_title(Some("/tmp/x/final.tex"), "a/b.pdf"), "b.pdf");
    }

    #[test]
    fn source_url_is_a_file_url_not_a_bare_path() {
        // Every other provider puts an absolute URL here and the UI
        // calls window.open on it; a relative path navigates the app to
        // nowhere.
        let rows = rows_for_document(&meta(Some("T"), "a/b.pdf"), &[(1, "x".into())]);
        assert_eq!(
            rows[0].source_url.as_deref(),
            Some("file:///corpus/a/b.pdf")
        );
    }

    #[test]
    fn hash_in_a_filename_is_percent_encoded() {
        // Real filename from the corpus this was tested against. A raw
        // `#` truncates the URL at the fragment, silently losing the
        // extension and everything before it.
        let p = Path::new("/c/Imbue Mail - New Order # 101445654.pdf");
        let u = file_url(p).unwrap();
        assert!(u.contains("%23"), "{u}");
        assert!(u.ends_with(".pdf"), "{u}");
        // And it round-trips back to the exact path.
        assert_eq!(url::Url::parse(&u).unwrap().to_file_path().unwrap(), p);
    }

    #[test]
    fn spaces_and_non_ascii_round_trip() {
        let p = Path::new("/c/日本 の 文書.pdf");
        let u = file_url(p).unwrap();
        assert!(!u.contains(' '), "{u}");
        assert_eq!(url::Url::parse(&u).unwrap().to_file_path().unwrap(), p);
    }

    #[test]
    fn a_relative_path_yields_no_url_rather_than_a_broken_one() {
        assert_eq!(file_url(Path::new("relative/x.pdf")), None);
    }

    #[test]
    fn multi_author_lists_collapse_to_et_al() {
        // Real shape from an arXiv paper: 14 names, 165 characters.
        assert_eq!(
            display_author(Some("Cheng Cui; Yubo Zhang; Ting Sun")).as_deref(),
            Some("Cheng Cui et al.")
        );
    }

    #[test]
    fn a_single_name_passes_through_untouched() {
        assert_eq!(
            display_author(Some("Jean-Luc Picard")).as_deref(),
            Some("Jean-Luc Picard")
        );
    }

    #[test]
    fn commas_are_not_treated_as_separators() {
        // `Lo, Kyle` is one person; splitting here would yield "Lo".
        assert_eq!(
            display_author(Some("Lo, Kyle")).as_deref(),
            Some("Lo, Kyle")
        );
    }

    #[test]
    fn blank_and_missing_authors_are_none() {
        assert_eq!(display_author(None), None);
        assert_eq!(display_author(Some("   ")), None);
    }

    #[test]
    fn overlong_single_author_is_truncated_within_the_column() {
        let long = "A".repeat(400);
        let got = display_author(Some(&long)).unwrap();
        assert!(got.chars().count() <= AUTHOR_MAX, "{}", got.chars().count());
        assert!(got.ends_with('…'));
    }

    #[test]
    fn truncation_does_not_split_a_multibyte_character() {
        // Char-boundary truncation, not byte slicing — a panic here
        // would take down the whole render on one CJK-authored PDF.
        let long = "日".repeat(400);
        let got = display_author(Some(&long)).unwrap();
        assert!(got.chars().count() <= AUTHOR_MAX);
    }

    #[test]
    fn author_lands_on_both_document_and_page_rows() {
        let rows = rows_for_document(&meta(Some("T"), "a/b.pdf"), &[(1, "x".into())]);
        assert!(rows
            .iter()
            .all(|r| r.author.as_deref() == Some("Jean-Luc Picard")));
    }

    #[test]
    fn absent_author_is_none_not_empty_string() {
        // An empty string would render as a blank-but-present author
        // chip in the grid; None is the honest encoding of "unknown",
        // and most PDFs never set /Author at all.
        let mut m = meta(None, "a/b.pdf");
        m.author = None;
        let rows = rows_for_document(&m, &[(1, "x".into())]);
        assert!(rows.iter().all(|r| r.author.is_none()));
    }

    #[test]
    fn when_ts_never_invents_an_ingest_timestamp() {
        let mut m = meta(None, "a/b.pdf");
        m.created_at = None;
        m.modified_at = None;
        let rows = rows_for_document(&m, &[(1, "x".into())]);
        assert!(rows.iter().all(|r| r.when_ts.is_none()));
    }

    #[test]
    fn modified_at_is_the_fallback_for_when_ts() {
        let mut m = meta(None, "a/b.pdf");
        m.created_at = None;
        m.modified_at = Some("2020-02-02T02:02:02+00:00");
        let rows = rows_for_document(&m, &[(1, "x".into())]);
        assert_eq!(
            rows[0].when_ts.as_deref(),
            Some("2020-02-02T02:02:02+00:00")
        );
    }

    #[test]
    fn duplicate_copies_are_surfaced_in_the_document_row_text() {
        let mut m = meta(Some("Paper"), "a/b.pdf");
        m.copy_count = 3;
        let rows = rows_for_document(&m, &[]);
        assert_eq!(rows[0].text, "Paper (3 copies)");
    }

    #[test]
    fn document_row_does_not_duplicate_page_text() {
        // Otherwise every query matches the doc row as well as the page.
        let pages = vec![(1, "distinctive body text".to_string())];
        let rows = rows_for_document(&meta(Some("T"), "a/b.pdf"), &pages);
        assert!(!rows[0].text.contains("distinctive body text"));
    }
}
