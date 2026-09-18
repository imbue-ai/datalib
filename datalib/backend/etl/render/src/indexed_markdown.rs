//! The per-source render output store: one doltlite database holding
//! every row a source's render produced, plus what render could not do.
//!
//! The index reads it by asking `dolt_diff` what moved since the commit it
//! last consumed, which is also how a document a source stopped holding gets
//! named and deleted.
//!
//! Every write goes through a SQL transaction that leaves the store in a
//! state a consumer may read — a document whole with its rows, edges and
//! problems, never a document with its rows deleted and not yet re-inserted.
//! That is what lets a doltlite commit land at any moment between them
//! (checkpoint, Ctrl-C, rescue, end of run) without anyone checking what is
//! in it. How many documents share one transaction is a throughput choice
//! ([`IndexedMarkdownStore::begin_batch`]); doltlite charges ~50ms per
//! statement outside one. See `docs/dev/plans/one_mode.md`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_schema::edges::DDL as EDGES_DDL;
use datalib_schema::grid_rows::DDL as GRID_ROWS_DDL;
use datalib_schema::markdowns::DDL as MARKDOWNS_DDL;
use datalib_schema::measurements::{SourceMeasurementRow, DDL as MEASUREMENTS_DDL};
use datalib_schema::problems::{ProblemRow, ScopeKind, Severity, DDL as PROBLEMS_DDL};
use datalib_schema::render_cursor::{RenderCursorRow, DDL as RENDER_CURSOR_DDL};
use datalib_schema::render_inputs::{DDL as RENDER_INPUTS_DDL, INDEX_DDL as RENDER_INPUTS_INDEX};

use crate::grid_index::{RenderedMarkdown, WriteLock};
use datalib_etl::bulk::BulkUpsertable;

/// File name inside a source's `render_markdown/`.
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
        .chain(PROBLEMS_DDL.iter())
        .chain(MEASUREMENTS_DDL.iter())
        .chain(RENDER_CURSOR_DDL.iter())
        .chain(RENDER_INPUTS_DDL.iter())
        .map(|(_table, ddl)| *ddl)
        .chain(std::iter::once(RENDER_INPUTS_INDEX))
        .collect()
}

/// One raw row a bucket's render asked for, found or not.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Input {
    /// A table of the raw store, bare name.
    pub table: String,
    /// Its primary key as text; a composite key's columns in
    /// `pragma_table_info` order, joined by `|` — see [`join_key`].
    pub id: String,
}

/// The `input_id` that stands for every row of a table: a bucket that
/// reads a table whole declares this rather than each key.
pub const WHOLE_TABLE: &str = "*";

impl Input {
    pub fn new(table: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            table: table.into(),
            id: id.into(),
        }
    }

    pub fn whole_table(table: impl Into<String>) -> Self {
        Self::new(table, WHOLE_TABLE)
    }
}

/// How a composite primary key is rendered as one `input_id`, on both
/// sides: the provider declaring it and the driver reading the diff.
pub fn join_key(parts: &[String]) -> String {
    parts.join("|")
}

/// A source's render output store, open for writing.
///
/// The pool is one connection wide and a transaction holds it, so every
/// read the driver may issue inside one goes through the write lock —
/// which hands back the held connection — rather than the pool, which
/// would wait on itself.
pub struct IndexedMarkdownStore {
    pool: SqlitePool,
    write_lock: WriteLock,
    path: PathBuf,
    /// The run-pinned "now" stamped onto problem rows — see
    /// [`Self::with_now`].
    now: String,
    /// The commit a reader was opened at; `None` for the owner's handle.
    pin: Option<datalib_etl::pin::Pin>,
}

/// Run a future to completion from a synchronous caller.
///
/// Without a runtime to join, the future runs on one process-wide
/// runtime rather than a fresh one per call. A per-call runtime is
/// dropped as soon as the future completes, and sqlx returns a
/// checked-out connection to its pool from a task *spawned* at drop
/// (`PoolConnection::drop`): kill the runtime first and that task never
/// runs, the pool forgets the connection, and the next `acquire` opens
/// a second connection to the same doltlite file while the first is
/// still closing on its worker thread — two live handles on one store,
/// surfacing as `database is locked` under load.
pub fn blocking<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => fallback_runtime().block_on(fut),
    }
}

fn fallback_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build a runtime for blocking store calls")
    })
}

impl IndexedMarkdownStore {
    pub fn open(rendered_root: &Path) -> Result<Self> {
        std::fs::create_dir_all(rendered_root)
            .with_context(|| format!("mkdir -p {}", rendered_root.display()))?;
        let path = path_for(rendered_root);
        let pool = blocking(datalib_etl::doltlite_raw::open_derived(&path, &store_ddl()))
            .with_context(|| format!("open indexed markdown store {}", path.display()))?;
        Ok(Self {
            write_lock: WriteLock::new(pool.clone()),
            pool,
            path,
            now: datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs(),
            pin: None,
        })
    }

    /// Open somebody else's render store to read it.
    ///
    /// The index is not this store's owner — the render step is — so this goes
    /// through [`datalib_etl::doltlite_raw::open_reader`] and performs none of the
    /// writes [`Self::open`] does on the way in. Read that function's note for
    /// what those are and why they are a hazard here specifically.
    ///
    /// Pinned at open — at `commit`, or HEAD — so every read through this
    /// handle names one commit: `changed_since`'s diff and
    /// `documents_matching`'s rows must agree. `None` means the store has
    /// no commit to read; the caller contributes nothing rather than
    /// reading the working set.
    ///
    /// No `now`, no write lock: nothing reached through this handle may write.
    pub fn open_for_reading(rendered_root: &Path, commit: Option<&str>) -> Result<Option<Self>> {
        let path = path_for(rendered_root);
        let Some(reader) = blocking(datalib_etl::doltlite_raw::open_reader(&path, commit))
            .with_context(|| format!("open render store for reading {}", path.display()))?
        else {
            return Ok(None);
        };
        let pool = reader.pool().clone();
        Ok(Some(Self {
            write_lock: WriteLock::new(pool.clone()),
            pool,
            path,
            now: String::new(),
            pin: Some(reader.pin().clone()),
        }))
    }

    /// The commit this reader reads at. `None` on the owner's handle.
    pub fn pin(&self) -> Option<&datalib_etl::pin::Pin> {
        self.pin.as_ref()
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

    /// The renderer versions the provider's own documents carry.
    ///
    /// The storage report is excluded: datalib renders it, not any of
    /// the source's processors, so measuring its version against what
    /// they declare would fail every source — and counting it as
    /// "declared" would blunt the check for the documents it exists to
    /// guard.
    pub fn render_versions(&self) -> Result<BTreeSet<u32>> {
        blocking(async {
            let rows = sqlx::query(
                "SELECT DISTINCT renderer_version FROM markdowns \
                 WHERE renderer_version IS NOT NULL AND kind <> ?",
            )
            .bind(datalib_schema::measurements::DOC_KIND)
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

    /// Open a batch: one SQL transaction that every write until
    /// [`Self::commit_batch`] joins. The driver holds one open across the
    /// documents between two checkpoints, so a render of 200k documents
    /// is a few hundred transactions rather than 200k.
    pub fn begin_batch(&self) -> Result<()> {
        blocking(self.write_lock.begin_transaction())
    }

    pub fn commit_batch(&self) -> Result<()> {
        blocking(self.write_lock.commit_transaction())
    }

    pub fn rollback_batch(&self) -> Result<()> {
        blocking(self.write_lock.rollback_transaction())
    }

    /// Run `f` as one SQL transaction, or as part of the batch already open.
    ///
    /// A unit of work — a document with its rows, the end-of-run sweep with
    /// the cursor — is replaced whole or not at all, and never straddles a
    /// batch boundary, because the boundary is only ever placed between two
    /// calls to this.
    pub fn transaction<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        if blocking(self.write_lock.in_transaction()) {
            return f();
        }
        blocking(self.write_lock.begin_transaction())?;
        match f() {
            Ok(v) => {
                blocking(self.write_lock.commit_transaction())?;
                Ok(v)
            }
            Err(e) => {
                let _ = blocking(self.write_lock.rollback_transaction());
                Err(e)
            }
        }
    }

    /// Store `md`, replacing what the store held for it. Always: a
    /// document whose rows and file come out unchanged writes identical
    /// rows, and doltlite's content-addressed tables then carry no diff
    /// for it — that, and nothing here, is how "unchanged" is decided.
    /// Write one document: its rows here and, when the document moved —
    /// a re-keyed path prefix, a chat re-periodized under another
    /// directory — the `.md` it used to be at goes, for the same reason
    /// [`Self::remove_document`] unlinks: a file nothing names is still
    /// served and still indexed. A document with no rows is removed the
    /// same way, the `.md` just written included; its problems stay.
    pub fn put_document(&self, out_dir: &Path, md: &RenderedMarkdown) -> Result<()> {
        let previous = self.transaction(|| {
            blocking(async {
                let previous: Option<String> = {
                    let mut guard = self.write_lock.acquire().await?;
                    sqlx::query_scalar("SELECT md_path FROM markdowns WHERE markdown_uuid = ?")
                        .bind(&md.markdown_uuid)
                        .fetch_optional(&mut **guard.conn())
                        .await
                        .with_context(|| format!("read md_path for {}", md.markdown_uuid))?
                        .flatten()
                };
                crate::grid_index::apply_one(&self.write_lock, out_dir, md)
                    .await
                    .with_context(|| format!("apply {}", md.markdown_uuid))?;
                self.sweep_problems(&md.markdown_uuid, &md.problems).await?;
                Ok(previous)
            })
        })?;
        let now = md
            .md_path
            .strip_prefix(out_dir)
            .unwrap_or(&md.md_path)
            .to_string_lossy();
        if md.rows.is_empty() {
            unlink_rendered(out_dir, &now);
        }
        if let Some(previous) = previous.filter(|p| *p != now) {
            unlink_rendered(out_dir, &previous);
        }
        Ok(())
    }

    /// Drop one document: its rows here, and the `.md` file itself.
    ///
    /// The file matters as much as the rows. `md_path` is what
    /// `/applet/unified_index/chat/{uuid}` serves and what qmd indexed, so a
    /// document deleted from the store but left on disk stays searchable and
    /// still resolves — a deletion the user can still read. The unlink
    /// follows the rows; inside a batch that later rolls back it leaves
    /// rows for a file that is gone, which the next run's removal of the
    /// same document repairs (an absent file is the state wanted).
    pub fn remove_document(&self, out_dir: &Path, markdown_uuid: &str) -> Result<()> {
        let md_path = self.transaction(|| {
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
                crate::grid_index::delete_document_rows(conn, markdown_uuid)
                    .await
                    .with_context(|| format!("remove {markdown_uuid} from the store"))?;
                sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?")
                    .bind(ScopeKind::Markdown.as_str())
                    .bind(markdown_uuid)
                    .execute(&mut **conn)
                    .await
                    .with_context(|| format!("remove {markdown_uuid} from the store"))?;
                Ok(md_path)
            })
        })?;
        if let Some(rel) = md_path {
            unlink_rendered(out_dir, &rel);
        }
        Ok(())
    }

    /// Record what `bucket_key` was rendered from, replacing what it
    /// declared last time. Joins the open batch, so a bucket's documents
    /// and its inputs reach the store together.
    pub fn put_inputs(&self, bucket_key: &str, inputs: &[Input]) -> Result<()> {
        self.transaction(|| {
            blocking(async {
                let mut guard = self.write_lock.acquire().await?;
                let conn = guard.conn();
                sqlx::query("DELETE FROM render_inputs WHERE bucket_key = ?")
                    .bind(bucket_key)
                    .execute(&mut **conn)
                    .await
                    .context("clear prior render_inputs")?;
                for chunk in inputs.chunks(datalib_etl::bulk::SQL_CHUNK / 3) {
                    let mut sql = String::from(
                        "INSERT OR IGNORE INTO render_inputs (bucket_key, input_table, input_id) VALUES ",
                    );
                    for (i, _) in chunk.iter().enumerate() {
                        if i > 0 {
                            sql.push_str(", ");
                        }
                        sql.push_str("(?, ?, ?)");
                    }
                    // Audited: a placeholder run sized from the chunk; every
                    // value is bound.
                    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                    for input in chunk {
                        q = q.bind(bucket_key).bind(&input.table).bind(&input.id);
                    }
                    q.execute(&mut **conn)
                        .await
                        .with_context(|| format!("write render_inputs for {bucket_key}"))?;
                }
                Ok(())
            })
        })
    }

    /// Every raw table any bucket declared an input from.
    pub fn input_tables(&self) -> Result<Vec<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            sqlx::query_scalar(
                "SELECT DISTINCT input_table FROM render_inputs ORDER BY input_table",
            )
            .fetch_all(&mut **guard.conn())
            .await
            .context("read render_inputs tables")
        })
    }

    /// The buckets that declared any of `changed` as an input — the
    /// reverse lookup. `changed` is `(table, id)` as the diff named them;
    /// a bucket that declared a whole table matches any row of it.
    pub fn buckets_reading(&self, changed: &[Input]) -> Result<HashSet<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let mut out = HashSet::new();
            let tables: BTreeSet<&str> = changed.iter().map(|i| i.table.as_str()).collect();
            for table in tables {
                out.extend(
                    sqlx::query_scalar::<_, String>(
                        "SELECT DISTINCT bucket_key FROM render_inputs WHERE input_table = ? AND input_id = ?",
                    )
                    .bind(table)
                    .bind(WHOLE_TABLE)
                    .fetch_all(&mut **guard.conn())
                    .await
                    .context("reverse lookup of whole-table inputs")?,
                );
            }
            for chunk in changed.chunks(datalib_etl::bulk::SQL_CHUNK / 2) {
                let mut sql = String::from(
                    "SELECT DISTINCT bucket_key FROM render_inputs WHERE (input_table, input_id) IN (",
                );
                for (i, _) in chunk.iter().enumerate() {
                    if i > 0 {
                        sql.push_str(", ");
                    }
                    sql.push_str("(?, ?)");
                }
                sql.push(')');
                // Audited: a placeholder run sized from the chunk; every
                // value is bound.
                let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
                for input in chunk {
                    q = q.bind(&input.table).bind(&input.id);
                }
                out.extend(
                    q.fetch_all(&mut **guard.conn())
                        .await
                        .context("reverse lookup in render_inputs")?,
                );
            }
            Ok(out)
        })
    }

    /// Every document rendered under any of `bucket_keys`, as
    /// `(bucket, document)`. One chunked query rather than one per
    /// bucket — a run declares as many buckets as it rendered.
    pub fn documents_for_buckets(&self, bucket_keys: &[&str]) -> Result<Vec<(String, String)>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let mut out = Vec::new();
            for chunk in bucket_keys.chunks(datalib_etl::bulk::SQL_CHUNK) {
                let mut sql = String::from(
                    "SELECT bucket_key, markdown_uuid FROM markdowns WHERE bucket_key IN (",
                );
                datalib_etl::bulk::push_placeholder_list(&mut sql, chunk.len());
                sql.push(')');
                // Audited: a placeholder run sized from the chunk; every
                // key is bound.
                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                for key in chunk {
                    q = q.bind(*key);
                }
                let rows = q
                    .fetch_all(&mut **guard.conn())
                    .await
                    .context("documents for buckets")?;
                for r in rows {
                    out.push((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?));
                }
            }
            Ok(out)
        })
    }

    /// The newest `items` sample per subject in `source_measurements`:
    /// what the storage report compares its counts against to decide
    /// whether anything moved.
    pub fn latest_items(&self) -> Result<HashMap<String, Option<i64>>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let rows = sqlx::query(
                "SELECT subject, items FROM source_measurements m \
                  WHERE measured_at_utc = (SELECT MAX(measured_at_utc) FROM source_measurements \
                                            WHERE subject = m.subject)",
            )
            .fetch_all(&mut **guard.conn())
            .await
            .context("read latest measurements")?;
            let mut out = HashMap::with_capacity(rows.len());
            for r in rows {
                out.insert(r.try_get::<String, _>(0)?, r.try_get::<Option<i64>, _>(1)?);
            }
            Ok(out)
        })
    }

    /// The renderer version a stored document carries, if it is there.
    pub fn document_version(&self, markdown_uuid: &str) -> Result<Option<u32>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let v: Option<Option<String>> = sqlx::query_scalar(
                "SELECT renderer_version FROM markdowns WHERE markdown_uuid = ?",
            )
            .bind(markdown_uuid)
            .fetch_optional(&mut **guard.conn())
            .await
            .context("read a document's version")?;
            Ok(v.flatten()
                .and_then(|v| v.rsplit('.').next().and_then(|s| s.parse::<u32>().ok())))
        })
    }

    /// Where the last render left off, if it recorded it.
    pub fn cursor(&self) -> Result<Option<RenderCursorRow>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            sqlx::query_as::<_, RenderCursorRow>("SELECT * FROM render_cursor LIMIT 1")
                .fetch_optional(&mut **guard.conn())
                .await
                .context("read the render cursor")
        })
    }

    /// Record where this render left off. Call it inside the transaction
    /// that holds the run's last work, so the cursor and the documents it
    /// describes reach the store together.
    pub fn write_cursor(&self, row: &RenderCursorRow) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            sqlx::query("DELETE FROM render_cursor")
                .execute(&mut **conn)
                .await
                .context("clear the prior render cursor")?;
            let sql = datalib_etl::bulk::insert_sql::<RenderCursorRow>();
            // Audited: `sql` is built from `RenderCursorRow`'s associated
            // consts; every value is bound.
            row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
                .execute(&mut **conn)
                .await
                .context("write the render cursor")?;
            Ok(())
        })
    }

    /// Every document this store holds. The other half of a sweep: a
    /// renderer that walked its whole raw store says what should be here,
    /// and whatever else is here is what the store lost.
    pub fn all_document_uuids(&self) -> Result<Vec<String>> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let rows = sqlx::query("SELECT markdown_uuid FROM markdowns")
                .fetch_all(&mut **guard.conn())
                .await
                .context("list every document in the store")?;
            rows.into_iter()
                .map(|r| r.try_get::<String, _>(0).map_err(Into::into))
                .collect()
        })
    }
}

/// Delete a rendered document's file, and the per-document directory it sat
/// in once that is empty (`<source>/render_markdown/<uuid>/all.md` is the usual
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
    async fn sweep_problems(&self, markdown_uuid: &str, problems: &[ProblemRow]) -> Result<()> {
        let mut guard = self.write_lock.acquire().await?;
        let conn = guard.conn();
        // Read the prior `first_seen_at_utc` for every uuid about to be
        // rewritten, *before* the delete. This is the whole reason the
        // store stamps these rather than the renderer: a renderer that
        // set both timestamps to "now" every run would make
        // `first_seen_at_utc` a synonym for `last_seen_at_utc`, and "this has
        // been broken since Tuesday" would be unanswerable.
        let seen: HashMap<String, String> = sqlx::query(
            "SELECT problem_uuid, first_seen_at_utc FROM problems \
             WHERE scope_kind = ? AND scope_key = ?",
        )
        .bind(ScopeKind::Markdown.as_str())
        .bind(markdown_uuid)
        .fetch_all(&mut **conn)
        .await
        .context("read prior first_seen_at_utc")?
        .into_iter()
        .map(|r| Ok((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?)))
        .collect::<Result<_>>()?;
        sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?")
            .bind(ScopeKind::Markdown.as_str())
            .bind(markdown_uuid)
            .execute(&mut **conn)
            .await
            .context("clear prior problems for this document")?;
        self.insert_problems(conn, problems, &seen).await
    }

    /// Insert problem rows, stamping `first_seen_at_utc` / `last_seen_at_utc`.
    /// `seen` maps a uuid to the `first_seen_at_utc` it already had, which
    /// is carried forward; anything absent is new and gets `now` for
    /// both.
    async fn insert_problems(
        &self,
        conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
        problems: &[ProblemRow],
        seen: &HashMap<String, String>,
    ) -> Result<()> {
        let now = datalib_time::split_stamp(&self.now);
        for p in problems {
            let stamped = ProblemRow {
                first_seen_at_utc: seen
                    .get(&p.problem_uuid)
                    .cloned()
                    .unwrap_or_else(|| now.utc.clone()),
                last_seen_at_utc: now.utc.clone(),
                tz_offset: now.tz_offset.clone(),
                ..p.clone()
            };
            // Same generated write path the rows use; see
            // `PortableTable`'s `BulkUpsertable` impl.
            let sql = datalib_etl::bulk::insert_sql::<ProblemRow>();
            // Audited: `sql` is built from `ProblemRow`'s
            // associated consts, never from row data; all values bound.
            stamped
                .bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
                .execute(&mut **conn)
                .await
                .with_context(|| format!("insert problem {}", p.problem_uuid))?;
        }
        Ok(())
    }

    /// Append one run's measurements to the source's series.
    ///
    /// Append, not upsert: this table is the history behind the
    /// sparkline, and the current value already lives in `grid_rows`.
    ///
    /// `INSERT OR REPLACE`, because the key is `(subject, measured_at_utc)`
    /// and a run stamps one pinned instant across every row it writes —
    /// so a re-run under the same `--now` should restate the series
    /// rather than fail the whole render on a duplicate carrying the
    /// same numbers.
    ///
    /// Hand-written rather than through `bulk::insert_sql`: the
    /// `PortableTable` derive emits no write path for a composite
    /// primary key, since `BulkUpsertable` assumes one `id` column.
    pub fn put_measurements(&self, samples: &[SourceMeasurementRow]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            for sample in samples {
                sqlx::query(
                    "INSERT OR REPLACE INTO source_measurements \
                     (subject, kind, measured_at_utc, tz_offset, bytes, items) \
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(&sample.subject)
                .bind(&sample.kind)
                .bind(&sample.measured_at_utc)
                .bind(&sample.tz_offset)
                .bind(sample.bytes)
                .bind(sample.items)
                .execute(&mut **conn)
                .await
                .with_context(|| {
                    format!(
                        "insert measurement {} at {}",
                        sample.subject, sample.measured_at_utc
                    )
                })?;
            }
            Ok(())
        })
    }

    /// Problems not attached to any document — a payload that would not
    /// deserialize has no `markdown_uuid` to hang off. Swept by the
    /// raw-store entity id instead, so they clear when that entity is
    /// next parsed successfully.
    pub fn put_entity_problems(&self, entity_id: &str, problems: &[ProblemRow]) -> Result<()> {
        blocking(async {
            let mut guard = self.write_lock.acquire().await?;
            let conn = guard.conn();
            let seen: HashMap<String, String> = sqlx::query(
                "SELECT problem_uuid, first_seen_at_utc FROM problems \
                 WHERE scope_kind = ? AND scope_key = ?",
            )
            .bind(ScopeKind::Entity.as_str())
            .bind(entity_id)
            .fetch_all(&mut **conn)
            .await
            .context("read prior first_seen_at_utc")?
            .into_iter()
            .map(|r| Ok((r.try_get::<String, _>(0)?, r.try_get::<String, _>(1)?)))
            .collect::<Result<_>>()?;
            sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?")
                .bind(ScopeKind::Entity.as_str())
                .bind(entity_id)
                .execute(&mut **conn)
                .await
                .context("clear prior problems for this entity")?;
            self.insert_problems(conn, problems, &seen).await
        })
    }

    /// Every commit in this store as `(hash, message)`, newest first.
    /// Empty without doltlite.
    pub fn log(&self) -> Result<Vec<(String, String)>> {
        blocking(async {
            if !datalib_etl::doltlite_raw::has_dolt_extensions(&self.pool).await {
                return Ok(Vec::new());
            }
            let rows = sqlx::query("SELECT commit_hash, message FROM dolt_log()")
                .fetch_all(&self.pool)
                .await
                .context("read dolt_log")?;
            rows.into_iter()
                .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
                .collect()
        })
    }

    pub fn documents(
        &self,
        out_dir: &Path,
        pin: &datalib_etl::pin::Pin,
    ) -> Result<Vec<RenderedMarkdown>> {
        self.documents_matching(out_dir, None, pin)
    }

    pub fn changed_since(
        &self,
        cursor: Option<&str>,
        pin: &datalib_etl::pin::Pin,
    ) -> Result<datalib_etl::doltlite_raw::DiffScan> {
        blocking(datalib_etl::doltlite_raw::scan_buckets(
            &self.pool,
            cursor,
            pin,
            &datalib_etl::doltlite_raw::DiffScanSpec {
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
        pin: &datalib_etl::pin::Pin,
    ) -> Result<Vec<RenderedMarkdown>> {
        let _ = pin; // the views were installed at `open_for_reading`
        blocking(async {
            let mds: Vec<datalib_schema::markdowns::MarkdownRow> =
                sqlx::query_as("SELECT * FROM pinned_markdowns markdowns ORDER BY markdown_uuid")
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
                    "SELECT * FROM pinned_grid_rows grid_rows WHERE markdown_uuid = ? ORDER BY uuid",
                )
                .bind(&md.markdown_uuid)
                .fetch_all(&self.pool)
                .await
                .with_context(|| format!("read rows for {}", md.markdown_uuid))?;
                let edges: Vec<datalib_schema::edges::EdgeRow> = sqlx::query_as(
                    "SELECT * FROM pinned_edges edges WHERE src_markdown_uuid = ? ORDER BY edge_uuid",
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
                    source_id: md.source_id.clone(),
                    upstream_cursor: md.upstream_cursor.clone(),
                    bucket_key: md.bucket_key.clone(),
                    md_path: match md.md_path.as_deref() {
                        Some(rel) => out_dir.join(rel),
                        None => PathBuf::from(&md.markdown_uuid),
                    },
                    render_version,
                    rows,
                    sections: Vec::new(),
                    edges,
                    problems: Vec::new(),
                });
            }
            Ok(out)
        })
    }

    /// Whole-store counts by severity: what the step reports at its
    /// end. A severity this build cannot name is an error — the store
    /// was written by a newer build and a silent zero would read as
    /// clean.
    pub fn problem_counts(&self) -> Result<HashMap<Severity, i64>> {
        blocking(async {
            let rows = sqlx::query("SELECT severity, COUNT(*) FROM problems GROUP BY severity")
                .fetch_all(&self.pool)
                .await
                .context("count problems")?;
            let mut out = HashMap::new();
            for r in rows {
                let word: String = r.try_get(0)?;
                let severity = Severity::parse(&word)
                    .with_context(|| format!("problems.severity: unknown spelling {word:?}"))?;
                out.insert(severity, r.try_get::<i64, _>(1)?);
            }
            Ok(out)
        })
    }

    /// One `dolt_commit` for the whole render, not one per document.
    pub fn commit(&self, summary: &str) -> Result<Option<String>> {
        blocking(datalib_etl::doltlite_raw::commit_run(&self.pool, summary))
    }

    /// The store's HEAD: its content version. `None` without doltlite.
    pub fn head(&self) -> Result<Option<String>> {
        blocking(datalib_etl::doltlite_raw::head_commit(&self.pool))
    }

    pub fn close(self) {
        blocking(self.pool.close());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::problems::{Outcome, Problem, Reason, Scope, Stage};
    use datalib_schema::providers::Provider;

    fn store(dir: &Path) -> IndexedMarkdownStore {
        IndexedMarkdownStore::open(dir).expect("open store")
    }

    /// A store call from a thread with no runtime must hand its
    /// connection back to the pool. With a runtime built and dropped per
    /// call, sqlx's return-to-pool task died with the runtime, the pool's
    /// size fell to 0, and the next call opened a second connection to
    /// the same file while the first was still closing — `database is
    /// locked`, about once in thirty runs under a parallel test load.
    #[test]
    fn a_store_call_without_a_runtime_returns_its_connection_to_the_pool() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        assert!(tokio::runtime::Handle::try_current().is_err());
        for i in 0..3 {
            st.put_document(td.path(), &doc(td.path(), &format!("d{i}"), "fp"))
                .unwrap();
            // `size` is the pool's own count of live connections; the
            // dying runtime dropped the guard that decrements it. Whether
            // it is idle yet is a race with the return task, so not asserted.
            assert_eq!(
                st.pool.size(),
                1,
                "call {i}: the one connection is still pooled"
            );
        }
        st.close();
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
            .created_at(Some("2026-01-01T00:00:00+00:00".to_string()))
            .is_document(true)
            .build()
            .expect("row")
    }

    fn doc(dir: &Path, markdown_uuid: &str, label: &str) -> RenderedMarkdown {
        doc_with(dir, markdown_uuid, label, Vec::new())
    }

    fn doc_with(
        dir: &Path,
        markdown_uuid: &str,
        _label: &str,
        problems: Vec<ProblemRow>,
    ) -> RenderedMarkdown {
        RenderedMarkdown {
            markdown_uuid: markdown_uuid.to_string(),
            source_id: "src".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path: dir.join(format!("{markdown_uuid}.md")),
            render_version: 7,
            rows: vec![row(markdown_uuid, markdown_uuid)],
            sections: Vec::new(),
            edges: Vec::new(),
            problems,
        }
    }

    /// A renderer that forgets to mark its document row, or marks two,
    /// fails its render outright: the old fallback ("the row whose uuid
    /// matches, else the first") is exactly the guess `is_document`
    /// exists to remove.
    #[test]
    fn a_document_must_have_exactly_one_document_row() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());

        let mut none = doc(td.path(), "d-none", "fp");
        none.rows[0].is_document = false;
        let err = st.put_document(td.path(), &none).unwrap_err();
        assert!(
            format!("{err:#}").contains("none of its 1 rows is marked is_document"),
            "{err:#}"
        );

        let mut two = doc(td.path(), "d-two", "fp");
        two.rows.push(row("d-two-extra", "d-two"));
        let err = st.put_document(td.path(), &two).unwrap_err();
        assert!(
            format!("{err:#}").contains("both marked is_document"),
            "{err:#}"
        );

        // Neither half-written document reached the store.
        let n: i64 = blocking(async {
            sqlx::query_scalar("SELECT COUNT(*) FROM markdowns")
                .fetch_one(&st.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 0);
        st.close();
    }

    /// The document row's stamps are what `markdowns` carries — copied,
    /// not recomputed from the inner rows, so a PR's `updated_at` wins
    /// over its last comment.
    #[test]
    fn markdowns_takes_its_stamps_from_the_document_row() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let mut d = doc(td.path(), "d-stamped", "fp");
        d.rows[0].created_at = Some("2026-01-01T00:00:00+00:00".into());
        d.rows[0].modified_at = Some("2026-03-01T00:00:00+00:00".into());
        let mut inner = row("d-stamped-m1", "d-stamped");
        inner.is_document = false;
        inner.created_at = Some("2026-02-01T00:00:00+00:00".into());
        d.rows.push(inner);
        st.put_document(td.path(), &d).unwrap();
        let (created, modified): (Option<String>, Option<String>) = blocking(async {
            sqlx::query_as(
                "SELECT created_at, modified_at FROM markdowns WHERE markdown_uuid = 'd-stamped'",
            )
            .fetch_one(&st.pool)
            .await
        })
        .unwrap();
        assert_eq!(created.as_deref(), Some("2026-01-01T00:00:00+00:00"));
        assert_eq!(modified.as_deref(), Some("2026-03-01T00:00:00+00:00"));
        st.close();
    }

    /// The storage report is rendered by datalib, not by any of the
    /// source's processors, so its version must stay out of
    /// `render_versions` — the set the render step checks against what
    /// those processors declare.
    ///
    /// Getting this wrong is not subtle and not local: every source in
    /// the pipeline failed its render with "carry render_version [1],
    /// which none of its processors declare", and a download-only
    /// source (whose only document *is* the report) failed with
    /// "none of its processors implement render_version".
    #[test]
    fn the_storage_report_is_not_counted_as_a_provider_render_version() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());

        let mut report = doc(td.path(), "storage-doc", "fp-1");
        report.render_version = 1;
        report.rows = vec![GridRow::builder()
            .uuid("storage-doc")
            .provider(datalib_schema::providers::Provider::Datalib)
            .kind("Source Size")
            .source_label("Storage")
            .conversation_uuid("storage-doc")
            .entire_chat("/chat/storage-doc")
            .text("src/raw — 1.0 KiB")
            .markdown_uuid(Some("storage-doc".to_string()))
            .byte_size(Some(1024))
            .is_document(true)
            .build()
            .expect("row")];
        st.put_document(td.path(), &report).expect("store report");

        assert!(
            st.render_versions().expect("versions").is_empty(),
            "a source whose only document is the storage report must \
             report no provider render versions at all"
        );

        // A real provider document alongside it is still counted.
        st.put_document(td.path(), &doc(td.path(), "real-doc", "fp-2"))
            .expect("store provider doc");
        assert_eq!(
            st.render_versions().expect("versions"),
            BTreeSet::from([7]),
            "the provider's version is reported; the report's is not"
        );
    }

    fn sample(
        subject: &str,
        at: &str,
        bytes: Option<i64>,
        items: Option<i64>,
    ) -> SourceMeasurementRow {
        SourceMeasurementRow {
            subject: subject.into(),
            kind: "tree".into(),
            measured_at_utc: at.into(),
            tz_offset: None,
            bytes,
            items,
        }
    }

    /// The whole point of the second table: a later run adds to the
    /// series rather than replacing it. If this ever upserts on
    /// `subject` alone there is no history left to draw, and nothing
    /// downstream would report an error — the newest number would still
    /// be right.
    #[test]
    fn a_second_run_appends_to_the_series_instead_of_replacing_it() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());

        st.put_measurements(&[
            sample("src/raw", "2026-09-01T10:00:00-07:00", Some(100), Some(1)),
            sample(
                "src/raw/a.doltlite_db",
                "2026-09-01T10:00:00-07:00",
                Some(90),
                None,
            ),
        ])
        .expect("first run");
        st.put_measurements(&[sample(
            "src/raw",
            "2026-09-02T10:00:00-07:00",
            Some(250),
            Some(2),
        )])
        .expect("second run");

        let series: Vec<(String, Option<i64>)> = blocking(async {
            sqlx::query(
                "SELECT measured_at_utc, bytes FROM source_measurements \
                 WHERE subject = 'src/raw' ORDER BY measured_at_utc",
            )
            .fetch_all(&st.pool)
            .await
            .expect("read the series")
            .into_iter()
            .map(|r| {
                (
                    r.try_get::<String, _>(0).unwrap(),
                    r.try_get::<Option<i64>, _>(1).unwrap(),
                )
            })
            .collect()
        });
        assert_eq!(
            series,
            vec![
                ("2026-09-01T10:00:00-07:00".to_string(), Some(100)),
                ("2026-09-02T10:00:00-07:00".to_string(), Some(250)),
            ],
            "both runs must survive"
        );
    }

    /// A NULL byte count is a real value — every table row carries one,
    /// because a content-addressed store has no per-table byte layout.
    /// It must round-trip as NULL rather than as 0.
    #[test]
    fn an_absent_byte_count_round_trips_as_null() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.put_measurements(&[sample(
            "src/raw#t",
            "2026-09-01T10:00:00-07:00",
            None,
            Some(5),
        )])
        .expect("write");

        let (bytes, items): (Option<i64>, Option<i64>) = blocking(async {
            let r = sqlx::query("SELECT bytes, items FROM source_measurements")
                .fetch_one(&st.pool)
                .await
                .expect("read back");
            (r.try_get(0).unwrap(), r.try_get(1).unwrap())
        });
        assert_eq!(bytes, None);
        assert_eq!(items, Some(5));
    }

    /// A re-run under the same pinned `--now` restates the series
    /// rather than failing the whole render on a duplicate key.
    #[test]
    fn re_measuring_at_the_same_instant_restates_rather_than_fails() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let at = "2026-09-01T10:00:00-07:00";
        st.put_measurements(&[sample("src/raw", at, Some(100), Some(1))])
            .expect("first");
        st.put_measurements(&[sample("src/raw", at, Some(140), Some(2))])
            .expect("same instant again must not fail");

        let (n, bytes): (i64, Option<i64>) = blocking(async {
            let r = sqlx::query("SELECT COUNT(*), MAX(bytes) FROM source_measurements")
                .fetch_one(&st.pool)
                .await
                .expect("read back");
            (r.try_get(0).unwrap(), r.try_get(1).unwrap())
        });
        assert_eq!(n, 1, "one instant is one sample");
        assert_eq!(bytes, Some(140), "the later write wins");
    }

    fn problem(uuid: &str, scope: &str) -> ProblemRow {
        ProblemRow::new(
            "src",
            Stage::GridRow,
            Scope::Markdown(scope),
            Some(uuid),
            Outcome::Nulled,
            Problem::field("created_at", Reason::CoercionFailed, "not-a-date"),
            Some(7),
        )
    }

    /// The write path has no skip of its own: the same document written
    /// twice is the same row twice, and doltlite reports nothing between
    /// the two commits. The store-level check is `dolt_diff` staying
    /// empty; here, that the second write is accepted and the row is one.
    #[tokio::test(flavor = "multi_thread")]
    async fn writing_the_same_document_twice_is_one_row() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(root, &doc(root, "md-1", "fp-1")).unwrap();
        s.put_document(root, &doc(root, "md-1", "fp-1")).unwrap();
        let n: i64 = blocking(async {
            sqlx::query_scalar("SELECT COUNT(*) FROM markdowns")
                .fetch_one(&s.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(s.render_versions().unwrap(), [7].into_iter().collect());
    }

    /// A document that comes back at another path — beeper's
    /// `<network>/` prefix after a room's network changed — leaves no
    /// file at the old one: the store's row moves, and a file the row no
    /// longer names is what `remove_document` exists to prevent.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_that_moves_leaves_no_file_behind() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        let mut first = doc(root, "md-1", "fp-1");
        first.md_path = root.join("old").join("md-1.md");
        std::fs::create_dir_all(first.md_path.parent().unwrap()).unwrap();
        std::fs::write(&first.md_path, "old").unwrap();
        s.put_document(root, &first).unwrap();

        let mut moved = doc(root, "md-1", "fp-1");
        moved.md_path = root.join("new").join("md-1.md");
        std::fs::create_dir_all(moved.md_path.parent().unwrap()).unwrap();
        std::fs::write(&moved.md_path, "new").unwrap();
        s.put_document(root, &moved).unwrap();

        assert!(!first.md_path.exists(), "the old file is gone");
        assert!(moved.md_path.exists(), "the new one stays");
        // The same path twice is left alone.
        s.put_document(root, &moved).unwrap();
        assert!(moved.md_path.exists());
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
    }

    fn doc_in(dir: &Path, markdown_uuid: &str, row_uuid: &str, fp: &str) -> RenderedMarkdown {
        let mut d = doc(dir, markdown_uuid, fp);
        d.rows = vec![row(row_uuid, markdown_uuid)];
        d
    }

    /// A row that moved between two documents of one source — a message
    /// re-bucketed into another period, a chat whose document id is
    /// minted from a name that changed — is taken over by the document
    /// that now emits it, whichever of the two renders first. Found by
    /// the provider contract harness on google_takeout: the incoming
    /// document failed on `UNIQUE constraint failed: grid_rows.uuid`
    /// while the old owner, rendered by an earlier run, still held it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_that_moved_between_documents_is_taken_over() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let earlier = store(root);
        earlier
            .put_document(root, &doc_in(root, "doc-a", "row-1", "fp-a"))
            .unwrap();
        earlier.close();

        // A second open is a second run, whatever the clock says.
        let later = store(root);
        later
            .put_document(root, &doc_in(root, "doc-b", "row-1", "fp-b"))
            .expect("the row moves to doc-b");
        let owner: String = blocking(async {
            sqlx::query_scalar("SELECT markdown_uuid FROM grid_rows WHERE uuid = 'row-1'")
                .fetch_one(&later.pool)
                .await
        })
        .unwrap();
        assert_eq!(owner, "doc-b");
        // doc-a re-renders without the row: its delete-by-owner must not
        // touch what doc-b now holds.
        later
            .put_document(root, &doc(root, "doc-a", "fp-a2"))
            .unwrap();
        let n: i64 = blocking(async {
            sqlx::query_scalar("SELECT COUNT(*) FROM grid_rows WHERE uuid = 'row-1'")
                .fetch_one(&later.pool)
                .await
        })
        .unwrap();
        assert_eq!(n, 1, "the moved row survives its old owner's re-render");
    }

    /// Two documents of one run minting one row uuid is a finding, not a
    /// move, and still fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_documents_in_one_run_minting_one_uuid_is_an_error() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_document(root, &doc_in(root, "doc-a", "row-1", "fp-a"))
            .unwrap();
        let err = s
            .put_document(root, &doc_in(root, "doc-b", "row-1", "fp-b"))
            .expect_err("a same-run collision must fail");
        assert!(format!("{err:#}").contains("written this run"), "{err:#}");
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
        assert_eq!(
            s.problem_counts().unwrap().get(&Severity::Warning).copied(),
            Some(2)
        );

        // A second run that only reprocesses md-2, and finds it clean.
        s.put_document(root, &doc(root, "md-2", "fp-2")).unwrap();

        let counts = s.problem_counts().unwrap();
        assert_eq!(
            counts.get(&Severity::Warning).copied(),
            Some(1),
            "md-2's problem cleared; md-1's must not have — it was never looked at"
        );
    }

    /// A document that comes back with no rows — every one rejected by
    /// validation — is not a document: its `markdowns` row goes, so a
    /// bucket sweep never finds it under the bucket it used to be in,
    /// and so does the `.md` just written, which nothing would resolve.
    /// The problems saying why stay. Found by the contract harness on a
    /// gitlab merge request re-keyed under another bucket whose rows all
    /// failed `created_at`: `markdowns` kept the old bucket.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_document_re_rendered_with_no_rows_is_gone_bucket_and_all() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        let mut first = doc(root, "md-1", "fp-1");
        first.bucket_key = Some("mr!17".into());
        std::fs::write(&first.md_path, "first").unwrap();
        s.put_document(root, &first).unwrap();
        assert_eq!(
            s.documents_for_buckets(&["mr!17"]).unwrap(),
            vec![("mr!17".to_string(), "md-1".to_string())]
        );

        let mut empty = doc_with(root, "md-1", "fp-2", vec![problem("row-a", "md-1")]);
        empty.bucket_key = Some("mr!17~new".into());
        empty.md_path = root.join("moved").join("md-1.md");
        empty.rows.clear();
        std::fs::create_dir_all(empty.md_path.parent().unwrap()).unwrap();
        std::fs::write(&empty.md_path, "empty").unwrap();
        s.put_document(root, &empty).unwrap();

        assert!(
            s.documents_for_buckets(&["mr!17", "mr!17~new"])
                .unwrap()
                .is_empty(),
            "no markdowns row under either bucket"
        );
        assert!(s.all_document_uuids().unwrap().is_empty());
        assert!(!first.md_path.exists(), "the old file is gone");
        assert!(!empty.md_path.exists(), "and so is the one just written");
        assert_eq!(
            s.problem_counts().unwrap().get(&Severity::Warning).copied(),
            Some(1),
            "the record of why every row went stays"
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
        assert_eq!(
            s.problem_counts().unwrap().get(&Severity::Warning).copied(),
            Some(1)
        );

        s.put_document(root, &doc(root, "md-1", "fp-2")).unwrap();
        assert!(
            s.problem_counts().unwrap().is_empty(),
            "reprocessed clean ⇒ no problem rows left"
        );
    }

    /// A bucket that declared a whole table is stale when any row of it
    /// moves, and one that declared a row is not stale for its neighbours.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_whole_table_input_matches_any_row_of_it() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        let s = store(root);
        s.put_inputs("page", &[Input::whole_table("readings")])
            .unwrap();
        s.put_inputs("one", &[Input::new("readings", "r-1")])
            .unwrap();

        let stale = s.buckets_reading(&[Input::new("readings", "r-2")]).unwrap();
        assert_eq!(stale, ["page".to_string()].into_iter().collect());
        let stale = s.buckets_reading(&[Input::new("readings", "r-1")]).unwrap();
        assert_eq!(
            stale,
            ["page".to_string(), "one".to_string()]
                .into_iter()
                .collect()
        );
        let stale = s.buckets_reading(&[Input::new("devices", "d-1")]).unwrap();
        assert!(stale.is_empty());
    }
}
