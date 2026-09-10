//! Raw-store schema for the `fsindex` provider.

use datalib_etl::bulk::BulkUpsertable;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

pub const DATA_TABLES: &[&str] = &["files", "scan_meta"];

// files

/// `files` — one row per entry visible to the indexer (after
/// `ignore` filtering), including directories and symlinks.
pub const FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS files (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    size            INTEGER NOT NULL,
    blake3          BLOB NOT NULL,
    symlink_target  TEXT NULL,
    identity_uuid   TEXT NULL
)";

// fsindex carries ZERO secondary indexes — only the two path primary
// key (on `files`), which in dolt is the clustered
// storage order and the row identity, not optional indexes.

/// One row in [`FILES_DDL`].
#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: String,
    pub kind: FileKind,
    pub size: i64,
    /// Raw 32-byte blake3 digest, stored as a BLOB. See
    /// [`super::hash::Blake3`].
    pub blake3: super::hash::Blake3,
    pub symlink_target: Option<String>,
    pub identity_uuid: Option<String>,
}

/// Discriminator for [`FileRow::kind`]. Round-trips to the
/// stored string via [`FileKind::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
}

impl FileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::File => "file",
            FileKind::Dir => "dir",
            FileKind::Symlink => "symlink",
        }
    }
}

impl BulkUpsertable for FileRow {
    const TABLE: &'static str = "files";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["kind", "size", "blake3", "symlink_target", "identity_uuid"];
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
    vec![FILES_DDL.to_string(), SCAN_META_DDL.to_string()]
}
