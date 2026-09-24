//! Shared machinery for the doltlite-backed raw stores every provider
//! writes: opening a store, the DDL every store gets for free, per-row
//! bookkeeping, and the `dolt_diff` scan that drives incremental render.
//!
//! Provider crates describe only their own object tables and upserts.
//!
//! The rules you need before changing anything here — primary keys,
//! bookkeeping sidecars, volatile fields, JSONB, why pools are size 1, why
//! DDL runs in two passes — are in `datalib/backend/etl/README.md`. Before
//! changing a `schema_raw.rs` struct or [`open`], read its §"Schema
//! self-healing" and §"The migration ladder": a change `ADD COLUMN`
//! cannot absorb is refused at open until a rung handles it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use datalib_flock::{FileLock, LockError};
pub use datalib_store_meta::{Migration, StoreKind};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

// Constants so every provider agrees and a rename has one search target.
// Only the DDL fragments below use them; provider SQL spells them inline.
pub const COL_ID: &str = "id";
pub const COL_PAYLOAD: &str = "payload";
pub const COL_FETCHED_AT: &str = "fetched_at_utc";
pub const COL_ATTEMPT_COUNT: &str = "attempt_count";
pub const COL_LAST_ATTEMPT_AT: &str = "last_attempt_at_utc";
pub const COL_LAST_ERROR: &str = "last_error";
pub const COL_TZ_OFFSET: &str = "tz_offset";

pub fn bookkeeping_ddl_for(table: &str) -> String {
    // No `DEFAULT` on any column here; writers bind every value
    // explicitly. The stamps are UTC; `tz_offset` is the offset the
    // writer's clock was in when it made the latest of them.
    format!(
        "CREATE TABLE IF NOT EXISTS {table}_bookkeeping (
            id TEXT PRIMARY KEY,
            fetched_at_utc TEXT NULL,
            attempt_count INTEGER NOT NULL,
            last_attempt_at_utc TEXT NULL,
            last_error TEXT NULL,
            volatile_payload TEXT NULL,
            tz_offset TEXT NULL
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
    started_at_utc TEXT NOT NULL,
    finished_at_utc TEXT NULL,
    tz_offset TEXT NULL,
    config TEXT NOT NULL,
    status TEXT NOT NULL,
    summary TEXT NULL
)";

/// Per-scope incremental-sync cursor, for providers (github, gitlab) whose
/// discovery is keyed by a search scope. `last_seen_at_utc` is a
/// provider-chosen timestamp, stored in UTC and compared back against the
/// configured refresh window when the next run picks its `since` floor.
pub const SYNC_SCOPE_STATE_DDL: &str = "CREATE TABLE IF NOT EXISTS sync_scope_state (
    scope TEXT PRIMARY KEY,
    last_seen_at_utc TEXT NOT NULL,
    tz_offset TEXT NULL
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
    updated_at_utc TEXT NOT NULL,
    tz_offset TEXT NULL
)";

/// DDL every provider gets for free, appended inside [`open`].
/// The raw store's `problems`: what a download could not do with one
/// record, keyed `<table>:<id>` under the entity scope. `source_id` is
/// left empty here — a download does not know its group's id — and the
/// render step, which does, mints the rows again with it as it carries
/// them into its own store. This copy is staging; nothing reads it but
/// render.
pub const PROBLEMS_DDL: &str = datalib_problems::DDL[0].1;

pub const SHARED_DDL: &[&str] = &[
    SYNC_RUNS_DDL,
    SYNC_SCOPE_STATE_DDL,
    SYNC_SCOPE_CONFIG_DDL,
    PROBLEMS_DDL,
];

/// The tables every raw store has that are datalib's, not the
/// source's: what a mirror must leave alone and a content diff must
/// skip. [`SHARED_DDL`]'s tables plus `_datalib_meta`, which every
/// store gets whether or not it is a raw one; a test keeps the list in
/// step with both.
pub const SHARED_TABLES: &[&str] = &[
    datalib_store_meta::TABLE,
    "sync_runs",
    "sync_scope_state",
    "sync_scope_config",
    "problems",
];

// ── Path helper ─────────────────────────────────────────────────────

pub fn db_path_for(p: &Path) -> PathBuf {
    if p.extension().and_then(|s| s.to_str()) == Some("doltlite_db") {
        return p.to_path_buf();
    }
    crate::raw_layout::entities_db(p)
}

// ── Open ────────────────────────────────────────────────────────────

/// The file a writer's claim on `db_path` lives in: a sibling, so the
/// store itself — which doltlite `flock`s for its own chunk-store lock —
/// is not what we lock. The name is what `datalib_core::disk` skips when
/// it measures a tree.
pub fn lock_path_for(db_path: &Path) -> PathBuf {
    let mut name = db_path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    db_path.with_file_name(name)
}

/// The branch every datalib writer works on.
///
/// `main` is what a reader reads, and a writer never touches it
/// directly: it works here, and fast-forwards `main` at each seal
/// ([`commit_run`]). Everything between two seals is invisible to a
/// reader until that moment — the rows, and the schema reconcile that
/// creates the tables, which is the half a pinned read could not
/// protect itself from (`pin.rs` reads the *working set's*
/// `sqlite_master`, not the pin's).
///
/// The name is also the signal. `fsindex` keeps one branch per scan root
/// and publishes none of them, so [`publish_to_main`] fires only for a
/// connection sitting on this exact branch and leaves every other one
/// alone.
pub const WRITER_BRANCH: &str = "datalib_writer";

/// Whether an error is "this build has no doltlite", rather than
/// anything about the store.
fn is_missing_function(e: &sqlx::Error) -> bool {
    e.to_string().contains("no such function")
}

/// Put a writer's connection on [`WRITER_BRANCH`], creating it the one
/// time it is not there yet.
///
/// In `after_connect` rather than once at open, because sqlx replaces a
/// connection that breaks and a replacement would start on the file's
/// default branch — where a writer's half-finished work would be
/// visible to every reader.
///
/// **`dolt_connect_branch`, not `dolt_checkout`.** Both leave the
/// session on the branch, but `dolt_checkout` persists a working set
/// for the branch it leaves and the one it enters, which costs a few
/// hundred bytes on *every* open — including opens of a store nobody
/// writes to. `dolt_connect_branch` only loads the branch's working set
/// and sets the session's branch and head: it serializes no refs and
/// commits nothing, so it writes nothing.
/// `reopening_an_untouched_store_does_not_grow_it` is what holds this.
async fn checkout_writer_branch(
    conn: &mut sqlx::sqlite::SqliteConnection,
) -> std::result::Result<(), sqlx::Error> {
    // Absent branch is `branch not found`, which is how we learn to
    // create it rather than a failure. Creating it is a real write, and
    // happens once per file.
    match sqlx::query("SELECT dolt_connect_branch(?)")
        .bind(WRITER_BRANCH)
        .execute(&mut *conn)
        .await
    {
        Ok(_) => {}
        // No doltlite: there are no branches to be on, and every read of
        // this store is as unpinned as every write.
        Err(e) if is_missing_function(&e) => return Ok(()),
        Err(_) => {
            sqlx::query("SELECT dolt_checkout('-b', ?)")
                .bind(WRITER_BRANCH)
                .execute(&mut *conn)
                .await?;
        }
    }
    // Read it back, because a selection that quietly did nothing is the
    // one failure this whole construction cannot survive: a fresh
    // connection is on the file's default branch, so a writer that
    // thinks it moved and did not writes to `main` — visible to every
    // reader, mid-batch, which is what the branch exists to prevent.
    // Measured in #691: a failed `dolt_checkout` is silent and the rows
    // land on the wrong branch. `fsindex::checkout_branch` reads back
    // for the same reason.
    let active: String = sqlx::query_scalar("SELECT active_branch()")
        .fetch_one(&mut *conn)
        .await?;
    if active != WRITER_BRANCH {
        return Err(sqlx::Error::Configuration(
            format!(
                "opened a writer on branch {active:?}, not {WRITER_BRANCH:?}: \
                 its rows would be visible to every reader before they are sealed"
            )
            .into(),
        ));
    }
    Ok(())
}

/// The commit a branch names, or `None` when this build has no doltlite
/// or the branch does not exist yet.
async fn branch_head(pool: &SqlitePool, branch: &str) -> Option<String> {
    sqlx::query_scalar("SELECT dolt_hashof(?)")
        .bind(branch)
        .fetch_optional(pool)
        .await
        .unwrap_or(None)
        .flatten()
}

/// Fast-forward `main` to the writer's branch: the moment a seal becomes
/// visible to every reader of this file.
///
/// A force-move rather than a `dolt_merge` because one writer per file
/// means `main` only ever moves here, so the branch is always a
/// descendant of `main` and a merge would be a fast-forward anyway. It
/// also keeps `main`'s history linear, which is what
/// `dolt_diff_<table>` between two of its commits rests on. Two writers
/// on one store would need the real merge.
///
/// A no-op on any other branch — see [`WRITER_BRANCH`].
///
/// `commit_run` calls this for you, and that is the seal every step and
/// provider should use. It is public for the one shape `commit_run`
/// cannot express: a commit that needs an argument of its own, such as
/// the `--date` the yolink fixture generator pins so `dolt_log()` does
/// not report build time. Such a caller commits by hand and then calls
/// this — a commit nobody can see is not a seal.
pub async fn publish_to_main(pool: &SqlitePool) -> Result<()> {
    let active: Option<String> = sqlx::query_scalar("SELECT active_branch()")
        .fetch_optional(pool)
        .await
        .unwrap_or(None);
    if active.as_deref() != Some(WRITER_BRANCH) {
        return Ok(());
    }
    // Re-pointing `main` at a commit it already names still writes a ref
    // chunk — ~676 bytes an open, on a store nobody touched.
    // `reopening_an_untouched_store_does_not_grow_it` is what notices,
    // and it is the only check that would: this kind of leak leaves
    // `dolt_log` unchanged and `dolt_status` clean.
    if branch_head(pool, WRITER_BRANCH).await == branch_head(pool, "main").await {
        return Ok(());
    }
    sqlx::query("SELECT dolt_branch('-f', 'main', ?)")
        .bind(WRITER_BRANCH)
        .execute(pool)
        .await
        .context("fast-forward main to the writer branch")?;
    Ok(())
}

/// The pool every open shares: one connection, never recycled.
///
/// Pool size 1 with no recycling because doltlite's HEAD, working set and
/// active branch are all per-connection, and a replacement connection starts
/// on `main` with a clean tree. See the README.
///
/// A writer's connection also holds the file's writer lock, for exactly
/// as long as the connection lives: the lock is handed to the connection
/// in the pool's `after_connect` hook and released by SQLite when the
/// connection closes ([`attach_writer_lock`]). So there is no handle that
/// can commit without holding the lock, and a second writer — another
/// process, or a second pool in this one — is refused at open with the
/// holder named, instead of committing the first one's half-written
/// batch (the README's "one writer per file"). `close().await` waits for
/// the connection to close, so it is also the moment the lock is free.
///
/// The acquire timeout is [`datalib_pin::acquire_timeout`].
async fn connect_pool(db_path: &Path, access: Access, on_branch: bool) -> Result<SqlitePool> {
    let writable = access == Access::ReadWrite;
    let mut options = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .acquire_timeout(datalib_pin::acquire_timeout());
    if writable {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        // Taken here, before the connection, so a refusal is this open's
        // error rather than a connect failure inside sqlx.
        let lock = std::sync::Mutex::new(Some(take_writer_lock(db_path)?));
        let db_path = db_path.to_path_buf();
        options = options.after_connect(move |conn, _meta| {
            let taken = lock.lock().unwrap_or_else(|e| e.into_inner()).take();
            let db_path = db_path.clone();
            Box::pin(async move {
                // The first connection gets the lock taken at open; a
                // replacement — sqlx re-connecting after the first broke —
                // takes it afresh, and is refused like anyone else if the
                // old connection is still closing.
                let lock = match taken {
                    Some(lock) => lock,
                    None => take_writer_lock(&db_path)
                        .map_err(|e| sqlx::Error::Configuration(e.into()))?,
                };
                attach_writer_lock(conn, lock).await?;
                if !on_branch {
                    return Ok(());
                }
                checkout_writer_branch(conn).await
            })
        });
    }
    // No `journal_mode` pragma: doltlite manages its own storage and rejects
    // it outright.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("sqlite uri for {}", db_path.display()))?
        // A reader never conjures a store: an absent file is a real error for
        // it, where for the owner it is the first run.
        .create_if_missing(writable)
        .read_only(!writable);
    options.connect_with(opts).await.context("open sqlite pool")
}

/// Give the lock to the connection, to be released when the connection
/// closes. SQLite has no user-data slot on a connection, but a function
/// registered with a destructor has its destructor run at
/// `sqlite3_close` — the call `Pool::close()` waits on — so a no-op
/// function whose "application data" is the lock ties the two lifetimes
/// together. Doltlite is the SQLite API, so the C entry point is
/// declared here rather than through `libsqlite3-sys`, which is not a
/// direct dependency.
async fn attach_writer_lock(
    conn: &mut sqlx::sqlite::SqliteConnection,
    lock: FileLock,
) -> std::result::Result<(), sqlx::Error> {
    use std::ffi::{c_char, c_int, c_void};

    extern "C" {
        fn sqlite3_create_function_v2(
            db: *mut c_void,
            name: *const c_char,
            n_arg: c_int,
            text_rep: c_int,
            app: *mut c_void,
            x_func: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void)>,
            x_step: Option<unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_void)>,
            x_final: Option<unsafe extern "C" fn(*mut c_void)>,
            x_destroy: Option<unsafe extern "C" fn(*mut c_void)>,
        ) -> c_int;
    }
    unsafe extern "C" fn holds(_ctx: *mut c_void, _n: c_int, _args: *mut *mut c_void) {}
    unsafe extern "C" fn release(app: *mut c_void) {
        // Safety: `app` is the `Box<FileLock>` leaked below, and SQLite
        // calls this exactly once, when the connection closes.
        drop(unsafe { Box::from_raw(app as *mut FileLock) });
    }
    const SQLITE_UTF8: c_int = 1;

    let mut handle = conn.lock_handle().await?;
    let db = handle.as_raw_handle().as_ptr() as *mut c_void;
    let app = Box::into_raw(Box::new(lock)) as *mut c_void;
    // Safety: `db` is the live connection sqlx just handed us, the name is
    // a NUL-terminated literal, and the callbacks match SQLite's
    // signatures (the argument types are opaque pointers on both sides).
    let rc = unsafe {
        sqlite3_create_function_v2(
            db,
            c"datalib_writer_lock".as_ptr(),
            0,
            SQLITE_UTF8,
            app,
            Some(holds),
            None,
            None,
            Some(release),
        )
    };
    if rc != 0 {
        // SQLite does not run the destructor on a failed registration.
        drop(unsafe { Box::from_raw(app as *mut FileLock) });
        return Err(sqlx::Error::Configuration(
            format!("register the writer-lock holder on the connection: sqlite rc {rc}").into(),
        ));
    }
    Ok(())
}

/// How long a writer waits for a lock its own process still holds. A
/// dropped handle's connection closes on sqlx's worker thread a moment
/// after the drop, and that moment is the whole reason the README says
/// close, not drop; waiting it out keeps a stray drop from becoming a
/// refusal that depends on the machine's speed. A second *live* writer
/// in this process is still refused, a little later.
const OWN_CLOSE_GRACE: Duration = Duration::from_secs(2);

fn take_writer_lock(db_path: &Path) -> Result<FileLock> {
    let lock_path = lock_path_for(db_path);
    let mine = format!("(pid {})", std::process::id());
    let started = std::time::Instant::now();
    let mut lock = loop {
        match FileLock::acquire(&lock_path) {
            Ok(lock) => break lock,
            Err(LockError::Held { holder, .. })
                if holder.as_deref().is_some_and(|h| h.ends_with(&mine))
                    && started.elapsed() < OWN_CLOSE_GRACE =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(LockError::Held { holder, .. }) => bail!(
                "{} already has a writer: {}. One writer per doltlite file — wait for it, \
                 or open read-only (datalib/backend/etl/README.md, \"Connection pools\")",
                db_path.display(),
                holder.unwrap_or_else(|| "(holder unknown)".to_string())
            ),
            Err(other) => return Err(anyhow!("{other}")),
        }
    };
    if started.elapsed() > Duration::from_millis(50) {
        tracing::warn!(
            path = %db_path.display(),
            waited_ms = started.elapsed().as_millis() as u64,
            "a previous writer in this process was still closing; a handle was \
             dropped where it should have been close().await-ed"
        );
    }
    let program = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "?".to_string());
    lock.describe(&format!("{program} {mine}"));
    Ok(lock)
}

/// What `open` does with a table whose stored shape the DDL can no
/// longer be reached from by `ADD COLUMN` alone: a column removed,
/// renamed or retyped, a key or a `NOT NULL` changed. See the README
/// §"Schema self-healing".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnSchemaBreak {
    /// Fail the open, naming the table and the change, and leave the
    /// file as it was. Raw stores: their rows may be the only copy.
    Refuse,
    /// Drop the table, recreate it empty, and forget the store's
    /// cursors so the next run refills it. Derived stores, whose rows
    /// are a function of another store.
    Rebuild,
}

/// [`open`] without the shared download-bookkeeping tables. A *derived*
/// store — render output, an index, a blob CAS — would otherwise get
/// `sync_runs` and the scope tables as three empty tables suggesting a
/// provenance it lacks. `kind` is what its `_datalib_meta` names it.
/// Always [`OnSchemaBreak::Rebuild`]: every row is a function of some
/// other store, so a rebuild costs a pass over that store.
pub async fn open_derived(db_path: &Path, ddl: &[&str], kind: StoreKind) -> Result<SqlitePool> {
    open_inner(db_path, ddl, false, kind, OnSchemaBreak::Rebuild, &[]).await
}

/// A raw store, for the process that owns it, with no migrations.
/// [`OnSchemaBreak::Refuse`].
pub async fn open(db_path: &Path, extra_ddl: &[&str]) -> Result<SqlitePool> {
    open_migrating(db_path, extra_ddl, &[]).await
}

/// [`open`] for a provider whose `schema_raw.rs` keeps a migration
/// ladder: the rungs above the store's `schema_version` run first, one
/// commit each, and the DDL is compared to the result. A ladder is how
/// a non-additive change reaches an existing store without a refusal.
pub async fn open_migrating(
    db_path: &Path,
    extra_ddl: &[&str],
    ladder: &[Migration],
) -> Result<SqlitePool> {
    open_inner(
        db_path,
        extra_ddl,
        true,
        StoreKind::Raw,
        OnSchemaBreak::Refuse,
        ladder,
    )
    .await
}

/// [`open`] with the policy said rather than taken from the process.
pub async fn open_with(
    db_path: &Path,
    extra_ddl: &[&str],
    on_break: OnSchemaBreak,
) -> Result<SqlitePool> {
    open_inner(db_path, extra_ddl, true, StoreKind::Raw, on_break, &[]).await
}

/// The error [`OnSchemaBreak::Refuse`] fails an open with: every table
/// whose stored shape the DDL cannot be reached from additively, and
/// what differs. The file was not changed.
#[derive(Debug)]
pub struct SchemaBreak {
    pub store: PathBuf,
    /// `(table, what differs)`, in DDL order.
    pub breaks: Vec<(String, String)>,
}

impl std::fmt::Display for SchemaBreak {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{} has a shape this build's DDL cannot be reached from by adding columns, \
             and its rows may be the only copy, so nothing was changed:",
            self.store.display()
        )?;
        for (table, what) in &self.breaks {
            writeln!(f, "  {table}: {what}")?;
        }
        write!(
            f,
            "Either add a rung to the provider's migration ladder (etl/README.md §\"The \
             migration ladder\"), or — if upstream still has the data — empty this \
             source and download it again: `datalib-dag --reset <source>/ingest --sync \
             <source>/ingest <config>`."
        )
    }
}

impl std::error::Error for SchemaBreak {}

/// Open a store to read data somebody else owns.
///
/// **[`open`] writes on the way in, and that is fine for the process that owns
/// the store and wrong for everyone else.** It discards a dirty working tree,
/// reconciles the schema, and then commits. For the owner those are three
/// useful things. For a reader they are three ways to write to a file it does
/// not own, and under streaming the damage is specific: the reader's own open
/// throws away the producer's half-written batch.
///
/// So this does none of it: connect read-only, and hand back the pool. The
/// connection is opened `read_only`, so "a reader must not write" is enforced
/// by the engine (`attempt to write a readonly database`) rather than left as
/// an intention — and creating the `pinned_<table>` views still works, since
/// they live in the per-connection temp schema rather than in the file.
///
/// And it pins at open: the handle it hands back names one commit — the
/// one the caller was given, or HEAD — and has the `pinned_<table>` views
/// installed, so every content read through [`Reads::At`] names that
/// commit however long the pass runs. A store with nothing committed
/// yields `None` rather than a reader onto its working set; the caller
/// decides what that means (a consumer does nothing that pass).
///
/// A schema this store has not got yet is the owner's to add on its next run,
/// and a read naming a column it lacks fails at prepare time saying so. Probe
/// with [`column_exists`] and fall back where that is a real possibility;
/// slack's `load_channels` is the worked example.
///
/// [`Reads::At`]: crate::pin::Reads::At
pub async fn open_reader(db_path: &Path, commit: Option<&str>) -> Result<Option<Reader>> {
    let pool = connect_pool(db_path, Access::ReadOnly, false).await?;
    let pin = match commit {
        Some(commit) => Some(crate::pin::Pin::at(commit)?),
        None => crate::pin::head(&pool).await?,
    };
    let Some(pin) = pin else {
        pool.close().await;
        return Ok(None);
    };
    crate::pin::install_views(&pool, &pin)
        .await
        .with_context(|| format!("pin {} for reading", db_path.display()))?;
    Ok(Some(Reader { pool, pin }))
}

/// A read-only connection with no pin, for the blob CAS alone.
///
/// Not because the CAS cannot be pinned — it is committed like every
/// other store, blobs before the entities that name them
/// (`raw_store::SealState::seal`) — but because nothing has moved it
/// over yet. `blob_cas::open_cas_reader` has the note. Everything else
/// reads through [`open_reader`].
pub(crate) async fn open_reader_unpinned(db_path: &Path) -> Result<SqlitePool> {
    connect_pool(db_path, Access::ReadOnly, false).await
}

/// A store somebody else writes, read at one commit. Derefs to its pool,
/// so queries run against `&*reader` or [`Reader::pool`]; content reads
/// name tables through [`Reads::At`](crate::pin::Reads::At) with
/// [`Reader::pin`].
pub struct Reader {
    pool: SqlitePool,
    pin: crate::pin::Pin,
}

impl Reader {
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn pin(&self) -> &crate::pin::Pin {
        &self.pin
    }

    /// Wait for the connection to actually go away; dropping the handle
    /// only schedules that.
    pub async fn close(self) {
        self.pool.close().await;
    }
}

impl std::ops::Deref for Reader {
    type Target = SqlitePool;

    fn deref(&self) -> &SqlitePool {
        &self.pool
    }
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
    kind: StoreKind,
    on_break: OnSchemaBreak,
    ladder: &[Migration],
) -> Result<SqlitePool> {
    // Logged at every call so a stray second pool against an already-open
    // file is attributable: with max_connections=1 it surfaces only as
    // "database is locked" on dolt_commit.
    let started = std::time::Instant::now();
    let store = path_label(db_path);
    tracing::info!(store, "opening the store");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    let pool = connect_pool(db_path, Access::ReadWrite, true).await?;
    // Before anything writes: a store a newer line of datalib wrote is
    // refused whole, because the reconcile below would drop what it
    // does not know (`datalib_store_meta::guard`).
    let written_by = datalib_store_meta::read(&pool)
        .await
        .with_context(|| format!("read _datalib_meta of {}", db_path.display()))?;
    if let Err(newer) = datalib_store_meta::refuse_if_newer(db_path, written_by.as_ref()) {
        pool.close().await;
        return Err(anyhow::Error::new(newer));
    }
    // A store starts at its last commit. Whatever a crashed or interrupted
    // writer left in the working set was never at a seal boundary, and every
    // reader pins commits, so nobody was promised it.
    discard_dirty_working_tree(&pool, db_path).await?;
    // A writer that died between its commit and its publication left the
    // branch ahead of `main`. The commit is a seal the last run meant to
    // make, so finish it rather than leaving it stranded where no reader
    // can see it.
    publish_to_main(&pool).await?;
    // The ladder, before the DDL is compared to anything: a rung is how
    // the store gets from the shape an older build left to the one this
    // DDL declares. Each rung is its own commit, so a crash between two
    // leaves a store the next open resumes from. A file with no tables
    // — a new one — has nothing to climb: the DDL below creates it at
    // the ladder's top. And a caller with no DDL (`open_index`, a reset)
    // does no schema work at all, the ladder included.
    let shared: &[&str] = if include_shared { SHARED_DDL } else { &[] };
    let bare = extra_ddl.is_empty() && shared.is_empty();
    let stored_version = datalib_store_meta::ladder::stored_version(&pool).await?;
    let top = datalib_store_meta::ladder::top(ladder);
    let rungs = if bare || user_tables(&pool).await?.is_empty() {
        Vec::new()
    } else {
        datalib_store_meta::ladder::pending(ladder, stored_version)?
    };
    if stored_version > top && !bare {
        pool.close().await;
        return Err(
            anyhow::Error::new(datalib_store_meta::ladder::AheadOfLadder {
                stored: stored_version,
                top,
            })
            .context(format!("open {}", db_path.display())),
        );
    }
    for rung in rungs {
        // The meta table has to exist for the rung to bump the version;
        // a store from before the table is at version 0 and gets it here.
        sqlx::query(datalib_store_meta::DDL)
            .execute(&pool)
            .await
            .context("create _datalib_meta before migrating")?;
        datalib_store_meta::ladder::apply(&pool, rung).await?;
        commit_run(&pool, &format!("migrate v{}: {}", rung.version, rung.name))
            .await
            .with_context(|| format!("commit migration v{}", rung.version))?;
    }
    // Tables, then indexes — see the README for why the order is
    // load-bearing. `parse_create_table_name` returns `None` for exactly
    // the statements that must wait.
    // `_datalib_meta` first, in every store: it says which build wrote
    // the file, and it rides in the same schema commit as the rest.
    let meta_ddl: &[&str] = &[datalib_store_meta::DDL];
    let ddl = || meta_ddl.iter().chain(extra_ddl).chain(shared);
    let is_create_table = |stmt: &&&str| parse_create_table_name(stmt).is_some();
    // Every table is planned against the file as it is before anything
    // is created or altered: a refusal has to leave the file untouched,
    // and it has to name every break, not the first.
    let mut plans = Vec::new();
    for stmt in ddl().filter(is_create_table) {
        let table = parse_create_table_name(stmt).expect("filtered on it");
        let plan = plan_table_schema(&pool, stmt, &table)
            .await
            .with_context(|| format!("plan schema: {table}"))?;
        plans.push((*stmt, table, plan));
    }
    // A break in an empty table loses nothing, so it is rebuilt whatever
    // the policy: that is how a cleared store gets past a shape this build
    // cannot reach.
    let mut breaks: Vec<(String, String)> = Vec::new();
    for (_, table, plan) in &plans {
        if let TablePlan::Break(what) = plan {
            if !table_is_empty(&pool, table).await? {
                breaks.push((table.clone(), what.clone()));
            }
        }
    }
    if !breaks.is_empty() && on_break == OnSchemaBreak::Refuse {
        pool.close().await;
        return Err(anyhow::Error::new(SchemaBreak {
            store: db_path.to_path_buf(),
            breaks,
        }));
    }
    let mut created = Vec::new();
    let mut recreated = Vec::new();
    for (stmt, table, plan) in &plans {
        match apply_table_plan(&pool, stmt, table, plan, on_break)
            .await
            .with_context(|| format!("reconcile schema: {table}"))?
        {
            Reconciled::Kept => {}
            Reconciled::Created => created.push(table.clone()),
            Reconciled::Recreated => recreated.push(table.clone()),
        }
    }
    // A table that appeared in a store that already had others is as
    // empty as a recreated one, and a cursor that says "read through
    // here" would leave it empty until upstream changed. A store whose
    // every table was just created is a first open, with no cursor to
    // forget.
    let first_open = created.len() == plans.len();
    if !recreated.is_empty() || (!created.is_empty() && !first_open) {
        forget_cursors(&pool, &created, &recreated).await?;
    }
    // Indexes last, so they see the reconciled columns — and so a
    // recreate costs no index.
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
    // A caller with no DDL of its own installs its schema itself, writes
    // the meta rows for it and commits it itself (`open_index`); a commit
    // here would carry nothing it wants.
    if bare {
        return Ok(pool);
    }
    let meta_moved = datalib_store_meta::write(
        &pool,
        kind,
        &datalib_store_meta::schema_hash(ddl().copied()),
        top,
    )
    .await
    .with_context(|| format!("write _datalib_meta for {}", db_path.display()))?;
    // Commit the schema before handing back the pool: doltlite only
    // materializes `dolt_diff_<table>` for tables that exist at HEAD, so an
    // uncommitted table makes the first sync's delta vanish with a warning.
    // The message names the build when the meta rows moved — a new
    // datalib, or a new shape — so `dolt_log` reads as an upgrade history.
    let message = if meta_moved {
        format!(
            "schema: apply DDL (datalib {})",
            datalib_runtime::build_id::DATALIB_VERSION
        )
    } else {
        "schema: apply DDL".to_string()
    };
    commit_run(&pool, &message)
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
    // Reachable only past `bare`, so `_datalib_meta` and the caller's DDL
    // have been created -- the predicate's "a file with no tables is not
    // readable" arm cannot fire here.
    anyhow::ensure!(
        crate::pin::carries_committed_schema(&pool).await,
        "opened {} but its tables are not committed: either the schema \
         commit did not take, or this binary is not linked against doltlite. \
         A reader cannot tell either from a source that lost every row.",
        db_path.display()
    );
    let elapsed_ms = started.elapsed().as_millis() as u64;
    tracing::info!(store, elapsed_ms, "the store is open");
    Ok(pool)
}

/// One column's introspected shape, from `PRAGMA table_xinfo`. Two
/// columns compare equal when a row written under one reads correctly
/// under the other: the name, the declared type, nullability, the
/// default, the place in the primary key, and whether it is generated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// Upper-cased with its whitespace collapsed, so `varchar(36)` and
    /// `VARCHAR (36)` are one type.
    pub decl_type: String,
    pub not_null: bool,
    pub default: Option<String>,
    /// 1-based position in the primary key, 0 when not part of it.
    pub pk: i64,
    /// `hidden` 2 (VIRTUAL) or 3 (STORED).
    pub generated: bool,
    /// Only a STORED generated column cannot be added to an existing
    /// table; a VIRTUAL one can.
    pub stored_generated: bool,
}

impl ColumnInfo {
    fn describe(&self) -> String {
        let mut s = format!("{} {}", self.name, self.decl_type);
        if self.not_null {
            s.push_str(" NOT NULL");
        }
        if let Some(d) = &self.default {
            s.push_str(&format!(" DEFAULT {d}"));
        }
        if self.pk > 0 {
            s.push_str(&format!(" PRIMARY KEY#{}", self.pk));
        }
        if self.generated {
            s.push_str(if self.stored_generated {
                " GENERATED STORED"
            } else {
                " GENERATED VIRTUAL"
            });
        }
        s
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
/// Every table in the file that is ours to touch.
async fn table_is_empty(pool: &SqlitePool, table: &str) -> Result<bool> {
    // Audited: `table` is parsed from our own static DDL.
    let any: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS(SELECT 1 FROM \"{table}\")"
    )))
    .fetch_one(pool)
    .await
    .with_context(|| format!("is {table} empty"))?;
    Ok(!any)
}

async fn user_tables(pool: &SqlitePool) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(pool)
    .await
    .context("list tables")
}

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
        let decl_type: String = r.try_get("type").unwrap_or_default();
        cols.push(ColumnInfo {
            name,
            decl_type: decl_type
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_uppercase(),
            not_null: not_null != 0,
            default: r
                .try_get::<Option<String>, _>("dflt_value")
                .ok()
                .flatten()
                .map(|d| d.trim().to_string()),
            pk: r.try_get("pk").unwrap_or(0),
            generated: hidden == 2 || hidden == 3,
            stored_generated: hidden == 3,
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
    // A fresh in-memory database per call, not a shared scratch pool: the
    // probe table name is a constant, so concurrent reconciles would drop
    // each other's table.
    let probe = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
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

/// How `table` in the file differs from `create_sql`, for a caller that
/// rebuilds on any difference: `None` when the table is absent or has
/// exactly the declared shape, else one line naming what moved.
pub async fn table_drift(
    pool: &SqlitePool,
    create_sql: &str,
    table: &str,
) -> Result<Option<String>> {
    let actual = table_columns(pool, table).await?;
    if actual.is_empty() {
        return Ok(None);
    }
    let declared = declared_columns(create_sql, table).await?;
    Ok(shape_drift(&declared, &actual))
}

/// What differs between two column lists, or `None` when nothing does.
fn shape_drift(declared: &[ColumnInfo], actual: &[ColumnInfo]) -> Option<String> {
    let mut parts = Vec::new();
    let missing: Vec<&str> = declared
        .iter()
        .filter(|d| !actual.iter().any(|a| a.name == d.name))
        .map(|d| d.name.as_str())
        .collect();
    let unexpected: Vec<&str> = actual
        .iter()
        .filter(|a| !declared.iter().any(|d| d.name == a.name))
        .map(|a| a.name.as_str())
        .collect();
    let changed: Vec<String> = declared
        .iter()
        .filter_map(|d| {
            let a = actual.iter().find(|a| a.name == d.name)?;
            (a != d).then(|| {
                format!(
                    "{} (stored: {}; declared: {})",
                    d.name,
                    a.describe(),
                    d.describe()
                )
            })
        })
        .collect();
    if !missing.is_empty() {
        parts.push(format!("missing: [{}]", missing.join(", ")));
    }
    if !unexpected.is_empty() {
        parts.push(format!("unexpected: [{}]", unexpected.join(", ")));
    }
    if !changed.is_empty() {
        parts.push(format!("changed: [{}]", changed.join("; ")));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
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

enum Reconciled {
    Kept,
    /// The table did not exist and was created.
    Created,
    /// The table was dropped and recreated empty.
    Recreated,
}

/// What it takes to bring one table in the file to its DDL, decided
/// before anything is touched.
#[derive(Debug, PartialEq, Eq)]
enum TablePlan {
    /// Present, and every column has the declared shape.
    Kept,
    /// Absent.
    Create,
    /// Present, and reachable by `ALTER TABLE … ADD COLUMN` with these
    /// clauses, verbatim from the DDL.
    Add(Vec<String>),
    /// Present, and not reachable additively: what differs.
    Break(String),
}

/// Whether a column clause can go through `ALTER TABLE … ADD COLUMN`.
/// SQLite refuses a key, a `NOT NULL` with no default and a STORED
/// generated column there; anything else it accepts, a VIRTUAL
/// generated column included.
fn can_be_added(col: &ColumnInfo) -> bool {
    col.pk == 0 && !(col.not_null && col.default.is_none()) && !col.stored_generated
}

async fn plan_table_schema(pool: &SqlitePool, create_sql: &str, table: &str) -> Result<TablePlan> {
    let actual = table_columns(pool, table).await?;
    if actual.is_empty() {
        return Ok(TablePlan::Create);
    }
    let declared = declared_columns(create_sql, table).await?;
    let Some(drift) = shape_drift(&declared, &actual) else {
        return Ok(TablePlan::Kept);
    };
    // Additive means: every difference is a declared column the file
    // lacks, and each one can be added.
    let additive = actual.iter().all(|a| declared.iter().any(|d| d == a))
        && declared
            .iter()
            .filter(|d| !actual.iter().any(|a| a.name == d.name))
            .all(can_be_added);
    if !additive {
        return Ok(TablePlan::Break(drift));
    }
    let mut clauses = Vec::new();
    for col in declared
        .iter()
        .filter(|d| !actual.iter().any(|a| a.name == d.name))
    {
        match column_clause(create_sql, &col.name) {
            Some(clause) => clauses.push(clause),
            None => {
                return Ok(TablePlan::Break(format!(
                    "{drift}; and the clause for {} could not be read out of the DDL",
                    col.name
                )))
            }
        }
    }
    Ok(TablePlan::Add(clauses))
}

/// The column definition for `column` as the DDL wrote it — the text
/// between the top-level commas of the `CREATE TABLE` body that starts
/// with that name — so an `ADD COLUMN` carries whatever the declaration
/// carried (a `DEFAULT`, a `COLLATE`, a generation expression).
fn column_clause(create_sql: &str, column: &str) -> Option<String> {
    let open = create_sql.find('(')?;
    let body = &create_sql[open + 1..];
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut clauses = Vec::new();
    let mut in_quote: Option<char> = None;
    for (i, c) in body.char_indices() {
        match (in_quote, c) {
            (Some(q), _) if c == q => in_quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => in_quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') if depth == 0 => {
                clauses.push(&body[start..i]);
                break;
            }
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 => {
                clauses.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    clauses
        .into_iter()
        .map(str::trim)
        .find(|clause| {
            clause
                .split(|c: char| c.is_whitespace() || c == '(')
                .next()
                .map(|first| first.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']'))
                == Some(column)
        })
        .map(|clause| clause.split_whitespace().collect::<Vec<_>>().join(" "))
}

async fn apply_table_plan(
    pool: &SqlitePool,
    create_sql: &str,
    table: &str,
    plan: &TablePlan,
    on_break: OnSchemaBreak,
) -> Result<Reconciled> {
    match plan {
        TablePlan::Kept => return Ok(Reconciled::Kept),
        TablePlan::Create => {
            // Audited: `create_sql` is our own static DDL.
            sqlx::query(sqlx::AssertSqlSafe(create_sql))
                .execute(pool)
                .await
                .with_context(|| format!("create {table}"))?;
            return Ok(Reconciled::Created);
        }
        TablePlan::Add(clauses) => {
            let mut added_all = true;
            for clause in clauses {
                // Audited: `clause` is a slice of our own static DDL.
                let sql = format!("ALTER TABLE {table} ADD COLUMN {clause}");
                match sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
                    Ok(_) => tracing::info!(
                        table,
                        column = %clause,
                        "doltlite_raw: added a column an older store lacked"
                    ),
                    Err(e) if on_break == OnSchemaBreak::Refuse => {
                        // Planned as additive and refused by the engine:
                        // the file has this one ALTER less than the DDL
                        // wants and nothing else, which the next open
                        // discards with the working set.
                        return Err(anyhow::Error::new(e)
                            .context(format!("ALTER TABLE {table} ADD COLUMN {clause}")));
                    }
                    Err(e) => {
                        tracing::warn!(
                            table,
                            column = %clause,
                            error = %format!("{e:#}"),
                            "doltlite_raw: ADD COLUMN failed; rebuilding the table"
                        );
                        added_all = false;
                        break;
                    }
                }
            }
            if added_all {
                return Ok(Reconciled::Kept);
            }
        }
        TablePlan::Break(what) => {
            tracing::warn!(
                table,
                what = %what,
                "doltlite_raw: the stored shape cannot be reached by ADD COLUMN; \
                 dropping and recreating the table from the DDL"
            );
        }
    }
    // Audited: `table` is parsed from our own static DDL.
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE IF EXISTS {table}")))
        .execute(pool)
        .await
        .with_context(|| format!("drop {table} for schema recreate"))?;
    sqlx::query(sqlx::AssertSqlSafe(create_sql))
        .execute(pool)
        .await
        .with_context(|| format!("recreate {table}"))?;
    Ok(Reconciled::Recreated)
}

/// The tables a resume cursor can live in, store-wide. Per-row cursors
/// (a sidecar's `last_ts_ms`, an address book's `ctag`) go with the
/// table that holds them; these three outlive any one table.
const CURSOR_TABLES: &[&str] = &[
    "sync_scope_state",
    "sync_scope_config",
    crate::file_checkpoint::INGESTED_FILES_TABLE,
];

/// A cursor is only valid under the schema that set it. A recreated
/// table is empty, and so is one that just appeared, and a cursor that
/// says "read through here" would let the next run resume past rows the
/// table does not have — a store that stays empty until upstream
/// changes, with nothing saying why. So either forgets every cursor in
/// the store, and the next run walks from the start into every table,
/// which the unchanged ones absorb as no-op upserts.
async fn forget_cursors(pool: &SqlitePool, created: &[String], recreated: &[String]) -> Result<()> {
    let mut cleared = Vec::new();
    for table in CURSOR_TABLES {
        if table_columns(pool, table).await?.is_empty() {
            continue;
        }
        // Audited: `table` is one of the `&'static str` names above.
        let n = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(pool)
            .await
            .with_context(|| format!("clear {table} after schema recreate"))?
            .rows_affected();
        if n > 0 {
            cleared.push(format!("{table}={n}"));
        }
    }
    tracing::warn!(
        created = %created.join(","),
        recreated = %recreated.join(","),
        cursors_cleared = %cleared.join(","),
        "doltlite_raw: a table is empty that the store's cursors would skip past, \
         so the cursors were cleared; the next run walks from the start"
    );
    Ok(())
}

/// Seal a crashed prior run's orphaned working-tree changes into their own
/// commit, so the next successful commit doesn't fold two runs' work into one
/// `dolt_log` entry — and so a dirty tree at open gets logged.
///
/// Errors are swallowed: a stock-libsqlite3 build (CI, no doltlite
/// extensions) has no `dolt_status` at all.
/// `dolt_reset --hard`, plus the part it leaves behind: like `git reset
/// --hard`, it restores tracked tables and ignores an untracked one, and a
/// writer that died after `CREATE TABLE` and before its first commit leaves
/// exactly that. `dolt_clean` takes those, the way `git clean` does. Every
/// commit here is `-Am`, so anything still dirty after this rides into the
/// schema commit a few lines later — which is why both halves run.
async fn discard_dirty_working_tree(pool: &SqlitePool, db_path: &Path) -> Result<()> {
    // `dolt_status` is a vtab; stock SQLite errors with "no such table".
    let dirty: std::result::Result<i64, sqlx::Error> =
        sqlx::query_scalar("SELECT count(*) FROM dolt_status")
            .fetch_one(pool)
            .await;
    let count = match dirty {
        Ok(n) => n,
        Err(e) if e.to_string().contains("no such table") => return Ok(()),
        Err(e) => return Err(anyhow::Error::new(e).context("probe dolt_status")),
    };
    if count == 0 {
        return Ok(());
    }
    tracing::warn!(
        path = %db_path.display(),
        dirty_entries = count,
        "discard_dirty_working_tree: a prior writer left {count} dirty entries; \
         starting from the last commit"
    );
    sqlx::query("SELECT dolt_reset('--hard')")
        .execute(pool)
        .await
        .context("dolt_reset --hard")?;
    sqlx::query("SELECT dolt_clean()")
        .execute(pool)
        .await
        .context("dolt_clean")?;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM dolt_status")
        .fetch_one(pool)
        .await
        .context("re-probe dolt_status")?;
    if left != 0 {
        bail!("{left} entries still dirty after dolt_reset --hard and dolt_clean");
    }
    Ok(())
}

// ── sync_runs ───────────────────────────────────────────────────────

pub async fn start_run(pool: &SqlitePool, config: &Value) -> Result<i64> {
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let cfg = serde_json::to_string(config).context("serialize run config")?;
    let row = sqlx::query(
        "INSERT INTO sync_runs (started_at_utc, tz_offset, config, status) \
         VALUES (?, ?, ?, 'running') RETURNING run_id",
    )
    .bind(&now)
    .bind(&tz_offset)
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
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let s = serde_json::to_string(summary).context("serialize run summary")?;
    sqlx::query(
        "UPDATE sync_runs SET finished_at_utc = ?, tz_offset = ?, status = ?, summary = ? \
         WHERE run_id = ?",
    )
    .bind(&now)
    .bind(&tz_offset)
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

/// The run a step belongs to, from the runner's environment
/// (`docs/dev/step_protocol.md`), or `None` outside a run.
pub const RUN_ID_ENV: &str = "DATALIB_DAG_RUN_ID";

/// The data root, from the same environment.
pub const DATA_ROOT_ENV: &str = "DATALIB_DAG_DATA_ROOT";

/// A store's path as a log line names it: under the data root when the
/// runner said where that is, since every store's is the same prefix.
fn store_label(pool: &SqlitePool) -> String {
    path_label(pool.connect_options().get_filename())
}

fn path_label(path: &Path) -> String {
    let under_root = std::env::var_os(DATA_ROOT_ENV).and_then(|root| path.strip_prefix(root).ok());
    under_root.unwrap_or(path).display().to_string()
}

/// A commit message with the run stamped on its end — `… run=<id>` —
/// so the commit can be joined back to the run's log. The history
/// reader parses exactly this suffix.
pub fn stamp_run(msg: &str) -> String {
    stamp_run_with(msg, std::env::var(RUN_ID_ENV).ok().as_deref())
}

fn stamp_run_with(msg: &str, run_id: Option<&str>) -> String {
    match run_id {
        Some(id) if !id.trim().is_empty() => format!("{msg} run={}", id.trim()),
        _ => msg.to_string(),
    }
}

pub async fn commit_run(pool: &SqlitePool, msg: &str) -> Result<Option<String>> {
    if !has_dolt_extensions(pool).await {
        return Ok(None);
    }
    let started = std::time::Instant::now();
    let store = store_label(pool);
    // "nothing to commit" is a legitimate outcome: a pass that fetched
    // nothing new leaves the working set clean.
    let hash = match sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', ?)")
        .bind(stamp_run(msg))
        .fetch_optional(pool)
        .await
    {
        Ok(opt) => opt.flatten(),
        Err(e) if e.to_string().contains("nothing to commit") => None,
        Err(e) => return Err(anyhow::Error::new(e).context("dolt_commit")),
    };
    // The seal is the commit *and* its publication: a commit a reader
    // cannot see is not a seal. Between the two a crash leaves the
    // branch ahead of `main`, which the next `open` finishes.
    if hash.is_some() {
        publish_to_main(pool).await?;
    }
    let elapsed_ms = started.elapsed().as_millis() as u64;
    // `message` is the sentence's own field name in tracing, so the
    // commit message goes under another.
    match &hash {
        Some(hash) => tracing::debug!(store, hash, commit_message = msg, elapsed_ms, "committed"),
        None => tracing::debug!(store, commit_message = msg, elapsed_ms, "nothing to commit"),
    }
    Ok(hash)
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

/// The content tables whose rows differ between two commits: every
/// table but the `*_bookkeeping` sidecars, [`SHARED_TABLES`] and
/// `ingested_files`. The question a provider's test asks after
/// ingesting the same input twice under two different nows — the
/// answer must be empty, or a stamp the store mints is sitting in a
/// content row, and every consumer that diffs the store will find
/// that row changed on every run.
pub async fn content_tables_changed(
    pool: &SqlitePool,
    from: &str,
    to: &str,
) -> Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT coalesce(to_table_name, from_table_name) FROM dolt_diff_summary \
         WHERE from_ref = ? AND to_ref = ? AND data_change = 1",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .context("dolt_diff_summary")?;
    let mut changed: Vec<String> = rows
        .into_iter()
        .filter(|t| {
            !t.ends_with("_bookkeeping")
                && !SHARED_TABLES.contains(&t.as_str())
                && t != crate::file_checkpoint::INGESTED_FILES_TABLE
        })
        .collect();
    changed.sort();
    changed.dedup();
    Ok(changed)
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
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&url)
        .await
        .with_context(|| format!("open read-only {}", db_path.display()))?;
    let head = head_commit(&pool).await;
    pool.close().await;
    head
}

/// The raw store's whole-store problem counts by severity, read-only,
/// for the step's report. Empty for a store that is not there yet or
/// predates the table.
pub async fn problem_counts_at_path(
    db_path: &Path,
) -> Result<HashMap<datalib_problems::Severity, i64>> {
    if !db_path.exists() {
        return Ok(HashMap::new());
    }
    let url = format!("sqlite://{}?mode=ro", db_path.display());
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&url)
        .await
        .with_context(|| format!("open read-only {}", db_path.display()))?;
    let rows = sqlx::query("SELECT severity, COUNT(*) FROM problems GROUP BY severity")
        .fetch_all(&pool)
        .await;
    pool.close().await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) if crate::pin::is_missing_table(&e, "problems") => return Ok(HashMap::new()),
        Err(e) => return Err(e).context("count the raw store's problems"),
    };
    let mut out = HashMap::new();
    for r in rows {
        let word: String = r.try_get(0)?;
        let severity = datalib_problems::Severity::parse(&word)
            .with_context(|| format!("problems.severity: unknown spelling {word:?}"))?;
        out.insert(severity, r.try_get::<i64, _>(1)?);
    }
    Ok(out)
}

// ── Reset ───────────────────────────────────────────────────────────

/// Empty every table and commit, so the store reads as a source with
/// nothing in it while its history keeps every row. The tables stay, so
/// a reader diffing from an earlier commit sees every row deleted — how
/// a clear takes a source's documents out of what renders it. A table
/// whose shape this build refuses is rebuilt by the owner's next open,
/// since it is empty. `_datalib_meta` is kept: it still says who wrote
/// the file. A store that does not exist has nothing to reset.
pub async fn reset_store(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let pool = open_inner(
        db_path,
        &[],
        false,
        StoreKind::Raw,
        OnSchemaBreak::Refuse,
        &[],
    )
    .await?;
    let result = async {
        // Listed before the transaction: the pool has one connection,
        // and a query on it inside the transaction would wait forever.
        let tables = user_tables(&pool).await?;
        let mut tx = pool.begin().await.context("begin reset tx")?;
        for table in tables.iter().filter(|t| *t != datalib_store_meta::TABLE) {
            // Audited: `table` is a name read out of `sqlite_master`.
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM \"{table}\"")))
                .execute(&mut *tx)
                .await
                .with_context(|| format!("reset: empty {table}"))?;
        }
        tx.commit().await.context("commit reset tx")?;
        commit_run(&pool, "reset").await.map(|_| ())
    }
    .await;
    pool.close().await;
    result.with_context(|| format!("reset {}", db_path.display()))
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

/// `result = None` is success (sets `fetched_at_utc`, clears `last_error`);
/// `Some(err)` is failure (leaves `fetched_at_utc`, sets `last_error`). Both bump
/// `attempt_count` and set `last_attempt_at_utc`.
///
/// Upserts, so it is safe even when [`ensure_object_row`] hasn't pre-seeded.
pub async fn record_object_attempt(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
    result: Option<&str>,
) -> Result<()> {
    record_object_bookkeeping(tx, table, id, result).await?;
    record_fetch_problem(tx, table, id, result.map(NotFetched::Failed)).await
}

/// The sidecar half of an attempt, without the `problems` row: the data
/// stub, the attempt count, the stamps and `last_error`. Split out
/// because a deliberate skip wants exactly this bookkeeping and a
/// different problem (see [`record_object_skipped`]).
async fn record_object_bookkeeping(
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
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let sql = match result {
        None => format!(
            "INSERT INTO {table}_bookkeeping \
                (id, fetched_at_utc, attempt_count, last_attempt_at_utc, last_error, tz_offset)
             VALUES (?, ?, 1, ?, NULL, ?)
             ON CONFLICT(id) DO UPDATE SET
                fetched_at_utc = excluded.fetched_at_utc,
                attempt_count = {table}_bookkeeping.attempt_count + 1,
                last_attempt_at_utc = excluded.last_attempt_at_utc,
                last_error = NULL,
                tz_offset = excluded.tz_offset"
        ),
        Some(_) => format!(
            "INSERT INTO {table}_bookkeeping (id, attempt_count, last_attempt_at_utc, last_error, tz_offset)
             VALUES (?, 1, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                attempt_count = {table}_bookkeeping.attempt_count + 1,
                last_attempt_at_utc = excluded.last_attempt_at_utc,
                last_error = excluded.last_error,
                tz_offset = excluded.tz_offset"
        ),
    };
    // Audited: both arms interpolate only `table`; the rest is bound.
    let q = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(id).bind(&now);
    let q = match result {
        None => q,
        Some(err) => q.bind(err),
    };
    let q = q.bind(&tz_offset);
    q.execute(&mut **tx)
        .await
        .with_context(|| format!("record_object_attempt {table}={id}"))?;
    Ok(())
}

/// A record the download declined to fetch, because a limit in the
/// config said not to.
///
/// The bookkeeping is a failed attempt's, deliberately: `last_error`
/// carries what the rule measured, and it is what puts the record in
/// [`failed_ids`], so raising the limit picks the file up on the next
/// run. Only the `problems` row differs — nothing went wrong here, and
/// a person reading the Manage screen should not be told it did.
pub async fn record_object_skipped(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
    reason: datalib_problems::Reason,
    detail: &str,
) -> Result<()> {
    record_object_bookkeeping(tx, table, id, Some(detail)).await?;
    record_fetch_problem(tx, table, id, Some(NotFetched::Skipped { reason, detail })).await
}

/// Why a record has no payload after this run touched it. A failure is
/// something that went wrong and may not next time; a skip is a rule we
/// applied on purpose. They must not read the same on the Manage
/// screen, and they do not carry the same severity.
#[derive(Debug, Clone, Copy)]
pub enum NotFetched<'a> {
    Failed(&'a str),
    /// Declined on purpose — `reason` says which rule, `detail` says
    /// what it measured.
    Skipped {
        reason: datalib_problems::Reason,
        detail: &'a str,
    },
}

impl NotFetched<'_> {
    fn detail(&self) -> &str {
        match self {
            NotFetched::Failed(err) => err,
            NotFetched::Skipped { detail, .. } => detail,
        }
    }
}

/// The `problems` row behind a failed attempt, or its absence behind a
/// successful one. A failure on a record that has never fetched is a
/// dropped record — an error; one on a record that fetched before
/// leaves the earlier copy in place, and is a warning: what the reader
/// sees is stale, not missing. A skip is neither: nothing was lost that
/// was not meant to be, so it is `Ok` and `Info` whatever came before.
async fn record_fetch_problem(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &str,
    id: &str,
    result: Option<NotFetched<'_>>,
) -> Result<()> {
    use crate::bulk::BulkUpsertable;
    use datalib_problems::{
        Outcome, Problem, ProblemRow, Reason, Scope, ScopeKind, Severity, Stage,
    };
    let entity_id = format!("{table}:{id}");
    let first_seen: Option<String> = sqlx::query_scalar(
        "SELECT first_seen_at_utc FROM problems \
         WHERE scope_kind = ? AND scope_key = ? AND stage = ?",
    )
    .bind(ScopeKind::Entity.as_str())
    .bind(&entity_id)
    .bind(Stage::Fetch.as_str())
    .fetch_optional(&mut **tx)
    .await
    .with_context(|| format!("read the fetch problem of {entity_id}"))?;
    sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ? AND stage = ?")
        .bind(ScopeKind::Entity.as_str())
        .bind(&entity_id)
        .bind(Stage::Fetch.as_str())
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear the fetch problem of {entity_id}"))?;
    let Some(not_fetched) = result else {
        return Ok(());
    };
    let err = not_fetched.detail();
    // An earlier successful fetch — the sidecar's `fetched_at_utc`,
    // which every table has, payload-bearing or CAS edge — means the
    // reader still has something, just not the latest. The row above
    // has already bumped the attempt, and a failure leaves that stamp
    // alone.
    let fetched_sql =
        format!("SELECT fetched_at_utc IS NOT NULL FROM {table}_bookkeeping WHERE id = ?");
    // Audited: `table` interpolated as an identifier; `id` is bound.
    let fetched_before: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(fetched_sql))
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .with_context(|| format!("probe {entity_id} for an earlier fetch"))?
        .unwrap_or(false);
    let (outcome, severity, reason) = match not_fetched {
        NotFetched::Skipped { reason, .. } => (Outcome::Ok, Severity::Info, reason),
        NotFetched::Failed(_) if fetched_before => {
            (Outcome::Ok, Severity::Warning, Reason::FetchFailed)
        }
        NotFetched::Failed(_) => (Outcome::Dropped, Severity::Error, Reason::FetchFailed),
    };
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let row = ProblemRow {
        first_seen_at_utc: first_seen.unwrap_or_else(|| now.clone()),
        last_seen_at_utc: now,
        tz_offset: Some(tz_offset),
        ..ProblemRow::new(
            "",
            Stage::Fetch,
            Scope::Entity(&entity_id),
            None,
            outcome,
            Problem::record(reason, err).severity(severity),
            None,
        )
    };
    let sql = crate::bulk::insert_sql::<ProblemRow>();
    // Audited: `sql` is built from `ProblemRow`'s associated consts,
    // never from row data; all values bound.
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("record the fetch problem of {entity_id}"))?;
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
    now: &datalib_time::IsoOffsetTimestamp,
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
    let now = datalib_time::IsoOffsetTimestamp::now_local();
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
///
/// Two sets, because "what to render" and "what the diff named" part ways
/// when a fan-out table changed: then every bucket renders, but the ones
/// the diff named are still the only ones that can have vanished, and a
/// provider that stopped probing them left every document deleted in
/// that range in the store for good.
#[derive(Debug, Clone, Default)]
pub struct DiffScan {
    /// The buckets to load and render. `None` → every bucket: no cursor,
    /// a cursor this store cannot resolve, a `global_fanout_tables` row
    /// changed, or no doltlite extension.
    pub render: Option<std::collections::HashSet<String>>,
    /// The buckets the diff named as added, modified or removed since
    /// the cursor — the ones to probe for removal. `None` only when there
    /// is no range to diff over.
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
            render: None,
            changed_buckets: None,
            new_head,
            scan_elapsed: None,
        });
    };

    let mut fanout_changed = false;
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
            fanout_changed = true;
            break;
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
        // Datalib never replaces a store file, so a cursor the store cannot
        // resolve means somebody did it by hand. Loud, every run, until
        // the next successful pass writes a cursor this store knows.
        Err(e) => {
            tracing::warn!(
                error = %e,
                from_ref,
                "dolt_diff scan could not use this cursor — cold-starting (render everything)"
            );
            return Ok(DiffScan {
                render: None,
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
        render: if fanout_changed {
            None
        } else {
            Some(set.clone())
        },
        changed_buckets: Some(set),
        new_head,
        scan_elapsed: Some(elapsed),
    })
}

/// The primary-key columns of `table`, in `pragma_table_info` order.
pub async fn primary_key_columns(pool: &SqlitePool, table: &str) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT name FROM pragma_table_info(?) WHERE pk > 0 ORDER BY pk")
        .bind(table)
        .fetch_all(pool)
        .await
        .with_context(|| format!("pragma_table_info({table})"))?;
    rows.into_iter()
        .map(|r| {
            use sqlx::Row;
            r.try_get::<String, _>(0).map_err(Into::into)
        })
        .collect()
}

/// The primary keys of every row of `table` that is not `unchanged`
/// between `from_ref` and `to_ref`, rendered as text the way
/// `render_inputs.input_id` is: a composite key's columns in
/// `pragma_table_info` order, joined by `|`. A removed row's key comes
/// from its `from_` side, so a deletion names the row that left.
pub async fn changed_keys(
    pool: &SqlitePool,
    table: &str,
    from_ref: &str,
    to_ref: &str,
) -> Result<Vec<String>> {
    let pk = primary_key_columns(pool, table).await?;
    if pk.is_empty() {
        anyhow::bail!("{table} has no primary key, so dolt_diff_{table} cannot name its rows");
    }
    let parts: Vec<String> = pk
        .iter()
        .map(|c| format!("CAST(coalesce(to_{c}, from_{c}) AS TEXT)"))
        .collect();
    let sql = format!(
        "SELECT {} FROM dolt_diff_{table} \
          WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'",
        parts.join(" || '|' || ")
    );
    // Audited: `table` and its columns come from `sqlite_master` and
    // `pragma_table_info` of the store itself; both refs are bound.
    sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql))
        .bind(from_ref)
        .bind(to_ref)
        .fetch_all(pool)
        .await
        .with_context(|| format!("dolt_diff_{table} keys from {from_ref} to {to_ref}"))
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
    let rows = sqlx::query("SELECT scope, last_seen_at_utc FROM sync_scope_state")
        .fetch_all(pool)
        .await
        .context("select sync_scope_state")?;
    let mut out = HashMap::with_capacity(rows.len());
    for r in rows {
        let scope: String = r.try_get("scope").unwrap_or_default();
        let ts: String = r.try_get("last_seen_at_utc").unwrap_or_default();
        if !scope.is_empty() && !ts.is_empty() {
            out.insert(scope, ts);
        }
    }
    Ok(out)
}

/// `last_seen_at` is whatever offset-bearing stamp the provider chose;
/// the row keeps it as UTC with the offset beside it. A value that is
/// not a stamp at all (email keeps its opaque JMAP state tokens and
/// Gmail history ids here) is kept as written, with no offset.
pub async fn upsert_scope_state(pool: &SqlitePool, scope: &str, last_seen_at: &str) -> Result<()> {
    let stamp = datalib_time::split_stamp(last_seen_at);
    sqlx::query(
        "INSERT INTO sync_scope_state (scope, last_seen_at_utc, tz_offset) VALUES (?, ?, ?)
         ON CONFLICT(scope) DO UPDATE SET last_seen_at_utc = excluded.last_seen_at_utc,
            tz_offset = excluded.tz_offset",
    )
    .bind(scope)
    .bind(&stamp.utc)
    .bind(&stamp.tz_offset)
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

    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    /// `SHARED_TABLES` is what the mirror engine and a content diff
    /// read; a table added to `SHARED_DDL` without it would be dropped
    /// by the next mirror run and recreated by the next open, forever.
    #[test]
    fn shared_tables_names_every_shared_ddl_table() {
        let from_ddl: Vec<String> = std::iter::once(datalib_store_meta::DDL)
            .chain(SHARED_DDL.iter().copied())
            .filter_map(parse_create_table_name)
            .collect();
        assert_eq!(from_ddl, SHARED_TABLES);
    }

    /// The stamp is what `datalib_history` parses back out, so its
    /// shape is a contract: one trailing ` run=<id>`, and nothing when
    /// there is no run.
    #[test]
    fn commit_messages_carry_the_run_id_as_a_trailing_stamp() {
        assert_eq!(
            stamp_run_with("download slack: msgs=4", Some("0199-abc")),
            "download slack: msgs=4 run=0199-abc"
        );
        assert_eq!(stamp_run_with("seal", Some("  ")), "seal");
        assert_eq!(stamp_run_with("seal", None), "seal");
    }

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
            .idle_timeout(None)
            .max_lifetime(None)
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

    /// Two writers on one store used to make each other's `dolt_commit`
    /// fail with `commit conflict` — a timing bug, because the second
    /// open itself succeeded. Now the second open is the failure: the
    /// first's connection holds the file's writer lock for as long as it
    /// lives, and the refusal names it. A reader is not a writer and
    /// opens beside it; and once the writer has `close().await`ed the
    /// store is free again — that call waits for the connection to
    /// close, and the connection is what held the lock.
    #[tokio::test]
    async fn a_second_writer_on_a_live_store_is_refused_and_names_the_holder() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("entities.doltlite_db");
        let first = open_test(&db).await;
        assert!(FileLock::is_held(&lock_path_for(&db)));

        let owned = test_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let err = open(&db, &slices)
            .await
            .expect_err("a second writer must be refused");
        let msg = format!("{err:#}");
        assert!(msg.contains("already has a writer"), "{msg}");
        assert!(
            msg.contains(&format!("pid {}", std::process::id())),
            "the holder is named: {msg}"
        );
        let _reader = open_reader(&db, None)
            .await
            .expect("a reader is not a writer");

        first.close().await;
        assert!(
            !FileLock::is_held(&lock_path_for(&db)),
            "close() waited for the connection, and the connection held the lock"
        );
        let again = open(&db, &slices)
            .await
            .expect("free once the writer closed");
        again.close().await;
    }

    // ── Opening costs nothing ─────────────────────────────────────

    /// Re-opening a store nobody wrote to must not change one byte of it.
    ///
    /// Asserting on the byte size is the point: this leak left `dolt_log`
    /// unchanged, `dolt_status` clean and every step reporting no work, so
    /// every cheaper proxy was already true while it was live.
    ///
    /// It also holds the reason `checkout_writer_branch` uses
    /// `dolt_connect_branch`: `dolt_checkout` persists a working set for
    /// the branch it leaves and the one it enters, which put ~500 bytes
    /// into an untouched store on every open and nothing else noticed.
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

    /// A record the config told us not to fetch is not a failure. It
    /// reads `info` / `ok` with the rule's own reason, where a failure
    /// on a never-fetched record reads `error` / `dropped` — and it
    /// stays in `failed_ids`, which is what picks the file up if the
    /// limit is raised.
    #[tokio::test]
    async fn a_skip_the_config_asked_for_is_not_a_failed_fetch() {
        use datalib_problems::{Reason, Severity};
        let d = tempdir().unwrap();
        let p = d.path().join("s.doltlite_db");
        let pool = open_test(&p).await;

        let mut tx = pool.begin().await.unwrap();
        record_object_skipped(
            &mut tx,
            "widgets",
            "w1",
            Reason::OverSizeLimit,
            "size 25107330 > limit 5000000",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let row = sqlx::query(
            "SELECT severity, outcome, reason, sample FROM problems WHERE scope_key = ?",
        )
        .bind("widgets:w1")
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.get::<String, _>(0), Severity::Info.as_str());
        assert_eq!(row.get::<String, _>(1), "ok", "nothing was lost");
        assert_eq!(row.get::<String, _>(2), Reason::OverSizeLimit.as_str());
        assert_eq!(row.get::<String, _>(3), "size 25107330 > limit 5000000");

        assert_eq!(
            failed_ids(&pool, "widgets").await.unwrap(),
            vec!["w1".to_string()],
            "a skip stays eligible, so raising the limit picks it up"
        );

        // And a real failure on the same table still reads as one.
        let mut tx = pool.begin().await.unwrap();
        record_object_error(&mut tx, "widgets", "w2", "HTTP 500")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let failed = sqlx::query("SELECT severity, reason FROM problems WHERE scope_key = ?")
            .bind("widgets:w2")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(failed.get::<String, _>(0), Severity::Error.as_str());
        assert_eq!(failed.get::<String, _>(1), Reason::FetchFailed.as_str());
    }

    /// A failed fetch is a `problems` row on the entity: an error
    /// when the record has never fetched, a warning when an earlier
    /// fetch left something behind; the same failure twice is one row
    /// with its first-seen stamp kept; and a fetch that succeeds — by
    /// the single or the bulk path — clears it.
    #[tokio::test]
    async fn a_failed_fetch_is_a_problem_until_the_record_fetches() {
        use datalib_problems::Severity;
        let d = tempdir().unwrap();
        let p = d.path().join("f.doltlite_db");
        let pool = open_test(&p).await;
        let rows = |pool: &SqlitePool| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "SELECT severity, scope_key, sample, first_seen_at_utc FROM problems \
                     ORDER BY scope_key",
                )
                .fetch_all(&pool)
                .await
                .unwrap()
                .iter()
                .map(|r| {
                    (
                        r.get::<String, _>(0),
                        r.get::<String, _>(1),
                        r.get::<String, _>(2),
                        r.get::<String, _>(3),
                    )
                })
                .collect::<Vec<_>>()
            }
        };

        let mut tx = pool.begin().await.unwrap();
        record_object_error(&mut tx, "widgets", "w1", "HTTP 500")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let first = rows(&pool).await;
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].0,
            Severity::Error.as_str(),
            "never fetched: the record is missing"
        );
        assert_eq!(first[0].1, "widgets:w1");
        assert_eq!(first[0].2, "HTTP 500");

        let mut tx = pool.begin().await.unwrap();
        record_object_error(&mut tx, "widgets", "w1", "HTTP 503")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let again = rows(&pool).await;
        assert_eq!(again.len(), 1, "one row per entity, not one per attempt");
        assert_eq!(again[0].2, "HTTP 503", "the newest error");
        assert_eq!(again[0].3, first[0].3, "first seen is kept");

        let mut tx = pool.begin().await.unwrap();
        record_object_attempt(&mut tx, "widgets", "w1", None)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(rows(&pool).await.is_empty(), "a fetch clears it");

        // Fetched once, then failing: what the reader has is stale, not
        // missing.
        let mut tx = pool.begin().await.unwrap();
        record_object_error(&mut tx, "widgets", "w1", "HTTP 429")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let stale = rows(&pool).await;
        assert_eq!(stale[0].0, Severity::Warning.as_str());

        // The bulk success path clears it too.
        let mut tx = pool.begin().await.unwrap();
        crate::bulk::bulk_upsert_bookkeeping(
            &mut tx,
            "widgets",
            ["w1"],
            &datalib_time::IsoOffsetTimestamp::now_local(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(rows(&pool).await.is_empty());
        pool.close().await;
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
            fetched_at_utc TEXT NULL,
            attempt_count INTEGER NOT NULL,
            last_attempt_at_utc TEXT NULL,
            last_error TEXT NULL
        )";
        {
            let pool = open(&p, &[WIDGETS_DDL, old_bk]).await.unwrap();
            sqlx::query("INSERT INTO widgets_bookkeeping (id, attempt_count) VALUES ('w1', 3)")
                .execute(&pool)
                .await
                .unwrap();
            commit_run(&pool, "setup").await.unwrap();
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
            commit_run(&pool, "setup").await.unwrap();
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

    /// A reset empties content, cursors and the run log alike and
    /// commits, so the rows are still in history, and the tables stay:
    /// a reader diffing from before it sees every row deleted.
    #[tokio::test]
    async fn a_reset_empties_everything_and_keeps_it_in_history() {
        let d = tempdir().unwrap();
        let p = d.path().join("entities.doltlite_db");
        const EDGE: &str =
            "CREATE TABLE IF NOT EXISTS edges (id TEXT PRIMARY KEY, blake3 TEXT NULL)";
        let pool = open(&p, &[WIDGETS_DDL, EDGE]).await.unwrap();
        if !has_dolt_extensions(&pool).await {
            pool.close().await;
            return;
        }
        for sql in [
            "INSERT INTO widgets (id) VALUES ('w1')",
            "INSERT INTO edges VALUES ('e1', 'aa')",
            "INSERT INTO sync_scope_state (scope, last_seen_at_utc) VALUES ('s', 't')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        start_run(&pool, &json!({})).await.unwrap();
        commit_run(&pool, "setup").await.unwrap();
        let commits_before = count(&pool, "dolt_log").await;
        pool.close().await;

        reset_store(&p).await.unwrap();
        let pool = open(&p, &[WIDGETS_DDL, EDGE]).await.unwrap();
        for table in ["widgets", "edges", "sync_scope_state", "sync_runs"] {
            assert_eq!(count(&pool, table).await, 0, "{table} emptied");
        }
        assert_eq!(
            count(&pool, "dolt_log").await,
            commits_before + 1,
            "the reset is one commit, and the reopen finds nothing to change"
        );
        let deleted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM dolt_diff_widgets WHERE diff_type = 'removed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(deleted, 1, "the row reads as deleted, not as a table gone");
        let logged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_at_sync_runs('HEAD~1')")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(logged, 1, "and it is still in history");
        pool.close().await;
    }

    async fn count(pool: &SqlitePool, table: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM \"{table}\""
        )))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// A column the DDL no longer declares cannot be reached by ADD, so a
    /// raw store refuses the open: the error names the table and the
    /// column, and the file — the column, the row — is exactly as it was.
    /// A reset empties the table without needing the DDL, and the next
    /// open, finding nothing to lose, rebuilds it to the new shape.
    #[tokio::test]
    async fn a_removed_column_is_refused_untouched_and_gone_once_reset() {
        let d = tempdir().unwrap();
        let p = d.path().join("recreate.doltlite_db");
        {
            let pool = open(&p, &[STALE_WIDGETS_DDL]).await.unwrap();
            sqlx::query("INSERT INTO widgets (id, legacy_col) VALUES ('w1', 'x')")
                .execute(&pool)
                .await
                .unwrap();
            commit_run(&pool, "setup").await.unwrap();
            pool.close().await;
        }

        let err = match open(&p, &[WIDGETS_DDL]).await {
            Ok(pool) => {
                pool.close().await;
                panic!("a removed column must refuse the open");
            }
            Err(e) => e,
        };
        let brk = err
            .downcast_ref::<SchemaBreak>()
            .unwrap_or_else(|| panic!("a SchemaBreak, got {err:#}"));
        assert_eq!(brk.breaks.len(), 1);
        assert_eq!(brk.breaks[0].0, "widgets");
        assert!(
            brk.breaks[0].1.contains("unexpected: [legacy_col]"),
            "{}",
            brk.breaks[0].1
        );
        assert!(brk.to_string().contains("--reset"));

        // Untouched: the column and the row are still there, and the
        // shape the DDL that wrote it declares still opens it.
        let pool = open(&p, &[STALE_WIDGETS_DDL]).await.unwrap();
        let v: String = sqlx::query_scalar("SELECT legacy_col FROM widgets WHERE id = 'w1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, "x");
        pool.close().await;

        reset_store(&p).await.unwrap();
        let pool = open(&p, &[WIDGETS_DDL])
            .await
            .expect("an emptied table is rebuilt, not refused");
        let cols = table_columns(&pool, "widgets").await.unwrap();
        assert!(
            !cols.iter().any(|c| c.name == "legacy_col"),
            "legacy_col should be gone after drop+recreate"
        );
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM widgets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        pool.close().await;
    }

    /// A rename the reconcile would refuse goes through when the owner
    /// declares it as a rung: the rung runs against the old shape, the
    /// rows survive, the store records the version and the commit, the
    /// DDL then matches, and a second open runs nothing. A store already
    /// above the ladder's top is refused — a newer build migrated it.
    #[tokio::test]
    async fn a_ladder_carries_a_store_across_a_rename() {
        let d = tempdir().unwrap();
        let p = d.path().join("ladder.doltlite_db");
        const V0: &str = "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n TEXT)";
        const V1: &str = "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, m TEXT)";
        const LADDER: &[Migration] = &[Migration {
            version: 1,
            name: "t.n becomes t.m",
            apply: |c| {
                Box::pin(async move {
                    sqlx::query("ALTER TABLE t RENAME COLUMN n TO m")
                        .execute(&mut *c)
                        .await?;
                    Ok(())
                })
            },
        }];
        {
            let pool = open(&p, &[V0]).await.unwrap();
            sqlx::query("INSERT INTO t VALUES ('a', 'kept')")
                .execute(&pool)
                .await
                .unwrap();
            commit_run(&pool, "rows").await.unwrap();
            pool.close().await;
        }

        let pool = open_migrating(&p, &[V1], LADDER)
            .await
            .expect("the rung carries it");
        let m: String = sqlx::query_scalar("SELECT m FROM t WHERE id = 'a'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(m, "kept");
        let meta = datalib_store_meta::read(&pool).await.unwrap().unwrap();
        assert_eq!(meta.schema_version, 1);
        let messages: Vec<String> = sqlx::query_scalar("SELECT message FROM dolt_log()")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("migrate v1: t.n becomes t.m")),
            "{messages:?}"
        );
        let commits = messages.len();
        pool.close().await;

        // Again: nothing pending, nothing committed.
        let pool = open_migrating(&p, &[V1], LADDER).await.unwrap();
        let again: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(again as usize, commits);
        pool.close().await;

        // A build whose ladder is shorter than the store's version.
        let err = match open(&p, &[V1]).await {
            Ok(pool) => {
                pool.close().await;
                panic!("a store above the ladder must refuse");
            }
            Err(e) => e,
        };
        assert!(
            err.downcast_ref::<datalib_store_meta::ladder::AheadOfLadder>()
                .is_some(),
            "{err:#}"
        );

        // A reset store keeps its version: nothing is left to climb.
        reset_store(&p).await.unwrap();
        let pool = open_migrating(&p, &[V1], LADDER)
            .await
            .expect("a reset store opens");
        let meta = datalib_store_meta::read(&pool).await.unwrap().unwrap();
        assert_eq!(meta.schema_version, 1);
        pool.close().await;
    }

    /// Every non-additive change to a table with rows refuses, and the
    /// message says which: a changed type, a `NOT NULL` added, a key
    /// moved, a column that needs a value existing rows do not have.
    #[tokio::test]
    async fn every_non_additive_change_is_refused_by_name() {
        let d = tempdir().unwrap();
        let cases: &[(&str, &str, &str)] = &[
            (
                "type",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n TEXT)",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n INTEGER)",
            ),
            (
                "not_null",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n TEXT)",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n TEXT NOT NULL)",
            ),
            (
                "pk",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n TEXT)",
                "CREATE TABLE IF NOT EXISTS t (id TEXT, n TEXT, PRIMARY KEY (id, n))",
            ),
            (
                "default",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n INTEGER DEFAULT 0)",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n INTEGER DEFAULT 1)",
            ),
            (
                "rename",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n TEXT)",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, m TEXT)",
            ),
            (
                "not_null_no_default",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)",
                "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, n TEXT NOT NULL)",
            ),
        ];
        for (name, before, after) in cases {
            let p = d.path().join(format!("{name}.doltlite_db"));
            let pool = open(&p, &[before]).await.unwrap();
            sqlx::query("INSERT INTO t (id) VALUES ('x')")
                .execute(&pool)
                .await
                .unwrap();
            commit_run(&pool, "setup").await.unwrap();
            pool.close().await;
            match open(&p, &[after]).await {
                Ok(pool) => {
                    pool.close().await;
                    panic!("{name}: must refuse");
                }
                Err(e) => {
                    let brk = e
                        .downcast_ref::<SchemaBreak>()
                        .unwrap_or_else(|| panic!("{name}: a SchemaBreak, got {e:#}"));
                    assert_eq!(brk.breaks[0].0, "t", "{name}");
                    assert!(brk.breaks[0].1.contains('n'), "{name}: {}", brk.breaks[0].1);
                }
            }
        }
    }

    /// The additive changes an existing store absorbs without a refusal
    /// and without losing a row: a nullable column, one with a default,
    /// and a VIRTUAL generated column — the shape the docs recommend for
    /// a field derivable from the payload.
    #[tokio::test]
    async fn additive_changes_are_added_in_place() {
        let d = tempdir().unwrap();
        let p = d.path().join("add.doltlite_db");
        const BEFORE: &str = "CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY, payload TEXT)";
        const AFTER: &str = "CREATE TABLE IF NOT EXISTS t (
            id TEXT PRIMARY KEY,
            payload TEXT,
            note TEXT NULL,
            n INTEGER NOT NULL DEFAULT 0,
            kind TEXT GENERATED ALWAYS AS (json_extract(payload, '$.kind')) VIRTUAL
        )";
        {
            let pool = open(&p, &[BEFORE]).await.unwrap();
            sqlx::query("INSERT INTO t (id, payload) VALUES ('a', '{\"kind\":\"x\"}')")
                .execute(&pool)
                .await
                .unwrap();
            commit_run(&pool, "setup").await.unwrap();
            pool.close().await;
        }
        let pool = open(&p, &[AFTER]).await.expect("additive");
        let (n, kind): (i64, String) = sqlx::query_as("SELECT n, kind FROM t WHERE id = 'a'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            (n, kind.as_str()),
            (0, "x"),
            "the row is kept and the new columns read"
        );
        // And the second open finds nothing to do: the shapes agree.
        pool.close().await;
        let pool = open(&p, &[AFTER]).await.unwrap();
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        let pool = open(&p, &[AFTER]).await.unwrap();
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, before, "an unchanged shape commits nothing");
        pool.close().await;
    }

    /// A table that appears in a store that already had others is as
    /// empty as a recreated one, so the cursors go; a store whose every
    /// table is new is a first open and has none to lose.
    #[tokio::test]
    async fn a_new_table_forgets_the_stores_cursors() {
        let d = tempdir().unwrap();
        let p = d.path().join("new_table.doltlite_db");
        store_with_cursors(&p, &[WIDGETS_DDL]).await;

        const GADGETS: &str = "CREATE TABLE IF NOT EXISTS gadgets (id TEXT PRIMARY KEY)";
        let pool = open(&p, &[WIDGETS_DDL, GADGETS]).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM widgets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "the table that was there keeps its row");
        assert_eq!(
            cursor_counts(&pool).await,
            (0, 0, 0),
            "a new, empty table is one the cursors would skip past"
        );
        pool.close().await;
    }

    /// `column_clause` hands back a column's definition as the DDL wrote
    /// it, whatever punctuation the other columns carry.
    #[test]
    fn column_clause_reads_a_definition_out_of_the_ddl() {
        const DDL: &str = "CREATE TABLE IF NOT EXISTS t (
            id TEXT PRIMARY KEY,
            n INTEGER NOT NULL DEFAULT 0,
            note TEXT DEFAULT 'a, b (c)',
            kind TEXT GENERATED ALWAYS AS (json_extract(payload, '$.kind')) VIRTUAL,
            UNIQUE (id, n)
        )";
        assert_eq!(
            column_clause(DDL, "n").as_deref(),
            Some("n INTEGER NOT NULL DEFAULT 0")
        );
        assert_eq!(
            column_clause(DDL, "note").as_deref(),
            Some("note TEXT DEFAULT 'a, b (c)'")
        );
        assert_eq!(
            column_clause(DDL, "kind").as_deref(),
            Some("kind TEXT GENERATED ALWAYS AS (json_extract(payload, '$.kind')) VIRTUAL")
        );
        assert_eq!(column_clause(DDL, "missing"), None);
    }

    const STALE_WIDGETS_DDL: &str = "CREATE TABLE IF NOT EXISTS widgets (
            id TEXT PRIMARY KEY,
            name TEXT NULL,
            payload TEXT NULL,
            legacy_col TEXT NULL
        )";

    /// A store with every kind of resume cursor a provider keeps: a scope
    /// cursor, the scope's config record, and a file checkpoint.
    async fn store_with_cursors(p: &Path, ddl: &[&str]) {
        let pool = open(p, ddl).await.unwrap();
        crate::file_checkpoint::ensure_schema(&pool).await.unwrap();
        sqlx::query("INSERT INTO widgets (id, name) VALUES ('w1', 'gadget')")
            .execute(&pool)
            .await
            .unwrap();
        upsert_scope_state(&pool, "widgets/walk", "2026-09-18T10:00:00+00:00")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sync_scope_config (scope, config, updated_at_utc, tz_offset) \
             VALUES ('widgets', '{}', '2026-09-18T10:00:00+00:00', '+00:00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ingested_files (scope, rel_path, blake3, size_bytes, last_finished_at_utc) \
             VALUES ('widgets/files', 'a.json', 'aa', 1, '2026-09-18T10:00:00+00:00')",
        )
        .execute(&pool)
        .await
        .unwrap();
        commit_run(&pool, "setup").await.unwrap();
        pool.close().await;
    }

    async fn cursor_counts(pool: &SqlitePool) -> (i64, i64, i64) {
        let n = |sql: &'static str| async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(pool)
                .await
                .unwrap()
        };
        (
            n("SELECT COUNT(*) FROM sync_scope_state").await,
            n("SELECT COUNT(*) FROM sync_scope_config").await,
            n("SELECT COUNT(*) FROM ingested_files").await,
        )
    }

    /// A recreated table is empty, and a cursor that survived it would
    /// let the next run resume past rows the table no longer has: the
    /// store then stays empty until upstream changes, with nothing saying
    /// why. Recreating forgets every cursor in the store.
    #[tokio::test]
    async fn a_recreate_forgets_the_stores_cursors() {
        let d = tempdir().unwrap();
        let p = d.path().join("recreate_cursors.doltlite_db");
        store_with_cursors(&p, &[STALE_WIDGETS_DDL]).await;

        let pool = open_with(&p, &[WIDGETS_DDL], OnSchemaBreak::Rebuild)
            .await
            .unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM widgets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "the column removal recreated the table");
        assert_eq!(
            cursor_counts(&pool).await,
            (0, 0, 0),
            "every cursor must go with the rows it pointed past"
        );
        pool.close().await;
    }

    /// The other reconcile path, pinned so the two cannot drift: an added
    /// column keeps the rows, so it keeps the cursors too.
    #[tokio::test]
    async fn an_added_column_keeps_the_stores_cursors() {
        let d = tempdir().unwrap();
        let p = d.path().join("add_keeps_cursors.doltlite_db");
        store_with_cursors(&p, &[WIDGETS_DDL]).await;

        let pool = open(&p, &[STALE_WIDGETS_DDL]).await.unwrap();
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM widgets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "ADD COLUMN keeps the row");
        assert_eq!(cursor_counts(&pool).await, (1, 1, 1));
        pool.close().await;
    }

    #[tokio::test]
    async fn open_creates_tables_idempotently() {
        let d = tempdir().unwrap();
        let p = d.path().join("x.doltlite_db");
        open_test(&p).await.close().await;
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
                .idle_timeout(None)
                .max_lifetime(None)
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
                "INSERT INTO widgets_bookkeeping (id, fetched_at_utc, attempt_count) VALUES ('w1', '2026-06-03T00:00:00Z', 0)",
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

            // Delete + reinsert IDENTICAL data, plus a new fetched_at_utc — the
            // integration-test shape.
            for sql in [
                "DELETE FROM widgets",
                "DELETE FROM widgets_bookkeeping",
                "INSERT INTO widgets (id, name, payload) VALUES ('w1', 'one', NULL)",
                "INSERT INTO widgets_bookkeeping (id, fetched_at_utc, attempt_count) VALUES ('w1', '2026-06-03T00:00:05Z', 0)",
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
    /// A fan-out hit renders everything and *still* names what the diff
    /// saw: a bucket deleted in the same range as a fan-out row is
    /// probed for removal, not lost. Found by the render model test on
    /// its first seed — every diff-narrowed provider had the hole.
    #[tokio::test]
    async fn a_fanout_hit_renders_everything_but_keeps_the_named_buckets() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = open(
            &tmp.path().join("scan.doltlite_db"),
            &[
                "CREATE TABLE IF NOT EXISTS notes (id TEXT PRIMARY KEY, bucket TEXT)",
                "CREATE TABLE IF NOT EXISTS users (id TEXT PRIMARY KEY, name TEXT)",
            ],
        )
        .await
        .unwrap();
        if !has_dolt_extensions(&pool).await {
            return;
        }
        for sql in [
            "INSERT INTO notes VALUES ('n1', 'b1')",
            "INSERT INTO notes VALUES ('n2', 'b2')",
            "INSERT INTO users VALUES ('u1', 'ann')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let first = commit_run(&pool, "two notes").await.unwrap().unwrap();
        for sql in [
            "DELETE FROM notes WHERE id = 'n2'",
            "UPDATE users SET name = 'anne' WHERE id = 'u1'",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let second = commit_run(&pool, "n2 gone, ann renamed")
            .await
            .unwrap()
            .unwrap();
        let pin = crate::pin::Pin::at(&second).unwrap();
        let spec = DiffScanSpec {
            global_fanout_tables: &["users"],
            bucket_query: "SELECT DISTINCT coalesce(to_bucket, from_bucket) FROM dolt_diff_notes \
                           WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'",
        };
        let scan = scan_buckets(&pool, Some(&first), &pin, &spec)
            .await
            .unwrap();
        assert!(scan.render.is_none(), "a user changed: every note renders");
        let named = scan
            .changed_buckets
            .expect("the range is still there to probe");
        assert_eq!(
            named,
            ["b2".to_string()].into_iter().collect(),
            "the deleted note's bucket is still named for the removal probe"
        );
        pool.close().await;
    }

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
            scan.render.is_none() && scan.changed_buckets.is_none(),
            "cold start means `None`, i.e. render everything, and no range to probe"
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

        let reader = open_reader(&path, Some(&commit))
            .await
            .unwrap()
            .expect("a committed store is readable");
        let err = sqlx::query("INSERT INTO t VALUES (2)")
            .execute(reader.pool())
            .await
            .expect_err("a reader must not be able to write the store");
        assert!(
            err.to_string().contains("readonly"),
            "expected a readonly-database error, got: {err}"
        );

        // The views were installed at open, on a read-only connection.
        assert_eq!(reader.pin().commit(), commit);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pinned_t")
            .fetch_one(reader.pool())
            .await
            .unwrap();
        assert_eq!(n, 1, "and the pinned read works through them");
        reader.close().await;

        // A store with nothing committed has nothing to read: no reader,
        // rather than one onto the working set. `open_derived`, so the
        // shared bookkeeping tables do not get committed on the way in.
        let empty = tmp.path().join("empty.doltlite_db");
        let w = open_derived(&empty, &[], StoreKind::Raw).await.unwrap();
        sqlx::query("CREATE TABLE u (id INTEGER PRIMARY KEY)")
            .execute(&w)
            .await
            .unwrap();
        w.close().await;
        drop(w);
        assert!(open_reader(&empty, None).await.unwrap().is_none());
    }

    /// Opening a store *discards* whatever it finds dirty: a writer that died
    /// between seals left rows no reader was ever promised, and sealing them
    /// would commit a torn state — a conversation without its blobs, half a
    /// channel. The store starts at its last commit, and an untracked table
    /// the dead writer created goes too, because `dolt_reset --hard` leaves
    /// it and the schema commit that follows would otherwise adopt it.
    #[tokio::test]
    async fn opening_a_store_discards_whatever_was_left_dirty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("crashed.doltlite_db");
        let a = open(
            &path,
            &["CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)"],
        )
        .await
        .unwrap();
        if !has_dolt_extensions(&a).await {
            return;
        }
        sqlx::query("INSERT INTO t VALUES (1, 'sealed')")
            .execute(&a)
            .await
            .unwrap();
        commit_run(&a, "baseline").await.unwrap();
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
            .fetch_one(&a)
            .await
            .unwrap();
        // The crash: an insert, an update, and a table created but never
        // committed.
        sqlx::query("INSERT INTO t VALUES (2, 'torn')")
            .execute(&a)
            .await
            .unwrap();
        sqlx::query("UPDATE t SET v = 'rewritten' WHERE id = 1")
            .execute(&a)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE half_made (id INTEGER PRIMARY KEY)")
            .execute(&a)
            .await
            .unwrap();
        sqlx::query("INSERT INTO half_made VALUES (9)")
            .execute(&a)
            .await
            .unwrap();
        a.close().await;

        // The same owner, with the same DDL: nothing about the shape moved.
        let b = open(
            &path,
            &["CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)"],
        )
        .await
        .unwrap();
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_log()")
            .fetch_one(&b)
            .await
            .unwrap();
        assert_eq!(after, before, "open() must not commit what it found dirty");
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, v FROM t ORDER BY id")
            .fetch_all(&b)
            .await
            .unwrap();
        assert_eq!(
            rows,
            vec![(1, "sealed".to_string())],
            "the working set is the last commit again"
        );
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'half_made'",
        )
        .fetch_all(&b)
        .await
        .unwrap();
        assert!(
            tables.is_empty(),
            "an untracked table the crash left is dropped"
        );
        let dirty: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_status")
            .fetch_one(&b)
            .await
            .unwrap();
        assert_eq!(
            dirty, 0,
            "and nothing is left for the schema commit to sweep"
        );
        b.close().await;
    }

    /// Every store an owner opens says which build wrote it, committed
    /// with its schema — so a pinned reader sees it at HEAD, and a store
    /// from before the table existed reads as `None` rather than failing.
    /// A shape change moves the hash and is committed under a message
    /// that names the build; the same build with the same DDL commits
    /// nothing.
    #[tokio::test]
    async fn every_store_names_the_build_that_wrote_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta.doltlite_db");
        const T1: &str = "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)";
        const T2: &str = "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)";

        let a = open(&path, &[T1]).await.unwrap();
        if !has_dolt_extensions(&a).await {
            return;
        }
        let meta = datalib_store_meta::read(&a)
            .await
            .unwrap()
            .expect("written at open");
        assert_eq!(meta.store_kind, Some(StoreKind::Raw));
        assert_eq!(
            meta.datalib_version,
            datalib_runtime::build_id::DATALIB_VERSION
        );
        let doltlite = meta.doltlite_version.as_deref().expect("dolt_version()");
        assert!(
            doltlite.split('.').count() == 3
                && doltlite.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "dolt_version() is a dotted number, got {doltlite:?}"
        );
        let hash1 = meta.schema_hash.clone();
        a.close().await;

        // At HEAD, through a pinned reader: the rows rode the schema commit.
        let reader = open_reader(&path, None).await.unwrap().expect("committed");
        let pinned: i64 = sqlx::query_scalar("SELECT count(*) FROM pinned__datalib_meta")
            .fetch_one(reader.pool())
            .await
            .unwrap();
        assert_eq!(pinned, 6, "every meta row is at HEAD");
        reader.close().await;

        // The shape moves: the hash moves with it, and the commit says so.
        let b = open(&path, &[T2]).await.unwrap();
        let meta = datalib_store_meta::read(&b).await.unwrap().unwrap();
        assert_ne!(meta.schema_hash, hash1, "a DDL change moves the hash");
        // Newest first without an ORDER BY, as `head_commit` reads it:
        // `date` is whole seconds and this test fits inside one.
        let message: String = sqlx::query_scalar("SELECT message FROM dolt_log() LIMIT 1")
            .fetch_one(&b)
            .await
            .unwrap();
        assert!(
            message.starts_with("schema: apply DDL (datalib "),
            "a commit that moved the meta names the build, got {message:?}"
        );
        b.close().await;

        // A store from before the table existed reads as absent.
        let old = tmp.path().join("old.doltlite_db");
        let c = connect_pool(&old, Access::ReadWrite, true).await.unwrap();
        sqlx::query(T1).execute(&c).await.unwrap();
        assert_eq!(datalib_store_meta::read(&c).await.unwrap(), None);
        c.close().await;
    }

    /// A store a newer line of datalib wrote is refused before anything
    /// touches it: the error names both versions, and the table keeps
    /// its rows and its columns — the reconcile that would have dropped
    /// them never ran. The same line's builds still open it.
    #[tokio::test]
    async fn a_store_a_newer_build_wrote_is_refused_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("newer.doltlite_db");
        const WIDE: &str = "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT)";
        const NARROW: &str = "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)";

        let a = open(&path, &[WIDE]).await.unwrap();
        if !has_dolt_extensions(&a).await {
            return;
        }
        sqlx::query("INSERT INTO t VALUES (1, 'kept')")
            .execute(&a)
            .await
            .unwrap();
        // What a newer release would have written.
        sqlx::query("UPDATE _datalib_meta SET value = '99.0.0' WHERE key = 'datalib_version'")
            .execute(&a)
            .await
            .unwrap();
        commit_run(&a, "as if by 99.0.0").await.unwrap();
        a.close().await;

        // An older build, with a DDL that would have dropped `v`.
        let err = match open(&path, &[NARROW]).await {
            Ok(pool) => {
                pool.close().await;
                panic!("an older build opened a newer store");
            }
            Err(e) => e,
        };
        let newer = err
            .downcast_ref::<datalib_store_meta::NewerBuild>()
            .unwrap_or_else(|| panic!("a NewerBuild error, got {err:#}"));
        assert_eq!(newer.wrote, "99.0.0");
        assert_eq!(newer.running, datalib_runtime::build_id::DATALIB_VERSION);

        // Untouched: the row and the column are still there.
        let reader = open_reader(&path, None).await.unwrap().unwrap();
        let v: String = sqlx::query_scalar("SELECT v FROM pinned_t WHERE id = 1")
            .fetch_one(reader.pool())
            .await
            .unwrap();
        assert_eq!(v, "kept");
        reader.close().await;

        // The same line again (a patch apart) is not a downgrade.
        let patch = {
            let mut parts: Vec<String> = datalib_runtime::build_id::DATALIB_VERSION
                .split('.')
                .map(String::from)
                .collect();
            let last = parts.last_mut().unwrap();
            *last = (last.parse::<u64>().unwrap() + 1).to_string();
            parts.join(".")
        };
        let c = connect_pool(&path, Access::ReadWrite, true).await.unwrap();
        sqlx::query("UPDATE _datalib_meta SET value = ? WHERE key = 'datalib_version'")
            .bind(&patch)
            .execute(&c)
            .await
            .unwrap();
        commit_run(&c, "as if by the next patch").await.unwrap();
        c.close().await;
        let d = open(&path, &[WIDE]).await.expect("a patch apart opens");
        d.close().await;
    }

    /// A schema commit carries schema and nothing else. `open` commits its
    /// DDL with `-Am`, which stages whatever is dirty; the only thing that
    /// keeps a crashed writer's rows out of that commit is the discard that
    /// runs first. So: leave a row uncommitted, reopen with a DDL that adds
    /// a column, and read the schema commit's row diff — it must be empty,
    /// and the table at HEAD must hold exactly what was committed before.
    #[tokio::test]
    async fn a_schema_commit_touches_no_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("schema_only.doltlite_db");
        const V1: &str = "CREATE TABLE IF NOT EXISTS widgets (id TEXT PRIMARY KEY, name TEXT)";
        const V2: &str =
            "CREATE TABLE IF NOT EXISTS widgets (id TEXT PRIMARY KEY, name TEXT, colour TEXT)";
        {
            let pool = open(&path, &[V1]).await.unwrap();
            if !has_dolt_extensions(&pool).await {
                return;
            }
            sqlx::query("INSERT INTO widgets VALUES ('w1', 'gadget'), ('w2', 'widget')")
                .execute(&pool)
                .await
                .unwrap();
            commit_run(&pool, "data").await.unwrap();
            // The crash: a row written after the last commit.
            sqlx::query("INSERT INTO widgets VALUES ('w3', 'torn')")
                .execute(&pool)
                .await
                .unwrap();
            pool.close().await;
        }

        let pool = open(&path, &[V2]).await.unwrap();
        let (head, message): (String, String) = sqlx::query_as(
            "SELECT commit_hash, message FROM dolt_log() ORDER BY date DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            message.starts_with("schema: apply DDL"),
            "the newest commit is the schema commit, got {message:?}"
        );
        let rows_in_diff: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM dolt_diff_widgets WHERE to_commit = ?")
                .bind(&head)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows_in_diff, 0, "a schema commit's row diff is empty");
        // Audited: the hash came from dolt_log a moment ago; the table is a literal.
        let at_head: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT id FROM dolt_at_widgets('{head}') ORDER BY id"
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(at_head, vec!["w1".to_string(), "w2".to_string()]);
        pool.close().await;
    }

    /// A writer's pool is on [`WRITER_BRANCH`], every connection of it.
    ///
    /// The guard against the quiet failure: a fresh connection is on the
    /// file's default branch, so a selection that does nothing leaves the
    /// writer on `main`, where every row it writes is visible to every
    /// reader the moment it lands rather than when it is sealed. Nothing
    /// else would notice — the rows are all there, the commits all
    /// happen, and only the isolation is gone.
    #[tokio::test]
    async fn a_writers_pool_is_on_the_writer_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("entities.doltlite_db");
        let pool = open(
            &path,
            &["CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap();
        if !has_dolt_extensions(&pool).await {
            return;
        }
        let active: String = sqlx::query_scalar("SELECT active_branch()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            active, WRITER_BRANCH,
            "a writer is on {active:?}; anything it writes is unsealed and \
             visible to every reader"
        );
        pool.close().await;

        // And the second open of the same file, where the branch already
        // exists and `dolt_connect_branch` is what puts us on it.
        let pool = open(
            &path,
            &["CREATE TABLE IF NOT EXISTS t (id TEXT PRIMARY KEY)"],
        )
        .await
        .unwrap();
        let active: String = sqlx::query_scalar("SELECT active_branch()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(active, WRITER_BRANCH, "reopen landed on {active:?}");
        pool.close().await;
    }

    /// A fresh connection starts on the file's **stored default branch**,
    /// and ours must stay `main` however many times a writer uses its own.
    ///
    /// Not "a connection always starts on `main`" — that is the
    /// consequence, not the rule. In doltlite the default branch is
    /// `zDefaultBranch` in the persisted refs block: seeding sets it to
    /// the branch it created, `csEnsureDefaultBranch` falls back to
    /// `main` only for a file carrying none, and `dolt_default_branch(x)`
    /// moves it. Nothing in this repo calls that — and if anything ever
    /// did, every reader would silently start on a writer's branch and
    /// read uncommitted rows, which is the failure this whole
    /// construction exists to prevent. So assert the rule, not the
    /// consequence.
    ///
    /// Measured through a *reader*: a writer is put on [`WRITER_BRANCH`]
    /// by `after_connect` whether or not it inherited anything, so it
    /// could not tell the two apart.
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
        if !has_dolt_extensions(&first).await {
            return;
        }
        // The writer is on its own branch, and a checkout somewhere else
        // again is still only this connection's business.
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

        let second = connect_pool(&path, Access::ReadOnly, false).await.unwrap();
        let active: String = sqlx::query_scalar("SELECT active_branch()")
            .fetch_one(&second)
            .await
            .unwrap();
        let default: String = sqlx::query_scalar("SELECT dolt_default_branch()")
            .fetch_one(&second)
            .await
            .unwrap();
        assert_eq!(
            default, "main",
            "the file's default branch moved; every reader now starts on \
             {default:?} and reads whatever a writer left uncommitted there"
        );
        assert_eq!(
            active, default,
            "a new connection did not start on the file's default branch"
        );
        second.close().await;
    }
}
