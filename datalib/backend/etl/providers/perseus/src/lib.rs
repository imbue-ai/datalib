//! Perseus Digital Library provider for [`datalib_etl`]: the download
//! half — the TEI editions of classical works. Rendering them into the
//! source's `render_markdown/` tree lives in
//! [`datalib_etl_perseus_render`].

pub mod ingest;
pub mod processor;

/// CTS work URN (no edition suffix). `__cts__.xml` edition `urn`s are
/// `<this>.<edition-id>`, which [`datalib_etl_perseus_render::render::parse`] strips to
/// recover the edition id.
pub const WORK_URN: &str = "urn:cts:greekLit:tlg0003.tlg001";

/// Filename prefix every edition TEI shares:
/// `tlg0003.tlg001.<edition-id>.xml`. The parser strips this (and the
/// `.xml` suffix) to recover the edition id.
pub const TLG_FILE_PREFIX: &str = "tlg0003.tlg001.";

pub fn cts_urn() -> &'static str {
    WORK_URN
}

/// Displayed in the grid's `project` column.
pub const WORK_TITLE: &str = "Thucydides, Histories";

/// Short label embedded in grid `kind` strings + chapter titles.
pub const WORK_SHORT: &str = "Thucydides";
