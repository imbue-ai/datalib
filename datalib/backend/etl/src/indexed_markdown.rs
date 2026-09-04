//! The per-source render output store: one doltlite database holding
//! every row a source's render produced, plus what render could not do.
//!
//! Replaces the per-document `<id>.grid_rows.json` sidecar. The
//! argument for the swap, in one sentence: **the sidecar tree was a
//! hand-rolled version of what doltlite already does one stage
//! earlier.** Download → raw store gets "what changed since my cursor"
//! from `dolt_diff`; render → sidecar tree re-implemented the same
//! question with fingerprints and full tree walks, and most of the
//! render driver's complexity was the cost of that re-implementation.
//! See `docs/dev/data_architecture_parse_and_render.md` §2.
//!
//! ```text
//! <data_root>/<name>/rendered_md/
//!   indexed_markdown.doltlite_db   <- this
//!   <chat_uuid>/<period>.md        <- still files; qmd indexes the tree
//! ```
//!
//! ## The tables are the index's tables
//!
//! `grid_rows`, `markdowns` and `edges` here use the **same derived DDL**
//! as the unified index, and are written by the **same code**
//! ([`crate::grid_index::apply_one`]). "All the same rows that will get
//! stacked into the unified index" is therefore true by construction
//! rather than by convention — there is no second projection to keep in
//! step, and no second INSERT to drift.
//!
//! `render_problems` is the fourth table and the new one: R1's sink.
//! It lives *here*, beside the rows, rather than in a log or a store of
//! its own, because a document's rows and the record of what was
//! dropped or nulled getting them there then commit in one transaction
//! and can never disagree about which run they came from.
//!
//! ## One writer, and why that is the file boundary
//!
//! Doltlite's working set is per *file* and shared across processes, so
//! two writers on one file commit each other's in-flight rows. The DAG
//! runs steps with `parallelism: 4`, so a single shared problems store
//! would have four concurrent writers. One file per source gives each
//! render step sole ownership of its own — the same rule the raw stores
//! follow, for the same reason.
//!
//! It also must not live inside the source's `raw/entities.doltlite_db`:
//! that file's single writer is the *download* step, and render writing
//! to it would reintroduce the problem across stages instead of across
//! sources.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_schema::edges::DDL as EDGES_DDL;
use datalib_schema::grid_rows::DDL as GRID_ROWS_DDL;
use datalib_schema::markdowns::DDL as MARKDOWNS_DDL;
use datalib_schema::render_problems::{RenderProblemRow, DDL as RENDER_PROBLEMS_DDL};

use crate::bulk::BulkUpsertable;
use crate::grid_index::{RenderedMarkdown, WriteLock};

/// File name inside a source's `rendered_md/`.
///
/// `.doltlite_db`, matching every other store in the tree — the
/// extension is what `doltlite_raw::db_path_for` branches on and what
/// the CLI recipes in `AGENTS.md` glob for.
pub const STORE_FILE: &str = "indexed_markdown.doltlite_db";

/// Where a source's store lives, given its `rendered_md/` directory.
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
///
/// Sync on the outside: the render path is driven by `futures`'
/// executor on a blocking thread, and providers reach their databases
/// through `block_in_place` + `Handle::current().block_on` (see
/// `slack::render::parse::parse_doltlite`). This follows that, so a
/// renderer does not have to become async to write a row.
pub struct IndexedMarkdownStore {
    pool: SqlitePool,
    write_lock: WriteLock,
    path: PathBuf,
}

/// Run a future to completion from the render path's sync context.
fn blocking<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

impl IndexedMarkdownStore {
    /// Open (creating if absent) the store for one source.
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
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `markdown_uuid → source_fingerprint` for every document already
    /// in the store — the render skip state.
    ///
    /// This is the query that replaces walking the whole sidecar tree
    /// and parsing every header to rebuild the same map.
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

    /// Every distinct renderer version stamped into this store.
    ///
    /// A healthy store holds exactly the versions the current
    /// processors produce; anything else was written by a different
    /// build. `renderer_version` is stored as `"<index>.<render>"`, so
    /// this parses the trailing component back out.
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

    /// Write one rendered document: its `markdowns` row, its
    /// `grid_rows`, its outgoing `edges`, and whatever render had to say
    /// about the records that produced them.
    ///
    /// Rows and edges are replaced wholesale for this `markdown_uuid`
    /// (delete-then-insert, via [`crate::grid_index::apply_one`]), which
    /// is what makes re-rendering a document idempotent.
    ///
    /// `problems` are swept the same way, and the scoping is the subtle
    /// part — see [`Self::sweep_problems`].
    pub fn put_document(
        &self,
        out_dir: &Path,
        md: &RenderedMarkdown,
        problems: &[RenderProblemRow],
    ) -> Result<()> {
        blocking(async {
            crate::grid_index::apply_one(&self.write_lock, out_dir, md, None)
                .await
                .with_context(|| format!("apply {}", md.markdown_uuid))?;
            self.sweep_problems(&md.markdown_uuid, problems).await
        })
    }

    /// Replace this document's problem rows with `problems`.
    ///
    /// **Scoped to the document actually reprocessed, never to the
    /// run.** The obvious rule — delete every row this run did not
    /// re-emit — is wrong in a way that would quietly empty the table:
    /// render is incremental, so a steady-state run touches almost
    /// nothing, and "not re-emitted" overwhelmingly means *not looked
    /// at*, not *fixed*. A skipped document keeps its rows, which is
    /// correct: its last known state is still current.
    async fn sweep_problems(
        &self,
        markdown_uuid: &str,
        problems: &[RenderProblemRow],
    ) -> Result<()> {
        let mut guard = self.write_lock.acquire().await?;
        let conn = guard.conn();
        sqlx::query("DELETE FROM render_problems WHERE scope_kind = 'markdown' AND scope_key = ?")
            .bind(markdown_uuid)
            .execute(&mut **conn)
            .await
            .context("clear prior problems for this document")?;
        for p in problems {
            // Same generated write path the rows use; see
            // `PortableTable`'s `BulkUpsertable` impl.
            let sql = crate::bulk::insert_sql::<RenderProblemRow>();
            // Audited: `sql` is built from `RenderProblemRow`'s
            // associated consts, never from row data; all values bound.
            p.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
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
            sqlx::query(
                "DELETE FROM render_problems WHERE scope_kind = 'entity' AND scope_key = ?",
            )
            .bind(entity_id)
            .execute(&mut **conn)
            .await
            .context("clear prior problems for this entity")?;
            for p in problems {
                let sql = crate::bulk::insert_sql::<RenderProblemRow>();
                // Audited: see `sweep_problems`.
                p.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
                    .execute(&mut **conn)
                    .await
                    .with_context(|| format!("insert render_problem {}", p.uuid))?;
            }
            Ok(())
        })
    }

    /// How many problems the store currently holds, by outcome.
    ///
    /// R4 needs a numerator; this is it. The denominator — records read
    /// — has to come from the renderer, which is why R4 is blocked on
    /// the sink existing rather than the other way round.
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
    ///
    /// Per-document commits would put thousands of entries in
    /// `dolt_log` per run and drown the audit trail. Committing once at
    /// the end also buys the property that makes doltlite worth it here:
    /// `dolt_diff` over this store answers *"what did this render change
    /// about my data quality?"* — which problems appeared, which
    /// disappeared — and `dolt_log` gives the per-run history.
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
    use datalib_schema::render_problems::{Outcome, Problem, Reason};

    fn store(dir: &Path) -> IndexedMarkdownStore {
        IndexedMarkdownStore::open(dir).expect("open store")
    }

    fn row(uuid: &str, markdown_uuid: &str) -> GridRow {
        GridRow::builder()
            .uuid(uuid)
            .provider("test")
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
        RenderedMarkdown {
            markdown_uuid: markdown_uuid.to_string(),
            source_name: "src".into(),
            source_fingerprint: fingerprint.into(),
            upstream_cursor: None,
            md_path: dir.join(format!("{markdown_uuid}.md")),
            render_version: 7,
            rows: vec![row(markdown_uuid, markdown_uuid)],
            edges: Vec::new(),
        }
    }

    fn problem(uuid: &str, scope: &str) -> RenderProblemRow {
        RenderProblemRow {
            uuid: uuid.into(),
            scope_key: scope.into(),
            scope_kind: "markdown".into(),
            source_name: "src".into(),
            stage: "grid_row".into(),
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

        s.put_document(root, &doc(root, "md-1", "fp-1"), &[])
            .unwrap();

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
        s.put_document(root, &doc(root, "md-1", "fp-1"), &[])
            .unwrap();
        s.put_document(root, &doc(root, "md-1", "fp-2"), &[])
            .unwrap();

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
            &doc(root, "md-1", "fp-1"),
            &[problem("row-a", "md-1")],
        )
        .unwrap();
        s.put_document(
            root,
            &doc(root, "md-2", "fp-1"),
            &[problem("row-b", "md-2")],
        )
        .unwrap();
        assert_eq!(s.problem_counts().unwrap().get("nulled").copied(), Some(2));

        // A second run that only reprocesses md-2, and finds it clean.
        s.put_document(root, &doc(root, "md-2", "fp-2"), &[])
            .unwrap();

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
            &doc(root, "md-1", "fp-1"),
            &[problem("row-a", "md-1")],
        )
        .unwrap();
        assert_eq!(s.problem_counts().unwrap().get("nulled").copied(), Some(1));

        s.put_document(root, &doc(root, "md-1", "fp-2"), &[])
            .unwrap();
        assert!(
            s.problem_counts().unwrap().is_empty(),
            "reprocessed clean ⇒ no problem rows left"
        );
    }
}
