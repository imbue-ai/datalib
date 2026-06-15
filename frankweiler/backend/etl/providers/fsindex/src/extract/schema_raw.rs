//! Raw-store schema for the `fsindex` provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! §"Schema first" for the conventions every `schema_raw.rs` follows.
//!
//! ## What this provider is
//!
//! `fsindex` is a directory-tree scanner. The "upstream" is a local
//! filesystem subtree rooted at some absolute path; the entity rows
//! are files and directories under that root.
//!
//! Two design choices distinguish it from every other provider in
//! the tree, and both are load-bearing for everything below:
//!
//! 1. **Content and cursor live in separate entity tables.** The
//!    user explicitly does not want filesystem-side bookkeeping
//!    (mtime, inode) to pollute `dolt diff` on file contents. So
//!    `files` carries the semantic content (kind, size, blake3,
//!    symlink target, optional identity uuid) and `file_stats`
//!    carries the cursor data Unison uses for fast-rescan decisions
//!    (mtime, inode, dev). Both are plain entity tables; fsindex runs
//!    the bookkeeping-free write path (no `_bookkeeping` sidecars —
//!    truncate-and-rebuild plus a single `dolt_commit` per scan is its
//!    attempt model). Keeping the two tables *split* is also what
//!    preserves cross-tree prolly dedup — see `STORAGE_NOTES.md` §3.
//!    This split is orthogonal to the
//!    framework's events-vs-bookkeeping split (which is about
//!    attempt tracking, not about content-vs-cursor); see
//!    [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//!    §"Events vs bookkeeping" for the framework's axis, and the
//!    sibling [`EXTRACT.md`](../../EXTRACT.md) §"Why two entity tables"
//!    for ours.
//!
//! 2. **Multi-root via doltlite branches, one db per source.** Per
//!    §"Operating assumptions" we keep "one writer per doltlite
//!    file." Two scan roots that want to share storage and gain
//!    prolly-tree dedup pick the same `<name>.doltlite_db` and
//!    different `target_doltlite_branch` values in their `sources:`
//!    entries; sync's orchestrator serializes them. Branch-level
//!    diff (`SELECT … FROM main.files m FULL JOIN b.files l USING(id)
//!    WHERE m.blake3 IS NOT l.blake3`) is the comparison/sync UX
//!    we're building toward.
//!
//! ## PK choice
//!
//! Every entry row keys by **root-relative path** as a `TEXT` PK,
//! posix-style ("dir/sub/leaf.txt", forward slashes, no leading
//! slash, no trailing slash even for directories). The path is the
//! upstream identifier in the §"Object identity" sense — stable
//! across re-scans of the same tree, distinct from any
//! filesystem-side identity (inode, fs uuid, etc.).
//! `ON CONFLICT(id) DO UPDATE` works because rescans hit the same
//! paths.
//!
//! Identity *across* renames/moves is a separate concern handled by
//! the breadcrumb UUID — see [Identity UUIDs](#identity-uuids) below.
//!
//! ## Why typed columns and not JSONB payloads
//!
//! Every other entity table in the framework uses a JSONB `payload`
//! column to preserve verbatim upstream wire bytes (§"Wire-fidelity
//! of the raw store"). `fsindex` deviates: every column is typed,
//! none of the rows carry a `payload`.
//!
//! Two reasons:
//!
//! 1. **There is no opaque upstream wire to preserve.** The
//!    "upstream" is the OS `stat` call; we control its encoding into
//!    columns 1:1. Wire-fidelity collapses to "preserve the stat
//!    fields the OS gave us, do not synthesize ones it didn't" —
//!    symmetric with the §"No fabricated timestamps" rule. There is
//!    no opaque-bag-of-fields to round-trip.
//! 2. **Per-row size matters at fsindex's scale.** Tens of millions
//!    of rows are the design target. A JSONB envelope (`{"kind":
//!    "file","size":N,"blake3":"<64 hex>"}`) adds ~20 bytes of
//!    key/quote/delimiter overhead per row over typed columns,
//!    every read pays a `jsonb_extract` per virtual column, and
//!    every write pays a JSON encode. At 50M rows that overhead is
//!    ~1 GB of bloat plus measurable CPU on every scan.
//!
//! The trade-off: adding a column later is `ALTER TABLE ADD COLUMN`,
//! not a no-op payload-key addition. For a schema this small and
//! stable, that's acceptable.
//!
//! ## Identity UUIDs
//!
//! Optionally, directories carry a **Ship-of-Theseus identity UUID**
//! stamped into a `.fsindex.yaml` breadcrumb file inside the
//! directory. The UUID survives renames and moves because it travels
//! with the directory's content. The breadcrumb is opt-in per
//! `stamp_me_with_uuid: true` in an ancestor `.fsindex.yaml`; the
//! scanner mutating the filesystem to write breadcrumbs is the one
//! side effect this provider performs — see
//! [`EXTRACT.md`](../../EXTRACT.md) §"Stamping policy" for the
//! gating rules.
//!
//! **The UUID is not a PK.** A `cp -r` of a stamped directory
//! produces two copies of the same UUID at different paths. That is
//! a real and expected case, surfaced as a fork finding via
//! `SELECT identity_uuid, COUNT(*) FROM files
//!  WHERE identity_uuid IS NOT NULL
//!  GROUP BY identity_uuid HAVING COUNT(*) > 1`. The path remains
//! the PK; the UUID is a *secondary identity hint*. It is not
//! indexed today (the column is almost entirely NULL — see
//! `STORAGE_NOTES.md` §2); add an index if a real move/fork workload
//! needs it.
//!
//! ## Directory tree-hash canonicalization
//!
//! `files.blake3` for a directory is defined as blake3 over a
//! canonical encoding of its immediate children, sorted by name.
//! The encoding for each child is:
//!
//! ```text
//!   name_bytes ‖ 0x00 ‖ kind_tag ‖ child_blake3 (32 raw bytes) ‖ 0x0a
//! ```
//!
//! where `kind_tag` is one ASCII byte: `F` (file), `D` (directory),
//! `L` (symlink). Children are sorted by byte-lexical order of
//! `name_bytes` before concatenation. The hash is over the
//! concatenation of all children's encodings (empty string → empty
//! dir hash).
//!
//! Two rules govern what counts as a "child":
//!
//! 1. **Scanner-controlled files are excluded.** `.fsindex.yaml`
//!    (both the options file and the breadcrumb — same file) is not
//!    a child for tree-hash purposes. Otherwise the act of stamping
//!    a directory would invalidate its own blake3, fanning out a
//!    rehash storm to every ancestor on first scan.
//! 2. **Ignored entries are excluded.** Entries matched by the
//!    cascaded `ignore` patterns are absent from `files` entirely
//!    and contribute nothing to the parent's tree-hash. To detect
//!    the case "same content, different ignore config" the parent
//!    `scan_meta` row carries an `options_fingerprint` so two
//!    superficially-equal dir hashes computed under different
//!    visibility configs are distinguishable at the meta layer.
//!
//! The encoding matches git's tree-object spirit (sorted children
//! with name + kind + child-hash) without being byte-identical to
//! git's; we don't need git compatibility, we need stable canonical
//! ordering and resilience against name collisions across kinds.
//!
//! ## Hand-rolled `BulkUpsertable` impls
//!
//! Without a JSONB `payload` column, `#[derive(WirePayloadRow)]`
//! does not apply — its macro contract is specifically the
//! `WirePayload { id, payload }` shape. The hand-rolled
//! `BulkUpsertable` impls below follow the same pattern slack's
//! `RepliesPagesRow` uses. The right long-term fix is a
//! `#[derive(BulkUpsertable)]` macro for non-payload tables, called
//! out in [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! §"Deferred work" — when that lands, every impl in this file
//! collapses to its struct definition.

use frankweiler_etl::bulk::BulkUpsertable;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

pub const DATA_TABLES: &[&str] = &["files", "file_stats", "scan_meta"];

// ─────────────────────────────────────────────────────────────────────
// files
// ─────────────────────────────────────────────────────────────────────

/// `files` — one row per entry visible to the indexer (after
/// `ignore` filtering), including directories and symlinks.
///
/// This is the **content entity**. Its columns carry the semantic
/// identity of the entry — what would have to change for a rescan
/// to register a real diff. Filesystem-mechanical fields (mtime,
/// inode, dev) live in the sibling [`FILE_STATS_DDL`] table so that
/// `dolt diff files` shows content changes only.
///
/// Columns:
/// - `id` — root-relative posix path. Primary key. See "PK choice"
///   in the module docstring.
/// - `kind` — `'file'` | `'dir'` | `'symlink'`. Not indexed (no column
///   is — see the "ZERO secondary indexes" note above `FileRow`).
/// - `size` — bytes. For a file, the byte length of its contents.
///   For a directory, the sum of the sizes of its visible children
///   (recursive). For a symlink, the byte length of the link
///   target.
/// - `blake3` — the raw 32-byte blake3 digest, stored as a `BLOB`
///   (not 64-char hex — saves ~35 B/row; see `STORAGE_NOTES.md` §2).
///   For files: of the file bytes. For directories: of the canonical
///   tree encoding (see "Directory tree-hash canonicalization" in the
///   module docstring). For symlinks: of the link target bytes (so a
///   retarget registers as a content change). Not indexed; dup
///   detection (`GROUP BY blake3`) and cross-branch move/sync diffs are
///   whole-corpus scans done in RAM, not point lookups — see the
///   "ZERO secondary indexes" note above `FileRow`.
/// - `symlink_target` — link target string. NULL unless
///   `kind = 'symlink'`.
/// - `identity_uuid` — directory breadcrumb UUID. NULL for
///   unstamped directories and for every file/symlink row. Not
///   indexed. Fork detection
///   (`GROUP BY identity_uuid HAVING COUNT(*) > 1`) and
///   move-across-rename detection (`JOIN … USING(identity_uuid)`) run
///   in RAM after a full load.
pub const FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS files (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    size            INTEGER NOT NULL,
    blake3          BLOB NOT NULL,
    symlink_target  TEXT NULL,
    identity_uuid   TEXT NULL
)";

// fsindex carries ZERO secondary indexes — only the two path primary
// keys (on `files` and `file_stats`), which in dolt are the clustered
// storage order and the row identity, not optional indexes.
//
// The raw store's only jobs are durable content-addressed storage and
// prolly-tree diff between commits/branches; neither touches a secondary
// index (diff walks the PK-ordered chunks). Every analysis query —
// dup clustering (`GROUP BY blake3`), fork/move detection
// (`GROUP BY identity_uuid`, JOIN on blake3), even the cross-branch sync
// diff (`m.blake3 IS NOT l.blake3`) — is a whole-corpus scan, so the
// intended workflow streams the full table into RAM once and indexes it
// there. A secondary index only earns its keep for selective point
// lookups against the on-disk store without a full scan, which this
// store never does.
//
// And the cost is steep: each secondary index on a TEXT-PK table
// re-stores the full path as its row back-reference (~130–166 B/row at
// 60-char paths — about the size of the rest of the row), so even one
// blake3 index nearly doubled the per-row footprint (215 MB → 346 MB at
// 1M rows). See `STORAGE_NOTES.md` §2. Re-adding any of these is a
// one-line `CREATE INDEX` if a SQL-side, too-big-for-RAM workload ever
// materializes.

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
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(self.kind.as_str())
            .bind(self.size)
            .bind(&self.blake3[..])
            .bind(self.symlink_target.as_deref())
            .bind(self.identity_uuid.as_deref())
    }
}

// ─────────────────────────────────────────────────────────────────────
// file_stats
// ─────────────────────────────────────────────────────────────────────

/// `file_stats` — one row per entry that appears in [`FILES_DDL`].
///
/// This is the **cursor entity**. Its columns carry the
/// filesystem-mechanical state the scanner needs to decide whether
/// to rehash an entry on the next pass — Unison's `(mtime, size,
/// inode)` triple, plus a `dev` for cross-mount disambiguation and
/// a `stamp_kind` discriminator for filesystems where inode is
/// meaningless.
///
/// Rescans read `(file_stats.mtime_ns, file_stats.size,
/// file_stats.inode, file_stats.dev, file_stats.stamp_kind)` for
/// each known path, stat the path, and compare. Only on mismatch
/// (or `stamp_kind = 'rescan'`, the sentinel meaning "previous run
/// was interrupted before fingerprint completed") is the file
/// reopened and rehashed. This is the fast-rescan property Unison
/// gets out of `dataClearlyUnchanged` in `src/fpcache.ml:243`.
///
/// Columns:
/// - `id` — root-relative posix path. Primary key. Always equals
///   some `files.id`; we do not foreign-key the relationship in
///   sqlite (no enforcement gain, and rescan needs to write into
///   this table before the matching `files` row in some recovery
///   paths). Maintained by writer discipline.
/// - `mtime_ns` — modification time as integer nanoseconds since
///   epoch. Filesystem-supplied; we do not normalize tz (it's an
///   instant, not a wall-clock).
/// - `size` — bytes. Mirrors `files.size` at the moment of the last
///   stat; the duplication is deliberate so the rescan compare can
///   read this row alone without joining `files`.
/// - `stamp_kind` — `'inode'` | `'nostamp'` | `'rescan'`. Encodes
///   Unison's `InodeStamp | NoStamp | RescanStamp` enum.
///   `'nostamp'` is for filesystems where inode is unstable across
///   re-stat (some FUSE mounts, network filesystems); the rescan
///   compare drops the inode check on those entries. `'rescan'` is
///   set when a prior run was interrupted mid-fingerprint and the
///   next pass must rehash regardless of `(mtime, size, inode)`
///   agreement.
/// - `inode` — integer inode number from `stat`. NULL unless
///   `stamp_kind = 'inode'`.
/// - `dev` — integer device number from `stat`. NULL unless
///   `stamp_kind = 'inode'`. `(dev, inode)` is the true filesystem
///   identity on unix; `dev` matters when the scan root crosses
///   mount points.
/// - `ctime_ns` — change time as integer nanoseconds, captured
///   opportunistically. NULL when not available. Not part of the
///   fast-rescan compare; there for forensic reads.
pub const FILE_STATS_DDL: &str = "CREATE TABLE IF NOT EXISTS file_stats (
    id          TEXT PRIMARY KEY,
    mtime_ns    INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    stamp_kind  TEXT NOT NULL,
    inode       INTEGER NULL,
    dev         INTEGER NULL,
    ctime_ns    INTEGER NULL
)";

/// One row in [`FILE_STATS_DDL`].
#[derive(Debug, Clone)]
pub struct FileStatsRow {
    pub id: String,
    pub mtime_ns: i64,
    pub size: i64,
    pub stamp_kind: StampKind,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
    pub ctime_ns: Option<i64>,
}

/// Discriminator for [`FileStatsRow::stamp_kind`]. Mirrors Unison's
/// `InodeStamp | NoStamp | RescanStamp` enum from `src/fileinfo.mli`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampKind {
    Inode,
    NoStamp,
    Rescan,
}

impl StampKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StampKind::Inode => "inode",
            StampKind::NoStamp => "nostamp",
            StampKind::Rescan => "rescan",
        }
    }
}

impl BulkUpsertable for FileStatsRow {
    const TABLE: &'static str = "file_stats";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["mtime_ns", "size", "stamp_kind", "inode", "dev", "ctime_ns"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(self.mtime_ns)
            .bind(self.size)
            .bind(self.stamp_kind.as_str())
            .bind(self.inode)
            .bind(self.dev)
            .bind(self.ctime_ns)
    }
}

// ─────────────────────────────────────────────────────────────────────
// scan_meta
// ─────────────────────────────────────────────────────────────────────

/// `scan_meta` — one row per scan source, recording the per-root
/// state that doesn't belong on any individual entry.
///
/// In the single-root-per-branch model the table holds exactly one
/// row. The table shape is preserved for symmetry with other
/// providers' "small metadata table" pattern (contacts'
/// `accounts`, signal's `ingested_backups`) and so a future "merge
/// two scan roots into one branch" path stays trivial.
///
/// Columns:
/// - `id` — the `name:` of the `sources:` entry that produced this
///   scan (e.g. `"laptop_home"`). Stable across moves of the data
///   root because it's user-supplied in config, not derived from
///   the filesystem. This is the same per-source stable identifier
///   used everywhere else in the framework — `.doltlite_db`
///   filenames, log lines, cursor file paths. Primary key.
/// - `abs_path` — current absolute path of the scan root. May
///   change between scans if the user moves the data root; `id`
///   stays stable, this updates.
/// - `os` — `'macos'` | `'linux'` | `'windows'` | ... at scan time.
///   Drives `stamp_kind` defaults on `file_stats`.
/// - `case_sensitive` — 0/1 bool. Captured at first scan; macOS's
///   default HFS+/APFS is case-insensitive, which matters for
///   collation when comparing branches across roots from different
///   OSes.
/// - `inode_stable` — 0/1 bool. 0 on filesystems where we choose
///   `stamp_kind = 'nostamp'` for every row.
/// - `options_fingerprint` — hex blake3 over the resolved cascaded
///   options (`ignore` patterns, `stamp_me_with_uuid` configuration,
///   etc.) that were active for this scan. Two branches with the
///   same tree but different `options_fingerprint` may legitimately
///   have different `files.blake3` for dir rows because their
///   visible-children sets differ; the fingerprint is what tells a
///   diff tool "those aren't really the same tree."
/// - `last_scan_at` — ISO-8601 with explicit offset, produced via
///   `frankweiler_time::IsoOffsetTimestamp::now_local()`.
/// - `scanner_version` — semver string of the
///   `frankweiler-etl-fsindex` crate at scan time. Bump invalidates
///   the tree-hash canonicalization if the algorithm ever needs to
///   change (analogous to a provider's `RENDER_VERSION` lever).
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
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
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

// ─────────────────────────────────────────────────────────────────────
// Composer
// ─────────────────────────────────────────────────────────────────────

/// Full DDL block.
///
/// **No `<t>_bookkeeping` sidecars.** Unlike every other provider,
/// fsindex deliberately omits the framework's per-row attempt-tracking
/// sidecars (`attempt_count`, `last_attempt_at`, `last_error`). Two
/// reasons:
///
/// 1. **There's no retry/attempt model to track.** A `read(2)` either
///    succeeds or it's a real error; there's no flaky upstream API to
///    re-poll. Unreadable entries are logged + counted (`read_errors`,
///    `stat_errors` in the `fsindex_phase_breakdown` event), which is
///    all the durable evidence we need.
/// 2. **They double the row count.** Each sidecar mirrors its parent
///    1:1, so keeping them roughly doubles the bytes written and
///    committed at the tens-of-millions-of-rows design scale — and the
///    extra prolly-tree novelty makes `dolt_gc` (the only thing that
///    reclaims write-amplification) materially harder on a full disk.
///
/// fsindex therefore writes through
/// [`frankweiler_etl::bulk::bulk_upsert_entity_in_tx`] (no bookkeeping
/// stamp) and resets via a bookkeeping-free truncate (see
/// `extract::db::RawDb::reset`).
pub fn full_ddl() -> Vec<String> {
    vec![
        FILES_DDL.to_string(),
        FILE_STATS_DDL.to_string(),
        SCAN_META_DDL.to_string(),
    ]
}
