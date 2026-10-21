//! Raw-store schema for the `google_drive` provider. Every table keys on
//! Drive's own ids: a file by its file id, a revision and a comment by
//! `{file}#{their id}`, a capture by `{file}#{version}`. A file is held
//! at the Drive `version` it was captured for (`held_version` in
//! `files_bookkeeping`, `datalib_etl_web::owed`); nothing else carries a
//! version of its own.
//!
//! The native JSON of each captured version is a blob in the CAS,
//! gzipped, not a payload here: doltlite stores a large value whole, so a
//! one-cell edit to a 6 MB Sheet would cost 6 MB in this file and costs
//! about 44 KB in the CAS (docs/dev/plans/google_drive.md).

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, RawTable, WirePayloadRow};

pub const FILES: &str = "files";
pub const REVISIONS: &str = "revisions";
pub const COMMENTS: &str = "comments";
pub const CAPTURES: &str = "captures";
pub const CAPTURE_PARTS: &str = "capture_parts";

pub const DATA_TABLES: &[&str] = &[FILES, REVISIONS, COMMENTS, CAPTURES, CAPTURE_PARTS];

/// `files` — one row per file, keyed by Drive's file id: the file's
/// listing entry (`files(*)`) as of the newest version captured.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "files")]
pub struct FileRow {
    pub id_and_payload: WirePayload,
}

/// Fields of a listing entry that change between two reads of an
/// unedited file: a freshly signed thumbnail URL every time, and
/// avatar URLs now and then.
pub const FILE_VOLATILE_PATHS: &[dr::VolatilePath] = &[
    &["thumbnailLink"],
    &["lastModifyingUser", "photoLink"],
    &["sharingUser", "photoLink"],
    &["owners", dr::EVERY_ELEMENT, "photoLink"],
    &["permissions", dr::EVERY_ELEMENT, "photoLink"],
];

/// `revisions` — one row per revision Drive lists for a file, keyed
/// `{file_id}#{revision_id}`. Upserted on each capture and never
/// deleted, so a revision Google later merges away stays here. Empty for
/// a file the account may only comment on.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "revisions", index = "revisions_by_file:file_id")]
pub struct RevisionRow {
    pub id_and_payload: WirePayload,
    pub file_id: String,
}

pub const REVISION_VOLATILE_PATHS: &[dr::VolatilePath] = &[&["lastModifyingUser", "photoLink"]];

/// `comments` — one row per comment thread, keyed
/// `{file_id}#{comment_id}`, its replies inline as Drive returns them.
/// Listed with `includeDeleted`, so a deleted comment comes back marked
/// `deleted` rather than missing; never deleted here.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "comments", index = "comments_by_file:file_id")]
pub struct CommentRow {
    pub id_and_payload: WirePayload,
    pub file_id: String,
}

pub const COMMENT_VOLATILE_PATHS: &[dr::VolatilePath] = &[
    &["author", "photoLink"],
    &["replies", dr::EVERY_ELEMENT, "author", "photoLink"],
];

/// `captures` — one row per version of a file a run captured, keyed
/// `{file_id}#{version}`. Append-only: the history of a file is these
/// rows, not this store's commit log.
///
/// - `version` is Drive's file `version` the listing named; the content
///   may be a little newer, if the file was edited while it was fetched.
/// - `modified_time` is the listing's `modifiedTime`.
/// - `native_revision_id` is the native JSON's own `revisionId` (Docs
///   and Slides; Sheets have none).
/// - `head_revision_id` is the newest Drive revision listed at capture,
///   the number old-revision exports are asked for by.
///
/// The sidecar's `volatile_payload` holds the image links taken out of
/// the native JSON, `{"image_links": {"<JSON pointer>": "<url>"}}`.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "captures", index = "captures_by_file:file_id")]
pub struct CaptureRow {
    pub id: String,
    pub file_id: String,
    pub version: String,
    pub modified_time: Option<String>,
    pub native_revision_id: Option<String>,
    pub head_revision_id: Option<String>,
}

impl CaptureRow {
    pub fn id_for(file_id: &str, version: &str) -> String {
        format!("{file_id}#{version}")
    }
}

/// `capture_parts` — the bytes of one capture in the CAS, keyed
/// `{capture_id}#{part}`. `part` is one of:
///
/// - [`PART_NATIVE`]: the native API's JSON, gzipped, with its image
///   links taken out. Google signs each link afresh on every read, so
///   left in they would make every read of an unedited file differ;
/// - `export.<ext>` ([`export_part`]): Drive's export in that format;
/// - `image:<JSON pointer>` ([`image_part`]): an image the native JSON
///   points at, by where it points from.
///
/// `blake3` is NULL for an export Google declined to make (over its
/// 10 MB limit); the capture carries a warning saying so.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "capture_parts")]
pub struct CapturePartRow {
    pub id: String,
    pub capture_id: String,
    pub part: String,
    pub blake3: Option<String>,
}

pub const PART_NATIVE: &str = "native.json.gz";
pub const NATIVE_CONTENT_TYPE: &str = "application/gzip";

pub fn export_part(extension: &str) -> String {
    format!("export.{extension}")
}

pub fn image_part(pointer: &str) -> String {
    format!("image:{pointer}")
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[];

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![FileRow::ddl()];
    out.extend(RevisionRow::all_ddl());
    out.extend(CommentRow::all_ddl());
    out.extend(CaptureRow::all_ddl());
    out.extend(CapturePartRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
