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
            // Audited: `table` iterates a `&'static str` const array of our own table
            // names; no runtime data reaches the statement.
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
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
    ///
    /// The `WHERE` clause is the render gate, and it is per *page*, not
    /// per document: anything with at least one readable page is worth
    /// converting, and the pages we could not read are noted in the
    /// markdown and counted in `ocr_page_count`. It used to read
    /// `d.needs_ocr = 0`, which skipped a document entirely if any one
    /// of its pages was a scan — so every `Mixed` document, the exact
    /// case the classification exists to describe, rendered nothing
    /// (issue #173). `has_encoding_issues` still suppresses the whole
    /// document; see [`super::schema_raw::document_is_renderable`],
    /// which this mirrors, for why that one is all-or-nothing.
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
              WHERE d.has_encoding_issues = 0
                AND d.page_count > d.ocr_page_count
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::schema_raw::{document_is_renderable, PdfDocumentRow, PdfKind};

    const NOW: &str = "2364-04-13T08:45:00-07:00";

    fn doc(blake3: &str, page_count: i64, ocr_page_count: i64, enc: bool) -> PdfDocumentRow {
        PdfDocumentRow {
            blake3: blake3.to_string(),
            size: 1,
            page_count,
            pdf_type: PdfKind::Mixed,
            confidence: 0.7,
            // Set the way `identify` now sets it: inclusive, so this
            // column cannot be what the gate keys on.
            needs_ocr: ocr_page_count > 0,
            ocr_page_count,
            has_encoding_issues: enc,
            title: None,
            author: None,
            doc_created_at: None,
            doc_modified_at: None,
            content_blake3: None,
            pdf_id_permanent: None,
            xmp_document_id: None,
            xmp_instance_id: None,
            xmp_original_document_id: None,
            first_seen_at: NOW.to_string(),
        }
    }

    fn path_row(id: &str, blake3: &str) -> PdfPathRow {
        PdfPathRow {
            id: id.to_string(),
            blake3: blake3.to_string(),
            mtime_ns: 0,
            size: 1,
            stamp_kind: StampKind::from_str_or_rescan("rescan"),
            inode: None,
            dev: None,
            last_seen_at: NOW.to_string(),
        }
    }

    /// The render gate, exercised through the query that actually runs.
    ///
    /// The bug this pins (#173) lived in the `WHERE` clause, so a test
    /// of any Rust-side predicate could not have caught it — the one
    /// that existed asserted `PdfKind::Mixed` was convertible and passed
    /// happily while the query skipped every Mixed document.
    #[tokio::test(flavor = "multi_thread")]
    async fn renders_only_documents_with_readable_pages() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let db = RawDb::open(&tmp.path().join("entities.doltlite_db")).await?;

        let docs = vec![
            // A long report with three scanned inserts: the #173 case.
            doc("aa", 200, 3, false),
            // Fully text.
            doc("bb", 2, 0, false),
            // Fully scanned: nothing to convert.
            doc("cc", 4, 4, false),
            // Readable pages, but the text decodes to mojibake.
            doc("dd", 10, 1, true),
        ];
        let paths: Vec<PdfPathRow> = docs
            .iter()
            .map(|d| path_row(&format!("{}.pdf", d.blake3), &d.blake3))
            .collect();
        db.write_batch(&docs, &paths).await?;

        let got: Vec<String> = db
            .convertible_documents(Path::new("/corpus"))
            .await?
            .into_iter()
            .map(|t| t.blake3)
            .collect();
        assert_eq!(got, vec!["aa".to_string(), "bb".to_string()]);

        // ...and the Rust-side statement of the same rule agrees, so the
        // doc comment on `document_is_renderable` is not describing
        // something other than what ships.
        for d in &docs {
            assert_eq!(
                document_is_renderable(d.page_count, d.ocr_page_count, d.has_encoding_issues),
                got.contains(&d.blake3),
                "predicate and query disagree on {}",
                d.blake3
            );
        }
        Ok(())
    }
}
