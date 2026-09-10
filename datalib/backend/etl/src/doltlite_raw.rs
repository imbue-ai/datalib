//! Shared machinery for the doltlite-backed raw stores every provider
//! writes: opening a store, the DDL every store gets for free, per-row
//! bookkeeping, and the `dolt_diff` scan that drives incremental render.
//!
//! Provider crates describe only their own object tables and upserts.
//!
//! The rules you need before changing anything here — primary keys,
//! bookkeeping sidecars, volatile fields, JSONB, why pools are size 1, why
//! DDL runs in two passes — are in `datalib/backend/etl/README.md`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

// Constants so every provider agrees and a rename has one search target.
// Only the DDL fragments below use them; provider SQL spells them inline.
pub const COL_ID: &str = "id";
pub const COL_PAYLOAD: &str = "payload";
pub const COL_FETCHED_AT: &str = "fetched_at";
pub const COL_ATTEMPT_COUNT: &str = "attempt_count";
pub const COL_LAST_ATTEMPT_AT: &str = "last_attempt_at";
pub const COL_LAST_ERROR: &str = "last_error";

pub fn bookkeeping_ddl_for(table: &str) -> String {
    // No `DEFAULT` on any column here; writers bind every value
    // explicitly.
    format!(
        "CREATE TABLE IF NOT EXISTS {table}_bookkeeping (
            id TEXT PRIMARY KEY,
            fetched_at TEXT NULL,
            attempt_count INTEGER NOT NULL,
            last_attempt_at TEXT NULL,
            last_error TEXT NULL,
            volatile_payload TEXT NULL
        )"
    )
}

/// The `id` PK + `payload` JSONB pair every wire-payload table needs.
/// Embed it as the **first** field of a row struct: `#[derive(WirePayloadRow)]`
/// recognizes it by *type*, so a rename is a compile error rather than a
/// runtime SQL mismatch.
#[derive(Debug, Clone)]
pub struct WirePayload {
    pub id: String,
    pub payload: String,
}

/// A row type whose table is "wire-payload" shaped: id + payload + promoted
/// columns. `#[derive(WirePayloadRow)]` in `datalib-etl-macros` generates
/// this and the matching `BulkUpsertable` impl; `signal::ingest::schema_raw`
/// is the canonical use.
pub trait WirePayloadRow {
    fn ddl() -> String;
}

pub fn wire_payload_table_ddl(table: &str, promoted_columns: &[&str]) -> String {
    let promoted_block = if promoted_columns.is_empty() {
        String::new()
    } else {
        format!(",\n    {}", promoted_columns.join(",\n    "))
    };
    format!(
        "CREATE TABLE IF NOT EXISTS {table} (
    id             TEXT PRIMARY KEY,
    payload        TEXT NULL{promoted_block}
)"
    )
}

// Volatile-field split / overlay. See the README: some payloads carry
// per-fetch fields that churn without meaning anything, and leaving them in
// the content payload makes every re-download look like a change.

/// Object keys from the payload root down to a field to split out.
/// `&["updated"]` is top-level; `&["topic", "last_set"]` is nested.
pub type VolatilePath<'a> = &'a [&'a str];

/// Partition `payload` into `(base, volatile)`. `volatile` is `None` when
/// nothing matched. Exact inverse of [`overlay`]. A path that is absent — or
/// that would descend through a non-object — is skipped, so declaring a
/// volatile field some objects lack is harmless.
pub fn split_volatile(payload: &Value, paths: &[VolatilePath]) -> (Value, Option<Value>) {
    let mut base = payload.clone();
    let mut volatile = serde_json::Map::new();
    let mut any = false;
    for path in paths {
        if path.is_empty() {
            continue;
        }
        if let Some(taken) = remove_path(&mut base, path) {
            insert_path(&mut volatile, path, taken);
            any = true;
        }
    }
    (base, any.then_some(Value::Object(volatile)))
}

/// Deep-merge `volatile` onto `base`; the inverse of [`split_volatile`].
/// NOT RFC 7386 merge-patch: a `null` in `volatile` sets the key to `null`
/// rather than deleting it, because Slack payloads carry real nulls.
pub fn overlay(base: &Value, volatile: &Value) -> Value {
    match (base, volatile) {
        (Value::Object(b), Value::Object(v)) => {
            let mut out = b.clone();
            for (k, vv) in v {
                let merged = match out.get(k) {
                    Some(existing) => overlay(existing, vv),
                    None => vv.clone(),
                };
                out.insert(k.clone(), merged);
            }
            Value::Object(out)
        }
        _ => volatile.clone(),
    }
}

fn remove_path(root: &mut Value, path: &[&str]) -> Option<Value> {
    let (last, parents) = path.split_last()?;
    let mut cur = root;
    for key in parents {
        cur = match cur {
            Value::Object(m) => m.get_mut(*key)?,
            _ => return None,
        };
    }
    match cur {
        Value::Object(m) => m.remove(*last),
        _ => None,
    }
}

fn insert_path(obj: &mut serde_json::Map<String, Value>, path: &[&str], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut cur = obj;
    for key in parents {
        let entry = cur
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        match entry {
            Value::Object(m) => cur = m,
            // Declared parent path collided with a non-object leaf; bail
            // rather than clobber.
            _ => return,
        }
    }
    cur.insert((*last).to_string(), value);
}

// ── Shared DDL ──────────────────────────────────────────────────────

/// Append-only log of sync invocations, one row per `ingest::fetch`.
/// A crash mid-sync leaves its row at `status='running'`.
pub const SYNC_RUNS_DDL: &str = "CREATE TABLE IF NOT EXISTS sync_runs (
    run_id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    finished_at TEXT NULL,
    config TEXT NOT NULL,
    status TEXT NOT NULL,
    summary TEXT NULL
)";

/// Per-scope incremental-sync cursor, for providers (github, gitlab) whose
/// discovery is keyed by a search scope. `last_seen_at` is a provider-chosen
/// timestamp, compared back against the configured refresh window when the
/// next run picks its `since` floor.
pub const SYNC_SCOPE_STATE_DDL: &str = "CREATE TABLE IF NOT EXISTS sync_scope_state (
    scope TEXT PRIMARY KEY,
    last_seen_at TEXT NOT NULL
)";

/// The config subset that produced each scope's cursor, so a download can
/// spot config changes the cursor would otherwise swallow (a widened
/// `since`, a relaxed blob cap). Written only once a run has satisfied it;
/// see [`crate::scope_config`] for what belongs in the blob.
///
/// Separate from `sync_scope_state` because the two aren't 1:1 — a provider
/// can have config worth remembering with no cursor to hang it on.
pub const SYNC_SCOPE_CONFIG_DDL: &str = "CREATE TABLE IF NOT EXISTS sync_scope_config (
    scope TEXT PRIMARY KEY,
    config TEXT NOT NULL,
    updated_at TEXT NOT NULL
)";

/// DDL every provider gets for free, appended inside [`open`].
pub const SHARED_DDL: &[&str] = &[SYNC_RUNS_DDL, SYNC_SCOPE_STATE_DDL, SYNC_SCOPE_CONFIG_DDL];

// ── Path helper ─────────────────────────────────────────────────────

pub fn db_path_for(p: &Path) -> PathBuf {
    if p.extension().and_then(|s| s.to_str()) == Some("doltlite_db") {
        return p.to_path_buf();
    }
    crate::raw_layout::entities_db(p)
}

// ── Open ────────────────────────────────────────────────────────────

/// The pool every open shares: one connection, never recycled.
///
/// Pool size 1 with no recycling because doltlite's HEAD, working set and
/// active branch are all per-connection, and a replacement connection starts
/// on `main` with a clean tree. See the README.
///
/// `acquire_timeout` is far past sqlx's 30s default because cold opens of
/// multi-GB stores legitimately take 4-10s inside `sqlite3_open_v2`; 5min is
/// "something else is wrong" territory.
async fn connect_pool(db_path: &Path, access: Access) -> Result<SqlitePool> {
    let writable = access == Access::ReadWrite;
    if writable {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
    }
    // No `journal_mode` pragma: doltlite manages its own storage and rejects
    // it outright.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("sqlite uri for {}", db_path.display()))?
        // A reader never conjures a store: an absent file is a real error for
        // it, where for the owner it is the first run.
        .create_if_missing(writable)
        .read_only(!writable);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .acquire_timeout(Duration::from_secs(300))
        .connect_with(opts)
        .await
        .context("open sqlite pool")
}

/// [`open`] without the shared download-bookkeeping tables. A *derived*
/// store — render output, an index — would otherwise get `sync_runs` and the
/// scope tables as three empty tables suggesting a provenance it lacks.
pub async fn open_derived(db_path: &Path, ddl: &[&str]) -> Result<SqlitePool> {
    open_inner(db_path, ddl, false).await
}

pub async fn open(db_path: &Path, extra_ddl: &[&str]) -> Result<SqlitePool> {
    open_inner(db_path, extra_ddl, true).await
}

/// Open a store to read data somebody else owns.
///
/// **[`open`] writes on the way in, and that is fine for the process that owns
/// the store and wrong for everyone else.** It seals a dirty working tree into
/// a rescue commit, reconciles the schema, and then commits — with `-Am`, so
/// that last commit takes whatever else was dirty along with it. For the owner
/// those are three useful things. For a reader they are three ways to write to
/// a file it does not own, and under streaming the damage is specific: the
/// reader's own open turns the producer's half-written batch into a real
/// commit, which the reader then pins to and reads as though it were finished.
/// Pinning cannot save you from that, because by then the torn rows *are*
/// committed.
///
/// So this does none of it: connect read-only, and hand back the pool. The
/// connection is opened `read_only`, so "a reader must not write" is enforced
/// by the engine (`attempt to write a readonly database`) rather than left as
/// an intention — and creating the `pinned_<table>` views still works, since
/// they live in the per-connection temp schema rather than in the file.
///
/// A schema this store has not got yet is the owner's to add on its next run,
/// and a read naming a column it lacks fails at prepare time saying so. Probe
/// with [`column_exists`] and fall back where that is a real possibility;
/// slack's `load_channels` is the worked example.
pub async fn open_reader(db_path: &Path) -> Result<SqlitePool> {
    connect_pool(db_path, Access::ReadOnly).await
}

/// Whether a pool may write the file it opens. See [`open_reader`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    ReadWrite,
    ReadOnly,
}

async fn open_inner(
    db_path: &Path,
    extra_ddl: &[&str],
    include_shared: bool,
) -> Result<SqlitePool> {
    // Logged at every call so a stray second pool against an already-open
    // file is attributable: with max_connections=1 it surfaces only as
    // "database is locked" on dolt_commit.
    let started = std::time::Instant::now();
    tracing::info!(path = %db_path.display(), "doltlite_raw::open: opening sqlite pool");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    let pool = connect_pool(db_path, Access::ReadWrite).await?;
    // Seal anything a crashed prior run left dirty into its own commit, so
    // this run's `dolt_log` entry describes only this run.
    rescue_dirty_working_tree(&pool, db_path).await;
    // Tables, then the reconcile, then indexes — see the README for why the
    // order is load-bearing. `parse_create_table_name` returns `None` for
    // exactly the statements that must wait.
    let shared: &[&str] = if include_shared { SHARED_DDL } else { &[] };
    let ddl = || extra_ddl.iter().chain(shared.iter());
    let is_create_table = |stmt: &&&str| parse_create_table_name(stmt).is_some();
    for stmt in ddl().filter(is_create_table) {
        // Audited: every DDL statement is built in a provider's `schema_raw.rs`
        // from static consts; no user or upstream data reaches it.
        sqlx::query(sqlx::AssertSqlSafe(*stmt))
            .execute(&pool)
            .await
            .with_context(|| {
                format!(
                    "apply DDL: {}",
                    stmt.split_once('(').map(|p| p.0).unwrap_or(stmt)
                )
            })?;
    }
    // Add columns an older store predates, or drop+recreate when ADD
    // can't express the change.
    for stmt in ddl() {
        reconcile_table_schema(&pool, stmt).await.with_context(|| {
            format!(
                "reconcile schema: {}",
                stmt.split_once('(').map(|p| p.0).unwrap_or(stmt)
            )
        })?;
    }
    // Indexes last, so they see the reconciled columns — and so reconcile's
    // drop+recreate path costs no index.
    for stmt in ddl().filter(|s| !is_create_table(s)) {
        sqlx::query(sqlx::AssertSqlSafe(*stmt))
            .execute(&pool)
            .await
            .with_context(|| {
                format!(
                    "apply DDL: {}",
                    stmt.split_once('(').map(|p| p.0).unwrap_or(stmt)
                )
            })?;
    }
    // Commit the schema before handing back the pool: doltlite only
    // materializes `dolt_diff_<table>` for tables that exist at HEAD, so an
    // uncommitted table makes the first sync's delta vanish with a warning.
    commit_run(&pool, "schema: apply DDL")
        .await
        .context("commit schema after DDL")?;
    // And check it took, unconditionally.
    //
    // A store with tables but no committed schema is the one shape a reader
    // cannot tell from an empty source — `pin::head` refuses it for exactly
    // that reason — so an `open` that left one behind says so here, where
    // the store is still ours, rather than letting a consumer find it and
    // silently skip.
    //
    // Not gated on `has_dolt_extensions`. Every binary that links sqlx links
    // doltlite (MODULE.bazel routes `libsqlite3-sys` at our static archive),
    // so a build without the extensions is not a supported configuration —
    // it is a broken one, and it fails *quietly*: `commit_run` returns
    // `Ok(None)` so nothing ever commits, `head_commit` returns `Ok(None)`
    // so the runner content-hashes instead, and `pin::head` returns `None`
    // so every render skips. A whole pipeline that does nothing and reports
    // success. This is the first place that would notice, so it does.
    //
    // Same predicate the reader uses, not a second copy of it: a store this
    // says is fine and `pin::head` then refuses would be the worst of both.
    // A store with no tables at all passes -- nothing creates a view over
    // it, and a read fails loudly by itself.
    anyhow::ensure!(
        crate::pin::carries_committed_schema(&pool).await,
        "opened {} but its tables are not committed: either the schema \
         commit did not take, or this binary is not linked against doltlite. \
         A reader cannot tell either from a source that lost every row.",
        db_path.display()
    );
    tracing::info!(
        path = %db_path.display(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "doltlite_raw::open: pool ready"
    );
    Ok(pool)
}

/// One column's introspected shape, from `PRAGMA table_xinfo`.
struct ColumnInfo {
    name: String,
    decl_type: String,
    not_null: bool,
    default: Option<String>,
    /// `hidden` 2/3. The generation expression isn't recoverable from
    /// `table_xinfo`, so a missing generated column forces drop+recreate.
    generated: bool,
}

impl ColumnInfo {
    fn add_column_decl(&self) -> String {
        let ty = if self.decl_type.is_empty() {
            "TEXT"
        } else {
            self.decl_type.as_str()
        };
        let mut decl = format!("{} {ty}", self.name);
        if self.not_null {
            decl.push_str(" NOT NULL");
        }
        if let Some(d) = &self.default {
            decl.push_str(" DEFAULT ");
            decl.push_str(d);
        }
        decl
    }
}

/// Whether `table` has `column`, for **read-only** consumers that never get
/// [`open`]'s schema reconcile — the render side opens read-only, so naming a
/// column an older store lacks would fail at prepare time and sink the step.
/// A missing table reads as "no such column" rather than an error.
pub async fn column_exists(pool: &SqlitePool, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns(pool, table)
        .await?
        .iter()
        .any(|c| c.name == column))
}

/// Empty vec if the table does not exist (no error).
async fn table_columns(pool: &SqlitePool, table: &str) -> Result<Vec<ColumnInfo>> {
    // Audited: `table` is a quoted identifier from a `&'static str` or from
    // `parse_create_table_name` over our own DDL; never user input.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA table_xinfo(\"{table}\")"
    )))
    .fetch_all(pool)
    .await
    .with_context(|| format!("table_xinfo({table})"))?;
    let mut cols = Vec::with_capacity(rows.len());
    for r in &rows {
        let name: String = r.try_get("name").unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let not_null: i64 = r.try_get("notnull").unwrap_or(0);
        let hidden: i64 = r.try_get("hidden").unwrap_or(0);
        cols.push(ColumnInfo {
            name,
            decl_type: r.try_get("type").unwrap_or_default(),
            not_null: not_null != 0,
            default: r.try_get("dflt_value").ok().flatten(),
            generated: hidden == 2 || hidden == 3,
        });
    }
    Ok(cols)
}

/// The columns a `CREATE TABLE` DDL declares, learned by letting SQLite
/// parse it into a throwaway probe table so nothing here hand-rolls a parser.
///
/// The probe runs **in memory**, never against the store being opened: a
/// create+drop nets to nothing in the working tree but still appends chunks
/// nobody collects, so it made every `open` cost bytes. See the README.
async fn declared_columns(create_sql: &str, table: &str) -> Result<Vec<ColumnInfo>> {
    const PROBE: &str = "__datalib_schema_probe__";
    // A fresh database per call rather than one shared scratch pool:
    // A fresh in-memory database per call, not a shared scratch pool: the
    // probe table name is a constant, so concurrent reconciles would drop
    // each other's table.
    let probe = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::from_str("sqlite::memory:")
                .context("sqlite uri for the schema probe")?,
        )
        .await
        .context("open the in-memory schema probe")?;
    // The name's first occurrence is the name itself.
    let probe_sql = create_sql.replacen(table, PROBE, 1);
    // Audited: static DDL with only its table name replaced.
    sqlx::query(sqlx::AssertSqlSafe(probe_sql))
        .execute(&probe)
        .await
        .with_context(|| format!("build schema probe for {table}"))?;
    let cols = table_columns(&probe, PROBE).await;
    probe.close().await;
    cols
}

pub async fn declared_column_names(create_sql: &str, table: &str) -> Result<BTreeSet<String>> {
    Ok(declared_columns(create_sql, table)
        .await?
        .into_iter()
        .map(|c| c.name)
        .collect())
}

/// Empty when the table does not exist, matching [`table_columns`].
pub async fn actual_column_names(pool: &SqlitePool, table: &str) -> Result<BTreeSet<String>> {
    Ok(table_columns(pool, table)
        .await?
        .into_iter()
        .map(|c| c.name)
        .collect())
}

pub fn parse_create_table_name(sql: &str) -> Option<String> {
    let s = sql.trim_start();
    if !s.get(..12)?.eq_ignore_ascii_case("CREATE TABLE") {
        return None;
    }
    let mut rest = s[12..].trim_start();
    if rest.len() >= 13 && rest[..13].eq_ignore_ascii_case("IF NOT EXISTS") {
        rest = rest[13..].trim_start();
    }
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '(')
        .unwrap_or(rest.len());
    let name = rest[..end].trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']');
    (!name.is_empty()).then(|| name.to_string())
}

/// Add missing non-generated columns via `ALTER TABLE … ADD COLUMN`;
/// otherwise drop and recreate from the DDL. See the README for why the drop
/// is safe for raw stores and why `open` runs this between the table and
/// index halves of the DDL.
async fn reconcile_table_schema(pool: &SqlitePool, create_sql: &str) -> Result<()> {
    let Some(table) = parse_create_table_name(create_sql) else {
        return Ok(());
    };

    // Desired columns, via a probe built from this exact DDL.
    let desired = declared_columns(create_sql, &table).await?;

    // Empty ⇒ table doesn't exist (the DDL pass should have created it).
    let actual = table_columns(pool, &table).await?;
    if actual.is_empty() {
        // Audited: `create_sql` is our own static DDL.
        sqlx::query(sqlx::AssertSqlSafe(create_sql))
            .execute(pool)
            .await
            .with_context(|| format!("create missing table {table}"))?;
        return Ok(());
    }

    let actual_names: std::collections::HashSet<&str> =
        actual.iter().map(|c| c.name.as_str()).collect();
    let desired_names: std::collections::HashSet<&str> =
        desired.iter().map(|c| c.name.as_str()).collect();
    let has_extra = actual_names.iter().any(|n| !desired_names.contains(n));
    let missing: Vec<&ColumnInfo> = desired
        .iter()
        .filter(|c| !actual_names.contains(c.name.as_str()))
        .collect();

    if !has_extra && missing.is_empty() {
        return Ok(());
    }

    // Additive-only, no generated columns missing → ALTER ADD.
    if !has_extra && missing.iter().all(|c| !c.generated) {
        let mut added_all = true;
        for col in &missing {
            let sql = format!("ALTER TABLE {table} ADD COLUMN {}", col.add_column_decl());
            // Audited: identifiers come from our own static DDL.
            match sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
                Ok(_) => tracing::info!(
                    table = %table,
                    column = %col.name,
                    "doltlite_raw: added missing column to existing table"
                ),
                Err(e) => {
                    tracing::warn!(
                        table = %table,
                        column = %col.name,
                        error = %format!("{e:#}"),
                        "doltlite_raw: ADD COLUMN failed; falling back to drop+recreate"
                    );
                    added_all = false;
                    break;
                }
            }
        }
        if added_all {
            return Ok(());
        }
    }

    // Fallback: drop + recreate.
    tracing::warn!(
        table = %table,
        "doltlite_raw: schema not reconcilable by ADD COLUMN (column removed, \
         renamed, generated, or ADD failed); dropping and recreating from DDL"
    );
    // Audited: `table` is parsed from our own static DDL.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
        .execute(pool)
        .await
        .with_context(|| format!("drop {table} for schema recreate"))?;
    sqlx::query(sqlx::AssertSqlSafe(create_sql))
        .execute(pool)
        .await
        .with_context(|| format!("recreate {table}"))?;
    Ok(())
}

/// Seal a crashed prior run's orphaned working-tree changes into their own
/// commit, so the next successful commit doesn't fold two runs' work into one
/// `dolt_log` entry — and so a dirty tree at open gets logged.
///
/// Errors are swallowed: a stock-libsqlite3 build (CI, no doltlite
/// extensions) has no `dolt_status` at all.
async fn rescue_dirty_working_tree(pool: &SqlitePool, db_path: &Path) {
    // `dolt_status` is a vtab; stock SQLite errors with "no such table".
    let dirty: std::result::Result<i64, sqlx::Error> =
        sqlx::query_scalar("SELECT count(*) FROM dolt_status")
            .fetch_one(pool)
            .await;
    let count = match dirty {
        Ok(n) => n,
        Err(e) => {
            // "no doltlite extensions" is expected and silent; anything else
            // is worth a warning.
            let msg = e.to_string();
            if !msg.contains("no such table") {
                tracing::warn!(
                    path = %db_path.display(),
                    error = %e,
                    "rescue_dirty_working_tree: probe failed"
                );
            }
            return;
        }
    };
    if count == 0 {
        return;
    }
    let msg = format!(
        "rescue: pre-run snapshot of orphaned working tree ({count} dirty entries) at {}",
        datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339()
    );
    tracing::warn!(
        path = %db_path.display(),
        dirty_entries = count,
        "rescue_dirty_working_tree: prior run left {count} dirty entries; sealing into its own commit",
    );
    if let Err(e) = sqlx::query("SELECT dolt_commit('-Am', ?)")
        .bind(&msg)
        .execute(pool)
        .await
    {
        tracing::warn!(
            path = %db_path.display(),
            error = %e,
            "rescue_dirty_working_tree: dolt_commit failed; the next ETL commit will fold the dirty rows in implicitly"
        );
    }
}

// ── sync_runs ───────────────────────────────────────────────────────

pub async fn start_run(pool: &SqlitePool, config: &Value) -> Result<i64> {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let cfg = serde_json::to_string(config).context("serialize run config")?;
    let row = sqlx::query(
        "INSERT INTO sync_runs (started_at, config, status) VALUES (?, ?, 'running') RETURNING run_id",
    )
    .bind(&now)
    .bind(&cfg)
    .fetch_one(pool)
    .await
    .context("insert sync_runs")?;
    let id: i64 = row.try_get("run_id").context("read run_id")?;
    Ok(id)
}

pub async fn finish_run(
    pool: &SqlitePool,
    run_id: i64,
    status: &str,
    summary: &Value,
) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let s = serde_json::to_string(summary).context("serialize run summary")?;
    sqlx::query("UPDATE sync_runs SET finished_at = ?, status = ?, summary = ? WHERE run_id = ?")
        .bind(&now)
        .bind(status)
        .bind(&s)
        .bind(run_id)
        .execute(pool)
        .await
        .context("update sync_runs")?;
    Ok(())
}

// ── dolt commit ─────────────────────────────────────────────────────

/// Whether this connection's libsqlite3 is doltlite rather than stock, so
/// callers can skip commits silently in builds that don't link it.
pub async fn has_dolt_extensions(pool: &SqlitePool) -> bool {
    let res = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pragma_function_list WHERE name = 'dolt_commit'",
    )
    .fetch_one(pool)
    .await;
    matches!(res, Ok(n) if n > 0)
}

pub async fn commit_run_at_path(out_dir: &Path, msg: &str) -> Result<Option<String>> {
    let db_path = db_path_for(out_dir);
    if !db_path.exists() {
        return Ok(None);
    }
    let pool = open(&db_path, &[]).await.context("open for commit")?;
    let hash = commit_run(&pool, msg).await?;
    pool.close().await;
    Ok(hash)
}

pub async fn commit_run(pool: &SqlitePool, msg: &str) -> Result<Option<String>> {
    if !has_dolt_extensions(pool).await {
        return Ok(None);
    }
    // "nothing to commit" is a legitimate outcome: the rescue commit in
    // `open` may already have swept everything up.
    match sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', ?)")
        .bind(msg)
        .fetch_optional(pool)
        .await
    {
        Ok(opt) => Ok(opt.flatten()),
        Err(e) if e.to_string().contains("nothing to commit") => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context("dolt_commit")),
    }
}

/// The store's current HEAD, which is its *content version*: doltlite
/// advances HEAD only when a commit changed something, so two downloads that
/// pulled the same rows leave the same hash. That is what a step reports to
/// the DAG runner, instead of hashing a multi-gigabyte store.
///
/// `Ok(None)` against stock libsqlite3 or an empty log; the runner then
/// content-hashes instead.
pub async fn head_commit(pool: &SqlitePool) -> Result<Option<String>> {
    if !has_dolt_extensions(pool).await {
        return Ok(None);
    }
    sqlx::query_scalar::<_, String>("SELECT commit_hash FROM dolt_log() LIMIT 1")
        .fetch_optional(pool)
        .await
        .context("read dolt_log head")
}

/// [`head_commit`] against a store on disk; `Ok(None)` if nobody has
/// downloaded it yet.
///
/// Deliberately not via [`open`], which is the write path: it would create
/// bookkeeping tables inside a blob CAS and advance HEAD — a version read
/// that changes the version it reads.
pub async fn head_commit_at_path(db_path: &Path) -> Result<Option<String>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let url = format!("sqlite://{}?mode=ro", db_path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .with_context(|| format!("open read-only {}", db_path.display()))?;
    let head = head_commit(&pool).await;
    pool.close().await;
    head
}

// ── Reset ───────────────────────────────────────────────────────────

/// Truncate every per-row table and its sidecar in one transaction, so the
/// next `ingest::fetch` re-downloads from upstream. `sync_runs` and
/// `sync_scope_state` survive — audit log and resume cursor, not content.
///
/// Table names are interpolated; callers pass trusted identifiers.
pub async fn truncate_data_tables(pool: &SqlitePool, data_tables: &[&str]) -> Result<()> {
    let mut tx = pool.begin().await.context("begin truncate tx")?;
    for table in data_tables {
        for sql in [
            format!("DELETE FROM {table}"),
            format!("DELETE FROM {table}_bookkeeping"),
        ] {
            // Audited: identifiers per this fn's documented contract; every
            // callsite passes a `&'static str`.
            sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .execute(&mut *tx)
                .await
                .with_context(|| format!("truncate {sql}"))?;
        }
    }
    tx.commit().await.context("commit truncate tx")?;
    Ok(())
}

// ── Generic object-table ops ────────────────────────────────────────

/// Pre-seed an `id`-only row (NULL payload) and its sidecar, for an entity
/// we know exists upstream but haven't fetched. Existing rows are untouched.
/// Takes a transaction so both inserts land atomically.
///
/// `table` is interpolated; callers pass a trusted identifier.
pub async fn ensure_object_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
) -> Result<()> {
    let data_sql = format!("INSERT INTO {table} (id) VALUES (?) ON CONFLICT(id) DO NOTHING");
    // Audited: `table` interpolated per the contract above; `id` is bound.
    sqlx::query(sqlx::AssertSqlSafe(data_sql))
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("ensure_object_row data {table}={id}"))?;

    let bk_sql = format!(
        "INSERT INTO {table}_bookkeeping (id, attempt_count) VALUES (?, 0) ON CONFLICT(id) DO NOTHING"
    );
    sqlx::query(sqlx::AssertSqlSafe(bk_sql))
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("ensure_object_row bookkeeping {table}={id}"))?;
    Ok(())
}

/// `result = None` is success (sets `fetched_at`, clears `last_error`);
/// `Some(err)` is failure (leaves `fetched_at`, sets `last_error`). Both bump
/// `attempt_count` and set `last_attempt_at`.
///
/// Upserts, so it is safe even when [`ensure_object_row`] hasn't pre-seeded.
pub async fn record_object_attempt(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
    result: Option<&str>,
) -> Result<()> {
    // Keep the always-paired invariant: a failure recorded before any
    // successful fetch has no data row yet.
    let stub_sql = format!("INSERT OR IGNORE INTO {table} (id) VALUES (?)");
    // Audited: `table` interpolated as an identifier; `id` is bound.
    sqlx::query(sqlx::AssertSqlSafe(stub_sql))
        .bind(id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("record_object_attempt data stub {table}={id}"))?;
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let sql = match result {
        None => format!(
            "INSERT INTO {table}_bookkeeping (id, fetched_at, attempt_count, last_attempt_at, last_error)
             VALUES (?, ?, 1, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                fetched_at = excluded.fetched_at,
                attempt_count = {table}_bookkeeping.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL"
        ),
        Some(_) => format!(
            "INSERT INTO {table}_bookkeeping (id, attempt_count, last_attempt_at, last_error)
             VALUES (?, 1, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                attempt_count = {table}_bookkeeping.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = excluded.last_error"
        ),
    };
    // Audited: both arms interpolate only `table`; the rest is bound.
    let q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id).bind(&now);
    let q = match result {
        None => q,
        Some(err) => q.bind(err),
    };
    q.execute(&mut **tx)
        .await
        .with_context(|| format!("record_object_attempt {table}={id}"))?;
    Ok(())
}

/// The single chokepoint a provider calls once its own
/// `INSERT … ON CONFLICT(id) DO UPDATE` has run inside `tx`: stamp
/// bookkeeping, commit, then mirror the row to the wire tape.
///
/// The tape append fires only after the commit succeeds, so a rolled-back tx
/// leaves no orphan line. A tape failure logs at `error!` but does not fail
/// the caller — the doltlite row is the source of truth, and failing here
/// would lie about whether the data landed.
pub async fn write_event_to_raw_storage_layer(
    tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    tape: Option<&crate::event_tape::EventTape>,
    table: &str,
    id: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    write_events_to_raw_storage_layer(tx, tape, &[(table, id, payload)]).await
}

pub async fn write_events_to_raw_storage_layer(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    tape: Option<&crate::event_tape::EventTape>,
    events: &[(&str, &str, &serde_json::Value)],
) -> Result<()> {
    for (table, id, _) in events {
        record_object_attempt(&mut tx, table, id, None).await?;
    }
    tx.commit()
        .await
        .context("commit write_events_to_raw_storage_layer tx")?;
    if let Some(t) = tape {
        for (table, id, payload) in events {
            if let Err(e) = t.append(table, id, payload) {
                tracing::error!(
                    event = "event_tape_append_failed",
                    table = *table,
                    id = *id,
                    error = %format!("{e:#}"),
                    "event tape append failed after doltlite commit; row IS persisted, tape is missing a line — investigate"
                );
            }
        }
    }
    Ok(())
}

// Defined in `crate::bulk` (a primitive of the bulk write path), re-exported
// here because providers reach for it next to the chokepoint.
pub use crate::bulk::EventBatch;

/// Bulk sibling of [`write_event_to_raw_storage_layer`], for a provider that
/// has already issued its chunked entity upserts inside `tx`: stamps one
/// bookkeeping batch per [`EventBatch`], commits, then appends to the tape.
///
/// Non-event tables (sidecars, file-imported data with no wire) want
/// [`crate::bulk::bulk_upsert_bookkeeping`] directly, with no tape.
pub async fn bulk_upsert_events(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    tape: Option<&crate::event_tape::EventTape>,
    batches: &[EventBatch<'_>],
    now: &str,
) -> Result<()> {
    for b in batches {
        crate::bulk::bulk_upsert_bookkeeping(
            &mut tx,
            b.table,
            b.rows.iter().map(|(id, _)| *id),
            now,
        )
        .await?;
    }
    tx.commit().await.context("commit bulk_upsert_events tx")?;
    if let Some(t) = tape {
        for b in batches {
            if b.rows.is_empty() {
                continue;
            }
            if let Err(e) = t.append_batch(b) {
                tracing::error!(
                    event = "event_tape_append_failed",
                    table = b.table,
                    count = b.rows.len(),
                    error = %format!("{e:#}"),
                    "event tape append_batch failed after doltlite commit; rows ARE persisted, tape is missing lines — investigate"
                );
            }
        }
    }
    Ok(())
}

/// [`bulk_upsert_events`] for the common case where the caller has a
/// [`crate::bulk::BulkUpsertable`] row vec rather than rows already written
/// into a transaction. `payloads` carries one `(id, &Value)` per row so the
/// tape line can mirror the upstream JSON.
pub async fn bulk_upsert_with_tape<T: crate::bulk::BulkUpsertable>(
    pool: &sqlx::SqlitePool,
    tape: Option<&crate::event_tape::EventTape>,
    rows: &[T],
    payloads: &[(&str, &serde_json::Value)],
) -> Result<()> {
    bulk_upsert_with_tape_split(pool, tape, rows, payloads, &[]).await
}

/// [`bulk_upsert_with_tape`] where the caller has already run
/// [`split_volatile`]: `rows` carry the content half, `volatile` the split-out
/// fields (written to the sidecar in the same tx), and `tape_payloads` the
/// full reconstructed wire object. Ids absent from `volatile` leave
/// `volatile_payload` NULL.
pub async fn bulk_upsert_with_tape_split<T: crate::bulk::BulkUpsertable>(
    pool: &sqlx::SqlitePool,
    tape: Option<&crate::event_tape::EventTape>,
    rows: &[T],
    tape_payloads: &[(&str, &serde_json::Value)],
    volatile: &[(&str, &serde_json::Value)],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin bulk_upsert_with_tape {} tx", T::TABLE))?;
    crate::bulk::bulk_upsert_in_tx(&mut tx, rows, &now).await?;
    set_volatile_payloads_in_tx(&mut tx, T::TABLE, volatile).await?;
    tx.commit()
        .await
        .with_context(|| format!("commit bulk_upsert_with_tape {} tx", T::TABLE))?;
    if let Some(t) = tape {
        let batch = EventBatch {
            table: T::TABLE,
            rows: tape_payloads,
        };
        if let Err(e) = t.append_batch(&batch) {
            tracing::error!(
                event = "event_tape_append_failed",
                table = T::TABLE,
                count = tape_payloads.len(),
                error = %format!("{e:#}"),
                "event tape append_batch failed after doltlite commit; rows ARE persisted, tape is missing lines — investigate"
            );
        }
    }
    Ok(())
}

async fn set_volatile_payloads_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    volatile: &[(&str, &serde_json::Value)],
) -> Result<()> {
    if volatile.is_empty() {
        return Ok(());
    }
    let bk = format!("{table}_bookkeeping");
    // `Arc<str>` because sqlx 0.9 takes the query string by value: a
    // per-row `&str` would copy the statement once per row.
    let sql: std::sync::Arc<str> =
        format!("UPDATE {bk} SET volatile_payload = jsonb(?) WHERE id = ?").into();
    for (id, value) in volatile {
        let text = serde_json::to_string(value)
            .with_context(|| format!("serialize volatile_payload {bk}={id}"))?;
        // Audited: only `bk` is interpolated; JSON text and id are bound.
        sqlx::query(sqlx::AssertSqlSafe(std::sync::Arc::clone(&sql)))
            .bind(text)
            .bind(*id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("set volatile_payload {bk}={id}"))?;
    }
    Ok(())
}

// ── dolt_diff incremental-render scan ───────────────────────────────

/// Result of a [`scan_buckets`] scan.
#[derive(Debug, Clone, Default)]
pub struct DiffScan {
    /// `Some(set)` → render only these buckets. `None` → cold start (no
    /// cursor, a globally-fanning table changed, the query errored, or no
    /// doltlite extension). Render everything.
    pub changed_buckets: Option<std::collections::HashSet<String>>,
    /// HEAD at scan time, to stamp into the render cursor on success. `None`
    /// leaves the cursor unwritten so the next run cold-starts again.
    pub new_head: Option<String>,
    /// Time in the union query; `None` if we cold-started before running it.
    pub scan_elapsed: Option<std::time::Duration>,
}

/// Spec for [`scan_buckets`].
pub struct DiffScanSpec<'a> {
    /// Bare entity-table names whose changes mean "render every bucket" —
    /// typically tables that appear in every rendered doc's header
    /// (`workspaces`, `users`, `channels`, `me`, …). Any non-`unchanged` row
    /// in one of these short-circuits the scan.
    pub global_fanout_tables: &'a [&'a str],
    /// SQL projecting bucket keys for changed rows, with `last_render_hash`
    /// bound at parameter index 1, the commit being scanned to at index 2,
    /// and the bucket key in column 0.
    ///
    /// By convention a `UNION` across `dolt_diff_<table>` vtabs with
    /// `WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'`.
    /// See any provider's `parse.rs`.
    ///
    /// **`?2`, never the literal `'HEAD'`.** `HEAD` is symbolic and resolves
    /// when the query runs, while `new_head` below was sampled a statement
    /// earlier — so against a store somebody is still committing to, the two
    /// name different commits and the caller ends up hunting for rows its pin
    /// does not have. Binding the hash makes the scan and the reads that
    /// follow it agree by construction.
    pub bucket_query: &'a str,
}

/// Of `bucket_ids`, the ones no row of any `(table, id_column)` pair still
/// carries — the buckets whose upstream entity is gone.
///
/// Asked of the raw store rather than inferred from what parse returned, and
/// the difference is the point: every provider's loader filters (`payload IS
/// NOT NULL` at minimum), so a bucket missing from a parse result may simply
/// be one we have not fetched the body of yet. Deleting on that reading would
/// destroy a live document. "The store has no row with this id" is the only
/// claim that means the entity went away.
///
/// Table and column names are interpolated; callers pass trusted identifiers.
pub async fn buckets_without_rows(
    pool: &sqlx::SqlitePool,
    reads: crate::pin::Reads<'_>,
    bucket_ids: &std::collections::HashSet<String>,
    id_columns: &[(&str, &str)],
) -> Result<Vec<String>> {
    if bucket_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut present: std::collections::HashSet<String> = std::collections::HashSet::new();
    let ids: Vec<&String> = bucket_ids.iter().collect();
    for (table, column) in id_columns {
        for chunk in ids.chunks(crate::bulk::SQL_CHUNK) {
            let table = reads.table(table);
            let mut sql = format!("SELECT DISTINCT {column} FROM {table} WHERE {column} IN (");
            crate::bulk::push_placeholder_list(&mut sql, chunk.len());
            sql.push(')');
            // Audited: `table` / `column` are `&'static str` at every
            // callsite (each provider names its own tables); the IN-list is a
            // placeholder run sized from the chunk and every id is bound.
            let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind((*id).clone());
            }
            match q.fetch_all(pool).await {
                Ok(found) => present.extend(found),
                Err(e) => {
                    // A table this provider does not have yet (an older
                    // store) must not read as "every bucket vanished".
                    tracing::warn!(
                        table,
                        error = %e,
                        "vanished-bucket probe failed; treating every bucket as present",
                    );
                    return Ok(Vec::new());
                }
            }
        }
    }
    let mut gone: Vec<String> = bucket_ids
        .iter()
        .filter(|b| !present.contains(*b))
        .cloned()
        .collect();
    gone.sort();
    Ok(gone)
}

/// Look up HEAD, short-circuit if any [`DiffScanSpec::global_fanout_tables`]
/// changed, then project the per-bucket changed set.
///
/// Any failure short of "no last hash" falls back to cold start:
/// render-everything is always safe, partial-render against a stale diff is
/// not.
/// Does this error mean the query named something the store does not have,
/// rather than that the *cursor* named a commit it does not have?
///
/// The distinction decides whether a failed scan is a bug to surface or a
/// stale cursor to cold-start past. Matching on the message is crude, but
/// sqlx surfaces both as a bare `Error::Database` and the text is the only
/// thing that separates them.
fn is_missing_schema(e: &sqlx::Error) -> bool {
    let msg = e.to_string();
    msg.contains("no such table") || msg.contains("no such column")
}

pub async fn scan_buckets(
    pool: &sqlx::SqlitePool,
    last_render_hash: Option<&str>,
    pin: &crate::pin::Pin,
    spec: &DiffScanSpec<'_>,
) -> Result<DiffScan> {
    // The caller pinned first and hands us the commit, rather than us sampling
    // HEAD here. That ordering is load-bearing twice over: the diff and the
    // content reads that follow it name one commit by construction, and the
    // `pinned_<table>` views already exist by the time `bucket_query` runs —
    // which it needs, because those queries join live tables against the diff.
    let to_ref = pin.commit().to_string();
    let new_head = Some(to_ref.clone());

    let Some(from_ref) = last_render_hash else {
        return Ok(DiffScan {
            changed_buckets: None,
            new_head,
            scan_elapsed: None,
        });
    };

    for table in spec.global_fanout_tables {
        let sql = format!(
            "SELECT 1 FROM dolt_diff_{table} \
              WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged' LIMIT 1"
        );
        // Audited: `table` comes from a static list; both refs are bound.
        let any: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(from_ref)
            .bind(&to_ref)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
        if any.is_some() {
            return Ok(DiffScan {
                changed_buckets: None,
                new_head,
                scan_elapsed: None,
            });
        }
    }

    let started = std::time::Instant::now();
    // Audited: `bucket_query` is a fixed projection from provider source;
    // both refs are bound, at parameter indexes 1 and 2.
    let res = sqlx::query(sqlx::AssertSqlSafe(spec.bucket_query))
        .bind(from_ref)
        .bind(&to_ref)
        .fetch_all(pool)
        .await;
    let elapsed = started.elapsed();
    // A failed scan used to cold-start unconditionally, logging at `info`
    // through `tracing` — which the tests that would have caught it do not
    // capture. That is the AGENTS.md hazard exactly: it *succeeded*,
    // re-rendering everything and reaching the right answer the slow way, so
    // a bucket query broken by a rename survived a whole review.
    //
    // But the two causes are not the same thing, and only one is a bug.
    //
    // A `from_ref` this store has never heard of — a cursor left by a reset,
    // a rebuild, a store replaced wholesale — is a real condition, and
    // re-reading everything is the correct answer to it.
    //
    // A query naming a table or column that does not exist is a bug in the
    // query. Absorbing it means every render silently does the most expensive
    // possible thing, forever, and nothing ever says why.
    let rows = match res {
        Ok(r) => r,
        Err(e) if is_missing_schema(&e) => {
            return Err(anyhow::Error::new(e).context(
                "dolt_diff bucket scan names a table or column this store does \
                 not have. That is a bug in the query, not a stale cursor: \
                 cold-starting past it would re-render everything on every run \
                 and never say why.",
            ))
        }
        Err(e) => {
            tracing::info!(
                error = %e,
                "dolt_diff scan could not use this cursor — cold-starting (render everything)"
            );
            return Ok(DiffScan {
                changed_buckets: None,
                new_head,
                scan_elapsed: Some(elapsed),
            });
        }
    };
    use sqlx::Row;
    let set: std::collections::HashSet<String> =
        rows.iter().map(|r| r.get::<String, _>(0)).collect();
    Ok(DiffScan {
        changed_buckets: Some(set),
        new_head,
        scan_elapsed: Some(elapsed),
    })
}

pub async fn record_object_error(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
    err: &str,
) -> Result<()> {
    record_object_attempt(tx, table, id, Some(err)).await
}

pub async fn failed_ids(pool: &SqlitePool, table: &str) -> Result<Vec<String>> {
    let sql = format!(
        "SELECT t.id FROM {table} t \
         LEFT JOIN {table}_bookkeeping b ON b.id = t.id \
         WHERE b.last_error IS NOT NULL \
            OR (t.payload IS NULL AND COALESCE(b.attempt_count, 0) > 0)"
    );
    // Audited: `table` interpolated as an identifier; no runtime values.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .with_context(|| format!("select failed_ids({table})"))?;
    Ok(rows
        .iter()
        .filter_map(|r| r.try_get::<String, _>("id").ok())
        .collect())
}

pub async fn load_payloads(
    pool: &SqlitePool,
    reads: crate::pin::Reads<'_>,
    table: &str,
) -> Result<Vec<Value>> {
    // `json(payload)` so we get text back whether the column holds a JSONB
    // blob or a JSON text literal.
    let table = reads.table(table);
    let sql = format!(
        "SELECT json(payload) AS payload FROM {table} WHERE payload IS NOT NULL ORDER BY id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .with_context(|| format!("select {table} payloads"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let payload: String = match r.try_get("payload") {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Ok(v) = serde_json::from_str::<Value>(&payload) {
            out.push(v);
        }
    }
    Ok(out)
}

pub async fn load_payloads_with_id(
    pool: &SqlitePool,
    reads: crate::pin::Reads<'_>,
    table: &str,
) -> Result<Vec<(String, Value)>> {
    let table = reads.table(table);
    let sql = format!(
        "SELECT id, json(payload) AS payload FROM {table} WHERE payload IS NOT NULL ORDER BY id"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .with_context(|| format!("select {table} id+payloads"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: String = match r.try_get("id") {
            Ok(s) => s,
            Err(_) => continue,
        };
        let payload: String = match r.try_get("payload") {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Ok(v) = serde_json::from_str::<Value>(&payload) {
            out.push((id, v));
        }
    }
    Ok(out)
}

// ── sync_scope_state ────────────────────────────────────────────────

pub async fn load_scope_state(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query("SELECT scope, last_seen_at FROM sync_scope_state")
        .fetch_all(pool)
        .await
        .context("select sync_scope_state")?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in rows {
        let scope: String = r.try_get("scope").unwrap_or_default();
        let ts: String = r.try_get("last_seen_at").unwrap_or_default();
        if !scope.is_empty() && !ts.is_empty() {
            out.insert(scope, ts);
        }
    }
    Ok(out)
}

pub async fn upsert_scope_state(pool: &SqlitePool, scope: &str, last_seen_at: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO sync_scope_state (scope, last_seen_at) VALUES (?, ?)
         ON CONFLICT(scope) DO UPDATE SET last_seen_at = excluded.last_seen_at",
    )
    .bind(scope)
    .bind(last_seen_at)
    .execute(pool)
    .await
    .with_context(|| format!("upsert sync_scope_state {scope}"))?;
    Ok(())
}

#[cfg(test)]
// Test diagnostics print under stock libsqlite3, where the probe is meant
// to fail.
#[allow(clippy::disallowed_macros)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    const WIDGETS_DDL: &str = "CREATE TABLE IF NOT EXISTS widgets (
            id TEXT PRIMARY KEY,
            name TEXT NULL,
            payload TEXT NULL
        )";

    fn test_ddl() -> Vec<String> {
        vec![WIDGETS_DDL.to_string(), bookkeeping_ddl_for("widgets")]
    }

    async fn plain_pool(p: &Path) -> SqlitePool {
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", p.display()))
                    .unwrap()
                    .create_if_missing(true),
            )
            .await
            .unwrap()
    }

    async fn open_test(p: &Path) -> SqlitePool {
        let owned = test_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        open(p, &slices).await.unwrap()
    }

    /// Two live connections to one store make each other's `dolt_commit`
    /// fail — the reason every caller must hold exactly one handle.
    ///
    /// `dolt_commit` takes the store's lock without waiting, and reports
    /// whoever else holds it as `commit conflict: another connection
    /// committed to this branch` whether or not that peer committed
    /// anything. Ordinary DML retries under a busy handler and so rides
    /// out the overlap; only the commit surfaces it. That asymmetry is
    /// why a second pool is a timing bug rather than an immediate one,
    /// and why in the field it reads as a CI flake.
    ///
    /// Both sides commit in lockstep so the contention is forced rather
    /// than hoped for. The round count is what makes a false pass
    /// impossible in practice; if this ever fails, doltlite has started
    /// waiting for the store lock, and the pool rules can be revisited.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_live_pools_on_one_store_break_each_others_commits() {
        const ROUNDS: usize = 64;

        let dir = tempdir().unwrap();
        let db = dir.path().join("entities.doltlite_db");
        let first = open_test(&db).await;
        if !has_dolt_extensions(&first).await {
            eprintln!("[two-pool test] stock libsqlite3 — nothing to contend over");
            first.close().await;
            return;
        }
        let owned = test_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let second = open(&db, &slices).await.expect("second open");

        let conflicts = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let mut writers = Vec::new();
        for (tag, pool) in [("a", first.clone()), ("b", second.clone())] {
            let conflicts = conflicts.clone();
            let gate = gate.clone();
            writers.push(tokio::spawn(async move {
                for round in 0..ROUNDS {
                    sqlx::query("INSERT OR REPLACE INTO widgets (id, name) VALUES (?, ?)")
                        .bind(format!("{tag}-{round}"))
                        .bind(tag)
                        .execute(&pool)
                        .await
                        .unwrap();
                    gate.wait().await;
                    if let Err(e) = commit_run(&pool, tag).await {
                        let msg = format!("{e:#}");
                        assert!(msg.contains("commit conflict"), "unexpected error: {msg}");
                        conflicts.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }));
        }
        for w in writers {
            w.await.unwrap();
        }

        second.close().await;
        first.close().await;
        assert!(
            conflicts.load(Ordering::Relaxed) > 0,
            "{ROUNDS} rounds of simultaneous commits from two pools on one \
             store produced no conflict. Either doltlite now waits for the \
             store lock instead of failing, or this test stopped contending; \
             find out which before deleting it."
        );
    }

    // ── Opening costs nothing ─────────────────────────────────────

    /// Re-opening a store nobody wrote to must not change one byte of it.
    ///
    /// Asserting on the byte size is the point: this leak left `dolt_log`
    /// unchanged, `dolt_status` clean and every step reporting no work, so
    /// every cheaper proxy was already true while it was live.
    #[tokio::test]
    async fn reopening_an_untouched_store_does_not_grow_it() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("entities.doltlite_db");

        // The first open creates the file and commits the schema, so it is the
        // one open that is supposed to write. Settle it before measuring.
        open_test(&db).await.close().await;
        open_test(&db).await.close().await;
        let settled = std::fs::metadata(&db).unwrap().len();

        for i in 0..3 {
            open_test(&db).await.close().await;
            let now = std::fs::metadata(&db).unwrap().len();
            assert_eq!(
                now,
                settled,
                "open #{} grew an untouched store by {} bytes",
                i + 3,
                now as i64 - settled as i64,
            );
        }
    }

    // ── HEAD as a content version ─────────────────────────────────

    /// The property every reported version rests on: the same data produces
    /// the same string. A version that moved every run would re-render
    /// forever; one that never moved would skip real work. Both fail silently.
    #[tokio::test]
    async fn head_commit_is_stable_across_a_no_op_run() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("entities.doltlite_db");

        let pool = open_test(&path).await;
        sqlx::query("INSERT INTO widgets (id, payload) VALUES ('a', '{}')")
            .execute(&pool)
            .await
            .unwrap();
        commit_run(&pool, "first").await.unwrap();
        let v1 = head_commit(&pool).await.unwrap().expect("doltlite HEAD");
        pool.close().await;

        // A wave that pulls nothing new: commit_run finds a clean tree and
        // returns None, but HEAD — and so the version — holds.
        let pool = open_test(&path).await;
        assert!(commit_run(&pool, "second").await.unwrap().is_none());
        let v2 = head_commit(&pool).await.unwrap().expect("doltlite HEAD");
        pool.close().await;
        assert_eq!(
            v1, v2,
            "an unchanged store must report an unchanged version"
        );

        // Real new data moves it.
        let pool = open_test(&path).await;
        sqlx::query("INSERT INTO widgets (id, payload) VALUES ('b', '{}')")
            .execute(&pool)
            .await
            .unwrap();
        commit_run(&pool, "third").await.unwrap();
        let v3 = head_commit(&pool).await.unwrap().unwrap();
        pool.close().await;
        assert_ne!(v1, v3, "new rows must move the version");
    }

    /// Reading a version must not write. `open` provisions shared DDL and
    /// commits, so using it here would advance the very HEAD being read — one
    /// spurious full re-render per source on first upgrade.
    #[tokio::test]
    async fn head_commit_at_path_does_not_touch_the_store() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("blobs.doltlite_db");

        // As `BlobCas::open` builds it: cas_objects only.
        let pool = plain_pool(&path).await;
        sqlx::query(crate::blob_cas::CAS_OBJECTS_DDL)
            .execute(&pool)
            .await
            .unwrap();
        commit_run(&pool, "cas init").await.unwrap();
        let before = head_commit(&pool).await.unwrap();
        pool.close().await;
        assert!(before.is_some(), "fixture must have a HEAD to compare");

        let read = head_commit_at_path(&path).await.unwrap();
        assert_eq!(read, before, "the read must report HEAD as it stands");

        // A plain connection — `open` would provision the tables we are
        // checking for.
        let pool = plain_pool(&path).await;
        let after = head_commit(&pool).await.unwrap();
        let tables: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(&pool)
                .await
                .unwrap();
        pool.close().await;
        assert_eq!(after, before, "reading a version must not advance HEAD");
        assert!(
            !tables.iter().any(|t| t == "sync_runs"),
            "reading a version must not provision write-path tables: {tables:?}"
        );
    }

    /// A store nobody has downloaded yet has no version to report.
    #[tokio::test]
    async fn head_commit_at_path_is_none_for_a_missing_store() {
        let td = tempfile::tempdir().unwrap();
        let missing = td.path().join("nope.doltlite_db");
        assert!(head_commit_at_path(&missing).await.unwrap().is_none());
    }

    // ── Volatile split / overlay ──────────────────────────────────

    #[test]
    fn split_overlay_roundtrip_lossless() {
        // A real-shaped Slack channel payload: a top-level volatile field, a
        // legitimate null, a nested object and an array — all must round-trip.
        let payload = json!({
            "id": "C011QT8HGAC",
            "name": "dashboard",
            "parent_conversation": null,
            "updated": 1724742699826i64,
            "topic": { "creator": "U1", "last_set": 0, "value": "" },
            "previous_names": [],
            "shared_team_ids": ["TSTHRQ7MY"],
        });
        // One top-level path and one nested, to exercise both.
        let paths: &[VolatilePath] = &[&["updated"], &["topic", "last_set"]];

        let (base, volatile) = split_volatile(&payload, paths);

        assert!(base.get("updated").is_none());
        assert!(base["topic"].get("last_set").is_none());

        assert!(base.get("parent_conversation").unwrap().is_null());
        assert_eq!(base["topic"]["value"], json!(""));

        let volatile = volatile.expect("volatile fields present");
        assert_eq!(volatile["updated"], json!(1724742699826i64));
        assert_eq!(volatile["topic"]["last_set"], json!(0));

        // Lossless: overlaying reconstructs the exact wire payload.
        assert_eq!(overlay(&base, &volatile), payload);
    }

    #[test]
    fn split_volatile_absent_paths_is_noop() {
        let payload = json!({ "id": "C1", "name": "x" });
        let (base, volatile) = split_volatile(&payload, &[&["updated"], &["topic", "last_set"]]);
        assert_eq!(base, payload);
        assert!(volatile.is_none());
    }

    #[test]
    fn overlay_treats_null_as_a_value_not_a_delete() {
        // Unlike RFC 7386 merge-patch, a null sets the key to null rather than
        // removing it.
        let base = json!({ "a": 1, "b": 2 });
        let volatile = json!({ "b": null });
        assert_eq!(overlay(&base, &volatile), json!({ "a": 1, "b": null }));
    }

    #[test]
    fn overlay_of_none_split_is_identity() {
        // When nothing was volatile, base IS the payload.
        let payload = json!({ "id": "C1", "deep": { "k": [1, 2, 3] } });
        let (base, volatile) = split_volatile(&payload, &[&["nope"]]);
        assert!(volatile.is_none());
        assert_eq!(base, payload);
    }

    #[tokio::test]
    async fn volatile_payload_roundtrips_through_sidecar() {
        // End to end through the DB: split, store the halves, read both back,
        // and overlay to get the original payload.
        let d = tempdir().unwrap();
        let p = d.path().join("v.doltlite_db");
        let pool = open_test(&p).await;

        let full = json!({ "id": "w1", "name": "gadget", "updated": 123456789i64 });
        let (base, volatile) = split_volatile(&full, &[&["updated"]]);
        let volatile = volatile.expect("updated is volatile");

        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO widgets (id, name, payload) VALUES ('w1', 'gadget', jsonb(?))")
            .bind(serde_json::to_string(&base).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO widgets_bookkeeping (id, attempt_count) VALUES ('w1', 1)")
            .execute(&mut *tx)
            .await
            .unwrap();
        set_volatile_payloads_in_tx(&mut tx, "widgets", &[("w1", &volatile)])
            .await
            .unwrap();
        tx.commit().await.unwrap();

        let base_text: String =
            sqlx::query_scalar("SELECT json(payload) FROM widgets WHERE id = 'w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let vol_text: String = sqlx::query_scalar(
            "SELECT json(volatile_payload) FROM widgets_bookkeeping WHERE id = 'w1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let base_v: Value = serde_json::from_str(&base_text).unwrap();
        let vol_v: Value = serde_json::from_str(&vol_text).unwrap();

        // The content table no longer carries the volatile field...
        assert!(base_v.get("updated").is_none());
        // ...but overlaying the sidecar reconstructs the wire payload.
        assert_eq!(overlay(&base_v, &vol_v), full);
    }

    // ── Schema self-healing (reconcile_table_schema) ──────────────

    #[test]
    fn parse_create_table_name_cases() {
        assert_eq!(
            parse_create_table_name("CREATE TABLE IF NOT EXISTS foo (id TEXT)").as_deref(),
            Some("foo")
        );
        assert_eq!(
            parse_create_table_name("CREATE TABLE bar(x INT)").as_deref(),
            Some("bar")
        );
        assert_eq!(
            parse_create_table_name("create table if not exists \"baz\" (id TEXT)").as_deref(),
            Some("baz")
        );
        // Not a CREATE TABLE → no columns to reconcile.
        assert_eq!(parse_create_table_name("CREATE INDEX i ON foo(x)"), None);
        assert_eq!(parse_create_table_name("SELECT 1"), None);
    }

    #[tokio::test]
    async fn open_adds_missing_column_to_existing_db() {
        // A DB created under an older bookkeeping schema (no
        // `volatile_payload`), reopened with the current DDL.
        let d = tempdir().unwrap();
        let p = d.path().join("migrate.doltlite_db");
        let old_bk = "CREATE TABLE IF NOT EXISTS widgets_bookkeeping (
            id TEXT PRIMARY KEY,
            fetched_at TEXT NULL,
            attempt_count INTEGER NOT NULL,
            last_attempt_at TEXT NULL,
            last_error TEXT NULL
        )";
        {
            let pool = open(&p, &[WIDGETS_DDL, old_bk]).await.unwrap();
            sqlx::query("INSERT INTO widgets_bookkeeping (id, attempt_count) VALUES ('w1', 3)")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        let pool = open(&p, &[WIDGETS_DDL, &bookkeeping_ddl_for("widgets")])
            .await
            .unwrap();
        let cols = table_columns(&pool, "widgets_bookkeeping").await.unwrap();
        assert!(
            cols.iter().any(|c| c.name == "volatile_payload"),
            "volatile_payload should have been ADDed"
        );
        // Pre-existing row survived → ALTER ADD, not a recreate.
        let n: i64 =
            sqlx::query_scalar("SELECT attempt_count FROM widgets_bookkeeping WHERE id = 'w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 3);

        sqlx::query(
            "UPDATE widgets_bookkeeping SET volatile_payload = jsonb('{\"updated\":1}') WHERE id = 'w1'",
        )
        .execute(&pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn open_adds_column_and_its_index_together() {
        // The shape every column-adding schema change takes: a new column AND
        // an index over it, landing on a store that predates both. The index
        // must wait for the reconcile, or `open` dies with "no such column"
        // before it can self-heal.
        let d = tempdir().unwrap();
        let p = d.path().join("col_and_index.doltlite_db");
        {
            let pool = open(&p, &[WIDGETS_DDL]).await.unwrap();
            sqlx::query("INSERT INTO widgets (id, name) VALUES ('w1', 'gadget')")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        const NEW_WIDGETS_DDL: &str = "CREATE TABLE IF NOT EXISTS widgets (
            id TEXT PRIMARY KEY,
            name TEXT NULL,
            payload TEXT NULL,
            tag TEXT NULL
        )";
        const NEW_INDEX: &str = "CREATE INDEX IF NOT EXISTS idx_widgets_tag ON widgets(tag)";
        let pool = open(&p, &[NEW_WIDGETS_DDL, NEW_INDEX])
            .await
            .expect("open must self-heal a column that a new index covers");

        let cols = table_columns(&pool, "widgets").await.unwrap();
        assert!(
            cols.iter().any(|c| c.name == "tag"),
            "tag should have been ADDed"
        );
        // ADD COLUMN, not a recreate: the pre-existing row survived.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM widgets WHERE id = 'w1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        // The index exists — the second DDL pass ran.
        let idx: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_widgets_tag'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(idx, 1, "idx_widgets_tag should have been created");
    }

    #[tokio::test]
    async fn open_drops_and_recreates_on_removed_column() {
        // A column the current DDL no longer declares can't be reconciled by
        // ADD; it must drop+recreate.
        let d = tempdir().unwrap();
        let p = d.path().join("recreate.doltlite_db");
        let stale = "CREATE TABLE IF NOT EXISTS widgets (
            id TEXT PRIMARY KEY,
            name TEXT NULL,
            payload TEXT NULL,
            legacy_col TEXT NULL
        )";
        {
            let pool = open(&p, &[stale]).await.unwrap();
            sqlx::query("INSERT INTO widgets (id, legacy_col) VALUES ('w1', 'x')")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        let pool = open(&p, &[WIDGETS_DDL]).await.unwrap();
        let cols = table_columns(&pool, "widgets").await.unwrap();
        assert!(
            !cols.iter().any(|c| c.name == "legacy_col"),
            "legacy_col should be gone after drop+recreate"
        );
        // Recreate wipes rows — acceptable for a raw store, which re-fetches.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM widgets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn open_creates_tables_idempotently() {
        let d = tempdir().unwrap();
        let p = d.path().join("x.doltlite_db");
        let _ = open_test(&p).await;
        // Re-opening doesn't error, and the shared tables exist.
        let pool = open_test(&p).await;
        sqlx::query("SELECT COUNT(*) FROM sync_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn db_path_for_places_db_inside_dir() {
        let p = Path::new("/tmp/raw/whatever");
        assert_eq!(
            db_path_for(p),
            PathBuf::from("/tmp/raw/whatever/entities.doltlite_db")
        );
        let q = Path::new("/tmp/raw/whatever/entities.doltlite_db");
        assert_eq!(db_path_for(q), q);
    }

    /// `commit_run` returns `Ok(None)` rather than failing on stock
    /// libsqlite3, and under bazel exercises the real path: a real hash, in
    /// `dolt_log`, with the message we passed.
    ///
    /// Prints which libsqlite3 is linked, to catch "we thought we were on
    /// doltlite" builds.
    #[tokio::test]
    async fn diagnostic_print_sqlite_identity() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("probe.doltlite_db")).await;
        let ver: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let src: String = sqlx::query_scalar("SELECT sqlite_source_id()")
            .fetch_one(&pool)
            .await
            .unwrap();
        let scalar_count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pragma_function_list WHERE name LIKE 'dolt_%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // Also call `dolt_commit` directly — eponymous functions don't always
        // appear in pragma_function_list.
        let direct_call = sqlx::query("SELECT dolt_commit('-Am', 'probe')")
            .execute(&pool)
            .await;
        eprintln!(
            "[sqlite probe] version={ver} source_id={src} dolt_funcs_in_pragma={scalar_count} direct_dolt_commit_ok={}",
            direct_call.is_ok(),
        );
        if let Err(e) = direct_call {
            eprintln!("[sqlite probe] direct_call error: {e}");
        }
    }

    #[tokio::test]
    async fn commit_run_returns_hash_and_dolt_log_entry_or_skips() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("commit.doltlite_db")).await;

        if !has_dolt_extensions(&pool).await {
            // Stock SQLite: commit_run returns None without error, and there
            // is no dolt_log to inspect.
            let hash = commit_run(&pool, "stock-sqlite probe")
                .await
                .expect("commit_run ok");
            assert!(
                hash.is_none(),
                "expected None on stock SQLite, got {hash:?}"
            );
            eprintln!("[commit_run test] stock libsqlite3 — dolt_log not asserted");
            return;
        }

        // Per-session committer identity, so dolt_commit doesn't error on a
        // missing user.email.
        sqlx::query("SELECT dolt_config('user.name', 'datalib-test')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("SELECT dolt_config('user.email', 'test@datalib.local')")
            .execute(&pool)
            .await
            .unwrap();

        // Give dolt something to record.
        sqlx::query("INSERT INTO widgets (id, name) VALUES ('w1', 'first')")
            .execute(&pool)
            .await
            .unwrap();

        let msg = "test commit: rows=1";
        let hash = commit_run(&pool, msg)
            .await
            .expect("commit_run ok")
            .expect("doltlite linked but commit_run returned None");
        assert!(!hash.is_empty(), "doltlite returned empty commit hash");

        // The returned hash must appear in dolt_log with our message —
        // confirms the version-control SQL surface is really live, not just
        // that the function exists.
        let logged_msg: String =
            sqlx::query_scalar("SELECT message FROM dolt_log() WHERE commit_hash = ? LIMIT 1")
                .bind(&hash)
                .fetch_one(&pool)
                .await
                .expect("dolt_log lookup");
        assert_eq!(logged_msg, msg, "dolt_log message mismatch");
    }

    /// The per-source download commit path end to end: stage a row and drop
    /// the pool as a download does, then reopen through `commit_run_at_path`
    /// as the step does, and check the commit lands in `dolt_log`.
    ///
    /// Also covers the no-op for a path that was never created —
    /// `interrupt_commit_all` walks every enabled source, and some have no
    /// file yet.
    #[tokio::test]
    async fn commit_run_at_path_persists_across_pool_lifetimes() {
        let d = tempdir().unwrap();
        let db = d.path().join("source.doltlite_db");

        // Phase 1: simulate a download — open, write, close.
        {
            let pool = open_test(&db).await;
            if !has_dolt_extensions(&pool).await {
                eprintln!("[commit_run_at_path test] stock libsqlite3 — full assertion skipped");
                // The no-op-on-missing-file path shouldn't depend on doltlite.
                let missing = d.path().join("never_created.doltlite_db");
                let hash = commit_run_at_path(&missing, "ignored")
                    .await
                    .expect("missing-path open should succeed");
                assert!(hash.is_none(), "expected None on missing path");
                return;
            }
            // Per-session committer identity (doltlite requires this).
            sqlx::query("SELECT dolt_config('user.name', 'datalib-download-test')")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("SELECT dolt_config('user.email', 'download@datalib.local')")
                .execute(&pool)
                .await
                .unwrap();

            let run_id = start_run(&pool, &json!({"source": "test"})).await.unwrap();
            sqlx::query("INSERT INTO widgets (id, name) VALUES ('w-download', 'staged')")
                .execute(&pool)
                .await
                .unwrap();
            finish_run(&pool, run_id, "ok", &json!({"rows": 1}))
                .await
                .unwrap();
            pool.close().await;
        }

        // Phase 2: the orchestrator's hook. `open`'s rescue commit has already
        // sealed phase 1's orphaned writes, so this trailing commit finds
        // nothing dirty and returns None — the documented post-condition.
        let msg = "download source: rows=1 commit_run_at_path test";
        let trailing = commit_run_at_path(&db, msg)
            .await
            .expect("commit_run_at_path ok");
        assert!(
            trailing.is_none(),
            "trailing commit should be a no-op after rescue swept the orphaned writes; got {trailing:?}"
        );

        // Reopening a third time proves the orphaned writes were sealed by the
        // rescue at phase 2's open, not lost.
        let verify = open_test(&db).await;
        let logged: Vec<String> =
            sqlx::query_scalar("SELECT message FROM dolt_log() ORDER BY date DESC")
                .fetch_all(&verify)
                .await
                .expect("dolt_log lookup after reopen");
        assert!(
            logged.iter().any(|m| m.starts_with("rescue: ")),
            "expected a rescue commit in dolt_log; got {logged:?}"
        );

        // Pointing at a never-created file must neither create one nor error.
        let missing = d.path().join("never_created.doltlite_db");
        let h2 = commit_run_at_path(&missing, "ignored")
            .await
            .expect("missing-path open should succeed");
        assert!(h2.is_none(), "expected None on missing path");
        assert!(
            !missing.exists(),
            "missing-path call must not create the file"
        );
    }

    #[tokio::test]
    async fn run_lifecycle() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("y.doltlite_db")).await;
        let id = start_run(&pool, &json!({"x": 1})).await.unwrap();
        finish_run(&pool, id, "ok", &json!({"done": true}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn error_and_retry_flow() {
        let d = tempdir().unwrap();
        let pool = open_test(&d.path().join("z.doltlite_db")).await;
        // Pre-seed the row + sidecar, then record two failures in their own
        // transactions.
        {
            let mut tx = pool.begin().await.unwrap();
            ensure_object_row(&mut tx, "widgets", "w1").await.unwrap();
            tx.commit().await.unwrap();
        }
        for err in ["boom", "boom2"] {
            let mut tx = pool.begin().await.unwrap();
            record_object_error(&mut tx, "widgets", "w1", err)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let failed = failed_ids(&pool, "widgets").await.unwrap();
        assert_eq!(failed, vec!["w1".to_string()]);

        let attempts: i64 =
            sqlx::query_scalar("SELECT attempt_count FROM widgets_bookkeeping WHERE id = 'w1'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 2);
    }

    /// Regression guard for "pool size 1 against doltlite" (see the README).
    ///
    /// At `max_connections=1` a `dolt_commit` followed by `dolt_log()` must
    /// produce a consistent view; that is asserted. At 2 and 4 we only run the
    /// path and print what happens, so a future doltlite upgrade can be
    /// compared against the historical shape — asserting there would codify a
    /// bug as a requirement.
    ///
    /// Skips out on stock libsqlite3.
    #[tokio::test]
    async fn dolt_log_visibility_across_pool_sizes() {
        for max_conns in [1u32, 2, 4] {
            let d = tempdir().unwrap();
            let db_path = d.path().join(format!("probe_{max_conns}.doltlite_db"));
            // Apply DDL through the normal open() for the canonical shape,
            // then re-open with a tunable pool size.
            let _ = open_test(&db_path).await;

            let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
                .unwrap()
                .create_if_missing(true);
            let pool = SqlitePoolOptions::new()
                .max_connections(max_conns)
                .connect_with(opts)
                .await
                .unwrap();

            if !has_dolt_extensions(&pool).await {
                eprintln!("[pool_probe] stock libsqlite3 — skipping (max_conns={max_conns})");
                continue;
            }

            sqlx::query("SELECT dolt_config('user.name', 'pool-probe')")
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("SELECT dolt_config('user.email', 'pool-probe@x')")
                .execute(&pool)
                .await
                .unwrap();

            // Return the sqlx error rather than panicking, so the failure mode
            // at each pool size is observable.
            let try_exec = |sql: &'static str| {
                let pool = pool.clone();
                async move {
                    sqlx::query(sql)
                        .execute(&pool)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                }
            };

            let mut errs: Vec<String> = Vec::new();
            for sql in [
                "INSERT INTO widgets (id, name, payload) VALUES ('w1', 'one', NULL)",
                "INSERT INTO widgets_bookkeeping (id, fetched_at, attempt_count) VALUES ('w1', '2026-06-03T00:00:00Z', 0)",
            ] {
                if let Err(e) = try_exec(sql).await {
                    errs.push(format!("setup `{sql}`: {e}"));
                }
            }

            let h1: Result<Option<String>, String> =
                sqlx::query_scalar("SELECT dolt_commit('-Am', 'pool-probe-first')")
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| e.to_string());

            // Delete + reinsert IDENTICAL data, plus a new fetched_at — the
            // integration-test shape.
            for sql in [
                "DELETE FROM widgets",
                "DELETE FROM widgets_bookkeeping",
                "INSERT INTO widgets (id, name, payload) VALUES ('w1', 'one', NULL)",
                "INSERT INTO widgets_bookkeeping (id, fetched_at, attempt_count) VALUES ('w1', '2026-06-03T00:00:05Z', 0)",
            ] {
                if let Err(e) = try_exec(sql).await {
                    errs.push(format!("reset `{sql}`: {e}"));
                }
            }

            // Second commit — the call that errored at max_conns>=2.
            let h2: Result<Option<String>, String> =
                sqlx::query_scalar("SELECT dolt_commit('-Am', 'pool-probe-second')")
                    .fetch_optional(&pool)
                    .await
                    .map_err(|e| e.to_string());

            let messages: Result<Vec<String>, String> =
                sqlx::query_scalar("SELECT message FROM dolt_log() ORDER BY date ASC")
                    .fetch_all(&pool)
                    .await
                    .map_err(|e| e.to_string());

            eprintln!(
                "[pool_probe max_conns={max_conns}]\n  \
                 setup_errors={errs:?}\n  \
                 h1={h1:?}\n  \
                 h2={h2:?}\n  \
                 messages={messages:?}"
            );

            // Regression guard only for the supported configuration.
            if max_conns == 1 {
                assert!(
                    errs.is_empty(),
                    "max_conns=1: no setup errors should fire; got {errs:?}"
                );
                let h1 = h1
                    .clone()
                    .expect("max_conns=1: first dolt_commit should not error")
                    .expect("max_conns=1: first dolt_commit should return a hash");
                let h2 = h2
                    .clone()
                    .expect("max_conns=1: second dolt_commit should not error")
                    .expect("max_conns=1: second dolt_commit should return a hash");
                assert_ne!(
                    h1, h2,
                    "max_conns=1: second commit hash should differ from first"
                );
                let msgs = messages
                    .clone()
                    .expect("max_conns=1: dolt_log read should succeed");
                assert!(
                    msgs.iter().any(|m| m == "pool-probe-first"),
                    "max_conns=1: first commit message missing from dolt_log: {msgs:?}"
                );
                assert!(
                    msgs.iter().any(|m| m == "pool-probe-second"),
                    "max_conns=1: second commit message missing from dolt_log: {msgs:?}"
                );
            }
            // Deliberately no assertion at 2 and 4 — the eprintln above just
            // records whatever doltlite does.

            pool.close().await;
        }
    }

    /// `open` must leave its single connection alone for the life of the
    /// pool. sqlx's stock `idle_timeout` / `max_lifetime` would retire it, and
    /// a replacement starts on `main` — rows would quietly land on the wrong
    /// branch with no error.
    #[tokio::test]
    async fn open_disables_connection_recycling() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = open(&tmp.path().join("recycle.doltlite_db"), &[])
            .await
            .unwrap();
        let opts = pool.options();
        assert_eq!(
            opts.get_max_connections(),
            1,
            "doltlite session state is per-connection; the pool must hold exactly one"
        );
        assert_eq!(
            opts.get_idle_timeout(),
            None,
            "an idle timeout would retire the connection carrying the active branch"
        );
        assert_eq!(
            opts.get_max_lifetime(),
            None,
            "a max lifetime would retire the connection carrying the active branch"
        );
        pool.close().await;
    }

    /// What the setting above defends against: a *different* connection to
    /// the same file does not inherit the active branch.
    /// A bucket query naming a table that does not exist must fail, where a
    /// cursor naming a commit that does not exist must cold-start. Both used
    /// to cold-start, which is how a query broken by a table rename rendered
    /// everything on every run and said so only through a `tracing` line no
    /// test captured.
    #[tokio::test]
    async fn a_broken_bucket_query_fails_where_a_stale_cursor_cold_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = open(
            &tmp.path().join("scan.doltlite_db"),
            &["CREATE TABLE IF NOT EXISTS notes (id TEXT PRIMARY KEY, bucket TEXT)"],
        )
        .await
        .unwrap();
        if !has_dolt_extensions(&pool).await {
            return;
        }
        sqlx::query("INSERT INTO notes VALUES ('n1', 'b1')")
            .execute(&pool)
            .await
            .unwrap();
        let commit = commit_run(&pool, "one note").await.unwrap().unwrap();
        let pin = crate::pin::Pin::at(&commit).unwrap();

        let good = DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "SELECT DISTINCT to_bucket FROM dolt_diff_notes                            WHERE from_ref = ?1 AND to_ref = ?2 AND to_bucket IS NOT NULL",
        };
        // A cursor this store never had: a reset, a rebuild, a replaced store.
        let stale = "0000000000000000000000000000000000000000";
        let scan = scan_buckets(&pool, Some(stale), &pin, &good)
            .await
            .expect("a stale cursor cold-starts rather than failing");
        assert!(
            scan.changed_buckets.is_none(),
            "cold start means `None`, i.e. render everything"
        );

        let broken = DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "SELECT DISTINCT to_bucket FROM dolt_diff_notes_renamed_away                            WHERE from_ref = ?1 AND to_ref = ?2",
        };
        let err = scan_buckets(&pool, Some(&commit), &pin, &broken)
            .await
            .expect_err("a query naming a table that is not there must fail");
        assert!(
            format!("{err:#}").contains("bug in the query"),
            "the error should say it is a query bug, got: {err:#}"
        );
        pool.close().await;
    }

    /// `open_reader` is read-only at the engine, not merely by convention: a
    /// write through it fails rather than landing in a file the caller does
    /// not own. The pinned views still install, because they live in the
    /// per-connection temp schema rather than in the file.
    #[tokio::test]
    async fn a_reader_cannot_write_but_can_still_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ro.doltlite_db");
        let owner = open(
            &path,
            &["CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)"],
        )
        .await
        .unwrap();
        if !has_dolt_extensions(&owner).await {
            return;
        }
        sqlx::query("INSERT INTO t VALUES (1)")
            .execute(&owner)
            .await
            .unwrap();
        let commit = commit_run(&owner, "one row").await.unwrap().unwrap();
        owner.close().await;

        let reader = open_reader(&path).await.unwrap();
        let err = sqlx::query("INSERT INTO t VALUES (2)")
            .execute(&reader)
            .await
            .expect_err("a reader must not be able to write the store");
        assert!(
            err.to_string().contains("readonly"),
            "expected a readonly-database error, got: {err}"
        );

        crate::pin::install_views(&reader, &crate::pin::Pin::at(&commit).unwrap())
            .await
            .expect("temp views install on a read-only connection");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pinned_t")
            .fetch_one(&reader)
            .await
            .unwrap();
        assert_eq!(n, 1, "and the pinned read works through them");
        reader.close().await;
    }

    /// Opening a store *commits* whatever it finds dirty. Harmless when the
    /// prior writer is gone — that is what the rescue is for — and a hazard
    /// the moment a writer is still running: the reader's own open seals the
    /// producer's half-written batch into a commit, which is both a write by
    /// a reader and a way for torn rows to become legitimately committed.
    #[tokio::test]
    async fn opening_a_store_commits_whatever_was_left_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("rescue.doltlite_db");
        let a = open(
            &path,
            &["CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)"],
        )
        .await
        .unwrap();
        if !has_dolt_extensions(&a).await {
            return;
        }
        commit_run(&a, "baseline").await.unwrap();
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
            .fetch_one(&a)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (1)")
            .execute(&a)
            .await
            .unwrap();
        a.close().await;

        let b = open(&path, &[]).await.unwrap();
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
            .fetch_one(&b)
            .await
            .unwrap();
        assert_eq!(
            after,
            before + 1,
            "open() sealed the dirty row into a rescue commit"
        );
        let at_head: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_at_t('HEAD')")
            .fetch_one(&b)
            .await
            .unwrap();
        assert_eq!(at_head, 1, "and the row is now committed state");
        b.close().await;
    }

    #[tokio::test]
    async fn a_fresh_connection_starts_on_main() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("branch.doltlite_db");

        let first = open(
            &path,
            &["CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap();
        sqlx::query("SELECT dolt_checkout('-b', ?)")
            .bind("elsewhere")
            .execute(&first)
            .await
            .unwrap();
        let active: String = sqlx::query_scalar("SELECT active_branch()")
            .fetch_one(&first)
            .await
            .unwrap();
        assert_eq!(active, "elsewhere");
        first.close().await;

        let second = open(&path, &[]).await.unwrap();
        let active: String = sqlx::query_scalar("SELECT active_branch()")
            .fetch_one(&second)
            .await
            .unwrap();
        assert_eq!(
            active, "main",
            "a new connection inherited the previous one's branch; if doltlite \
             ever makes the active branch a property of the file, the \
             recycling guard in `open` can be revisited"
        );
        second.close().await;
    }
}
