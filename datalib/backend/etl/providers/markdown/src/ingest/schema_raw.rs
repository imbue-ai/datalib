//! Raw-store schema for the `markdown` provider.

use datalib_etl_macros::RawTable;

/// The tables a row of which a scan writes, each with a `_bookkeeping`
/// sidecar.
pub const DATA_TABLES: &[&str] = &[MARKDOWN_FILES, MARKDOWN_SCAN_META];

pub const MARKDOWN_FILES: &str = "markdown_files";
pub const MARKDOWN_SCAN_META: &str = "markdown_scan_meta";

/// One row per markdown file under the scan root, keyed by its path
/// relative to the root, slash-separated (`Projects/Warp core.md`). A
/// note *is* its path: editing it rewrites this row, and renaming or
/// moving it is a new row and the old one deleted, which is what a
/// person sees in an editor like Obsidian too.
///
/// `text` is the file's whole content, so the store keeps every version
/// a sync read and render never reads the file again. `blake3` and
/// `size` are of the bytes read; `text` is those bytes as UTF-8, with any
/// byte that is not replaced by U+FFFD (and a problem saying so).
/// `modified_at` is the file's mtime, a Unix stamp that names no clock,
/// so it ends in `Z`.
#[derive(Debug, Clone, PartialEq, Eq, RawTable)]
#[raw_table(table = "markdown_files")]
pub struct MarkdownFileRow {
    pub id: String,
    pub blake3: String,
    pub size: i64,
    pub modified_at: Option<String>,
    pub text: String,
}

/// Where the scan ran: one row, keyed by the source's id. Render reads it
/// to give each note a `file://` link to where it is on disk.
#[derive(Debug, Clone, PartialEq, Eq, RawTable)]
#[raw_table(table = "markdown_scan_meta")]
pub struct MarkdownScanMetaRow {
    pub id: String,
    pub abs_root: String,
}

pub fn full_ddl() -> Vec<String> {
    let mut out = vec![
        MarkdownFileRow::ddl(),
        MarkdownScanMetaRow::ddl(),
        // The resume cursor: a file whose bytes this source already
        // stored is not read into a row again.
        datalib_etl_files::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ];
    out.extend(
        DATA_TABLES
            .iter()
            .map(|t| datalib_etl::doltlite_raw::bookkeeping_ddl_for(t)),
    );
    out
}
