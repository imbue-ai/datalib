//! Doltlite-backed raw store for the `media` provider.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::fswalk::{StampCursor, StampKind};

use super::schema_raw::{
    full_ddl, MediaAudioRow, MediaFileRow, MediaItemRow, MediaPlaylistEntryRow, MediaPlaylistRow,
    MediaScanMetaRow, MediaVisualRow,
};

/// Conventional filename of this provider's entity store under
/// `<name>/raw/`.
pub fn db_path_for(raw_dir: &Path) -> PathBuf {
    datalib_etl::raw_layout::entities_db(raw_dir)
}

/// What a previous scan already knows, loaded once before the rebuild
/// so the walk never touches the database.
#[derive(Default)]
pub struct PrevCache {
    /// Per-path rescan cursor plus the hash we recorded for it. When
    /// the cursor still matches, we reuse the hash instead of reading
    /// the file.
    pub paths: HashMap<String, (StampCursor, String)>,
    /// Items already in `media_items`, by hex blake3. A path whose hash
    /// we reused *and* whose item row exists needs no work at all — no
    /// read, no container parse, no payload hash.
    pub known_items: HashSet<String>,
    /// Playlists recorded by a previous scan, by root-relative path.
    /// Consumed the same way [`Self::paths`] is.
    pub playlists: HashSet<String>,
}

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

/// One scan's worth of rows, accumulated and written together so that
/// an item and the path pointing at it land in the same transaction.
#[derive(Default)]
pub struct WriteBatch {
    pub items: Vec<MediaItemRow>,
    pub audio: Vec<MediaAudioRow>,
    pub visual: Vec<MediaVisualRow>,
    pub files: Vec<MediaFileRow>,
}

impl WriteBatch {
    pub fn len(&self) -> usize {
        self.files.len().max(self.items.len())
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
            && self.audio.is_empty()
            && self.visual.is_empty()
            && self.files.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.audio.clear();
        self.visual.clear();
        self.files.clear();
    }
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Load the rescan cache. Must run **before** [`Self::reset_paths`].
    pub async fn load_prev(&self) -> Result<PrevCache> {
        let mut cache = PrevCache::default();

        let rows = sqlx::query(
            "SELECT id, blake3, mtime_ns, size, stamp_kind, inode, dev FROM media_files",
        )
        .fetch_all(&self.pool)
        .await
        .context("load media_files cache")?;
        for r in rows {
            let id: String = r.get("id");
            let blake3: String = r.get("blake3");
            let cursor = StampCursor {
                mtime_ns: r.get("mtime_ns"),
                size: r.get("size"),
                stamp_kind: StampKind::from_str_or_rescan(&r.get::<String, _>("stamp_kind")),
                inode: r.get("inode"),
                dev: r.get("dev"),
            };
            cache.paths.insert(id, (cursor, blake3));
        }

        let items = sqlx::query("SELECT blake3 FROM media_items")
            .fetch_all(&self.pool)
            .await
            .context("load media_items ids")?;
        for r in items {
            cache.known_items.insert(r.get::<String, _>("blake3"));
        }

        let playlists = sqlx::query("SELECT id FROM media_playlists")
            .fetch_all(&self.pool)
            .await
            .context("load media_playlists ids")?;
        for r in playlists {
            cache.playlists.insert(r.get::<String, _>("id"));
        }
        Ok(cache)
    }

    /// Delete the path rows for files that are gone.
    ///
    /// `ids` is what the walk did **not** visit: the leftovers of the
    /// in-memory cache after each seen path was removed from it. That
    /// makes this a set difference rather than a timestamp sweep, which
    /// matters because `DATALIB_DAG_NOW` is pinned per run — two runs
    /// sharing a pinned `now` would make a `WHERE last_seen_at <> ?`
    /// sweep silently delete nothing.
    ///
    /// **Runs at the end of a scan, not the start.** Truncating up
    /// front — `fsindex` and `pdf` both do — throws away the rescan
    /// cursors for every file a killed scan had not yet reached. See
    /// [`super::schema_raw::DATA_TABLES`] and `DOWNLOAD.md`
    /// §"Interrupting a scan".
    pub async fn delete_files(&self, ids: &[String]) -> Result<u64> {
        self.delete_by_id("media_files", ids).await
    }

    /// Delete playlists that are gone, and their entries with them.
    pub async fn delete_playlists(&self, ids: &[String]) -> Result<u64> {
        let n = self.delete_by_id("media_playlists", ids).await?;
        self.delete_where("media_playlist_entries", "playlist_id", ids)
            .await?;
        Ok(n)
    }

    /// Drop one playlist's entries before its new ones are written.
    ///
    /// Entries are keyed `<path>#<position>`, so upserting alone would
    /// leave the tail behind when a playlist is *shortened* — the rows
    /// for the positions that no longer exist. Deleting the playlist's
    /// entries first makes the rewrite exact.
    pub async fn clear_playlist_entries(&self, playlist_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM media_playlist_entries WHERE playlist_id = ?")
            .bind(playlist_id)
            .execute(&self.pool)
            .await
            .context("clear playlist entries")?;
        Ok(())
    }

    async fn delete_by_id(&self, table: &str, ids: &[String]) -> Result<u64> {
        self.delete_where(table, "id", ids).await
    }

    async fn delete_where(&self, table: &str, column: &str, ids: &[String]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut removed = 0u64;
        let mut tx = self.pool.begin().await.context("begin delete tx")?;
        // Chunked so a library that lost a hundred thousand files does
        // not build one statement with a hundred thousand placeholders.
        for chunk in ids.chunks(datalib_etl::bulk::SQL_CHUNK) {
            let mut sql = format!("DELETE FROM {table} WHERE {column} IN (");
            datalib_etl::bulk::push_placeholder_list(&mut sql, chunk.len());
            sql.push(')');
            // Audited: `table` and `column` are literals at every callsite; the IN-list
            // is a placeholder run and every id is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind(id);
            }
            let r = q
                .execute(&mut *tx)
                .await
                .with_context(|| format!("delete from {table}"))?;
            removed += r.rows_affected();
        }
        tx.commit().await.context("commit delete tx")?;
        Ok(removed)
    }

    /// Record where this scan ran.
    pub async fn write_scan_meta(&self, row: &MediaScanMetaRow) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin scan_meta tx")?;
        bulk_upsert_entity_in_tx(&mut tx, std::slice::from_ref(row))
            .await
            .context("upsert media_scan_meta")?;
        tx.commit().await.context("commit scan_meta tx")?;
        Ok(())
    }

    /// The absolute scan root recorded by the last download.
    pub async fn scan_root(&self) -> Result<Option<PathBuf>> {
        let row = sqlx::query("SELECT abs_root FROM media_scan_meta ORDER BY id LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("read media_scan_meta")?;
        Ok(row.map(|r| PathBuf::from(r.get::<String, _>("abs_root"))))
    }

    /// One transaction per batch, items before files: the FK direction
    /// is `media_files.blake3 -> media_items.blake3`, so writing them
    /// together is what keeps a path row from ever referring to an item
    /// that is not there.
    pub async fn write_batch(&self, b: &WriteBatch) -> Result<()> {
        if b.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin write tx")?;
        bulk_upsert_entity_in_tx(&mut tx, &b.items)
            .await
            .context("upsert media_items")?;
        bulk_upsert_entity_in_tx(&mut tx, &b.audio)
            .await
            .context("upsert media_audio")?;
        bulk_upsert_entity_in_tx(&mut tx, &b.visual)
            .await
            .context("upsert media_visual")?;
        bulk_upsert_entity_in_tx(&mut tx, &b.files)
            .await
            .context("upsert media_files")?;
        tx.commit().await.context("commit write tx")?;
        Ok(())
    }

    pub async fn write_playlists(
        &self,
        playlists: &[MediaPlaylistRow],
        entries: &[MediaPlaylistEntryRow],
    ) -> Result<()> {
        if playlists.is_empty() && entries.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin playlist tx")?;
        bulk_upsert_entity_in_tx(&mut tx, playlists)
            .await
            .context("upsert media_playlists")?;
        bulk_upsert_entity_in_tx(&mut tx, entries)
            .await
            .context("upsert media_playlist_entries")?;
        tx.commit().await.context("commit playlist tx")?;
        Ok(())
    }
}
