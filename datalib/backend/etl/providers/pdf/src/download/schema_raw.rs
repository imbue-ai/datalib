//! Raw-store schema for the `pdf` provider.
//!
//! # Why two tables, and why the PK is a hash
//!
//! The question this provider answers is "what documents do I have?",
//! not "what files are on this disk." Those differ whenever the same
//! PDF exists twice — and in a personal corpus it almost always does:
//! the paper in `~/Downloads`, the copy in a Dropbox folder, the one
//! saved out of an email. Keying on path would convert, store, and
//! index that document three times, and a `mv` would read as a delete
//! plus an add.
//!
//! So the content entity is keyed on **`blake3(bytes)`**, and locations
//! hang off it:
//!
//! - [`PDF_DOCUMENTS_DDL`] — PK `blake3` (lowercase hex). One row per
//!   distinct document: page count, classification, extracted
//!   metadata, and the lineage hints below. A `mv` does not touch it;
//!   a byte change produces a new row.
//! - [`PDF_PATHS_DDL`] — PK `id` (root-relative path), FK `blake3`.
//!   Where copies live, plus Unison's `(mtime, size, inode, dev)`
//!   rescan cursor so an unchanged file skips the read.
//!
//! This mirrors fsindex's `files` / `file_stats` split and is motivated
//! the same way (see its `DOWNLOAD.md` §"Why two entity tables"): one
//! table changes only when content does, the other churns every scan,
//! and mixing them makes `dolt diff` noise-dominated. The difference is
//! which way the arrow points — fsindex keys both on path because *the
//! tree* is its subject; we key content on the hash because *the
//! document* is ours.
//!
//! # Ship of Theseus: lineage is a hint, never a key
//!
//! PDFs do carry identifiers. The trailer `/ID` array's first element
//! is spec'd to be permanent for the document's lifetime, and XMP Media
//! Management defines `xmpMM:DocumentID` (stable across edits),
//! `xmpMM:InstanceID` (fresh per save), and `xmpMM:OriginalDocumentID`
//! (the ancestor). Semantically that is exactly the versioning model we
//! want.
//!
//! It is also unreliable in both directions. Only Adobe-lineage tooling
//! emits XMP MM consistently — scanners, LaTeX, and browser
//! print-to-PDF mostly omit it — and `cp` duplicates whatever is there,
//! so two files can claim one `DocumentID`. That is the same trap
//! fsindex documented for its `.fsindex.yaml` breadcrumbs under the
//! heading "The UUID is not unique," and we take the same position:
//! these columns are **indexed secondary hints, not keys**. "Show me
//! every version of this document" is
//!
//! ```sql
//! SELECT blake3, title, doc_modified_at FROM pdf_documents
//!  WHERE xmp_document_id = ? ORDER BY doc_modified_at;
//! ```
//!
//! and the time axis comes free from `dolt_log` / `dolt_diff` over the
//! blake3-keyed rows, so no separate version table is needed.
//!
//! We deliberately do **not** stamp identity into the PDFs themselves.
//! fsindex stamps directories and explicitly refuses to stamp files;
//! for us it would be worse, since writing a breadcrumb into a document
//! changes its bytes and therefore its primary key.

use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::fswalk::StampKind;

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
    doc_created_at           TEXT NULL,
    doc_modified_at          TEXT NULL,
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
    "CREATE INDEX IF NOT EXISTS idx_pdf_documents_xmp_doc \
     ON pdf_documents (xmp_document_id)",
    "CREATE INDEX IF NOT EXISTS idx_pdf_documents_pdf_id \
     ON pdf_documents (pdf_id_permanent)",
];

pub const PDF_PATHS_DDL: &str = "CREATE TABLE IF NOT EXISTS pdf_paths (
    id          TEXT PRIMARY KEY,
    blake3      TEXT NOT NULL,
    mtime_ns    INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    stamp_kind  TEXT NOT NULL,
    inode       INTEGER NULL,
    dev         INTEGER NULL,
    last_seen_at TEXT NOT NULL
)";

/// Where the scan actually ran.
///
/// `pdf_paths.id` is root-relative — that is what keeps a moved data
/// root from rewriting every row — so *something* has to remember the
/// absolute root, or the render step cannot open the files. Recording
/// it here rather than re-reading `input_path` from the render step's
/// config means the two can never disagree: render converts exactly
/// the tree that was scanned, even if the config was edited in between.
/// Same reasoning as fsindex's `scan_meta`, and keyed the same way — on
/// the source name from config, not the path, so the row survives a
/// move of the root.
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

    /// Whether this classification yields text worth rendering without
    /// an OCR engine. `Mixed` counts: the text pages still convert, and
    /// the untouched pages are visible in `ocr_page_count`.
    pub fn is_convertible_without_ocr(self) -> bool {
        matches!(self, PdfKind::TextBased | PdfKind::Mixed)
    }
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
    pub doc_created_at: Option<String>,
    pub doc_modified_at: Option<String>,
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
        "doc_created_at",
        "doc_modified_at",
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
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.blake3)
            .bind(self.size)
            .bind(self.page_count)
            .bind(self.pdf_type.as_str())
            .bind(self.confidence)
            .bind(i64::from(self.needs_ocr))
            .bind(self.ocr_page_count)
            .bind(i64::from(self.has_encoding_issues))
            .bind(self.title.as_deref())
            .bind(self.doc_created_at.as_deref())
            .bind(self.doc_modified_at.as_deref())
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
    pub mtime_ns: i64,
    pub size: i64,
    pub stamp_kind: StampKind,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
    pub last_seen_at: String,
}

impl BulkUpsertable for PdfPathRow {
    const TABLE: &'static str = "pdf_paths";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "blake3",
        "mtime_ns",
        "size",
        "stamp_kind",
        "inode",
        "dev",
        "last_seen_at",
    ];
    const PAYLOAD_COLUMN: Option<&'static str> = None;

    fn id(&self) -> &str {
        &self.id
    }

    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.blake3)
            .bind(self.mtime_ns)
            .bind(self.size)
            .bind(self.stamp_kind.as_str())
            .bind(self.inode)
            .bind(self.dev)
            .bind(&self.last_seen_at)
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
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
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
    fn only_text_and_mixed_convert_without_ocr() {
        assert!(PdfKind::TextBased.is_convertible_without_ocr());
        assert!(PdfKind::Mixed.is_convertible_without_ocr());
        assert!(!PdfKind::Scanned.is_convertible_without_ocr());
        assert!(!PdfKind::ImageBased.is_convertible_without_ocr());
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
