//! The per-source render output store: one doltlite database holding
//! every row a source's render produced, plus what render could not do.
//!
//! The index reads it by asking `dolt_diff` what moved since the commit it
//! last consumed, which is also how a document a source stopped holding gets
//! named and deleted.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_schema::edges::DDL as EDGES_DDL;
use datalib_schema::grid_rows::DDL as GRID_ROWS_DDL;
use datalib_schema::markdowns::DDL as MARKDOWNS_DDL;
use datalib_schema::render_problems::{RenderProblemRow, ScopeKind, DDL as RENDER_PROBLEMS_DDL};

use crate::bulk::BulkUpsertable;
use crate::grid_index::{RenderedMarkdown, WriteLock};

/// File name inside a source's `rendered_md/`.
pub const STORE_FILE: &str = "indexed_markdown.doltlite_db";

pub fn path_for(rendered_root: &Path) -> PathBuf {
    rendered_root.join(STORE_FILE)
}

/// Every `CREATE TABLE` this store holds, in creation order.
///
/// One list so the DDL pass cannot cover a different set than the
/// schema check — the same reason `grid_index::index_ddl` exists.
fn store_ddl() -> Vec<&'static str> {
    GRID_ROWS_DDL
        .iter()
        .chain(MARKDOWNS_DDL.iter())
        .chain(EDGES_DDL.iter())
        .chain(RENDER_PROBLEMS_DDL.iter())
        .map(|(_table, ddl)| *ddl)
        .collect()
}

/// A source's render output store, open for writing.
pub struct IndexedMarkdownStore {
    pool: SqlitePool,
    write_lock: WriteLock,
    path: PathBuf,
    /// The run-pinned "now" stamped onto problem rows — see
    /// [`Self::with_now`].
    now: String,
}

/// Run a future to completion from a synchronous caller.
pub fn blocking<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build a runtime for a blocking store call")
            .block_on(fut),
    }
}

impl IndexedMarkdownStore {
    pub fn open(rendered_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(rendered_root)
            .with_context(|| format!("mkdir -p {}", rendered_root.display()))?;
        let path = path_for(rendered_root);
        let pool = blocking(crate::doltlite_raw::open_derived(&path, &store_ddl()))
            .with_context(|| format!("open indexed markdown store {}", path.display()))?;
        Ok(Self {
            write_lock: WriteLock::new(pool.clone()),
            pool,
            path,
            now: datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs(),
        })
    }

    /// Open somebody else's render store to read it.
    ///
    /// The index is not this store's owner — the render step is — so this goes
    /// through [`crate::doltlite_raw::open_reader`] and performs none of the
    /// writes [`Self::open`] does on the way in. Read that function's note for
    /// what those are and why they are a hazard here specifically.
    ///
    /// No `now`, no write lock: nothing reached through this handle may write.
    pub fn open_for_reading(rendered_root: &Path) -> Result<Self> {
        let path = path_for(rendered_root);
        let pool = blocking(crate::doltlite_raw::open_reader(&path))
            .with_context(|| format!("open render store for reading {}", path.display()))?;
        Ok(Self {
            write_lock: WriteLock::new(pool.clone()),
            pool,
            path,
            now: String::new(),
        })
    }

    /// Use the run-pinned "now" (`--now` / `$DATALIB_DAG_NOW`) for the
    /// problem timestamps this store stamps, so every row one render
    /// writes agrees. Without it the store samples its own clock at
    /// open, which is right for a test and near enough for a one-off
    /// tool, but leaves a long run's rows spread over its duration.
    pub fn with_now(mut self, now: &str) -> Self {
        if !now.is_empty() {
            self.now = now.to_string();
        }
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn prior_fingerprints(&self) -> Result<HashMap<String, String>> {
        blocking(async {
            let rows = sqlx::query(
                "SELECT markdown_uuid, source_fingerprint FROM markdowns \
                 WHERE source_fingerprint IS NOT NULL",
            )
            .fetch_all(&self.pool)
            .await
            .context("read prior fingerprints")?;
            let mut out = HashMap::with_capacity(rows.len());
            for r in rows {
                out.insert(r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?);
            }
            Ok(out)
        })
    }

    pub fn render_versions(&self) -> Result<BTreeSet<u32>> {
        blocking(async {
            let rows = sqlx::query(
                "SELECT DISTINCT renderer_version FROM markdowns \
                 WHERE renderer_version IS NOT NULL",
            )
            .fetch_all(&self.pool)
            .await
            .context("read renderer versions")?;
            let mut out = BTreeSet::new();
            for r in rows {
                let v: String = r.try_get(0)?;
                if let Some(n) = v.rsplit('.').next().and_then(|s| s.parse::<u32>().ok()) {
                    out.insert(n);
                }
            }
            Ok(out)
        })
    }

    pub fn put_document(&self, out_dir: &Path, md: &RenderedMarkdown) -> Result<()> {
        blocking(async {
            crate::grid_index::apply_one(&self.write_lock, out_dir, md, None)
                .await
                .with_context(|| format!("apply {}", md.markdown_uuid))?;
            self.sweep_problems(&md.markdown_uuid, &md.problems).await
        })
    }

    /// Drop one document: its rows here, and the `.md` file itself.
    ///
    /// The file matters as much as the rows. `md_path` is what
    /// `/applet/unified_index/chat/{uuid}` serves and what qmd indexed, so a
    /// document deleted from the store but left on disk stays searchable and
    /// still resolves — a deletion the user can still read.
    pub fn remove_document(&self, out_dir: &Path, markdown_uuid: &str) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            let md_path: Option<String> =
                sqlx::query_scalar("SELECT md_path FROM markdowns WHERE markdown_uuid = ?")
                    .bind(markdown_uuid)
                    .fetch_optional(&mut **conn)
                    .await
                    .with_context(|| format!("read md_path for {markdown_uuid}"))?
                    .flatten();
            for sql in [
                "DELETE FROM grid_rows WHERE markdown_uuid = ?",
                "DELETE FROM edges WHERE src_markdown_uuid = ?",
                "DELETE FROM markdowns WHERE markdown_uuid = ?",
            ] {
                sqlx::query(sql)
                    .bind(markdown_uuid)
                    .execute(&mut **conn)
                    .await
                    .with_context(|| format!("remove {markdown_uuid} from the store"))?;
            }
            sqlx::query("DELETE FROM render_problems WHERE scope_kind = ? AND scope_key = ?")
                .bind(ScopeKind::Markdown.as_str())
                .bind(markdown_uuid)
                .execute(&mut **conn)
                .await
                .with_context(|| format!("remove {markdown_uuid} from the store"))?;
            drop(guard);
            if let Some(rel) = md_path {
                unlink_rendered(out_dir, &rel);
            }
            Ok(())
        })
    }

    /// Every `markdown_uuid` whose rows belong to `conversation_uuid`.
    ///
    /// The indirection exists because a provider that periodizes — slack per
    /// thread-month, beeper and signal per period — turns one upstream
    /// conversation into several documents, and their count is a fact about
    /// what was rendered rather than anything the provider can recompute
    /// once the conversation is gone from the raw store. The store is the
    /// only thing that still knows.
    pub fn documents_for_conversation(&self, conversation_uuid: &str) -> Result<Vec<String>> {
        blocking(async {
            let rows = sqlx::query(
                "SELECT DISTINCT markdown_uuid FROM grid_rows \
                 WHERE conversation_uuid = ? AND markdown_uuid IS NOT NULL",
            )
            .bind(conversation_uuid)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("documents for conversation {conversation_uuid}"))?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>(0).map_err(Into::into))
                .collect()
        })
    }

    /// Every document this store holds. The other half of a retain sweep:
    /// a renderer that walked its whole raw store says what should be here,
    /// and whatever else is here is what the store lost.
    pub fn all_document_uuids(&self) -> Result<Vec<String>> {
        blocking(async {
            let rows = sqlx::query("SELECT markdown_uuid FROM markdowns")
                .fetch_all(&self.pool)
                .await
                .context("list every document in the store")?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>(0).map_err(Into::into))
                .collect()
        })
    }
}

/// Delete a rendered document's file, and the per-document directory it sat
/// in once that is empty (`<source>/rendered_md/<uuid>/all.md` is the usual
/// shape, and leaving the empty parent behind makes a deleted conversation
/// still look present to anyone listing the tree).
///
/// Best-effort by design: a file already gone is the state we wanted, and a
/// tree we cannot write is not worth failing a render over once the rows —
/// the thing the grid reads — are gone.
fn unlink_rendered(out_dir: &Path, md_path_rel: &str) {
    let abs = out_dir.join(md_path_rel);
    if let Err(e) = std::fs::remove_file(&abs) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                path = %abs.display(),
                error = %e,
                "render: could not delete the markdown of a document that went away",
            );
            return;
        }
    }
    if let Some(dir) = abs.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

impl IndexedMarkdownStore {
    async fn sweep_problems(
        &self,
        markdown_uuid: &str,
        problems: &[RenderProblemRow],
    ) -> Result<()> {
        let mut guard = self.write_lock.acquire().await?;
        let conn = guard.conn();
        // Read the prior `first_seen_at` for every uuid about to be
        // rewritten, *before* the delete. This is the whole reason the
        // store stamps these rather than the renderer: a renderer that
        // set both timestamps to "now" every run would make
        // `first_seen_at` a synonym for `last_seen_at`, and "this has
        // been broken since Tuesday" would be unanswerable.
        let seen: HashMap<String, String> = sqlx::query(
            "SELECT uuid, first_seen_at FROM render_problems \
             WHERE scope_kind = ? AND scope_key = ?",
        )
        .bind(ScopeKind::Markdown.as_str())
        .bind(markdown_uuid)
        .fetch_all(&mut **conn)
        .await
        .context("read prior first_seen_at")?
        .into_iter()
        .map(|r| Ok((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?)))
        .collect::<Result<_>>()?;
        sqlx::query("DELETE FROM render_problems WHERE scope_kind = ? AND scope_key = ?")
            .bind(ScopeKind::Markdown.as_str())
            .bind(markdown_uuid)
            .execute(&mut **conn)
            .await
            .context("clear prior problems for this document")?;
        self.insert_problems(conn, problems, &seen).await
    }

    /// Insert problem rows, stamping `first_seen_at` / `last_seen_at`.
    /// `seen` maps a uuid to the `first_seen_at` it already had, which
    /// is carried forward; anything absent is new and gets `now` for
    /// both.
    async fn insert_problems(
        &self,
        conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
        problems: &[RenderProblemRow],
        seen: &HashMap<String, String>,
    ) -> Result<()> {
        for p in problems {
            let stamped = RenderProblemRow {
                first_seen_at: seen
                    .get(&p.uuid)
                    .cloned()
                    .unwrap_or_else(|| self.now.clone()),
                last_seen_at: self.now.clone(),
                ..p.clone()
            };
            // Same generated write path the rows use; see
            // `PortableTable`'s `BulkUpsertable` impl.
            let sql = crate::bulk::insert_sql::<RenderProblemRow>();
            // Audited: `sql` is built from `RenderProblemRow`'s
            // associated consts, never from row data; all values bound.
            stamped
                .bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
                .execute(&mut **conn)
                .await
                .with_context(|| format!("insert render_problem {}", p.uuid))?;
        }
        Ok(())
    }

    /// Problems not attached to any document — a payload that would not
    /// deserialize has no `markdown_uuid` to hang off. Swept by the
    /// raw-store entity id instead, so they clear when that entity is
    /// next parsed successfully.
    pub fn put_entity_problems(
        &self,
        entity_id: &str,
        problems: &[RenderProblemRow],
    ) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            let seen: HashMap<String, String> = sqlx::query(
                "SELECT uuid, first_seen_at FROM render_problems \
                 WHERE scope_kind = ? AND scope_key = ?",
            )
            .bind(ScopeKind::Entity.as_str())
            .bind(entity_id)
            .fetch_all(&mut **conn)
            .await
            .context("read prior first_seen_at")?
            .into_iter()
            .map(|r| Ok((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?)))
            .collect::<Result<_>>()?;
            sqlx::query("DELETE FROM render_problems WHERE scope_kind = ? AND scope_key = ?")
                .bind(ScopeKind::Entity.as_str())
                .bind(entity_id)
                .execute(&mut **conn)
                .await
                .context("clear prior problems for this entity")?;
            self.insert_problems(conn, problems, &seen).await
        })
    }

    pub fn documents(
        &self,
        out_dir: &Path,
        pin: &crate::pin::Pin,
    ) -> Result<Vec<RenderedMarkdown>> {
        self.documents_matching(out_dir, None, pin)
    }

    pub fn changed_since(&self, cursor: Option<&str>) -> Result<crate::doltlite_raw::DiffScan> {
        blocking(crate::doltlite_raw::scan_buckets(
            &self.pool,
            cursor,
            &crate::doltlite_raw::DiffScanSpec {
                // Nothing in a render store fans out to "re-index
                // everything": every row already names the document it
                // belongs to. The providers need this for tables like
                // `users` / `channels`, whose rename shows up inside
                // every rendered doc; by the time rows reach here that
                // fan-out has already happened, on the render side.
                global_fanout_tables: &[],
                bucket_query: "
                    SELECT DISTINCT markdown_uuid FROM (
                        SELECT coalesce(to_markdown_uuid, from_markdown_uuid) AS markdown_uuid
                          FROM dolt_diff_markdowns
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_markdown_uuid, from_markdown_uuid) AS markdown_uuid
                          FROM dolt_diff_grid_rows
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                        UNION
                        SELECT coalesce(to_src_markdown_uuid, from_src_markdown_uuid)
                                 AS markdown_uuid
                          FROM dolt_diff_edges
                         WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    )
                    WHERE markdown_uuid IS NOT NULL
                ",
            },
        ))
    }

    /// [`documents`](Self::documents), restricted to `only` when it is
    /// `Some`. An id in `only` with no document behind it is simply
    /// absent from the result — that is how a *deletion* reaches the
    /// caller, which compares what it asked for against what it got.
    ///
    /// Reads at `pin`, which must be the commit the caller's
    /// [`changed_since`](Self::changed_since) scanned to — otherwise the
    /// changed set and the rows behind it describe different commits. This
    /// is the read side of the store; the write side above deliberately does
    /// not go through the pinned views, since a render step writing its own
    /// store has nothing to protect itself from.
    pub fn documents_matching(
        &self,
        out_dir: &Path,
        only: Option<&HashSet<String>>,
        pin: &crate::pin::Pin,
    ) -> Result<Vec<RenderedMarkdown>> {
        blocking(async {
            crate::pin::install_views(&self.pool, pin)
                .await
                .context("install pinned views over the render store")?;
            let mds: Vec<datalib_schema::markdowns::MarkdownRow> =
                sqlx::query_as("SELECT * FROM pinned_markdowns ORDER BY markdown_uuid")
                    .fetch_all(&self.pool)
                    .await
                    .context("read markdowns")?;
            let mds: Vec<_> = match only {
                Some(keep) => mds
                    .into_iter()
                    .filter(|m| keep.contains(&m.markdown_uuid))
                    .collect(),
                None => mds,
            };
            let mut out = Vec::with_capacity(mds.len());
            for md in mds {
                let rows: Vec<datalib_schema::grid_rows::GridRow> = sqlx::query_as(
                    "SELECT * FROM pinned_grid_rows WHERE markdown_uuid = ? ORDER BY uuid",
                )
                .bind(&md.markdown_uuid)
                .fetch_all(&self.pool)
                .await
                .with_context(|| format!("read rows for {}", md.markdown_uuid))?;
                let edges: Vec<datalib_schema::edges::EdgeRow> = sqlx::query_as(
                    "SELECT * FROM pinned_edges WHERE src_markdown_uuid = ? ORDER BY edge_uuid",
                )
                .bind(&md.markdown_uuid)
                .fetch_all(&self.pool)
                .await
                .with_context(|| format!("read edges for {}", md.markdown_uuid))?;
                // `renderer_version` is `"<index>.<render>"`; the render
                // half is what the renderer declared.
                let render_version = md
                    .renderer_version
                    .as_deref()
                    .and_then(|v| v.rsplit('.').next())
                    .and_then(|s| s.parse::<u32>().ok())
                    .unwrap_or(0);
                out.push(RenderedMarkdown {
                    markdown_uuid: md.markdown_uuid.clone(),
                    source_name: md.source_name.clone(),
                    source_fingerprint: md.source_fingerprint.clone().unwrap_or_default(),
                    upstream_cursor: md.upstream_cursor.clone(),
                    md_path: match md.md_path.as_deref() {
                        Some(rel) => out_dir.join(rel),
                        None => PathBuf::from(&md.markdown_uuid),
                    },
                    render_version,
                    rows,
                    edges,
                    problems: Vec::new(),
                });
            }
            Ok(out)
        })
    }

    pub fn problem_counts(&self) -> Result<HashMap<String, i64>> {
        blocking(async {
            let rows =
                sqlx::query("SELECT outcome, COUNT(*) FROM render_problems GROUP BY outcome")
                    .fetch_all(&self.pool)
                    .await
                    .context("count problems")?;
            let mut out = HashMap::new();
            for r in rows {
                out.insert(r.try_get::<String, _>(0)?, r.try_get::<i64, _>(1)?);
            }
            Ok(out)
        })
    }

    /// One `dolt_commit` for the whole render, not one per document.
    pub fn commit(&self, summary: &str) -> Result<Option<String>> {
        blocking(crate::doltlite_raw::commit_run(&self.pool, summary))
    }

    pub fn close(self) {
        blocking(self.pool.close());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;
    use datalib_schema::render_problems::Stage;
    use datalib_schema::render_problems::{Outcome, Problem, Reason};

    fn store(dir: &Path) -> IndexedMarkdownStore {
        IndexedMarkdownStore::open(dir).expect("open store")
    }

    fn row(uuid: &str, markdown_uuid: &str) -> GridRow {
        GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Test)
            .kind("Test")
            .source_label("Test")
            .conversation_uuid(markdown_uuid)
            .entire_chat(format!("/chat/{markdown_uuid}"))
            .text("hello")
            .markdown_uuid(Some(markdown_uuid.to_string()))
            .when_ts(Some("2026-01-01T00:00:00+00:00".to_string()))
            .build()
            .expect("row")
    }

    fn doc(dir: &Path, markdown_uuid: &str, fingerprint: &str) -> RenderedMarkdown {
        doc_with(dir, markdown_uuid, fingerprint, Vec::new())
    }

    fn doc_with(
        dir: &Path,
        markdown_uuid: &str,
        fingerprint: &str,
        problems: Vec<RenderProblemRow>,
    ) -> RenderedMarkdown {
        RenderedMarkdown {
            markdown_uuid: markdown_uuid.to_string(),
            source_name: "src".into(),
            source_fingerprint: fingerprint.into(),
            upstream_cursor: None,
            md_path: dir.join(format!("{markdown_uuid}.md")),
            render_version: 7,
            rows: vec![row(markdown_uuid, markdown_uuid)],
            edges: Vec::new(),
            problems,
        }
    }

    fn problem(uuid: &str, scope: &str) -> RenderProblemRow {
        RenderProblemRow {
            uuid: uuid.into(),
            scope_key: scope.into(),
            scope_kind: ScopeKind::Markdown.as_str().into(),
            source_name: "src".into(),
            stage: Stage::GridRow.as_str().into(),
            outcome: Outcome::Nulled.as_str().into(),
            problems: serde_json::to_string(&vec![Problem::field(
                "when_ts",
                Reason::CoercionFailed,
                "not-a-date",
            )])
            .unwrap(),
            first_seen_at: "2026-01-01T00:00:00+00:00".into(),
            last_seen_at: "2026-01-01T00:00:00+00:00".into(),
            render_version: 7,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_round_trips_and_its_fingerprint_comes_back() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        assert!(s.prior_fingerprints().unwrap().is_empty(), "fresh store");

        s.put_document(root, &doc(root, "md-1", "fp-1")).unwrap();

        let fps = s.prior_fingerprints().unwrap();
        assert_eq!(fps.get("md-1").map(String::as_str), Some("fp-1"));
        assert_eq!(s.render_versions().unwrap(), [7].into_iter().collect());
    }

    /// Re-rendering replaces a document's rows rather than accumulating
    /// them — the delete-then-insert that makes a re-render idempotent.
    #[tokio::test(flavor = "multi_thread")]
    async fn re_rendering_a_document_replaces_its_rows() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(root, &doc(root, "md-1", "fp-1")).unwrap();
        s.put_document(root, &doc(root, "md-1", "fp-2")).unwrap();

        let n: i64 = blocking(async {
            sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows")
                .fetch_one(&s.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 1, "one row, not two");
        assert_eq!(
            s.prior_fingerprints()
                .unwrap()
                .get("md-1")
                .map(String::as_str),
            Some("fp-2"),
            "the fingerprint moves with the re-render"
        );
    }

    /// The sweep is scoped to the document reprocessed. A document that
    /// was *skipped* this run keeps its problems — the failure mode
    /// worth a test, because the obvious "delete what wasn't re-emitted"
    /// rule empties the table on every steady-state run.
    #[tokio::test(flavor = "multi_thread")]
    async fn problems_survive_a_run_that_skipped_their_document() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(
            root,
            &doc_with(root, "md-1", "fp-1", vec![problem("row-a", "md-1")]),
        )
        .unwrap();
        s.put_document(
            root,
            &doc_with(root, "md-2", "fp-1", vec![problem("row-b", "md-2")]),
        )
        .unwrap();
        assert_eq!(s.problem_counts().unwrap().get("nulled").copied(), Some(2));

        // A second run that only reprocesses md-2, and finds it clean.
        s.put_document(root, &doc(root, "md-2", "fp-2")).unwrap();

        let counts = s.problem_counts().unwrap();
        assert_eq!(
            counts.get("nulled").copied(),
            Some(1),
            "md-2's problem cleared; md-1's must not have — it was never looked at"
        );
    }

    /// A problem clears when the document is reprocessed and comes back
    /// clean. This is "overwritten or removed upon later success".
    #[tokio::test(flavor = "multi_thread")]
    async fn a_fixed_document_loses_its_problems() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(
            root,
            &doc_with(root, "md-1", "fp-1", vec![problem("row-a", "md-1")]),
        )
        .unwrap();
        assert_eq!(s.problem_counts().unwrap().get("nulled").copied(), Some(1));

        s.put_document(root, &doc(root, "md-1", "fp-2")).unwrap();
        assert!(
            s.problem_counts().unwrap().is_empty(),
            "reprocessed clean ⇒ no problem rows left"
        );
    }
}
