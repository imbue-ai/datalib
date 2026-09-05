//! Raw-store schema for the `pdf` provider.

use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

use datalib_etl::bulk::BulkUpsertable;

/// Entity tables truncated and rebuilt by a scan. `pdf_paths` is
/// rebuilt so deletions fall out naturally (a path absent this scan is
/// simply not re-inserted); `pdf_documents` is **not** in this list —
/// see [`DATA_TABLES`] docs below.
pub const DATA_TABLES: &[&str] = &["pdf_paths"];

/// All tables, for DDL.
pub const ALL_TABLES: &[&str] = &["pdf_documents", "pdf_paths", "pdf_scan_meta"];

pub const PDF_DOCUMENTS_DDL: &str = "CREATE TABLE IF NOT EXISTS pdf_documents (
    blake3                   TEXT PRIMARY KEY,
    size                     INTEGER NOT NULL,
    page_count               INTEGER NOT NULL,
    pdf_type                 TEXT NOT NULL,
    confidence               REAL NOT NULL,
    needs_ocr                INTEGER NOT NULL,
    ocr_page_count           INTEGER NOT NULL,
    has_encoding_issues      INTEGER NOT NULL,
    title                    TEXT NULL,
    author                   TEXT NULL,
    doc_created_at           TEXT NULL,
    doc_modified_at          TEXT NULL,
    content_blake3           TEXT NULL,
    pdf_id_permanent         TEXT NULL,
    xmp_document_id          TEXT NULL,
    xmp_instance_id          TEXT NULL,
    xmp_original_document_id TEXT NULL,
    first_seen_at            TEXT NOT NULL
)";

/// Lineage lookups (`WHERE xmp_document_id = ?`) are point queries
/// against a column with no other access path, and unlike fsindex we
/// are at document scale (thousands of rows, not tens of millions), so
/// the index-size argument that rules them out there does not apply.
pub const PDF_DOCUMENTS_INDEXES: &[&str] = &[
    // The Ship-of-Theseus lookup this table now actually supports:
    // every byte-variant of one document. Unlike the two below it, this
    // column is populated for every parseable PDF, so the index earns
    // its keep rather than covering the 3-in-20 that carry XMP.
    "CREATE INDEX IF NOT EXISTS idx_pdf_documents_content \
     ON pdf_documents (content_blake3)",
    "CREATE INDEX IF NOT EXISTS idx_pdf_documents_xmp_doc \
     ON pdf_documents (xmp_document_id)",
    "CREATE INDEX IF NOT EXISTS idx_pdf_documents_pdf_id \
     ON pdf_documents (pdf_id_permanent)",
];

pub const PDF_PATHS_DDL: &str = "CREATE TABLE IF NOT EXISTS pdf_paths (
    id          TEXT PRIMARY KEY,
    blake3      TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
)";

/// Where the scan actually ran.
pub const PDF_SCAN_META_DDL: &str = "CREATE TABLE IF NOT EXISTS pdf_scan_meta (
    id           TEXT PRIMARY KEY,
    abs_root     TEXT NOT NULL,
    scanned_at   TEXT NOT NULL
)";

pub const PDF_PATHS_INDEXES: &[&str] = &[
    // The render side walks documents and needs their paths; the grid
    // row's `source_url` wants one representative location per doc.
    "CREATE INDEX IF NOT EXISTS idx_pdf_paths_blake3 ON pdf_paths (blake3)",
];

pub fn full_ddl() -> Vec<String> {
    let mut out = vec![
        PDF_DOCUMENTS_DDL.to_string(),
        PDF_PATHS_DDL.to_string(),
        PDF_SCAN_META_DDL.to_string(),
    ];
    out.extend(PDF_DOCUMENTS_INDEXES.iter().map(|s| s.to_string()));
    out.extend(PDF_PATHS_INDEXES.iter().map(|s| s.to_string()));
    out
}

/// How the classifier read a document. Round-trips through the stored
/// string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfKind {
    /// Has extractable text operators. Convertible today.
    TextBased,
    /// Image-only. Needs OCR; recorded and skipped.
    Scanned,
    /// Mostly images, minimal text.
    ImageBased,
    /// Some pages have text, some don't. Converted for the pages that
    /// do, with the rest recorded in `ocr_page_count`.
    Mixed,
}

impl PdfKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PdfKind::TextBased => "text_based",
            PdfKind::Scanned => "scanned",
            PdfKind::ImageBased => "image_based",
            PdfKind::Mixed => "mixed",
        }
    }
}

pub fn document_is_renderable(
    page_count: i64,
    ocr_page_count: i64,
    has_encoding_issues: bool,
) -> bool {
    !has_encoding_issues && page_count > ocr_page_count
}

/// One row in [`PDF_DOCUMENTS_DDL`].
#[derive(Debug, Clone)]
pub struct PdfDocumentRow {
    /// Lowercase hex blake3 of the file bytes. The document's identity.
    pub blake3: String,
    pub size: i64,
    pub page_count: i64,
    pub pdf_type: PdfKind,
    pub confidence: f64,
    /// True when at least one page carries no extractable text.
    pub needs_ocr: bool,
    pub ocr_page_count: i64,
    pub has_encoding_issues: bool,
    pub title: Option<String>,
    /// Info `/Author` or XMP `dc:creator`. Usually NULL — see
    /// [`super::identity::DocIdentity::author`].
    pub author: Option<String>,
    pub doc_created_at: Option<String>,
    pub doc_modified_at: Option<String>,
    /// Hash over the document's *content* — every object reachable from
    /// the catalog, metadata stripped — so retitling a PDF or letting a
    /// tool regenerate its trailer `/ID` does not read as a new
    /// document. `None` when the file is encrypted or unparseable. See
    /// [`super::content_hash`], and §"Ship of Theseus" below for why it
    /// is still a hint and not the key.
    pub content_blake3: Option<String>,
    pub pdf_id_permanent: Option<String>,
    pub xmp_document_id: Option<String>,
    pub xmp_instance_id: Option<String>,
    pub xmp_original_document_id: Option<String>,
    pub first_seen_at: String,
}

impl BulkUpsertable for PdfDocumentRow {
    const TABLE: &'static str = "pdf_documents";
    // Not the framework's usual `id`: this table's key IS the content
    // hash, and calling the column `blake3` keeps that legible in every
    // ad-hoc query and join against `pdf_paths.blake3`.
    const ID_COLUMN: &'static str = "blake3";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "size",
        "page_count",
        "pdf_type",
        "confidence",
        "needs_ocr",
        "ocr_page_count",
        "has_encoding_issues",
        "title",
        "author",
        "doc_created_at",
        "doc_modified_at",
        "content_blake3",
        "pdf_id_permanent",
        "xmp_document_id",
        "xmp_instance_id",
        "xmp_original_document_id",
        "first_seen_at",
    ];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.blake3
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.blake3)
            .bind(self.size)
            .bind(self.page_count)
            .bind(self.pdf_type.as_str())
            .bind(self.confidence)
            .bind(i64::from(self.needs_ocr))
            .bind(self.ocr_page_count)
            .bind(i64::from(self.has_encoding_issues))
            .bind(self.title.as_deref())
            .bind(self.author.as_deref())
            .bind(self.doc_created_at.as_deref())
            .bind(self.doc_modified_at.as_deref())
            .bind(self.content_blake3.as_deref())
            .bind(self.pdf_id_permanent.as_deref())
            .bind(self.xmp_document_id.as_deref())
            .bind(self.xmp_instance_id.as_deref())
            .bind(self.xmp_original_document_id.as_deref())
            .bind(&self.first_seen_at)
    }
}

/// One row in [`PDF_PATHS_DDL`].
#[derive(Debug, Clone)]
pub struct PdfPathRow {
    /// Root-relative, slash-separated path.
    pub id: String,
    /// Hex blake3 of the bytes at this path — the FK into
    /// `pdf_documents`.
    pub blake3: String,
    pub last_seen_at: String,
}

impl BulkUpsertable for PdfPathRow {
    const TABLE: &'static str = "pdf_paths";
    const TYPED_COLUMNS: &'static [&'static str] = &["blake3", "last_seen_at"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id).bind(&self.blake3).bind(&self.last_seen_at)
    }
}

/// One row in [`PDF_SCAN_META_DDL`].
#[derive(Debug, Clone)]
pub struct PdfScanMetaRow {
    /// The source name from config (`tng_pdfs`), not the path.
    pub id: String,
    pub abs_root: String,
    pub scanned_at: String,
}

impl BulkUpsertable for PdfScanMetaRow {
    const TABLE: &'static str = "pdf_scan_meta";
    const TYPED_COLUMNS: &'static [&'static str] = &["abs_root", "scanned_at"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id).bind(&self.abs_root).bind(&self.scanned_at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_kind_strings_round_trip_the_documented_values() {
        // These strings are on disk; changing one is a schema change.
        assert_eq!(PdfKind::TextBased.as_str(), "text_based");
        assert_eq!(PdfKind::Scanned.as_str(), "scanned");
        assert_eq!(PdfKind::ImageBased.as_str(), "image_based");
        assert_eq!(PdfKind::Mixed.as_str(), "mixed");
    }

    #[test]
    fn a_document_renders_when_any_page_carries_text() {
        // The #173 case: three scanned inserts in a 200-page report
        // must not cost the other 197 pages.
        assert!(document_is_renderable(200, 3, false));
        // ...and the degenerate ends still behave.
        assert!(document_is_renderable(1, 0, false));
        assert!(!document_is_renderable(4, 4, false), "fully scanned");
        assert!(!document_is_renderable(0, 0, false), "no pages at all");
    }

    #[test]
    fn encoding_issues_suppress_a_document_that_would_otherwise_render() {
        // Mojibake reads as text to everything downstream, so it would
        // be indexed and searched as if it meant something. A gap is
        // recoverable; a poisoned index is not.
        assert!(!document_is_renderable(200, 3, true));
        assert!(!document_is_renderable(1, 0, true));
    }

    #[test]
    fn documents_table_is_not_truncated_between_scans() {
        // Truncating it would drop `first_seen_at` and re-convert every
        // document whose path merely moved. Only the path table is
        // rebuilt.
        assert_eq!(DATA_TABLES, &["pdf_paths"]);
        assert!(ALL_TABLES.contains(&"pdf_documents"));
    }

    #[test]
    fn bound_column_counts_match_the_ddl() {
        // A mismatch here binds values into the wrong columns, which
        // sqlite will happily accept for same-typed neighbours.
        let doc_cols = PdfDocumentRow::TYPED_COLUMNS.len() + 1; // + PK
        assert_eq!(doc_cols, PDF_DOCUMENTS_DDL.matches(',').count() + 1);
        let path_cols = PdfPathRow::TYPED_COLUMNS.len() + 1;
        assert_eq!(path_cols, PDF_PATHS_DDL.matches(',').count() + 1);
        let meta_cols = PdfScanMetaRow::TYPED_COLUMNS.len() + 1;
        assert_eq!(meta_cols, PDF_SCAN_META_DDL.matches(',').count() + 1);
    }
}
