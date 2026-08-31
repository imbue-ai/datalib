//! Raw-store schema for the `media` provider.
//!
//! # Content on one side, locations on the other
//!
//! Same split as `pdf`, motivated the same way. The question is "what
//! media do I have?", not "what files are on this disk" — and in a
//! personal library the same song or photo exists several times over:
//! the album ripped once and synced twice, the picture in the camera
//! import and again in an exported album. Keying on path would count
//! each copy as its own item and turn a `mv` into a delete plus an add.
//!
//! So content is keyed on `blake3(bytes)` and locations hang off it:
//!
//! - [`MEDIA_ITEMS_DDL`] — PK `blake3`. One row per distinct file's
//!   worth of bytes: class, container, codec, duration, and the
//!   metadata-excluding [`MediaItemRow::payload_blake3`].
//! - [`MEDIA_FILES_DDL`] — PK `id` (root-relative path), FK `blake3`.
//!   Where copies live, plus Unison's `(mtime, size, inode, dev)`
//!   rescan cursor so an unchanged file skips the read.
//!
//! # Two class tables, not three
//!
//! The obvious split is music / photos / video, one table each. The
//! split that actually falls out of the data is **audio versus
//! visual**, because the line that matters is not the medium but the
//! kind of metadata:
//!
//! - [`MEDIA_AUDIO_DDL`] holds *tags describing a recording* — artist,
//!   album, track number. Typed by a person or a database, and they
//!   describe the work, not the capture.
//! - [`MEDIA_VISUAL_DDL`] holds *EXIF describing a capture* — camera,
//!   lens, exposure, GPS, the moment the shutter opened. Written by a
//!   device.
//!
//! Video sits squarely on the EXIF side. A phone's `.mov` carries the
//! same make, model, capture time and coordinates as the `.heic` shot
//! beside it, and a Live Photo is literally the two together. Giving
//! video its own table would mean duplicating a dozen capture columns
//! to express that a video was taken by a camera in a place at a time
//! — and then joining them back together for every "what did I shoot
//! on this trip?" query. The handful of genuinely video-only fields
//! (`frame_rate`, the two codec columns) are nullable columns on the
//! shared table instead. Duration belongs to both, so it lives on
//! `media_items`.
//!
//! # Playlists get their own two tables
//!
//! A playlist is not an item — it has no payload and no capture — and
//! its content *is* an ordered list of references. That does not fit
//! either class table, so [`MEDIA_PLAYLISTS_DDL`] and
//! [`MEDIA_PLAYLIST_ENTRIES_DDL`] carry it. See [`super::playlist`] for
//! why the unresolved entries are the valuable ones.
//!
//! # Every metadata column is a hint
//!
//! Nothing here is keyed on, joined through, or trusted. Tags are
//! wrong at a rate that would startle anyone who has not looked: clocks
//! set to the wrong year, `album_artist` differing across one album,
//! GPS from a phone that had not got a fix yet. We store what the file
//! says. The same position `pdf` takes on its `author` column, for the
//! same reason — a heuristic clean enough to drop the junk eventually
//! drops something real.

use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::fswalk::StampKind;

use super::kind::{Container, MediaClass};

/// Path-keyed tables, reconciled at the **end** of a scan rather than
/// truncated at the start.
///
/// The obvious implementation is `DELETE FROM …` up front and let the
/// rebuild re-add whatever is still there, which is what `fsindex` and
/// `pdf` do. It has a cost those two can live with and this provider
/// cannot: a scan killed halfway has already discarded the rescan
/// cursors for every file it had not reached, so the next run re-reads
/// them from disk although nothing changed. On a library measured in
/// terabytes that is the difference between resuming a scan and
/// restarting one.
///
/// So nothing is deleted up front. Instead the walk *consumes* the
/// in-memory cache — each visited path is removed from it — and
/// whatever is left at the end is, exactly, the set of paths that
/// disappeared. Those rows are deleted by id.
///
/// That formulation is also why the reconciliation is not a
/// `WHERE last_seen_at <> <this run>` sweep, which would be simpler:
/// `DATALIB_DAG_NOW` is pinned per run, so two runs sharing a pinned
/// `now` — a retry, a test — would sweep nothing and quietly keep rows
/// for deleted files. Set difference has no clock in it.
///
/// See `DOWNLOAD.md` §"Interrupting a scan".
///
/// The content-keyed tables (`media_items`, `media_audio`,
/// `media_visual`) are deliberately absent: content has no notion of
/// "no longer present", and dropping them would lose `first_seen_at`
/// and force a re-parse of every item whose path merely moved. See
/// `DOWNLOAD.md` §"Orphaned items".
pub const DATA_TABLES: &[&str] = &["media_files", "media_playlists", "media_playlist_entries"];

/// All tables, for DDL.
pub const ALL_TABLES: &[&str] = &[
    "media_items",
    "media_audio",
    "media_visual",
    "media_files",
    "media_playlists",
    "media_playlist_entries",
    "media_scan_meta",
];

pub const MEDIA_ITEMS_DDL: &str = "CREATE TABLE IF NOT EXISTS media_items (
    blake3          TEXT PRIMARY KEY,
    size            INTEGER NOT NULL,
    media_class     TEXT NOT NULL,
    container       TEXT NOT NULL,
    codec           TEXT NULL,
    duration_ms     INTEGER NULL,
    payload_blake3  TEXT NULL,
    payload_scheme  TEXT NULL,
    first_seen_at   TEXT NOT NULL
)";

pub const MEDIA_ITEMS_INDEXES: &[&str] = &[
    // The query the column exists for: every file that is the same
    // recording or the same exposure, regardless of how it was tagged.
    // A point lookup against a column with no other access path, at
    // item scale rather than fsindex scale, so the index earns its
    // keep.
    "CREATE INDEX IF NOT EXISTS idx_media_items_payload \
     ON media_items (payload_blake3)",
    // "Show me the video" / "show me the music" is the first thing any
    // view over this table does.
    "CREATE INDEX IF NOT EXISTS idx_media_items_class \
     ON media_items (media_class)",
];

pub const MEDIA_AUDIO_DDL: &str = "CREATE TABLE IF NOT EXISTS media_audio (
    blake3          TEXT PRIMARY KEY,
    title           TEXT NULL,
    artist          TEXT NULL,
    album           TEXT NULL,
    album_artist    TEXT NULL,
    composer        TEXT NULL,
    genre           TEXT NULL,
    date            TEXT NULL,
    track_no        INTEGER NULL,
    track_total     INTEGER NULL,
    disc_no         INTEGER NULL,
    disc_total      INTEGER NULL,
    bitrate_kbps    INTEGER NULL,
    sample_rate_hz  INTEGER NULL,
    channels        INTEGER NULL,
    bit_depth       INTEGER NULL
)";

pub const MEDIA_AUDIO_INDEXES: &[&str] = &[
    // Browsing by album is the one access pattern a music library has
    // that a `WHERE blake3 = ?` lookup cannot serve.
    "CREATE INDEX IF NOT EXISTS idx_media_audio_album ON media_audio (album)",
    "CREATE INDEX IF NOT EXISTS idx_media_audio_artist ON media_audio (artist)",
];

pub const MEDIA_VISUAL_DDL: &str = "CREATE TABLE IF NOT EXISTS media_visual (
    blake3          TEXT PRIMARY KEY,
    width           INTEGER NULL,
    height          INTEGER NULL,
    orientation     INTEGER NULL,
    captured_at     TEXT NULL,
    camera_make     TEXT NULL,
    camera_model    TEXT NULL,
    lens_model      TEXT NULL,
    iso             INTEGER NULL,
    exposure_time   TEXT NULL,
    f_number        REAL NULL,
    focal_length_mm REAL NULL,
    gps_lat         REAL NULL,
    gps_lon         REAL NULL,
    gps_altitude_m  REAL NULL,
    title           TEXT NULL,
    caption         TEXT NULL,
    keywords        TEXT NULL,
    frame_rate      REAL NULL,
    video_codec     TEXT NULL,
    audio_codec     TEXT NULL
)";

pub const MEDIA_VISUAL_INDEXES: &[&str] = &[
    // "What did I shoot that week?" — the axis a photo library is
    // browsed along, and the one an unindexed scan would make slow.
    "CREATE INDEX IF NOT EXISTS idx_media_visual_captured \
     ON media_visual (captured_at)",
];

pub const MEDIA_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS media_files (
    id           TEXT PRIMARY KEY,
    blake3       TEXT NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    size         INTEGER NOT NULL,
    stamp_kind   TEXT NOT NULL,
    inode        INTEGER NULL,
    dev          INTEGER NULL,
    last_seen_at TEXT NOT NULL
)";

pub const MEDIA_FILES_INDEXES: &[&str] = &[
    // Every "where are the copies of this item?" query, and the
    // playlist resolver's path → item lookup.
    "CREATE INDEX IF NOT EXISTS idx_media_files_blake3 ON media_files (blake3)",
];

pub const MEDIA_PLAYLISTS_DDL: &str = "CREATE TABLE IF NOT EXISTS media_playlists (
    id             TEXT PRIMARY KEY,
    blake3         TEXT NOT NULL,
    format         TEXT NOT NULL,
    title          TEXT NULL,
    entry_count    INTEGER NOT NULL,
    last_seen_at   TEXT NOT NULL
)";

pub const MEDIA_PLAYLIST_ENTRIES_DDL: &str = "CREATE TABLE IF NOT EXISTS media_playlist_entries (
    id              TEXT PRIMARY KEY,
    playlist_id     TEXT NOT NULL,
    position        INTEGER NOT NULL,
    target_raw      TEXT NOT NULL,
    target_kind     TEXT NOT NULL,
    resolved_path   TEXT NULL,
    ext_title       TEXT NULL,
    ext_duration_s  INTEGER NULL
)";

pub const MEDIA_PLAYLIST_ENTRIES_INDEXES: &[&str] = &[
    // Reading one playlist back in order — the only way this table is
    // ever meant to be read.
    "CREATE INDEX IF NOT EXISTS idx_media_playlist_entries_playlist \
     ON media_playlist_entries (playlist_id, position)",
    // The reverse question — which playlists reference this file? — is
    // a join from `resolved_path` to `media_files.id`, so the index
    // goes on the column we store rather than on a cached answer.
    "CREATE INDEX IF NOT EXISTS idx_media_playlist_entries_resolved \
     ON media_playlist_entries (resolved_path)",
];

/// Where the scan actually ran.
///
/// `media_files.id` is root-relative — that is what keeps a moved data
/// root from rewriting every row — so something has to remember the
/// absolute root. Keyed on the **source name** from config rather than
/// on the path, exactly as `fsindex` and `pdf` do, so the row survives
/// a move of the root.
pub const MEDIA_SCAN_META_DDL: &str = "CREATE TABLE IF NOT EXISTS media_scan_meta (
    id           TEXT PRIMARY KEY,
    abs_root     TEXT NOT NULL,
    scanned_at   TEXT NOT NULL
)";

pub fn full_ddl() -> Vec<String> {
    let mut out = vec![
        MEDIA_ITEMS_DDL.to_string(),
        MEDIA_AUDIO_DDL.to_string(),
        MEDIA_VISUAL_DDL.to_string(),
        MEDIA_FILES_DDL.to_string(),
        MEDIA_PLAYLISTS_DDL.to_string(),
        MEDIA_PLAYLIST_ENTRIES_DDL.to_string(),
        MEDIA_SCAN_META_DDL.to_string(),
    ];
    for set in [
        MEDIA_ITEMS_INDEXES,
        MEDIA_AUDIO_INDEXES,
        MEDIA_VISUAL_INDEXES,
        MEDIA_FILES_INDEXES,
        MEDIA_PLAYLIST_ENTRIES_INDEXES,
    ] {
        out.extend(set.iter().map(|s| s.to_string()));
    }
    out
}

/// One row in [`MEDIA_ITEMS_DDL`].
#[derive(Debug, Clone)]
pub struct MediaItemRow {
    /// Lowercase hex blake3 of the file bytes. The item's identity.
    pub blake3: String,
    pub size: i64,
    pub media_class: MediaClass,
    pub container: Container,
    pub codec: Option<String>,
    pub duration_ms: Option<i64>,
    /// Hash over the part of the file that carries the signal — the
    /// audio frames, the entropy-coded scan, the sensor strips — with
    /// tags, EXIF, XMP, ICC profiles and embedded previews left out.
    /// So retagging an MP3 or re-rendering a DNG's preview moves
    /// `blake3` while this holds.
    ///
    /// `None` for containers we have no recipe for, and for files past
    /// the configured `payload_max_bytes`. Deliberately **not** a
    /// fallback to the file hash: NULL says "we did not compute one",
    /// where a fallback would claim a metadata-independence the format
    /// never gave it. See [`super::payload`] for the full argument.
    pub payload_blake3: Option<String>,
    /// Which recipe produced [`Self::payload_blake3`], e.g.
    /// `mp3.frames.v1`. Two payload hashes are only comparable under
    /// one recipe, so the recipe is stored beside the digest and any
    /// change to what a recipe excludes bumps its version.
    pub payload_scheme: Option<String>,
    pub first_seen_at: String,
}

impl BulkUpsertable for MediaItemRow {
    const TABLE: &'static str = "media_items";
    // Not the framework's usual `id`: this table's key IS the content
    // hash, and naming the column `blake3` keeps that legible in every
    // ad-hoc query and join against `media_files.blake3`.
    const ID_COLUMN: &'static str = "blake3";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "size",
        "media_class",
        "container",
        "codec",
        "duration_ms",
        "payload_blake3",
        "payload_scheme",
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
            .bind(self.media_class.as_str())
            .bind(self.container.as_str())
            .bind(self.codec.as_deref())
            .bind(self.duration_ms)
            .bind(self.payload_blake3.as_deref())
            .bind(self.payload_scheme.as_deref())
            .bind(&self.first_seen_at)
    }
}

/// One row in [`MEDIA_AUDIO_DDL`].
#[derive(Debug, Clone)]
pub struct MediaAudioRow {
    pub blake3: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub genre: Option<String>,
    pub date: Option<String>,
    pub track_no: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_no: Option<i64>,
    pub disc_total: Option<i64>,
    pub bitrate_kbps: Option<i64>,
    pub sample_rate_hz: Option<i64>,
    pub channels: Option<i64>,
    pub bit_depth: Option<i64>,
}

impl BulkUpsertable for MediaAudioRow {
    const TABLE: &'static str = "media_audio";
    const ID_COLUMN: &'static str = "blake3";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "title",
        "artist",
        "album",
        "album_artist",
        "composer",
        "genre",
        "date",
        "track_no",
        "track_total",
        "disc_no",
        "disc_total",
        "bitrate_kbps",
        "sample_rate_hz",
        "channels",
        "bit_depth",
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
            .bind(self.title.as_deref())
            .bind(self.artist.as_deref())
            .bind(self.album.as_deref())
            .bind(self.album_artist.as_deref())
            .bind(self.composer.as_deref())
            .bind(self.genre.as_deref())
            .bind(self.date.as_deref())
            .bind(self.track_no)
            .bind(self.track_total)
            .bind(self.disc_no)
            .bind(self.disc_total)
            .bind(self.bitrate_kbps)
            .bind(self.sample_rate_hz)
            .bind(self.channels)
            .bind(self.bit_depth)
    }
}

/// One row in [`MEDIA_VISUAL_DDL`].
#[derive(Debug, Clone)]
pub struct MediaVisualRow {
    pub blake3: String,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub orientation: Option<i64>,
    /// ISO-8601. Carries a UTC offset when the file supplied one and is
    /// naive when it did not — see [`super::meta`] §"Timestamps" for
    /// why this one column deviates from the repo-wide convention.
    pub captured_at: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens_model: Option<String>,
    pub iso: Option<i64>,
    pub exposure_time: Option<String>,
    pub f_number: Option<f64>,
    pub focal_length_mm: Option<f64>,
    pub gps_lat: Option<f64>,
    pub gps_lon: Option<f64>,
    pub gps_altitude_m: Option<f64>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub keywords: Option<String>,
    pub frame_rate: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
}

impl BulkUpsertable for MediaVisualRow {
    const TABLE: &'static str = "media_visual";
    const ID_COLUMN: &'static str = "blake3";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "width",
        "height",
        "orientation",
        "captured_at",
        "camera_make",
        "camera_model",
        "lens_model",
        "iso",
        "exposure_time",
        "f_number",
        "focal_length_mm",
        "gps_lat",
        "gps_lon",
        "gps_altitude_m",
        "title",
        "caption",
        "keywords",
        "frame_rate",
        "video_codec",
        "audio_codec",
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
            .bind(self.width)
            .bind(self.height)
            .bind(self.orientation)
            .bind(self.captured_at.as_deref())
            .bind(self.camera_make.as_deref())
            .bind(self.camera_model.as_deref())
            .bind(self.lens_model.as_deref())
            .bind(self.iso)
            .bind(self.exposure_time.as_deref())
            .bind(self.f_number)
            .bind(self.focal_length_mm)
            .bind(self.gps_lat)
            .bind(self.gps_lon)
            .bind(self.gps_altitude_m)
            .bind(self.title.as_deref())
            .bind(self.caption.as_deref())
            .bind(self.keywords.as_deref())
            .bind(self.frame_rate)
            .bind(self.video_codec.as_deref())
            .bind(self.audio_codec.as_deref())
    }
}

/// One row in [`MEDIA_FILES_DDL`].
#[derive(Debug, Clone)]
pub struct MediaFileRow {
    /// Root-relative, slash-separated path.
    pub id: String,
    /// Hex blake3 of the bytes at this path — the FK into
    /// `media_items`.
    pub blake3: String,
    pub mtime_ns: i64,
    pub size: i64,
    pub stamp_kind: StampKind,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
    pub last_seen_at: String,
}

impl BulkUpsertable for MediaFileRow {
    const TABLE: &'static str = "media_files";
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

/// One row in [`MEDIA_PLAYLISTS_DDL`].
#[derive(Debug, Clone)]
pub struct MediaPlaylistRow {
    /// Root-relative path. Keyed on location rather than content
    /// because a playlist *is* a location-shaped thing — it is edited
    /// in place constantly, and two identical playlists in two folders
    /// are two playlists, not one seen twice.
    pub id: String,
    pub blake3: String,
    pub format: String,
    pub title: Option<String>,
    pub entry_count: i64,
    pub last_seen_at: String,
}

impl BulkUpsertable for MediaPlaylistRow {
    const TABLE: &'static str = "media_playlists";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["blake3", "format", "title", "entry_count", "last_seen_at"];
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
            .bind(&self.format)
            .bind(self.title.as_deref())
            .bind(self.entry_count)
            .bind(&self.last_seen_at)
    }
}

/// One row in [`MEDIA_PLAYLIST_ENTRIES_DDL`].
#[derive(Debug, Clone)]
pub struct MediaPlaylistEntryRow {
    /// `<playlist path>#<position>`. Position is part of the key
    /// because the same target legitimately appears more than once in
    /// one playlist.
    pub id: String,
    pub playlist_id: String,
    pub position: i64,
    /// The line from the file, verbatim. Never normalized, never
    /// dropped for failing to resolve — see [`super::playlist`].
    pub target_raw: String,
    pub target_kind: String,
    /// Root-relative path this entry points at, when it points inside
    /// the scanned tree. NULL for URLs, absolute paths, and traversals
    /// that climb out of the root.
    ///
    /// Computed from the raw target and the playlist's own path — pure
    /// string work, no I/O and no database. Whether a *file* is there
    /// is deliberately **not** stored: that is
    /// `JOIN media_files ON media_files.id = resolved_path`, and a
    /// stored answer would be a cached join that goes stale the moment
    /// a track is added or removed without the playlist being
    /// rescanned. See `DOWNLOAD.md` §"Playlists".
    pub resolved_path: Option<String>,
    pub ext_title: Option<String>,
    pub ext_duration_s: Option<i64>,
}

impl BulkUpsertable for MediaPlaylistEntryRow {
    const TABLE: &'static str = "media_playlist_entries";
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "playlist_id",
        "position",
        "target_raw",
        "target_kind",
        "resolved_path",
        "ext_title",
        "ext_duration_s",
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
            .bind(&self.playlist_id)
            .bind(self.position)
            .bind(&self.target_raw)
            .bind(&self.target_kind)
            .bind(self.resolved_path.as_deref())
            .bind(self.ext_title.as_deref())
            .bind(self.ext_duration_s)
    }
}

/// One row in [`MEDIA_SCAN_META_DDL`].
#[derive(Debug, Clone)]
pub struct MediaScanMetaRow {
    /// The source name from config (`tng_media`), not the path.
    pub id: String,
    pub abs_root: String,
    pub scanned_at: String,
}

impl BulkUpsertable for MediaScanMetaRow {
    const TABLE: &'static str = "media_scan_meta";
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
    fn content_keyed_tables_are_never_swept() {
        // Sweeping them would drop `first_seen_at` and re-parse every
        // item whose path merely moved. Only the path-keyed tables are.
        for t in ["media_items", "media_audio", "media_visual"] {
            assert!(
                !DATA_TABLES.contains(&t),
                "{t} is content-keyed and must survive a scan"
            );
        }
        for t in DATA_TABLES {
            assert!(ALL_TABLES.contains(t), "{t} must have DDL");
        }
    }

    #[test]
    fn every_declared_table_has_ddl() {
        let ddl = full_ddl().join("\n");
        for t in ALL_TABLES {
            assert!(
                ddl.contains(&format!("CREATE TABLE IF NOT EXISTS {t} ")),
                "no DDL for {t}"
            );
        }
    }

    /// `bulk_upsert` binds `ID_COLUMN` first and then `TYPED_COLUMNS`
    /// in order, so a mismatch between that list and `bind_into` writes
    /// every value into the wrong column — silently, since the types
    /// mostly coincide.
    #[test]
    fn bind_order_matches_the_declared_columns() {
        fn columns_in_ddl(ddl: &str) -> Vec<String> {
            ddl.lines()
                .skip(1)
                .filter_map(|l| l.split_whitespace().next())
                .filter(|w| !w.starts_with(')'))
                .map(|w| w.trim_end_matches(',').to_string())
                .filter(|w| !w.is_empty())
                .collect()
        }
        for (ddl, id_col, typed) in [
            (
                MEDIA_ITEMS_DDL,
                MediaItemRow::ID_COLUMN,
                MediaItemRow::TYPED_COLUMNS,
            ),
            (
                MEDIA_AUDIO_DDL,
                MediaAudioRow::ID_COLUMN,
                MediaAudioRow::TYPED_COLUMNS,
            ),
            (
                MEDIA_VISUAL_DDL,
                MediaVisualRow::ID_COLUMN,
                MediaVisualRow::TYPED_COLUMNS,
            ),
            (
                MEDIA_FILES_DDL,
                MediaFileRow::ID_COLUMN,
                MediaFileRow::TYPED_COLUMNS,
            ),
            (
                MEDIA_PLAYLISTS_DDL,
                MediaPlaylistRow::ID_COLUMN,
                MediaPlaylistRow::TYPED_COLUMNS,
            ),
            (
                MEDIA_PLAYLIST_ENTRIES_DDL,
                MediaPlaylistEntryRow::ID_COLUMN,
                MediaPlaylistEntryRow::TYPED_COLUMNS,
            ),
            (
                MEDIA_SCAN_META_DDL,
                MediaScanMetaRow::ID_COLUMN,
                MediaScanMetaRow::TYPED_COLUMNS,
            ),
        ] {
            let mut expect = vec![id_col.to_string()];
            expect.extend(typed.iter().map(|s| s.to_string()));
            assert_eq!(columns_in_ddl(ddl), expect, "column order for {id_col}");
        }
    }
}
