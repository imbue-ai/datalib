//! The SQLite→doltlite mirror engine.
//!
//! Every run rebuilds every mirrored table from the source and drops
//! whatever the source no longer has, then commits. That is the whole
//! model:
//!
//! ```text
//! drop EVERY table in the mirror
//! for each SOURCE table:  CREATE TABLE main.t (…);
//!                         INSERT INTO main.t SELECT … FROM src.t;
//! dolt_commit
//! ```
//!
//! The drop is unconditional — every mirror table, not just the ones the
//! source still has. That is what makes a table the source *removed*
//! disappear from HEAD instead of sitting there frozen and
//! indistinguishable from a live one, and it means there is no "is this
//! one stale?" question to get wrong.
//! `a_table_the_source_dropped_is_dropped_from_the_mirror` fails if the
//! drop is narrowed to the source's tables.
//!
//! ## Why rebuilding from scratch is free
//!
//! It looks wasteful and isn't, because doltlite stores a table as a
//! content-addressed prolly tree. A row written back byte-identical to
//! the row already at HEAD produces the same chunk and lands in the same
//! place, and a `CREATE TABLE` identical to the one already at HEAD is
//! likewise not a change. Drop a table, recreate it, refill it with the
//! same 419 rows, and `dolt_status` comes back **clean** — so an ingest
//! of an unchanged catalog produces no commit at all.
//!
//! Everything an incremental backup needs falls out of that:
//!
//! - **Diff quality is unaffected.** An edited row still reads as
//!   `modified` in `dolt_diff_<table>`, not as a removal plus an
//!   addition — dolt matches rows by primary key, and it neither knows
//!   nor cares that the table was dropped in between.
//! - **History is unaffected.** `dolt_history_<table>` keeps every prior
//!   version of a row across the drop, including across a schema change.
//! - **Schema evolution needs no code.** A new table, a new column, a
//!   removed column, a retyped one, a moved key — every one of them is
//!   just "the CREATE TABLE we emit this run differs from last run's",
//!   which is not a case to handle. There is no reconciliation logic in
//!   this crate, and there was: an earlier version compared the mirror's
//!   introspected shape against the source's and chose between ADD
//!   COLUMN and drop-and-recreate. It also had a bug that version cannot
//!   have — ten of a stock catalog's 133 tables recreated on *every* run
//!   because SQLite reports a non-`INTEGER PRIMARY KEY` column as
//!   nullable while dolt stores it NOT NULL, and the two shapes never
//!   compared equal. Nothing compares shapes now.
//!
//! It also means the ingester needs no cursor, no watermark, and no
//! change-tracking of its own: whatever the catalog says today becomes
//! HEAD, and history accumulates behind it.
//!
//! ## The copy runs inside SQLite, not inside Rust
//!
//! doltlite's amalgamation reads ordinary SQLite files as well as
//! `.doltlite_db` ones, so the mirror `ATTACH`es the catalog and moves
//! rows with `INSERT … SELECT`. No value ever crosses into Rust, which
//! is both much faster and — the part that matters — perfectly faithful:
//! SQLite's dynamic typing survives the hop, so a column holding an
//! integer in one row and a blob in the next arrives with both types
//! intact. Marshalling through Rust would force a decision about what
//! such a column "is".
//!
//! ## Scaling caveat
//!
//! Each table is filled inside its own transaction, so peak memory
//! scales with the largest single table rather than the whole catalog.
//! That split is not stylistic: doltlite holds a transaction's writes in
//! memory at roughly 3–4x the data size, so wrapping a whole run in one
//! transaction costs ~510 MB peak RSS for 150 MB of rows — fine here,
//! ~15 GB for a 4–5 GB catalog, which is not. The dolt commit at the end
//! still covers the whole run, so the run is still atomic *as history*:
//! a crash mid-run leaves HEAD untouched and a dirty working tree, which
//! `doltlite_raw::open` seals into its own rescue commit next time.
//!
//! A multi-hundred-GB database would want the copy chunked by
//! primary-key range too; a Lightroom catalog (tens of MB, low hundreds
//! of thousands of rows) is nowhere near that, and a 3.3 MB / 133-table
//! catalog mirrors in ~220 ms.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePool, SqlitePoolOptions};
use sqlx::{Connection, Row};

use datalib_etl::progress::Progress;

use super::plan::{self, ColumnSpec, KeyOrigin, SourceColumn, TableSpec};

/// Schema alias the source catalog is ATTACHed under. Deliberately
/// unlikely to collide with anything a user would name a database.
const SRC_SCHEMA: &str = "datalib_mirror_src";

/// Tables the mirror must never touch: `datalib_etl`'s shared
/// bookkeeping, created by `doltlite_raw::open`. A source table with one
/// of these names is a hard error rather than a silent clobber of the
/// store's own metadata.
const RESERVED_TABLES: &[&str] = &["sync_runs", "sync_scope_state", "sync_scope_config"];

/// Everything the engine needs. Built from `LightroomConfig` by the
/// processor, or from flags by the standalone CLI.
#[derive(Debug, Clone)]
pub struct MirrorOptions {
    /// The SQLite database to mirror (a `.lrcat`, for Lightroom).
    pub source_path: PathBuf,
    /// Take a `VACUUM INTO` snapshot before reading. See [`snapshot`].
    pub snapshot: bool,
    pub include_tables: Vec<String>,
    pub exclude_tables: Vec<String>,
    /// Already-expanded `Table.column` globs (the `skip_xmp` preset is
    /// folded in by the caller).
    pub exclude_columns: Vec<String>,
    pub stable_key_columns: Vec<String>,
    pub primary_keys: BTreeMap<String, Vec<String>>,
    /// Run `dolt_gc()` at the start of the run. See [`run`].
    pub gc: bool,
}

/// What one mirror run did. Feeds the run summary and the tests.
#[derive(Debug, Default, Clone)]
pub struct MirrorStats {
    pub tables: usize,
    pub rows: u64,
    /// Mirror tables the source no longer has, dropped so HEAD keeps
    /// meaning "the catalog as it is now". Not the per-run rebuild —
    /// that drops every table by definition and is not worth counting.
    pub stale_tables_dropped: usize,
    pub columns_dropped: usize,
    /// Tables keyed on a stable UNIQUE column instead of the declared
    /// primary key — the `id_global` rewrite.
    pub tables_restably_keyed: usize,
    pub source_bytes: u64,
}

/// A `VACUUM INTO` snapshot that deletes itself when dropped.
pub struct Snapshot {
    dir: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl Snapshot {
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// Whether this is a real snapshot or a pass-through of the live file.
    pub fn is_copy(&self) -> bool {
        self.dir.is_some()
    }
}

/// Take a consistent point-in-time copy of `source` via `VACUUM INTO`.
///
/// Lightroom keeps its catalog open — and in WAL mode — for as long as
/// it is running, so reading the live file can observe a torn view or
/// fail outright on a lock. `VACUUM INTO` runs inside a read transaction
/// on the source, so what lands is one coherent snapshot; it also drops
/// the freelist, which is why the snapshot is usually a little smaller
/// than the catalog.
///
/// If the read-only open fails — the classic case being a WAL catalog
/// whose `-shm` file we're not allowed to touch — this falls back to a
/// plain file copy of the catalog and its sidecars. That copy can be
/// torn if Lightroom writes mid-copy; SQLite's own WAL recovery repairs
/// the common cases, and the honest advice, logged at `warn`, is to
/// close Lightroom before backing up.
pub async fn snapshot(source: &Path) -> Result<Snapshot> {
    let dir = tempfile::tempdir().context("create snapshot tempdir")?;
    let dest = dir.path().join("snapshot.sqlite");

    match vacuum_into(source, &dest).await {
        Ok(()) => Ok(Snapshot {
            path: dest,
            dir: Some(dir),
        }),
        Err(e) => {
            tracing::warn!(
                source = %source.display(),
                error = %format!("{e:#}"),
                "lightroom: VACUUM INTO snapshot failed; falling back to a file copy. \
                 If the catalog is open in Lightroom the copy may be inconsistent — \
                 close Lightroom for a clean backup."
            );
            let dest = dir.path().join(
                source
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("snapshot.sqlite")),
            );
            std::fs::copy(source, &dest)
                .with_context(|| format!("copy {} for snapshot", source.display()))?;
            // The WAL and shared-memory sidecars carry committed pages
            // that aren't in the main file yet. Copy them alongside so
            // SQLite's recovery can replay them; absent ones are the
            // normal (non-WAL, or checkpointed) case.
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut side = source.as_os_str().to_os_string();
                side.push(suffix);
                let side = PathBuf::from(side);
                if side.exists() {
                    let mut to = dest.as_os_str().to_os_string();
                    to.push(suffix);
                    std::fs::copy(&side, PathBuf::from(to))
                        .with_context(|| format!("copy sidecar {}", side.display()))?;
                }
            }
            Ok(Snapshot {
                path: dest,
                dir: Some(dir),
            })
        }
    }
}

async fn vacuum_into(source: &Path, dest: &Path) -> Result<()> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", source.display()))
        .with_context(|| format!("sqlite uri for {}", source.display()))?
        .read_only(true)
        .create_if_missing(false);
    let mut conn = SqliteConnection::connect_with(&opts)
        .await
        .with_context(|| format!("open {} read-only", source.display()))?;
    // `VACUUM INTO` takes an expression, but binding the destination is
    // not portable across the versions we care about; the path is ours
    // (a tempdir), so escape it for a SQL string literal instead.
    let literal = dest.display().to_string().replace('\'', "''");
    // Audited: `literal` is our own tempdir destination path with `'` doubled
    // for a SQL string literal, as the comment above explains.
    let r = sqlx::query(sqlx::AssertSqlSafe(format!("VACUUM INTO '{literal}'")))
        .execute(&mut conn)
        .await
        .with_context(|| format!("VACUUM INTO {}", dest.display()));
    let _ = conn.close().await;
    r.map(|_| ())
}

/// Open a mirror store. Thin wrapper over `doltlite_raw::open` with no
/// provider DDL: the mirror's tables are discovered from the source at
/// run time, so they're applied by [`run`], not at open.
pub async fn open_mirror(db_path: &Path) -> Result<SqlitePool> {
    datalib_etl::doltlite_raw::open(db_path, &[]).await
}

/// Open a plain (non-doltlite) SQLite pool. Used by the tests and by
/// anything that wants to poke the source catalog directly.
pub async fn open_sqlite(path: &Path, create: bool) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .with_context(|| format!("sqlite uri for {}", path.display()))?
        .create_if_missing(create);
    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(300))
        .connect_with(opts)
        .await
        .with_context(|| format!("open sqlite pool at {}", path.display()))
}

/// Mirror `opts.source_path` into the doltlite store behind `pool`.
///
/// Does **not** commit — the caller owns that, because under the
/// orchestrator the commit is `RawStoreSession::finish`'s job (so a
/// Ctrl-C mid-run commits the same way a clean finish does).
///
/// With `opts.gc`, collects unreachable chunks *before* the copy rather
/// than after the commit. Same garbage either way — a run's leftovers
/// are collected by the next run — but this way gc happens while the
/// working tree is provably clean (`open_mirror` has just committed the
/// schema) and outside the commit lifecycle the orchestrator owns. It is
/// worth doing: on a 3.3 MB catalog with two versions of history the
/// store shrinks from 5.2 MB to 1.3 MB, with `dolt_log` and
/// `dolt_history_*` intact. It is off by default because it rewrites the
/// whole chunk store, which is time the routine no-op run shouldn't
/// spend.
pub async fn run(
    pool: &SqlitePool,
    opts: &MirrorOptions,
    progress: &Progress,
) -> Result<MirrorStats> {
    let source_bytes = std::fs::metadata(&opts.source_path)
        .with_context(|| format!("stat {}", opts.source_path.display()))?
        .len();

    if opts.gc {
        // Best-effort: a failed collection costs disk, not correctness,
        // and must not fail the backup.
        match sqlx::query("SELECT dolt_gc()").execute(pool).await {
            Ok(_) => tracing::info!("lightroom: collected unreachable chunks"),
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "lightroom: dolt_gc failed; continuing (the store keeps its garbage)"
            ),
        }
    }

    let snap = if opts.snapshot {
        Some(snapshot(&opts.source_path).await?)
    } else {
        None
    };
    let src_path = snap
        .as_ref()
        .map(|s| s.path().to_path_buf())
        .unwrap_or_else(|| opts.source_path.clone());

    // One connection for the whole run: ATTACH is connection-scoped, and
    // the pool is `max_connections(1)` anyway (doltlite's HEAD pointer is
    // per-connection — see `doltlite_raw`'s notes).
    let mut conn = pool.acquire().await.context("acquire mirror connection")?;

    let literal = src_path.display().to_string().replace('\'', "''");
    // Audited: `literal` is the source path, `'`-escaped; the schema alias is
    // a const through `quote_ident`.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ATTACH DATABASE '{literal}' AS {}",
        plan::quote_ident(SRC_SCHEMA)
    )))
    .execute(&mut *conn)
    .await
    .with_context(|| format!("attach source {}", src_path.display()))?;

    let result = mirror_attached(&mut conn, opts, progress).await;

    // Detach even on failure, so a retry on the same pooled connection
    // doesn't trip over a stale alias.
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DETACH DATABASE {}",
        plan::quote_ident(SRC_SCHEMA)
    )))
    .execute(&mut *conn)
    .await;
    drop(conn);
    drop(snap);

    let mut stats = result?;
    stats.source_bytes = source_bytes;
    Ok(stats)
}

async fn mirror_attached(
    conn: &mut SqliteConnection,
    opts: &MirrorOptions,
    progress: &Progress,
) -> Result<MirrorStats> {
    let specs = build_specs(&mut *conn, opts).await?;
    let mut stats = MirrorStats {
        tables: specs.len(),
        columns_dropped: specs.iter().map(|s| s.dropped_columns.len()).sum(),
        tables_restably_keyed: specs
            .iter()
            .filter(|s| s.key_origin == KeyOrigin::StableUnique)
            .count(),
        ..Default::default()
    };

    progress.set_length(Some(specs.len() as u64));

    // Empty the mirror, then refill it from the source. Everything goes,
    // including tables the source no longer has — see
    // [`drop_all_mirror_tables`].
    progress.set_message("clearing");
    let dropped = drop_all_mirror_tables(&mut *conn).await?;
    let wanted: BTreeSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    // For the run summary only. The drop above doesn't care whether a
    // table is stale, but "the catalog lost a table since last run" is
    // worth telling the user about.
    stats.stale_tables_dropped = dropped
        .iter()
        .filter(|n| !wanted.contains(n.as_str()))
        .inspect(|n| tracing::info!(table = %n, "lightroom: table gone from source"))
        .count();

    for spec in &specs {
        progress.set_message(&spec.name);
        stats.rows += rebuild_table(&mut *conn, spec).await?;
        progress.inc(1);
    }
    progress.finish_and_clear();
    Ok(stats)
}

/// Introspect the attached source and decide, per table, what the mirror
/// should look like.
async fn build_specs(conn: &mut SqliteConnection, opts: &MirrorOptions) -> Result<Vec<TableSpec>> {
    let names = plan::table_names(&mut *conn, SRC_SCHEMA).await?;
    let mut specs = Vec::new();
    for name in names {
        if !wants_table(opts, &name) {
            continue;
        }
        if RESERVED_TABLES.contains(&name.as_str()) {
            bail!(
                "source table {name:?} collides with the raw store's own bookkeeping table; \
                 exclude it with exclude_tables = [{name:?}]"
            );
        }
        let source_cols = plan::table_columns(&mut *conn, SRC_SCHEMA, &name).await?;
        let unique_cols = plan::unique_single_columns(&mut *conn, SRC_SCHEMA, &name).await?;
        specs.push(build_spec(opts, &name, &source_cols, &unique_cols)?);
    }
    Ok(specs)
}

fn wants_table(opts: &MirrorOptions, name: &str) -> bool {
    opts.include_tables
        .iter()
        .any(|p| datalib_etl_lightroom_config::glob_match(p, name))
        && !opts
            .exclude_tables
            .iter()
            .any(|p| datalib_etl_lightroom_config::glob_match(p, name))
}

/// Column filter + key selection for one table. Split out from
/// [`build_specs`] so it can be unit-tested without a database.
pub fn build_spec(
    opts: &MirrorOptions,
    name: &str,
    source_cols: &[SourceColumn],
    unique_cols: &[String],
) -> Result<TableSpec> {
    let mut columns: Vec<ColumnSpec> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for c in source_cols {
        // A generated column has no stored value to copy; mirroring the
        // expression is out of scope (and `table_xinfo` doesn't expose
        // it), so it's dropped like a filtered column.
        let filtered = c.generated
            || opts.exclude_columns.iter().any(|p| {
                datalib_etl_lightroom_config::glob_match(p, &format!("{name}.{}", c.spec.name))
            });
        if filtered {
            dropped.push(c.spec.name.clone());
        } else {
            columns.push(c.spec.clone());
        }
    }
    if columns.is_empty() {
        bail!("every column of table {name:?} was excluded; exclude the table instead");
    }

    let declared: Vec<String> = {
        let mut keyed: Vec<&SourceColumn> = source_cols.iter().filter(|c| c.pk_seq > 0).collect();
        keyed.sort_by_key(|c| c.pk_seq);
        keyed.iter().map(|c| c.spec.name.clone()).collect()
    };
    let present = |c: &String| columns.iter().any(|x| &x.name == c);

    let (pk, key_origin) = if let Some(over) = opts.primary_keys.get(name) {
        for c in over {
            if !present(c) {
                bail!(
                    "primary_keys override for {name:?} names column {c:?}, which is not mirrored"
                );
            }
        }
        let origin = if over.is_empty() {
            KeyOrigin::Keyless
        } else {
            KeyOrigin::Override
        };
        (over.clone(), origin)
    } else if let Some(stable) = opts
        .stable_key_columns
        .iter()
        .find(|c| unique_cols.contains(c) && present(c))
        // A stable key that IS the declared key is not a rewrite.
        .filter(|c| declared.as_slice() != std::slice::from_ref(*c))
    {
        (vec![stable.clone()], KeyOrigin::StableUnique)
    } else if !declared.is_empty() && declared.iter().all(present) {
        (declared, KeyOrigin::Declared)
    } else {
        // Either the source table is keyless, or its key was filtered
        // out. Keyless is a legitimate mirror shape: doltlite still
        // diffs the table, by row multiset rather than by key.
        (Vec::new(), KeyOrigin::Keyless)
    };

    Ok(TableSpec {
        name: name.to_string(),
        columns,
        pk,
        dropped_columns: dropped,
        key_origin,
    })
}

/// Rebuild one table: drop it, recreate it from the source's current
/// shape, refill it. Returns the row count written.
///
/// One transaction per table, DDL included — doltlite rolls back a
/// `DROP`/`CREATE` like any other statement, so a failure mid-run leaves
/// this table exactly as the previous run left it rather than empty or
/// half-filled.
///
/// The drop is unconditional and costs nothing: a table recreated with
/// the same shape and refilled with the same rows hashes to the chunks
/// already at HEAD, so `dolt_status` stays clean. That is what lets this
/// function be the *entire* schema story — no comparison against the
/// mirror's current shape, and so no way for the two to disagree.
///
/// The refill is one `INSERT … SELECT` from the `ATTACH`ed source. It
/// used to detour through a keyless staging table whenever the
/// destination's key was not a rowid alias, to route around a doltlite
/// bug ([dolthub/doltlite#2327]) that silently gave every row after the
/// first the *first* row's bytes for values past the source file's
/// local-payload limit — lengths and `typeof()` still right, no error,
/// and the damage survived `dolt_commit`. Fixed upstream in v0.11.53
/// ([dolthub/doltlite#2329]), which is what `MODULE.bazel` now pins, so
/// the detour is gone. The regression test that caught it guards its
/// absence: `large_values_round_trip_byte_for_byte` in
/// `mirror_roundtrip.rs` fails against the old pin without the detour.
/// `hack/doltlite_blob_bug/run.sh` re-checks the upstream behaviour
/// directly, without going through this crate.
///
/// [dolthub/doltlite#2327]: https://github.com/dolthub/doltlite/issues/2327
/// [dolthub/doltlite#2329]: https://github.com/dolthub/doltlite/pull/2329
async fn rebuild_table(conn: &mut SqliteConnection, spec: &TableSpec) -> Result<u64> {
    let mut tx = conn
        .begin()
        .await
        .with_context(|| format!("begin rebuild tx for {}", spec.name))?;
    // Audited: `create_ddl()` / `copy_sql()` render every *identifier* —
    // table and column names — through `plan::quote_ident`, which
    // double-quotes and escapes embedded quotes.
    //
    // NOT covered, and worth knowing: `ColumnSpec::decl()` also splices the
    // column's declared type and DEFAULT expression in verbatim, and both
    // come from `PRAGMA table_xinfo` on the attached source catalog — i.e.
    // from the .lrcat file, which is outside our control. A crafted catalog
    // can therefore influence this CREATE TABLE beyond the identifiers.
    // Blast radius is the mirror we are building (a fresh doltlite file);
    // the source is opened `read_only(true)` and copied via VACUUM INTO
    // before anything is attached, so it cannot be written back. Tracked
    // separately — this bump did not introduce it and does not fix it.
    sqlx::query(sqlx::AssertSqlSafe(spec.create_ddl()))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("create mirror table {}", spec.name))?;

    sqlx::query(sqlx::AssertSqlSafe(spec.copy_sql(SRC_SCHEMA)))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("copy rows into {}", spec.name))?;

    // Count from the table rather than trusting `rows_affected` on an
    // `INSERT … SELECT`: it is the number the summary reports and the
    // tests assert, so it should be read back from what actually landed.
    let n: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) AS n FROM main.{}",
        plan::quote_ident(&spec.name)
    )))
    .fetch_one(&mut *tx)
    .await
    .with_context(|| format!("count rows in {}", spec.name))?
    .get("n");
    tx.commit()
        .await
        .with_context(|| format!("commit rebuild tx for {}", spec.name))?;
    Ok(n as u64)
}

/// Drop every table in the mirror. Returns the names dropped.
///
/// Unconditional, and that is the point: narrowing this to "tables the
/// source still has" would leave a table the source *removed* frozen at
/// HEAD forever, indistinguishable from a live one. Dropping everything
/// and rebuilding from the source means HEAD always means "the catalog
/// as it is now", with no notion of staleness to compute or get wrong.
///
/// Cheap, because dropping a table and recreating it identically is not
/// a change to doltlite — see this module's header. The rows stay
/// recoverable from history either way (branch at an earlier commit).
///
/// Runs in its own transaction, before any rebuild. The raw store's own
/// bookkeeping ([`RESERVED_TABLES`]) is excluded, as is anything
/// `sqlite_%` (filtered out by [`plan::table_names`]).
async fn drop_all_mirror_tables(conn: &mut SqliteConnection) -> Result<Vec<String>> {
    let existing = plan::table_names(&mut *conn, "main").await?;
    let mut tx = conn.begin().await.context("begin drop-all tx")?;
    let mut dropped = Vec::new();
    for name in existing {
        if RESERVED_TABLES.contains(&name.as_str()) {
            continue;
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE IF EXISTS main.{}",
            plan::quote_ident(&name)
        )))
        .execute(&mut *tx)
        .await
        .with_context(|| format!("drop mirror table {name}"))?;
        dropped.push(name);
    }
    tx.commit().await.context("commit drop-all tx")?;
    Ok(dropped)
}

impl MirrorStats {
    /// One-line run summary, in the shape the DAG's step protocol shows.
    pub fn summary(&self) -> String {
        format!(
            "tables={} rows={} stale_tables_dropped={} dropped_columns={} \
             stable_keys={} source_bytes={}",
            self.tables,
            self.rows,
            self.stale_tables_dropped,
            self.columns_dropped,
            self.tables_restably_keyed,
            self.source_bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> MirrorOptions {
        MirrorOptions {
            source_path: PathBuf::from("/dev/null"),
            snapshot: false,
            include_tables: vec!["*".into()],
            exclude_tables: Vec::new(),
            exclude_columns: Vec::new(),
            stable_key_columns: vec!["id_global".into()],
            primary_keys: BTreeMap::new(),
            gc: false,
        }
    }

    fn scol(name: &str, ty: &str, pk_seq: i64) -> SourceColumn {
        SourceColumn {
            spec: ColumnSpec {
                name: name.into(),
                decl_type: ty.into(),
                not_null: false,
                default: None,
            },
            pk_seq,
            generated: false,
        }
    }

    /// The Lightroom table shape: `id_local INTEGER PRIMARY KEY` plus
    /// `id_global UNIQUE NOT NULL`.
    fn lightroom_cols() -> Vec<SourceColumn> {
        vec![
            scol("id_local", "INTEGER", 1),
            scol("id_global", "", 0),
            scol("xmp", "", 0),
        ]
    }

    #[test]
    fn stable_key_beats_the_declared_rowid_key() {
        let s = build_spec(
            &opts(),
            "Adobe_AdditionalMetadata",
            &lightroom_cols(),
            &["id_global".to_string()],
        )
        .unwrap();
        assert_eq!(s.pk, vec!["id_global".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::StableUnique);
        // id_local is still mirrored — it is data, just not the key.
        assert!(s.columns.iter().any(|c| c.name == "id_local"));
    }

    #[test]
    fn declared_key_is_used_when_no_stable_candidate_exists() {
        let cols = vec![scol("id_local", "INTEGER", 1), scol("v", "", 0)];
        let s = build_spec(&opts(), "AgHarvestedExifMetadata", &cols, &[]).unwrap();
        assert_eq!(s.pk, vec!["id_local".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::Declared);
    }

    #[test]
    fn disabling_stable_keys_mirrors_the_declared_key() {
        let mut o = opts();
        o.stable_key_columns.clear();
        let s = build_spec(
            &o,
            "Adobe_images",
            &lightroom_cols(),
            &["id_global".to_string()],
        )
        .unwrap();
        assert_eq!(s.pk, vec!["id_local".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::Declared);
    }

    #[test]
    fn a_non_integer_primary_key_is_mirrored_as_the_key() {
        // `CREATE TABLE MigrationSchemaVersion(version TEXT PRIMARY KEY)`
        // — a key that is not a rowid alias. SQLite reports the column as
        // nullable and dolt stores it NOT NULL; the mirror emits the
        // source's shape verbatim and lets dolt apply its own rule,
        // because nothing here ever compares the two. (An earlier design
        // did compare them, and rebuilt ten of a real catalog's 133
        // tables on every run over exactly this mismatch.)
        let cols = vec![SourceColumn {
            spec: ColumnSpec {
                name: "version".into(),
                decl_type: "TEXT".into(),
                not_null: false,
                default: None,
            },
            pk_seq: 1,
            generated: false,
        }];
        let s = build_spec(&opts(), "MigrationSchemaVersion", &cols, &[]).unwrap();
        assert_eq!(s.pk, vec!["version".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::Declared);
        assert!(s.create_ddl().contains(r#"PRIMARY KEY ("version")"#));
    }

    #[test]
    fn a_source_table_with_no_key_mirrors_keyless() {
        let cols = vec![scol("a", "", 0), scol("b", "", 0)];
        let s = build_spec(&opts(), "AgOzSpaceIds", &cols, &[]).unwrap();
        assert!(s.pk.is_empty());
        assert_eq!(s.key_origin, KeyOrigin::Keyless);
    }

    #[test]
    fn excluding_the_key_column_falls_back_to_keyless_rather_than_lying() {
        let mut o = opts();
        o.exclude_columns = vec!["T.id_local".into()];
        o.stable_key_columns.clear();
        let s = build_spec(&o, "T", &lightroom_cols(), &[]).unwrap();
        assert!(s.pk.is_empty());
        assert_eq!(s.key_origin, KeyOrigin::Keyless);
        assert_eq!(s.dropped_columns, vec!["id_local".to_string()]);
    }

    #[test]
    fn excluded_columns_are_absent_not_blanked() {
        let mut o = opts();
        o.exclude_columns = vec!["Adobe_AdditionalMetadata.xmp".into()];
        let s = build_spec(&o, "Adobe_AdditionalMetadata", &lightroom_cols(), &[]).unwrap();
        assert!(!s.columns.iter().any(|c| c.name == "xmp"));
        assert_eq!(s.dropped_columns, vec!["xmp".to_string()]);
        assert!(!s.create_ddl().contains("xmp"));
        assert!(!s.copy_sql("src").contains("xmp"));
    }

    #[test]
    fn primary_key_override_beats_everything() {
        let mut o = opts();
        o.primary_keys
            .insert("T".into(), vec!["id_local".into(), "id_global".into()]);
        let s = build_spec(&o, "T", &lightroom_cols(), &["id_global".to_string()]).unwrap();
        assert_eq!(s.pk, vec!["id_local".to_string(), "id_global".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::Override);
        assert!(s
            .create_ddl()
            .contains(r#"PRIMARY KEY ("id_local", "id_global")"#));
    }

    #[test]
    fn an_empty_override_forces_keyless() {
        let mut o = opts();
        o.primary_keys.insert("T".into(), Vec::new());
        let s = build_spec(&o, "T", &lightroom_cols(), &["id_global".to_string()]).unwrap();
        assert!(s.pk.is_empty());
        assert_eq!(s.key_origin, KeyOrigin::Keyless);
    }

    #[test]
    fn an_override_naming_an_unmirrored_column_is_an_error() {
        let mut o = opts();
        o.exclude_columns = vec!["T.xmp".into()];
        o.primary_keys.insert("T".into(), vec!["xmp".into()]);
        assert!(build_spec(&o, "T", &lightroom_cols(), &[]).is_err());
    }

    #[test]
    fn generated_columns_are_not_mirrored() {
        let mut cols = lightroom_cols();
        cols.push(SourceColumn {
            spec: ColumnSpec {
                name: "computed".into(),
                decl_type: "".into(),
                not_null: false,
                default: None,
            },
            pk_seq: 0,
            generated: true,
        });
        let s = build_spec(&opts(), "T", &cols, &[]).unwrap();
        assert!(!s.columns.iter().any(|c| c.name == "computed"));
        assert!(s.dropped_columns.contains(&"computed".to_string()));
    }

    #[test]
    fn excluding_every_column_is_an_error_not_an_empty_table() {
        let mut o = opts();
        o.exclude_columns = vec!["T.*".into()];
        assert!(build_spec(&o, "T", &lightroom_cols(), &[]).is_err());
    }

    #[test]
    fn table_filters_compose() {
        let mut o = opts();
        o.include_tables = vec!["Ag*".into()];
        o.exclude_tables = vec!["*Oz*".into()];
        assert!(wants_table(&o, "AgLibraryFile"));
        assert!(!wants_table(&o, "AgOzSpaceIds"));
        assert!(!wants_table(&o, "Adobe_images"));
    }
}
