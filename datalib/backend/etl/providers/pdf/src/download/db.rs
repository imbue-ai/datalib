//! Doltlite-backed raw store for the `pdf` provider.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::fswalk::{StampCursor, StampKind};

use super::schema_raw::{full_ddl, PdfDocumentRow, PdfPathRow, PdfScanMetaRow, DATA_TABLES};

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
    /// Documents already in `pdf_documents`, by hex blake3. A path
    /// whose hash we reused *and* whose document row exists needs no
    /// work at all.
    pub known_docs: HashSet<String>,
}

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
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

    /// Load the rescan cache. Must run **before** [`Self::reset`].
    pub async fn load_prev(&self) -> Result<PrevCache> {
        let mut cache = PrevCache::default();

        let rows =
            sqlx::query("SELECT id, blake3, mtime_ns, size, stamp_kind, inode, dev FROM pdf_paths")
                .fetch_all(&self.pool)
                .await
                .context("load pdf_paths cache")?;
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

        let docs = sqlx::query("SELECT blake3 FROM pdf_documents")
            .fetch_all(&self.pool)
            .await
            .context("load pdf_documents ids")?;
        for r in docs {
            cache.known_docs.insert(r.get::<String, _>("blake3"));
        }
        Ok(cache)
    }

    /// Truncate the **path** table so deletions fall out naturally: a
    /// path present last scan and absent now is simply not re-inserted.
    ///
    /// `pdf_documents` is deliberately left intact. It is keyed on
    /// content, not location, so it has no notion of "no longer
    /// present" — and dropping it would lose `first_seen_at` and force
    /// a full re-convert of every document whose path merely moved.
    /// Documents whose last path disappears become unreferenced rows;
    /// see `DOWNLOAD.md` §"Orphaned documents".
    pub async fn reset_paths(&self) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin truncate tx")?;
        for table in DATA_TABLES {
            sqlx::query(&format!("DELETE FROM {table}"))
                .execute(&mut *tx)
                .await
                .with_context(|| format!("truncate {table}"))?;
        }
        tx.commit().await.context("commit truncate tx")?;
        Ok(())
    }

    /// Record where this scan ran, so the render step does not have to
    /// be told again. See [`super::schema_raw::PDF_SCAN_META_DDL`].
    pub async fn write_scan_meta(&self, row: &PdfScanMetaRow) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin scan_meta tx")?;
        bulk_upsert_entity_in_tx(&mut tx, std::slice::from_ref(row))
            .await
            .context("upsert pdf_scan_meta")?;
        tx.commit().await.context("commit scan_meta tx")?;
        Ok(())
    }

    /// The absolute scan root recorded by the last download. `None`
    /// when no scan has run yet.
    pub async fn scan_root(&self) -> Result<Option<PathBuf>> {
        let row = sqlx::query("SELECT abs_root FROM pdf_scan_meta ORDER BY id LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("read pdf_scan_meta")?;
        Ok(row.map(|r| PathBuf::from(r.get::<String, _>("abs_root"))))
    }

    pub async fn write_batch(&self, docs: &[PdfDocumentRow], paths: &[PdfPathRow]) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin write tx")?;
        bulk_upsert_entity_in_tx(&mut tx, docs)
            .await
            .context("upsert pdf_documents")?;
        bulk_upsert_entity_in_tx(&mut tx, paths)
            .await
            .context("upsert pdf_paths")?;
        tx.commit().await.context("commit write tx")?;
        Ok(())
    }

    /// Everything the render side needs, joined: one row per document
    /// that is convertible, with a representative path to read from.
    ///
    /// `MIN(p.id)` makes the choice deterministic when a document has
    /// several copies, so two runs render byte-identical output.
    pub async fn convertible_documents(&self, root: &Path) -> Result<Vec<RenderTarget>> {
        let rows = sqlx::query(
            "SELECT d.blake3      AS blake3,
                    d.title       AS title,
                    d.author      AS author,
                    d.page_count  AS page_count,
                    d.pdf_type    AS pdf_type,
                    d.doc_created_at  AS doc_created_at,
                    d.doc_modified_at AS doc_modified_at,
                    MIN(p.id)     AS rel_path,
                    COUNT(p.id)   AS copy_count
               FROM pdf_documents d
               JOIN pdf_paths p ON p.blake3 = d.blake3
              WHERE d.needs_ocr = 0
              GROUP BY d.blake3
              ORDER BY d.blake3",
        )
        .fetch_all(&self.pool)
        .await
        .context("select convertible documents")?;

        Ok(rows
            .into_iter()
            .map(|r| {
                let rel: String = r.get("rel_path");
                RenderTarget {
                    blake3: r.get("blake3"),
                    title: r.get("title"),
                    author: r.get("author"),
                    page_count: r.get("page_count"),
                    pdf_type: r.get("pdf_type"),
                    doc_created_at: r.get("doc_created_at"),
                    doc_modified_at: r.get("doc_modified_at"),
                    abs_path: root.join(&rel),
                    rel_path: rel,
                    copy_count: r.get("copy_count"),
                }
            })
            .collect())
    }
}

/// One document the render step should convert.
#[derive(Debug, Clone)]
pub struct RenderTarget {
    pub blake3: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub page_count: i64,
    pub pdf_type: String,
    pub doc_created_at: Option<String>,
    pub doc_modified_at: Option<String>,
    /// Absolute path of a representative copy, for reading bytes.
    pub abs_path: PathBuf,
    /// That copy's root-relative path, for display.
    pub rel_path: String,
    /// How many paths currently hold these bytes.
    pub copy_count: i64,
}
