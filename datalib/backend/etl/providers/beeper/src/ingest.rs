//! One download of a Beeper Texts data directory: `index.db` and every
//! on-device bridge's `megabridge.db` through the SQLite mirror, then the
//! cached media files into the blob store. Everything is kept append-only:
//! the app's cache evicts, and keeping what it lets go is the point.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::CasInsert;
use datalib_etl::doltlite_raw::Migration;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::fswalk;
use datalib_etl_sqlite_mirror::{mirror, MirrorOptions, MirrorStats};
use sqlx::SqliteConnection;

pub const MEDIA_FILES: &str = "beeper_media_files";

/// One row per file copied out of `media/`, by its path under it: the
/// `mxc://` id an attachment names maps onto that path (render's
/// `media_path`).
const MEDIA_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS beeper_media_files (
    rel_path TEXT PRIMARY KEY,
    blake3 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    CHECK (length(blake3) = 64)
)";

/// The tables this provider keeps beside the mirrored ones.
pub const ALL_DDL: &[&str] = &[MEDIA_FILES_DDL];

datalib_etl::raw_db!(pub RawDb: CasEntityStore, ALL_DDL, LADDER);

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "the reshaped tables go: the store mirrors index.db",
    apply: |conn| Box::pin(drop_reshaped_tables(conn)),
}];

/// An earlier build read `index.db` into tables of its own. The next run
/// mirrors the cache whole, so they go rather than carry rows nothing
/// reads; their indexes go with them.
async fn drop_reshaped_tables(conn: &mut SqliteConnection) -> Result<()> {
    for table in [
        "rooms",
        "rooms_bookkeeping",
        "users",
        "users_bookkeeping",
        "events",
        "events_bookkeeping",
        "beeper_media_attachments",
        "beeper_media_attachments_bookkeeping",
    ] {
        // Audited: `table` is a literal from the list above.
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
            .execute(&mut *conn)
            .await
            .with_context(|| format!("drop {table}"))?;
    }
    Ok(())
}

/// Bytes read before they go into the blob store in one write.
const PUT_BATCH_BYTES: usize = 64 * 1024 * 1024;

pub fn default_beeper_data_dir() -> PathBuf {
    // The macOS location; Beeper Texts keeps nothing elsewhere that this
    // reads, so another platform names its directory in `texts.path`.
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.unwrap_or_default()
        .join("Library/Application Support/BeeperTexts")
}

/// The tables of `index.db` render reads. `mx_room_messages` declares
/// its local row number as its key, which a re-index renumbers; a
/// message is its Matrix event in its room.
fn index_options(index_db: &Path) -> MirrorOptions {
    MirrorOptions {
        include_tables: [
            "threads",
            "participants",
            "mx_room_messages",
            "mx_reactions",
        ]
        .map(String::from)
        .to_vec(),
        exclude_columns: vec!["mx_room_messages.id".to_string()],
        primary_keys: BTreeMap::from([(
            "mx_room_messages".to_string(),
            vec!["roomID".to_string(), "eventID".to_string()],
        )]),
        sidecar_tables: vec![MEDIA_FILES.to_string()],
        append_only: true,
        ..MirrorOptions::new(index_db)
    }
}

/// An on-device bridge's own record of each message: the network's id
/// for it (`id`, `part_id`) beside its Matrix event (`mxid`), which is
/// what matches a message here to the same one in a phone's backup.
/// Every bridge writes the same schema, so their `message` tables land in
/// one, told apart by `bridge_id`.
fn bridge_options(megabridge_db: &Path) -> MirrorOptions {
    MirrorOptions {
        include_tables: vec!["message".to_string()],
        exclude_columns: vec!["message.rowid".to_string()],
        primary_keys: BTreeMap::from([(
            "message".to_string(),
            vec!["bridge_id".to_string(), "mxid".to_string()],
        )]),
        sidecar_tables: vec![MEDIA_FILES.to_string()],
        append_only: true,
        ..MirrorOptions::new(megabridge_db)
    }
}

#[derive(Debug, Default)]
pub struct Summary {
    pub index: Option<MirrorStats>,
    pub bridges: usize,
    pub media_files_copied: usize,
}

impl Summary {
    pub fn summary(&self) -> String {
        let index = self
            .index
            .as_ref()
            .map_or_else(|| "index.db: nothing mirrored".to_string(), |s| s.summary());
        format!(
            "{index} bridges={} media_files_copied={}",
            self.bridges, self.media_files_copied
        )
    }
}

/// Mirror `beeper_dir` into `db`. Does not commit.
pub async fn fetch(
    beeper_dir: &Path,
    db: &RawDb,
    media: bool,
    progress: &Progress,
    found: &RunProblems,
) -> Result<Summary> {
    let index_db = beeper_dir.join("index.db");
    let index = mirror::run_or_report(db.pool(), &index_options(&index_db), progress, found)
        .await
        .with_context(|| format!("mirror {}", index_db.display()))?;

    let mut bridges = 0;
    for (dir, path) in megabridge_dbs(beeper_dir, found) {
        match mirror::run(db.pool(), &bridge_options(&path), progress).await {
            Ok(_) => bridges += 1,
            Err(e) => found.phase(&format!("megabridge {dir}"), format!("{e:#}")),
        }
    }

    let media_files_copied = if media {
        copy_media(db, &beeper_dir.join("media"), found).await?
    } else {
        0
    };
    Ok(Summary {
        index,
        bridges,
        media_files_copied,
    })
}

/// `(directory name, path)` of every `local-*/megabridge.db`, in name order.
fn megabridge_dbs(beeper_dir: &Path, found: &RunProblems) -> Vec<(String, PathBuf)> {
    let entries = match std::fs::read_dir(beeper_dir) {
        Ok(entries) => entries,
        Err(e) => {
            found.phase("megabridge", format!("{}: {e}", beeper_dir.display()));
            return Vec::new();
        }
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path().join("megabridge.db");
            (name.starts_with("local-") && path.is_file()).then_some((name, path))
        })
        .collect();
    out.sort();
    out
}

/// Copy every cached file the store does not hold yet. The app names a
/// cached file by its media id and never rewrites it, so one already held
/// is not read again, and one the app has since evicted keeps its row.
async fn copy_media(db: &RawDb, media_root: &Path, found: &RunProblems) -> Result<usize> {
    if !media_root.is_dir() {
        return Ok(0);
    }
    let held: HashSet<String> = sqlx::query_scalar("SELECT rel_path FROM beeper_media_files")
        .fetch_all(db.pool())
        .await
        .context("read beeper_media_files")?
        .into_iter()
        .collect();
    let (files, errors) = fswalk::walk_files(media_root, &[], None, |_| true)?;
    if let Some(first) = errors.first() {
        found.phase(
            "media",
            format!(
                "{} entries under {} would not list; first {}: {}",
                errors.len(),
                media_root.display(),
                first.path.display(),
                first.error
            ),
        );
    }

    let mut copied = 0;
    let mut pending: Vec<(String, Vec<u8>)> = Vec::new();
    let mut pending_bytes = 0;
    for f in files.into_iter().filter(|f| !held.contains(&f.rel)) {
        match std::fs::read(&f.path) {
            Ok(bytes) => {
                pending_bytes += bytes.len();
                pending.push((f.rel, bytes));
            }
            Err(e) => found.record_failed(MEDIA_FILES, &f.rel, e.to_string()),
        }
        if pending_bytes >= PUT_BATCH_BYTES {
            copied += store_media(db, std::mem::take(&mut pending)).await?;
            pending_bytes = 0;
        }
    }
    copied += store_media(db, pending).await?;
    // A folder that would not list hid the files under it from this run.
    if errors.is_empty() {
        found.records_tried_all(MEDIA_FILES);
    }
    Ok(copied)
}

async fn store_media(db: &RawDb, files: Vec<(String, Vec<u8>)>) -> Result<usize> {
    let inserts = files
        .iter()
        .map(|(rel, bytes)| CasInsert {
            id: rel.as_str(),
            bytes: bytes.as_slice(),
            content_type: None,
        })
        .collect();
    let keys = db
        .cas()
        .put_many(inserts)
        .await
        .context("put media files into the blob store")?;
    let mut tx = db.pool().begin().await.context("begin media files tx")?;
    for (rel, bytes) in &files {
        sqlx::query(
            "INSERT OR IGNORE INTO beeper_media_files (rel_path, blake3, size_bytes) \
             VALUES (?, ?, ?)",
        )
        .bind(rel)
        .bind(&keys[rel.as_str()])
        .bind(bytes.len() as i64)
        .execute(&mut *tx)
        .await
        .context("insert beeper_media_files")?;
    }
    tx.commit().await.context("commit media files tx")?;
    Ok(files.len())
}
