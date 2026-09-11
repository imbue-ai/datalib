//! Raw-store schema for the `fsindex` provider.
//!
//! Three tables: `files` (one row per file or symlink), `dirs` (one row
//! per directory, whose `blake3` covers its whole subtree) and
//! `scan_meta` (one row per scan root). Files and directories are kept
//! apart so that a diff of `dirs` alone tells the subtree story — a
//! moved directory is a handful of `dirs` rows, not one row per file
//! under it — and so that each table carries only the columns its kind
//! of entry has (`symlink_target` is meaningless on a directory,
//! `identity_uuid` and `entries` on a file).

use datalib_etl::bulk::BulkUpsertable;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

pub const DATA_TABLES: &[&str] = &["files", "dirs", "scan_meta"];

// fsindex carries ZERO secondary indexes: the path primary key on
// `files` and `dirs` is the clustered storage order and the row
// identity, which in dolt is what makes a subtree contiguous and its
// diff cheap. A secondary index on a TEXT-keyed table re-stores the
// full path per row (see `STORAGE_NOTES.md` §2).

// files

/// `files` — one row per file or symlink visible to the indexer
/// (after `ignore` filtering). Directories are in [`DIRS_DDL`].
pub const FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS files (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    size            INTEGER NOT NULL,
    blake3          BLOB NOT NULL,
    symlink_target  TEXT NULL
)";

/// One row in [`FILES_DDL`].
#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: String,
    pub kind: FileKind,
    pub size: i64,
    /// Raw 32-byte blake3 digest of the file's content — or, for a
    /// symlink, of its target path string. See [`super::hash::Blake3`].
    pub blake3: super::hash::Blake3,
    pub symlink_target: Option<String>,
}

/// Discriminator for [`FileRow::kind`]. Round-trips to the stored
/// string via [`FileKind::as_str`]. There is no `Dir`: a directory is
/// a [`DirRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Symlink,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Symlink => "symlink",
        }
    }
}

impl BulkUpsertable for FileRow {
    const TABLE: &'static str = "files";
    const TYPED_COLUMNS: &'static [&'static str] = &["kind", "size", "blake3", "symlink_target"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(self.kind.as_str())
            .bind(self.size)
            .bind(&self.blake3[..])
            .bind(self.symlink_target.as_deref())
    }
}

// dirs

/// `dirs` — one row per directory, the scan root included (its `id`
/// is the empty string). `size` and `entries` are rolled up over the
/// whole subtree; `blake3` is the tree-hash, so it changes whenever
/// anything below the directory does and survives a move intact.
pub const DIRS_DDL: &str = "CREATE TABLE IF NOT EXISTS dirs (
    id              TEXT PRIMARY KEY,
    size            INTEGER NOT NULL,
    entries         INTEGER NOT NULL,
    blake3          BLOB NOT NULL,
    identity_uuid   TEXT NULL
)";

/// One row in [`DIRS_DDL`].
#[derive(Debug, Clone)]
pub struct DirRow {
    pub id: String,
    /// Total content bytes of every file and symlink beneath this
    /// directory, recursively.
    pub size: i64,
    /// Files, symlinks and directories beneath this directory,
    /// recursively — the directory itself not counted.
    pub entries: i64,
    /// The tree-hash: blake3 over the canonical encoding of the
    /// immediate children's `(name, kind, blake3)`, see
    /// [`super::hash::hash_tree`].
    pub blake3: super::hash::Blake3,
    pub identity_uuid: Option<String>,
}

impl BulkUpsertable for DirRow {
    const TABLE: &'static str = "dirs";
    const TYPED_COLUMNS: &'static [&'static str] = &["size", "entries", "blake3", "identity_uuid"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(self.size)
            .bind(self.entries)
            .bind(&self.blake3[..])
            .bind(self.identity_uuid.as_deref())
    }
}

// scan_meta

/// `scan_meta` — one row per scan source, recording the per-root
/// state that doesn't belong on any individual entry.
pub const SCAN_META_DDL: &str = "CREATE TABLE IF NOT EXISTS scan_meta (
    id                  TEXT PRIMARY KEY,
    abs_path            TEXT NOT NULL,
    os                  TEXT NOT NULL,
    case_sensitive      INTEGER NOT NULL,
    inode_stable        INTEGER NOT NULL,
    options_fingerprint TEXT NOT NULL,
    last_scan_at        TEXT NOT NULL,
    scanner_version     TEXT NOT NULL
)";

/// One row in [`SCAN_META_DDL`].
#[derive(Debug, Clone)]
pub struct ScanMetaRow {
    pub id: String,
    pub abs_path: String,
    pub os: String,
    pub case_sensitive: bool,
    pub inode_stable: bool,
    pub options_fingerprint: String,
    pub last_scan_at: String,
    pub scanner_version: String,
}

impl BulkUpsertable for ScanMetaRow {
    const TABLE: &'static str = "scan_meta";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "abs_path",
        "os",
        "case_sensitive",
        "inode_stable",
        "options_fingerprint",
        "last_scan_at",
        "scanner_version",
    ];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(&self.abs_path)
            .bind(&self.os)
            .bind(self.case_sensitive as i64)
            .bind(self.inode_stable as i64)
            .bind(&self.options_fingerprint)
            .bind(&self.last_scan_at)
            .bind(&self.scanner_version)
    }
}

// Composer

pub fn full_ddl() -> Vec<String> {
    vec![
        FILES_DDL.to_string(),
        DIRS_DDL.to_string(),
        SCAN_META_DDL.to_string(),
    ]
}
