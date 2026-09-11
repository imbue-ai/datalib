//! The SQLite→doltlite mirror engine.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection, SqlitePool, SqlitePoolOptions};
use sqlx::{Connection, Row};

use datalib_etl::progress::Progress;
use datalib_source_common::glob_match;

use crate::plan::{self, ColumnSpec, KeyOrigin, SourceColumn, TableKind, TableSpec};

/// Schema alias the source catalog is ATTACHed under. Deliberately
/// unlikely to collide with anything a user would name a database.
const SRC_SCHEMA: &str = "datalib_mirror_src";

/// Tables the mirror must never touch: `datalib_etl`'s shared
/// bookkeeping, created by `doltlite_raw::open`. A source table with one
/// of these names is a hard error rather than a silent clobber of the
/// store's own metadata.
const RESERVED_TABLES: &[&str] = &["sync_runs", "sync_scope_state", "sync_scope_config"];

/// Everything the engine needs. Built from a provider's config by its
/// processor, or from flags by a standalone CLI.
#[derive(Debug, Clone)]
pub struct MirrorOptions {
    /// The SQLite database to mirror (a `.lrcat` for Lightroom,
    /// `Photos.sqlite` for Apple Photos).
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
    /// Tables keyed on a stable column instead of the declared primary
    /// key — the `id_global` / `ZUUID` rewrite, whether the source
    /// declared the column UNIQUE or the run checked it.
    pub tables_restably_keyed: usize,
    /// Virtual tables' backing storage, never mirrored: the virtual
    /// table itself carries the rows.
    pub shadow_tables_skipped: usize,
    /// Virtual tables whose module this build lacks, so their rows
    /// could not be read. Each one is also a warning in the log.
    pub virtual_tables_skipped: usize,
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
    pub fn is_copy(&self) -> bool {
        self.dir.is_some()
    }
}

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
                "sqlite_mirror: VACUUM INTO snapshot failed; falling back to a file copy. \
                 If the database is open in its application the copy may be \
                 inconsistent — close the application for a clean backup."
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
            Ok(_) => tracing::info!("sqlite_mirror: collected unreachable chunks"),
            Err(e) => tracing::warn!(
                error = %format!("{e:#}"),
                "sqlite_mirror: dolt_gc failed; continuing (the store keeps its garbage)"
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
    let mut stats = MirrorStats::default();
    let specs = build_specs(&mut *conn, opts, &mut stats).await?;
    stats.tables = specs.len();
    stats.columns_dropped = specs.iter().map(|s| s.dropped_columns.len()).sum();
    stats.tables_restably_keyed = specs
        .iter()
        .filter(|s| {
            matches!(
                s.key_origin,
                KeyOrigin::StableUnique | KeyOrigin::StableVerified
            )
        })
        .count();

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
        .inspect(|n| tracing::info!(table = %n, "sqlite_mirror: table gone from source"))
        .count();

    for spec in &specs {
        progress.set_message(&spec.name);
        stats.rows += rebuild_table(&mut *conn, spec).await?;
        progress.inc(1);
    }
    progress.finish_and_clear();
    Ok(stats)
}

async fn build_specs(
    conn: &mut SqliteConnection,
    opts: &MirrorOptions,
    stats: &mut MirrorStats,
) -> Result<Vec<TableSpec>> {
    let tables = plan::source_tables(&mut *conn, SRC_SCHEMA).await?;
    let mut specs = Vec::new();
    for table in tables {
        let name = table.name;
        if !wants_table(opts, &name) {
            continue;
        }
        if RESERVED_TABLES.contains(&name.as_str()) {
            bail!(
                "source table {name:?} collides with the raw store's own bookkeeping table; \
                 exclude it with exclude_tables = [{name:?}]"
            );
        }
        if table.kind == TableKind::Shadow {
            stats.shadow_tables_skipped += 1;
            continue;
        }
        let source_cols = match plan::table_columns(&mut *conn, SRC_SCHEMA, &name).await {
            Ok(cols) => cols,
            // A virtual table whose module this build lacks cannot be
            // read at all. Say so and move on rather than fail the
            // backup over an index: its shadow tables were skipped above,
            // so nothing of it lands.
            Err(e) if table.kind == TableKind::Virtual => {
                tracing::warn!(
                    table = %name,
                    error = %format!("{e:#}"),
                    "sqlite_mirror: cannot read virtual table; skipping it"
                );
                stats.virtual_tables_skipped += 1;
                continue;
            }
            Err(e) => return Err(e),
        };
        let unique_cols = plan::unique_single_columns(&mut *conn, SRC_SCHEMA, &name).await?;
        let verified_cols =
            verified_stable_columns(&mut *conn, opts, &name, &source_cols, &unique_cols).await?;
        specs.push(build_spec(
            opts,
            &name,
            &source_cols,
            &unique_cols,
            &verified_cols,
        )?);
    }
    Ok(specs)
}

/// The stable-key candidates the source did not declare UNIQUE but
/// which hold a distinct non-NULL value in every row right now. Apple
/// Photos indexes `ZUUID` without declaring it unique, so without this
/// check its tables would key on a rowid the app renumbers.
async fn verified_stable_columns(
    conn: &mut SqliteConnection,
    opts: &MirrorOptions,
    table: &str,
    source_cols: &[SourceColumn],
    unique_cols: &[String],
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for c in &opts.stable_key_columns {
        if unique_cols.contains(c) || !source_cols.iter().any(|s| &s.spec.name == c) {
            continue;
        }
        // Audited: table and column names come out of the source's own
        // schema and go through `plan::quote_ident`; the schema alias is
        // a const.
        let sql = format!(
            "SELECT COUNT(*) AS n, COUNT(DISTINCT {c}) AS distinct_n, COUNT({c}) AS non_null \
             FROM {s}.{t}",
            c = plan::quote_ident(c),
            s = plan::quote_ident(SRC_SCHEMA),
            t = plan::quote_ident(table),
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_one(&mut *conn)
            .await
            .with_context(|| format!("check stable key {table}.{c}"))?;
        let n: i64 = row.get("n");
        let distinct_n: i64 = row.get("distinct_n");
        let non_null: i64 = row.get("non_null");
        if n == distinct_n && n == non_null {
            out.push(c.clone());
        } else {
            tracing::warn!(
                table,
                column = %c,
                rows = n,
                distinct = distinct_n,
                non_null,
                "sqlite_mirror: stable key column is not unique and non-NULL in every row; \
                 keying on the declared primary key instead"
            );
        }
    }
    Ok(out)
}

fn wants_table(opts: &MirrorOptions, name: &str) -> bool {
    opts.include_tables.iter().any(|p| glob_match(p, name))
        && !opts.exclude_tables.iter().any(|p| glob_match(p, name))
}

pub fn build_spec(
    opts: &MirrorOptions,
    name: &str,
    source_cols: &[SourceColumn],
    unique_cols: &[String],
    verified_cols: &[String],
) -> Result<TableSpec> {
    let mut columns: Vec<ColumnSpec> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for c in source_cols {
        // A generated column has no stored value to copy; mirroring the
        // expression is out of scope (and `table_xinfo` doesn't expose
        // it), so it's dropped like a filtered column.
        let filtered = c.generated
            || opts
                .exclude_columns
                .iter()
                .any(|p| glob_match(p, &format!("{name}.{}", c.spec.name)));
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
        .find(|c| (unique_cols.contains(c) || verified_cols.contains(c)) && present(c))
        // A stable key that IS the declared key is not a rewrite.
        .filter(|c| declared.as_slice() != std::slice::from_ref(*c))
    {
        let origin = if unique_cols.contains(stable) {
            KeyOrigin::StableUnique
        } else {
            KeyOrigin::StableVerified
        };
        (vec![stable.clone()], origin)
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

async fn rebuild_table(conn: &mut SqliteConnection, spec: &TableSpec) -> Result<u64> {
    let mut tx = conn
        .begin()
        .await
        .with_context(|| format!("begin rebuild tx for {}", spec.name))?;
    // Audited: every fragment `create_ddl()` / `copy_sql()` take from the
    // source catalog — table names, column names, and the columns'
    // declared types — goes through `plan::quote_ident`, which
    // double-quotes and escapes embedded quotes. Nothing else from the
    // catalog reaches these statements: column DEFAULTs, the one other
    // piece of SQL text `table_xinfo` reports, are not mirrored at all
    // (see the `plan` module docs).
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
    pub fn summary(&self) -> String {
        format!(
            "tables={} rows={} stale_tables_dropped={} dropped_columns={} \
             stable_keys={} shadow_tables_skipped={} virtual_tables_skipped={} \
             source_bytes={}",
            self.tables,
            self.rows,
            self.stale_tables_dropped,
            self.columns_dropped,
            self.tables_restably_keyed,
            self.shadow_tables_skipped,
            self.virtual_tables_skipped,
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
            &[],
        )
        .unwrap();
        assert_eq!(s.pk, vec!["id_global".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::StableUnique);
        // id_local is still mirrored — it is data, just not the key.
        assert!(s.columns.iter().any(|c| c.name == "id_local"));
    }

    /// The Apple Photos shape: `ZUUID` is indexed but never declared
    /// UNIQUE, so the run itself has to vouch for it.
    #[test]
    fn a_verified_stable_key_beats_the_declared_rowid_key() {
        let cols = vec![
            scol("Z_PK", "INTEGER", 1),
            scol("ZUUID", "VARCHAR", 0),
            scol("ZFAVORITE", "INTEGER", 0),
        ];
        let mut o = opts();
        o.stable_key_columns = vec!["ZUUID".into()];
        let s = build_spec(&o, "ZASSET", &cols, &[], &["ZUUID".to_string()]).unwrap();
        assert_eq!(s.pk, vec!["ZUUID".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::StableVerified);
        // Unverified and undeclared, the same column is not a key.
        let s = build_spec(&o, "ZASSET", &cols, &[], &[]).unwrap();
        assert_eq!(s.pk, vec!["Z_PK".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::Declared);
    }

    #[test]
    fn declared_key_is_used_when_no_stable_candidate_exists() {
        let cols = vec![scol("id_local", "INTEGER", 1), scol("v", "", 0)];
        let s = build_spec(&opts(), "AgHarvestedExifMetadata", &cols, &[], &[]).unwrap();
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
            &[],
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
            },
            pk_seq: 1,
            generated: false,
        }];
        let s = build_spec(&opts(), "MigrationSchemaVersion", &cols, &[], &[]).unwrap();
        assert_eq!(s.pk, vec!["version".to_string()]);
        assert_eq!(s.key_origin, KeyOrigin::Declared);
        assert!(s.create_ddl().contains(r#"PRIMARY KEY ("version")"#));
    }

    #[test]
    fn a_source_table_with_no_key_mirrors_keyless() {
        let cols = vec![scol("a", "", 0), scol("b", "", 0)];
        let s = build_spec(&opts(), "AgOzSpaceIds", &cols, &[], &[]).unwrap();
        assert!(s.pk.is_empty());
        assert_eq!(s.key_origin, KeyOrigin::Keyless);
    }

    #[test]
    fn excluding_the_key_column_falls_back_to_keyless_rather_than_lying() {
        let mut o = opts();
        o.exclude_columns = vec!["T.id_local".into()];
        o.stable_key_columns.clear();
        let s = build_spec(&o, "T", &lightroom_cols(), &[], &[]).unwrap();
        assert!(s.pk.is_empty());
        assert_eq!(s.key_origin, KeyOrigin::Keyless);
        assert_eq!(s.dropped_columns, vec!["id_local".to_string()]);
    }

    #[test]
    fn excluded_columns_are_absent_not_blanked() {
        let mut o = opts();
        o.exclude_columns = vec!["Adobe_AdditionalMetadata.xmp".into()];
        let s = build_spec(&o, "Adobe_AdditionalMetadata", &lightroom_cols(), &[], &[]).unwrap();
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
        let s = build_spec(&o, "T", &lightroom_cols(), &["id_global".to_string()], &[]).unwrap();
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
        let s = build_spec(&o, "T", &lightroom_cols(), &["id_global".to_string()], &[]).unwrap();
        assert!(s.pk.is_empty());
        assert_eq!(s.key_origin, KeyOrigin::Keyless);
    }

    #[test]
    fn an_override_naming_an_unmirrored_column_is_an_error() {
        let mut o = opts();
        o.exclude_columns = vec!["T.xmp".into()];
        o.primary_keys.insert("T".into(), vec!["xmp".into()]);
        assert!(build_spec(&o, "T", &lightroom_cols(), &[], &[]).is_err());
    }

    #[test]
    fn generated_columns_are_not_mirrored() {
        let mut cols = lightroom_cols();
        cols.push(SourceColumn {
            spec: ColumnSpec {
                name: "computed".into(),
                decl_type: "".into(),
                not_null: false,
            },
            pk_seq: 0,
            generated: true,
        });
        let s = build_spec(&opts(), "T", &cols, &[], &[]).unwrap();
        assert!(!s.columns.iter().any(|c| c.name == "computed"));
        assert!(s.dropped_columns.contains(&"computed".to_string()));
    }

    #[test]
    fn excluding_every_column_is_an_error_not_an_empty_table() {
        let mut o = opts();
        o.exclude_columns = vec!["T.*".into()];
        assert!(build_spec(&o, "T", &lightroom_cols(), &[], &[]).is_err());
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
