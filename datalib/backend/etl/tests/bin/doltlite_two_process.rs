//! One side of a two-process doltlite concurrency test: a writer that
//! commits, a reader that pins, a reader that keeps re-opening and pinning
//! the way `grid_index` does, or a probe that reports a store's committed
//! state. Driven by `tests/doltlite_two_process.rs`, which is where the
//! scenarios and the assertions live.
//!
//! It is a separate binary because the question under test is what happens
//! *between processes* — doltlite's working set and its chunk-store lock are
//! per file, not per connection, so two pools inside one process cannot
//! stand in for it. The test process itself never opens a store: doltlite
//! takes its chunk-store lock with BSD `flock`, which a spawned child
//! inherits (`hack/doltlite_fork_bug/README.md`), so a coordinator that held
//! a connection while spawning would be measuring that instead.
//!
//! Each role writes one JSON report to `--out` and exits; nothing is printed
//! to stdout, so a crashed child is distinguishable from a slow one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use datalib_etl::doltlite_raw;
use datalib_etl::pin::Pin;
use serde_json::{json, Value};

const TABLE_DDL: &str = "CREATE TABLE IF NOT EXISTS entities (id TEXT PRIMARY KEY, body TEXT NULL)";

/// Rows the seeding writer commits before anyone pins to it. Small and odd,
/// so a count that picked up a later chunk is obvious.
const SEED_ROWS: usize = 3;

/// Rows the `hold` writer loads after deleting the seed, inside the same
/// transaction. Different from `SEED_ROWS` so the three states a reader can
/// be in -- before, half-done, after -- each count differently.
const RELOAD_ROWS: usize = 5;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut argv = std::env::args().skip(1);
    let role = argv
        .next()
        .ok_or_else(|| anyhow!("usage: <role> [--flag value]…"))?;
    let args = Args::parse(argv)?;
    let out = args.path("out")?;

    let report = match role.as_str() {
        "write" => write(&args).await,
        "double-open" => double_open(&args).await,
        "read" => read(&args).await,
        "churn" => churn(&args).await,
        "history" => history(&args).await,
        "probe" => probe(&args).await,
        "hang" => hang(&args).await,
        "reopen" => reopen(&args).await,
        "hold" => hold(&args).await,
        "watch" => watch(&args).await,
        other => bail!("unknown role {other:?}"),
    }?;
    write_atomic(&out, &serde_json::to_vec_pretty(&report)?)
}

// ── roles ───────────────────────────────────────────────────────────

/// Open read-write and keep committing until told to stop, so the reader's
/// whole window is covered by a writer that is actively committing.
async fn write(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let started = Instant::now();
    // A refused open is a result, not a crash: the test reads what the
    // refusal said.
    let pool = match doltlite_raw::open(&db, &[TABLE_DDL]).await {
        Ok(pool) => pool,
        Err(e) => {
            return Ok(json!({ "role": "write", "open_error": format!("{e:#}") }));
        }
    };
    let open_ms = started.elapsed().as_millis() as u64;
    if !doltlite_raw::has_dolt_extensions(&pool).await {
        pool.close().await;
        return Ok(json!({ "role": "write", "dolt": false }));
    }

    let mut errors: Vec<String> = Vec::new();
    let mut seed_pin = Value::Null;
    if args.flag("seed") {
        for i in 0..SEED_ROWS {
            insert(&pool, &format!("seed-{i}")).await?;
        }
        let hash = doltlite_raw::commit_run(&pool, "seed")
            .await?
            .ok_or_else(|| anyhow!("the seed commit committed nothing"))?;
        seed_pin = json!(hash);
        // Only now, so a reader that sees the pin file sees a real commit.
        write_atomic(&args.path("pin-out")?, hash.as_bytes())?;
    }

    let until = args.opt_path("until");
    let interval = Duration::from_millis(args.num("interval-ms", 100));
    let max_commits = args.num("max-commits", 0) as usize;
    let mut commits: Vec<Value> = Vec::new();
    for i in 0..max_commits {
        if until.as_deref().is_some_and(Path::exists) {
            break;
        }
        match commit_a_chunk(&pool, i).await {
            Ok(hash) => commits.push(json!({ "hash": hash, "at_ms": now_ms() })),
            Err(e) => errors.push(format!("{e:#}")),
        }
        tokio::time::sleep(interval).await;
    }

    let committed = committed_rows(&pool).await;
    pool.close().await;
    Ok(json!({
        "role": "write",
        "dolt": true,
        "open_ms": open_ms,
        "seed_pin": seed_pin,
        "commits": commits,
        "committed_rows": committed,
        "errors": errors,
    }))
}

/// Open read-only, pin to a commit taken before the writer's chunks, and
/// sample the pinned view repeatedly. Every sample must agree.
async fn read(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let pin = args.str("pin")?;
    let started = Instant::now();
    let reader = doltlite_raw::open_reader(&db, Some(pin))
        .await
        .context("open read-only")?
        .context("the seed commit was published, so there is something to pin")?;
    let open_ms = started.elapsed().as_millis() as u64;
    let pool = reader.pool();
    write_atomic(&args.path("ready-out")?, b"ready")?;

    let interval = Duration::from_millis(args.num("interval-ms", 250));
    let mut samples: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for _ in 0..args.num("samples", 12) {
        match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pinned_entities")
            .fetch_one(pool)
            .await
        {
            Ok(n) => samples.push(json!({ "count": n, "at_ms": now_ms() })),
            Err(e) => errors.push(format!("{e:#}")),
        }
        tokio::time::sleep(interval).await;
    }

    let pin = reader.pin().commit().to_string();
    reader.close().await;
    Ok(json!({
        "role": "read",
        "open_ms": open_ms,
        "pin": pin,
        "samples": samples,
        "errors": errors,
    }))
}

/// What `grid_index` does to a render store on every streaming pass, in a
/// loop: open read-only, pin HEAD, install the views, diff, read through the
/// views, close. Every step is a read, so none of it should cost a writer
/// anything -- this is the role that finds out.
async fn churn(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let until = args.opt_path("until");
    let rounds = args.num("rounds", 300) as usize;
    let mut errors: Vec<String> = Vec::new();
    let mut samples: Vec<Value> = Vec::new();
    let mut opened = 0usize;
    let mut pinned = 0usize;
    // The commit the previous round read to, as `grid_index` keeps a cursor.
    let mut cursor: Option<String> = None;
    for round in 0..rounds {
        if until.as_deref().is_some_and(Path::exists) {
            break;
        }
        samples.push(json!({ "at_ms": now_ms() }));
        let reader = match doltlite_raw::open_reader(&db, None).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                opened += 1;
                continue;
            }
            Err(e) => {
                errors.push(format!("round {round}: open: {e:#}"));
                continue;
            }
        };
        opened += 1;
        match one_pinned_pass(&reader, cursor.as_deref()).await {
            Ok(head) => {
                pinned += 1;
                cursor = Some(head);
            }
            Err(e) => errors.push(format!("round {round}: {e:#}")),
        }
        reader.close().await;
    }
    Ok(json!({
        "role": "churn",
        "opened": opened,
        "pinned": pinned,
        "samples": samples,
        "errors": errors,
    }))
}

/// What the Manage screen's commit-history panel does, in a loop: open
/// read-only, walk `dolt_log` with a `dolt_diff_stat` per commit, close.
/// Every statement is a read, and `dolt_status` showed that is not the same
/// as being harmless to a writer -- this is where the history reader's
/// statements earn the same verdict.
async fn history(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let until = args.opt_path("until");
    let rounds = args.num("rounds", 300) as usize;
    let mut errors: Vec<String> = Vec::new();
    let mut samples: Vec<Value> = Vec::new();
    let mut opened = 0usize;
    let mut commits_seen = 0usize;
    for round in 0..rounds {
        if until.as_deref().is_some_and(Path::exists) {
            break;
        }
        samples.push(json!({ "at_ms": now_ms() }));
        match datalib_history::read(&db, 50).await {
            Ok(h) => {
                opened += 1;
                commits_seen = commits_seen.max(h.commits.len());
            }
            Err(e) => errors.push(format!("round {round}: {e:#}")),
        }
    }
    Ok(json!({
        "role": "history",
        "opened": opened,
        "commits_seen": commits_seen,
        "samples": samples,
        "errors": errors,
    }))
}

/// The commit this pass read at.
async fn one_pinned_pass(reader: &doltlite_raw::Reader, cursor: Option<&str>) -> Result<String> {
    let pool = reader.pool();
    let pin = reader.pin();
    let _rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pinned_entities")
        .fetch_one(pool)
        .await?;
    let _changed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM dolt_diff_entities \
          WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'",
    )
    .bind(cursor.unwrap_or(pin.commit()))
    .bind(pin.commit())
    .fetch_one(pool)
    .await?;
    Ok(pin.commit().to_string())
}

/// Two read-write pools on one file inside ONE process — the shape
/// `AGENTS.md` warns about, and the one the two-process scenarios do not
/// reach. Both opens are bounded, so a pool that really does wait for the
/// other reports a timeout instead of hanging out the test's clock.
/// Nowadays the second open is refused outright; this reports what it
/// said, and that the first still commits.
async fn double_open(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let budget = Duration::from_millis(args.num("budget-ms", 20_000));

    let first = doltlite_raw::open(&db, &[TABLE_DDL])
        .await
        .context("first open")?;
    let started = Instant::now();
    let second = tokio::time::timeout(budget, doltlite_raw::open(&db, &[TABLE_DDL])).await;
    let second_open_ms = started.elapsed().as_millis() as u64;

    let (second, second_err) = match second {
        Err(_) => (
            None,
            json!(format!("timed out after {}ms", budget.as_millis())),
        ),
        Ok(Err(e)) => (None, json!(e.to_string())),
        Ok(Ok(pool)) => (Some(pool), Value::Null),
    };

    // With both open, does either still commit?
    let mut through: Vec<Value> = Vec::new();
    for (chunk, (label, pool)) in [("first", Some(&first)), ("second", second.as_ref())]
        .into_iter()
        .enumerate()
    {
        let Some(pool) = pool else { continue };
        // A distinct chunk per pool: the same ids twice would write the same
        // values and commit nothing, which would read as contention.
        let started = Instant::now();
        let outcome = tokio::time::timeout(budget, commit_a_chunk(pool, chunk)).await;
        through.push(json!({
            "pool": label,
            "ms": started.elapsed().as_millis() as u64,
            "error": match outcome {
                Err(_) => json!("timed out"),
                Ok(Err(e)) => json!(e.to_string()),
                Ok(Ok(_)) => Value::Null,
            },
        }));
    }

    if let Some(second) = second {
        second.close().await;
    }
    first.close().await;
    Ok(json!({
        "role": "double-open",
        "second_open_ms": second_open_ms,
        "second_open_error": second_err,
        "commits": through,
    }))
}

/// Seed and commit, then open a SQL transaction, insert into it, announce
/// readiness and wait to be killed. With `--commit` the transaction is
/// committed at the SQL level first, so the rows sit in the working set —
/// uncommitted to doltlite — when the kill lands. Never returns on its own:
/// the test's `kill -9` is the whole point.
async fn hang(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let pool = doltlite_raw::open(&db, &[TABLE_DDL])
        .await
        .context("open read-write")?;
    if !doltlite_raw::has_dolt_extensions(&pool).await {
        pool.close().await;
        write_atomic(&args.path("out")?, b"{\"dolt\": false}")?;
        write_atomic(&args.path("ready-out")?, b"no-dolt")?;
        std::future::pending::<()>().await;
        unreachable!();
    }
    for i in 0..SEED_ROWS {
        insert(&pool, &format!("seed-{i}")).await?;
    }
    doltlite_raw::commit_run(&pool, "seed")
        .await?
        .ok_or_else(|| anyhow!("the seed commit committed nothing"))?;

    let mut conn = pool.acquire().await.context("acquire")?;
    sqlx::query("BEGIN")
        .execute(&mut *conn)
        .await
        .context("BEGIN")?;
    for i in 0..args.num("rows", 5) {
        sqlx::query("INSERT INTO entities (id, body) VALUES (?, ?)")
            .bind(format!("in-flight-{i}"))
            .bind("x")
            .execute(&mut *conn)
            .await
            .context("insert in-flight row")?;
    }
    if args.flag("commit") {
        sqlx::query("COMMIT")
            .execute(&mut *conn)
            .await
            .context("COMMIT")?;
    }
    write_atomic(&args.path("ready-out")?, b"ready")?;
    std::future::pending::<()>().await;
    unreachable!()
}

/// The next writer's view of a store a killed process left behind: what
/// the working set holds after `open` has discarded whatever was dirty,
/// what HEAD holds, and what the log says.
async fn reopen(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let pool = doltlite_raw::open(&db, &[TABLE_DDL])
        .await
        .context("reopen read-write")?;
    let working_set: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities")
        .fetch_one(&pool)
        .await
        .context("count the working set")?;
    let messages: Vec<String> = sqlx::query_scalar("SELECT message FROM dolt_log()")
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
    let committed = committed_rows(&pool).await;
    pool.close().await;
    Ok(json!({
        "role": "reopen",
        "working_set_rows": working_set,
        "committed_rows": committed,
        "commit_messages": messages,
    }))
}

/// What a fresh process sees once everyone else has let go.
async fn probe(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let pool = datalib_pin::open_reader(&db)
        .await
        .context("open read-only")?;
    let head = doltlite_raw::head_commit(&pool).await?;
    let committed = committed_rows(&pool).await;
    pool.close().await;
    Ok(json!({ "role": "probe", "head": head, "committed_rows": committed }))
}

/// What `grid_index` does to the index on every pass, one step at a time:
/// seed and commit, then inside one SQL transaction delete every row and
/// load a different number back, then `COMMIT`, then `dolt_commit`. Each
/// step waits for a go-file from the test and announces itself with an
/// out-file, so a reader in another process can be sampled between any
/// two of them. The report is what the writer itself saw at each step.
async fn hold(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let pool = doltlite_raw::open(&db, &[TABLE_DDL])
        .await
        .context("open read-write")?;
    if !doltlite_raw::has_dolt_extensions(&pool).await {
        pool.close().await;
        return Ok(json!({ "role": "hold", "dolt": false }));
    }
    for i in 0..SEED_ROWS {
        insert(&pool, &format!("seed-{i}")).await?;
    }
    let seed = doltlite_raw::commit_run(&pool, "seed")
        .await?
        .ok_or_else(|| anyhow!("the seed commit committed nothing"))?;
    write_atomic(&args.path("pin-out")?, seed.as_bytes())?;

    let mut conn = pool.acquire().await.context("acquire")?;
    let mut steps: Vec<Value> = Vec::new();
    let mut step = |name: &str, own: i64| steps.push(json!({ "step": name, "working": own }));

    await_file(&args.path("delete-when")?)?;
    sqlx::query("BEGIN")
        .execute(&mut *conn)
        .await
        .context("BEGIN")?;
    sqlx::query("DELETE FROM entities")
        .execute(&mut *conn)
        .await
        .context("DELETE")?;
    step("deleted", count_on(&mut conn).await?);
    write_atomic(&args.path("deleted-out")?, b"deleted")?;

    await_file(&args.path("reload-when")?)?;
    for i in 0..RELOAD_ROWS {
        sqlx::query("INSERT INTO entities (id, body) VALUES (?, ?)")
            .bind(format!("reload-{i}"))
            .bind("y")
            .execute(&mut *conn)
            .await
            .context("insert reload row")?;
    }
    step("reloaded", count_on(&mut conn).await?);
    write_atomic(&args.path("reloaded-out")?, b"reloaded")?;

    await_file(&args.path("sql-commit-when")?)?;
    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .context("COMMIT")?;
    step("sql_committed", count_on(&mut conn).await?);
    write_atomic(&args.path("sql-committed-out")?, b"sql-committed")?;

    await_file(&args.path("dolt-commit-when")?)?;
    drop(conn);
    let commit = doltlite_raw::commit_run(&pool, "reload")
        .await?
        .ok_or_else(|| anyhow!("the reload commit committed nothing"))?;
    write_atomic(&args.path("dolt-committed-out")?, commit.as_bytes())?;

    let committed = committed_rows(&pool).await;
    pool.close().await;
    Ok(json!({
        "role": "hold",
        "dolt": true,
        "seed_pin": seed,
        "commit": commit,
        "steps": steps,
        "committed_rows": committed,
    }))
}

async fn count_on(conn: &mut sqlx::SqliteConnection) -> Result<i64> {
    sqlx::query_scalar("SELECT COUNT(*) FROM entities")
        .fetch_one(conn)
        .await
        .context("count on the writer's connection")
}

/// A long-lived read-only handle -- the shape the search applet holds --
/// sampling three things on every tick: the working set (`COUNT(*)` on the
/// bare table), `dolt_hashof('HEAD')`, and the count at that HEAD through
/// `dolt_at_`. Each sample is tagged with the phase the test says the
/// writer is in, read from `--phase-file`, so the test can say what a
/// reader sees at each point of the writer's pass.
async fn watch(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    // Unpinned on purpose: the working-set count is one of the things
    // measured.
    let pool = datalib_pin::open_reader(&db)
        .await
        .context("open read-only")?;
    write_atomic(&args.path("ready-out")?, b"ready")?;
    let phase_file = args.path("phase-file")?;
    let until = args.path("until")?;
    let interval = Duration::from_millis(args.num("interval-ms", 25));

    let mut samples: Vec<Value> = Vec::new();
    while !until.exists() {
        let phase = std::fs::read_to_string(&phase_file).unwrap_or_default();
        let started = Instant::now();
        let working = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entities")
            .fetch_one(&pool)
            .await
            .map_err(|e| e.to_string());
        let head = sqlx::query_scalar::<_, Option<String>>("SELECT dolt_hashof('HEAD')")
            .fetch_one(&pool)
            .await
            .map_err(|e| e.to_string());
        let pinned = match &head {
            Ok(Some(h)) => match Pin::at(h.clone()) {
                Ok(pin) => {
                    // Audited: `Pin::at` checked the hash is 40 hex characters;
                    // the table name is a literal.
                    let sql = format!("SELECT COUNT(*) FROM dolt_at_entities('{}')", pin.commit());
                    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
                        .fetch_one(&pool)
                        .await
                        .map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            },
            Ok(None) => Err("no HEAD".into()),
            Err(e) => Err(e.clone()),
        };
        let phase_after = std::fs::read_to_string(&phase_file).unwrap_or_default();
        samples.push(json!({
            "phase": phase,
            // A sample that straddled a phase change proves nothing about
            // either phase; the test drops it.
            "phase_after": phase_after,
            "at_ms": now_ms(),
            "ms": started.elapsed().as_millis() as u64,
            "working": working.as_ref().ok(),
            "working_error": working.as_ref().err(),
            "head": head.as_ref().ok().cloned().flatten(),
            "head_error": head.as_ref().err(),
            "pinned": pinned.as_ref().ok(),
            "pinned_error": pinned.as_ref().err(),
        }));
        tokio::time::sleep(interval).await;
    }
    pool.close().await;
    Ok(json!({ "role": "watch", "samples": samples }))
}

/// Wait for a go-file the test writes, giving up rather than hanging the
/// test's clock.
fn await_file(path: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !path.exists() {
        if Instant::now() > deadline {
            bail!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

// ── store helpers ───────────────────────────────────────────────────

async fn insert(pool: &sqlx::SqlitePool, id: &str) -> Result<()> {
    sqlx::query("INSERT OR REPLACE INTO entities (id, body) VALUES (?, ?)")
        .bind(id)
        .bind("x")
        .execute(pool)
        .await
        .with_context(|| format!("insert {id}"))?;
    Ok(())
}

async fn commit_a_chunk(pool: &sqlx::SqlitePool, chunk: usize) -> Result<String> {
    for row in 0..2 {
        insert(pool, &format!("chunk-{chunk}-{row}")).await?;
    }
    doltlite_raw::commit_run(pool, &format!("chunk {chunk}"))
        .await?
        .ok_or_else(|| anyhow!("chunk {chunk} committed nothing"))
}

/// Rows at HEAD, read through a pinned view rather than a plain `SELECT`, so
/// this counts committed state and not the working set.
async fn committed_rows(pool: &sqlx::SqlitePool) -> Value {
    let Ok(Some(head)) = doltlite_raw::head_commit(pool).await else {
        return Value::Null;
    };
    let Ok(pin) = Pin::at(head) else {
        return Value::Null;
    };
    // Audited: the hash came from `dolt_log` and `Pin::at` re-checked it is 40
    // hex characters; the table name is a literal.
    let sql = format!("SELECT COUNT(*) FROM dolt_at_entities('{}')", pin.commit());
    match sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
        .fetch_one(pool)
        .await
    {
        Ok(n) => json!(n),
        Err(_) => Value::Null,
    }
}

// ── plumbing ────────────────────────────────────────────────────────

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Write via a sibling temp file and rename, so a watcher polling for the
/// path never reads a half-written one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename onto {}", path.display()))
}

/// `--key value` pairs, plus bare `--key` for the one boolean.
struct Args(HashMap<String, String>);

impl Args {
    fn parse(argv: impl Iterator<Item = String>) -> Result<Args> {
        let mut map = HashMap::new();
        let mut argv = argv.peekable();
        while let Some(arg) = argv.next() {
            let key = arg
                .strip_prefix("--")
                .ok_or_else(|| anyhow!("expected --flag, got {arg:?}"))?
                .to_string();
            let takes_value = argv.peek().is_some_and(|v| !v.starts_with("--"));
            let value = if takes_value {
                argv.next().unwrap_or_default()
            } else {
                String::new()
            };
            map.insert(key, value);
        }
        Ok(Args(map))
    }

    fn str(&self, key: &str) -> Result<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| anyhow!("missing --{key}"))
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        Ok(PathBuf::from(self.str(key)?))
    }

    fn opt_path(&self, key: &str) -> Option<PathBuf> {
        self.0.get(key).map(PathBuf::from)
    }

    fn flag(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    fn num(&self, key: &str, default: u64) -> u64 {
        self.0
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }
}
