//! One side of a two-process doltlite concurrency test: a writer that
//! commits, a reader that pins, a reader that keeps re-opening and pinning
//! the way `grid_index` does, a reader that holds a transaction as its
//! snapshot, a reader that matches a plain SQLite terms file attached to
//! the commit it reads, or a probe that reports a store's committed state. Driven by
//! `tests/doltlite_two_process.rs`, which is where the scenarios and the
//! assertions live; `seal-existing` also drives a full-size measurement
//! (`hack/read_transaction_at_scale/`).
//!
//! It is a separate binary because the question under test is what happens
//! *between processes*: doltlite's working set lives in the file, and its
//! lock is SQLite's file lock on a sidecar, which one process's connections
//! share, so two pools inside one process cannot stand in for it
//! (`docs/dev/doltlite.md` § "Locks and writers").
//!
//! Each role writes one JSON report to `--out` and exits; nothing is printed
//! to stdout, so a crashed child is distinguishable from a slow one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use datalib_etl::doltlite_raw;
use datalib_etl::pin::Pin;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

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
        "txn-read" => txn_read(&args).await,
        "branch-read" => branch_read(&args).await,
        "rev-probe" => rev_probe(&args).await,
        "rev-read" => rev_read(&args).await,
        "terms-read" => terms_read(&args).await,
        "seal-existing" => seal_existing(&args).await,
        other => bail!("unknown role {other:?}"),
    }?;
    write_atomic(&out, &serde_json::to_vec_pretty(&report)?)
}

// ── roles ───────────────────────────────────────────────────────────

/// Open read-write and keep committing until told to stop, so the reader's
/// whole window is covered by a writer that is actively committing.
/// `--commits-out` says how many commits it has sealed so far.
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
    let terms = match args.opt_path("terms") {
        Some(path) => Some(terms_pool(&path).await?),
        None => None,
    };
    let mut terms_ms: Vec<u64> = Vec::new();
    let mut seed_pin = Value::Null;
    if args.flag("seed") {
        for i in 0..SEED_ROWS {
            insert(&pool, &format!("seed-{i}")).await?;
        }
        let hash = doltlite_raw::commit_run(&pool, "seed")
            .await?
            .ok_or_else(|| anyhow!("the seed commit committed nothing"))?;
        if let Some(terms) = &terms {
            let ids: Vec<String> = (0..SEED_ROWS).map(|i| format!("seed-{i}")).collect();
            write_terms(terms, &ids, None).await?;
        }
        seed_pin = json!(hash);
        // Only now, so a reader that sees the pin file sees a real commit.
        write_atomic(&args.path("pin-out")?, hash.as_bytes())?;
    }

    // Let readers set themselves up (a reader's branch is made once, ever)
    // before the contention under test starts.
    for go in args
        .0
        .get("go-when")
        .into_iter()
        .flat_map(|v| v.split(','))
        .filter(|g| !g.is_empty())
    {
        await_file(Path::new(go))?;
    }
    let until = args.opt_path("until");
    let interval = Duration::from_millis(args.num("interval-ms", 100));
    let max_commits = args.num("max-commits", 0) as usize;
    let txn = Duration::from_millis(args.num("txn-ms", 0));
    let commits_out = args.opt_path("commits-out");
    let mut commits: Vec<Value> = Vec::new();
    for i in 0..max_commits {
        if until.as_deref().is_some_and(Path::exists) {
            break;
        }
        let started = Instant::now();
        let sealed = if txn.is_zero() {
            commit_a_chunk(&pool, i).await
        } else {
            commit_a_chunk_in_a_held_transaction(&pool, i, txn).await
        };
        match sealed {
            Ok(hash) => {
                commits.push(json!({
                    "hash": hash,
                    "at_ms": now_ms(),
                    "ms": started.elapsed().as_millis() as u64,
                }));
                if let Some(path) = &commits_out {
                    write_atomic(path, commits.len().to_string().as_bytes())?;
                }
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
        // The terms follow the seal, as `grid_index` would write them: the
        // chunk's new rows, and the previous chunk's first row replaced.
        if let Some(terms) = &terms {
            let started = Instant::now();
            let ids: Vec<String> = (0..2).map(|row| format!("chunk-{i}-{row}")).collect();
            let replaced = i.checked_sub(1).map(|p| format!("chunk-{p}-0"));
            match write_terms(terms, &ids, replaced.as_deref()).await {
                Ok(()) => terms_ms.push(started.elapsed().as_millis() as u64),
                Err(e) => errors.push(format!("terms for chunk {i}: {e:#}")),
            }
        }
        tokio::time::sleep(interval).await;
    }
    if let Some(terms) = terms {
        terms.close().await;
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
        "terms_ms": terms_ms,
        "errors": errors,
        "size_after": file_size(&db),
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
        match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entities")
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
/// loop: open read-only at HEAD, diff, read, close. Every step is a read, so none of it should cost a writer
/// anything -- this is the role that finds out. `--dolt-status` adds a
/// `SELECT * FROM dolt_status` to every round.
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
        if args.flag("dolt-status") {
            if let Err(e) = sqlx::query("SELECT * FROM dolt_status")
                .fetch_all(reader.pool())
                .await
            {
                errors.push(format!("round {round}: dolt_status: {e:#}"));
            }
        }
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
    // `grid_index` asks a render store's shape before it reads anything.
    anyhow::ensure!(
        datalib_store_meta::read(pool).await?.is_some(),
        "no _datalib_meta at {}",
        pin.commit()
    );
    let _rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entities")
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
/// uncommitted to doltlite — when the kill lands. With `--dolt-commit` as
/// well, they are then `dolt_commit`ted on the writer's branch and never
/// published: the state between `commit_run`'s two halves. Never returns
/// on its own: the test's `kill -9` is the whole point.
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
    if args.flag("dolt-commit") {
        sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', 'stranded')")
            .fetch_one(&mut *conn)
            .await
            .context("dolt_commit on the writer's branch")?;
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
/// reader sees at each point of the writer's pass. `--sampled-out` says
/// how many samples so far began and ended in the current phase, so the
/// test can wait for them rather than for the clock.
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
    let sampled_out = args.path("sampled-out")?;
    let interval = Duration::from_millis(args.num("interval-ms", 25));

    let mut samples: Vec<Value> = Vec::new();
    let mut whole_in: HashMap<String, u64> = HashMap::new();
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
        if phase == phase_after {
            let n = whole_in.entry(phase.clone()).or_default();
            *n += 1;
            write_atomic(&sampled_out, format!("{phase} {n}").as_bytes())?;
        }
        tokio::time::sleep(interval).await;
    }
    pool.close().await;
    Ok(json!({ "role": "watch", "samples": samples }))
}

/// The search applet's snapshot (`docs/dev/plans/paged_grids.md`): a
/// read-only connection on `main` holding a transaction for `--hold-ms`,
/// then `COMMIT; BEGIN` onto whatever `main` is by then. Every sample is
/// tagged with its transaction, so the test can check each one read a
/// single commit.
async fn txn_read(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let table = identifier(args.str("table").unwrap_or("entities"))?;
    let until = args.path("until")?;
    let hold = Duration::from_millis(args.num("hold-ms", 200));
    let interval = Duration::from_millis(args.num("interval-ms", 5));
    let pool = datalib_pin::open_reader(&db)
        .await
        .context("open read-only")?;
    let mut conn = pool.acquire().await.context("acquire")?;
    write_atomic(&args.path("ready-out")?, b"ready")?;

    let mut samples: Vec<Value> = Vec::new();
    let mut txn = 0u64;
    while !until.exists() {
        sqlx::query("BEGIN")
            .execute(&mut *conn)
            .await
            .context("BEGIN")?;
        let opened = Instant::now();
        while opened.elapsed() < hold && !until.exists() {
            let mut s = sample_on(&mut conn, table)
                .await
                .with_context(|| format!("sample in transaction {txn}"))?;
            s["txn"] = json!(txn);
            samples.push(s);
            tokio::time::sleep(interval).await;
        }
        sqlx::query("COMMIT")
            .execute(&mut *conn)
            .await
            .context("COMMIT")?;
        txn += 1;
    }
    drop(conn);
    pool.close().await;
    Ok(json!({ "role": "txn-read", "samples": samples }))
}

/// A reader with a branch of its own, `--branch`: a read-write connection
/// on it, fast-forwarded to `main` with `dolt_merge('main')` every
/// `--hold-ms`, and plain-table reads in between. Takes none of our writer
/// lock -- the question is what doltlite's own locking makes of it. Each
/// sample carries the refresh it followed, so the test can check the
/// branch held still between two of them. `--refresh` picks the move:
/// `merge` (the default) or `reset` (`dolt_reset('--hard', 'main')`).
async fn branch_read(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let branch = args.str("branch")?.to_string();
    let until = args.path("until")?;
    let hold = Duration::from_millis(args.num("hold-ms", 100));
    let interval = Duration::from_millis(args.num("interval-ms", 5));
    let refresh_sql = match args.str("refresh").unwrap_or("merge") {
        "merge" => "SELECT dolt_merge('main')",
        "reset" => "SELECT dolt_reset('--hard', 'main')",
        other => bail!("unknown --refresh {other:?}"),
    };
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))?
        .create_if_missing(false)
        .busy_timeout(Duration::from_millis(args.num("busy-timeout-ms", 5000)));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .context("open read-write")?;
    let mut conn = pool.acquire().await.context("acquire")?;
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_branches WHERE name = ?")
        .bind(&branch)
        .fetch_one(&mut *conn)
        .await?;
    let retry_for = Duration::from_millis(args.num("retry-ms", 30_000));
    let refresh_budget = Duration::from_millis(args.num("refresh-retry-ms", 0));
    if exists == 0 {
        let create = format!("SELECT dolt_branch('{branch}', 'main')");
        retry_busy(&mut conn, &create, retry_for)
            .await
            .0
            .context("create the reader's branch")?;
    }
    sqlx::query("SELECT dolt_connect_branch(?)")
        .bind(&branch)
        .execute(&mut *conn)
        .await?;
    let active: String = sqlx::query_scalar("SELECT active_branch()")
        .fetch_one(&mut *conn)
        .await?;
    if active != branch {
        bail!("connected to {active:?}, not {branch:?}");
    }
    write_atomic(&args.path("ready-out")?, b"ready")?;

    let mut samples: Vec<Value> = Vec::new();
    let mut refreshes: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut txn = 0u64;
    while !until.exists() {
        let started = Instant::now();
        let (refreshed, attempts) = retry_busy(&mut conn, refresh_sql, refresh_budget).await;
        let ms = started.elapsed().as_millis() as u64;
        // A refresh that lost the race to the writer is not a failure: the
        // reader keeps the snapshot it has and tries again next time.
        let outcome = match &refreshed {
            Ok(()) => "ok",
            Err(e) if format!("{e:#}").contains("locked") => "busy",
            Err(e) => {
                errors.push(format!(
                    "refresh {txn} after {attempts} attempts, {ms} ms: {e:#}"
                ));
                "error"
            }
        };
        refreshes.push(json!({
            "txn": txn,
            "at_ms": now_ms(),
            "ms": ms,
            "attempts": attempts,
            "outcome": outcome,
        }));
        let opened = Instant::now();
        while opened.elapsed() < hold && !until.exists() {
            match sample_on(&mut conn, "entities").await {
                Ok(mut s) => {
                    s["txn"] = json!(txn);
                    samples.push(s);
                }
                Err(e) => errors.push(format!("sample after refresh {txn}: {e:#}")),
            }
            tokio::time::sleep(interval).await;
        }
        txn += 1;
    }
    let dirty: Vec<String> = sqlx::query_scalar("SELECT table_name FROM dolt_status")
        .fetch_all(&mut *conn)
        .await
        .unwrap_or_default();
    drop(conn);
    pool.close().await;
    Ok(json!({
        "role": "branch-read",
        "samples": samples,
        "refreshes": refreshes,
        "errors": errors,
        "dirty_at_end": dirty,
    }))
}

/// The writer's side of the full-size measurement: seal an existing store
/// `--seals` times, each seal changing `--rows` rows of `--table` (a
/// column toggled between NULL and a value), through `commit_run`, the
/// same seal every step uses. Opens a plain pool on the writer branch
/// rather than through `open`, so it runs against a copy of any store
/// without reconciling that store's schema. A refused seal ends the run:
/// the measurement is of a writer that is never refused.
async fn seal_existing(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let table = identifier(args.str("table")?)?;
    let column = identifier(args.str("column")?)?;
    let rows = args.num("rows", 1000) as i64;
    let pool = writer_pool(&db).await.context("open the writer branch")?;
    let total: i64 =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(&pool)
            .await?;
    // Audited: `table` and `column` passed `identifier`, so they are bare
    // lowercase names; every value is bound.
    let update = format!(
        "UPDATE {table} SET {column} = CASE WHEN {column} IS NULL THEN 'x' ELSE NULL END \
          WHERE rowid IN (SELECT rowid FROM {table} ORDER BY rowid LIMIT ? OFFSET ?)"
    );
    let mut commits: Vec<Value> = Vec::new();
    for i in 0..args.num("seals", 20) as i64 {
        let started = Instant::now();
        sqlx::query(sqlx::AssertSqlSafe(update.clone()))
            .bind(rows)
            .bind((i * rows) % total.max(1))
            .execute(&pool)
            .await
            .with_context(|| format!("seal {i}: update"))?;
        let hash = doltlite_raw::commit_run(&pool, &format!("seal {i}"))
            .await
            .with_context(|| format!("seal {i}: commit"))?;
        commits.push(json!({
            "hash": hash,
            "at_ms": now_ms(),
            "ms": started.elapsed().as_millis() as u64,
        }));
    }
    pool.close().await;
    Ok(json!({ "role": "seal-existing", "commits": commits, "size_after": file_size(&db) }))
}

/// A one-connection pool that never recycles, on the store's writer
/// branch. `dolt_connect_branch` rather than `dolt_checkout`, for the
/// reason `doltlite_raw` gives: it writes nothing.
async fn writer_pool(db: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db.display()))?
        .create_if_missing(false);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SELECT dolt_connect_branch(?)")
                    .bind(doltlite_raw::WRITER_BRANCH)
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
        .connect_with(opts)
        .await?;
    let active: String = sqlx::query_scalar("SELECT active_branch()")
        .fetch_one(&pool)
        .await?;
    if active != doltlite_raw::WRITER_BRANCH {
        bail!(
            "connected to {active:?}, not {:?}",
            doltlite_raw::WRITER_BRANCH
        );
    }
    Ok(pool)
}

/// What a reader sees now: the row count, then the commit it read at. The
/// count goes first because a table read is what reloads the root from
/// the file (`datalib_pin::head`).
async fn sample_on(conn: &mut sqlx::SqliteConnection, table: &str) -> Result<Value> {
    // Audited: `table` passed `identifier`.
    let count: i64 =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT COUNT(*) FROM {table}")))
            .fetch_one(&mut *conn)
            .await?;
    let head: Option<String> = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
        .fetch_one(&mut *conn)
        .await?;
    Ok(json!({ "at_ms": now_ms(), "count": count, "head": head }))
}

/// A read-only pool on one revision of the store, opened by path:
/// `<db>@<rev>`. A revision that is not a branch opens detached -- pinned
/// there, and refusing every write.
async fn open_revision(db: &Path, rev: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(format!("{}@{rev}", db.display()))
        .create_if_missing(false)
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}@{rev}", db.display()))
}

/// What one detached open can and cannot do, statement by statement.
async fn rev_probe(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let rev = args.str("rev")?;
    let other = args.str("other").unwrap_or(rev);
    let pool = match open_revision(&db, rev).await {
        Ok(p) => p,
        Err(e) => return Ok(json!({ "role": "rev-probe", "open_error": format!("{e:#}") })),
    };
    let mut out = serde_json::Map::new();
    let probes: [(&str, String); 7] = [
        ("active_branch", "SELECT quote(active_branch())".into()),
        ("count", "SELECT COUNT(*) FROM entities".into()),
        ("hashof_head", "SELECT dolt_hashof('HEAD')".into()),
        ("plan", "EXPLAIN QUERY PLAN SELECT * FROM entities WHERE id = 'seed-0'".into()),
        (
            "diff_to_other",
            format!(
                "SELECT COUNT(*) FROM dolt_diff_entities WHERE from_ref = '{rev}' AND to_ref = '{other}'"
            ),
        ),
        ("log", "SELECT COUNT(*) FROM dolt_log()".into()),
        ("insert", "INSERT INTO entities (id, body) VALUES ('probe', 'x')".into()),
    ];
    for (name, sql) in probes {
        // Audited: `rev` and `other` are hashes the test read from the store.
        let got = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(&pool)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        use sqlx::Row;
                        (0..r.len())
                            .map(|i| {
                                r.try_get::<String, _>(i)
                                    .or_else(|_| r.try_get::<i64, _>(i).map(|n| n.to_string()))
                                    .unwrap_or_else(|_| "?".into())
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect::<Vec<_>>()
            });
        out.insert(
            name.into(),
            match got {
                Ok(rows) => json!({ "ok": rows }),
                Err(e) => json!({ "err": e.to_string() }),
            },
        );
    }
    pool.close().await;
    out.insert("role".into(), json!("rev-probe"));
    Ok(Value::Object(out))
}

/// The reader that needs no branch: look up `main`'s tip on a read-only
/// connection, open `<db>@<tip>` read-only and detached, read it for
/// `--hold-ms`, close, and go again. Each window's samples carry the tip it
/// opened at.
async fn rev_read(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let until = args.path("until")?;
    let hold = Duration::from_millis(args.num("hold-ms", 100));
    let interval = Duration::from_millis(args.num("interval-ms", 5));
    write_atomic(&args.path("ready-out")?, b"ready")?;
    let mut samples: Vec<Value> = Vec::new();
    let mut opens: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut txn = 0u64;
    while !until.exists() {
        let started = Instant::now();
        let tip = match main_tip(&db).await {
            Ok(tip) => tip,
            Err(e) => {
                errors.push(format!("window {txn}: read main's tip: {e:#}"));
                txn += 1;
                continue;
            }
        };
        let pool = match open_revision(&db, &tip).await {
            Ok(p) => p,
            Err(e) => {
                errors.push(format!("window {txn}: {e:#}"));
                txn += 1;
                continue;
            }
        };
        opens.push(json!({ "txn": txn, "ms": started.elapsed().as_millis() as u64 }));
        let opened = Instant::now();
        while opened.elapsed() < hold && !until.exists() {
            match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM entities")
                .fetch_one(&pool)
                .await
            {
                Ok(count) => samples.push(json!({
                    "txn": txn, "at_ms": now_ms(), "count": count, "head": tip,
                })),
                Err(e) => errors.push(format!("sample in window {txn}: {e:#}")),
            }
            tokio::time::sleep(interval).await;
        }
        pool.close().await;
        txn += 1;
    }
    Ok(json!({ "role": "rev-read", "samples": samples, "opens": opens, "errors": errors }))
}

/// `rev-read` with the terms file attached: each window opens `main`'s tip
/// detached, attaches the plain SQLite terms file read-only, and samples
/// one statement that counts the full-text index, the terms table, and
/// the terms whose row is in the commit. With `--pinned`, it reads as the
/// search applet does (`DoltRepo::pinned`): one read-only connection on
/// `main`, the file attached once outside any transaction, and each
/// window a transaction that holds one commit while it samples.
async fn terms_read(args: &Args) -> Result<Value> {
    if args.flag("pinned") {
        return pinned_terms_read(args).await;
    }
    let db = args.path("db")?;
    let terms = args.path("terms")?;
    let until = args.path("until")?;
    let hold = Duration::from_millis(args.num("hold-ms", 100));
    let interval = Duration::from_millis(args.num("interval-ms", 5));
    write_atomic(&args.path("ready-out")?, b"ready")?;
    // Audited: the path is this test's own tempdir, and has no quote in it.
    let attach = format!(
        "ATTACH 'file:{}?doltlite_engine=sqlite&mode=ro' AS t",
        terms.display()
    );
    let sample_sql = "SELECT \
        (SELECT COUNT(*) FROM t.terms_fts WHERE terms_fts MATCH 'email*'), \
        (SELECT COUNT(*) FROM t.terms), \
        (SELECT COUNT(*) FROM t.terms x JOIN entities g ON g.id = x.uuid), \
        (SELECT COUNT(*) FROM entities)";
    let mut samples: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut txn = 0u64;
    while !until.exists() {
        let window = async {
            let tip = main_tip(&db).await.context("read main's tip")?;
            let pool = open_revision(&db, &tip).await?;
            let mut conn = pool.acquire().await.context("acquire")?;
            sqlx::query(sqlx::AssertSqlSafe(attach.clone()))
                .execute(&mut *conn)
                .await
                .context("attach the terms file")?;
            let opened = Instant::now();
            while opened.elapsed() < hold && !until.exists() {
                let started = Instant::now();
                let row: (i64, i64, i64, i64) = sqlx::query_as(sample_sql)
                    .fetch_one(&mut *conn)
                    .await
                    .context("sample")?;
                samples.push(json!({
                    "txn": txn, "at_ms": now_ms(), "head": tip,
                    "ms": started.elapsed().as_millis() as u64,
                    "fts": row.0, "terms": row.1, "joined": row.2, "rows": row.3,
                }));
                tokio::time::sleep(interval).await;
            }
            drop(conn);
            pool.close().await;
            anyhow::Ok(())
        };
        if let Err(e) = window.await {
            errors.push(format!("window {txn}: {e:#}"));
        }
        txn += 1;
    }
    Ok(json!({ "role": "terms-read", "samples": samples, "errors": errors }))
}

async fn pinned_terms_read(args: &Args) -> Result<Value> {
    let db = args.path("db")?;
    let terms = args.path("terms")?;
    let until = args.path("until")?;
    let hold = Duration::from_millis(args.num("hold-ms", 100));
    let interval = Duration::from_millis(args.num("interval-ms", 5));
    let pool = datalib_pin::open_reader(&db)
        .await
        .context("open main read-only")?;
    // Audited: the path is this test's own tempdir, and has no quote in it.
    let attach = format!(
        "ATTACH 'file:{}?doltlite_engine=sqlite&mode=ro' AS t",
        terms.display()
    );
    sqlx::query(sqlx::AssertSqlSafe(attach))
        .execute(&pool)
        .await
        .context("attach the terms file")?;
    write_atomic(&args.path("ready-out")?, b"ready")?;
    let sample_sql = "SELECT \
        (SELECT COUNT(*) FROM t.terms_fts WHERE terms_fts MATCH 'email*'), \
        (SELECT COUNT(*) FROM t.terms), \
        (SELECT COUNT(*) FROM t.terms x JOIN entities g ON g.id = x.uuid), \
        (SELECT COUNT(*) FROM entities)";
    let mut samples: Vec<Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut txn = 0u64;
    while !until.exists() {
        let window = async {
            let mut tx = pool.begin().await.context("begin")?;
            let _: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master")
                .fetch_one(&mut *tx)
                .await
                .context("load the commit")?;
            let head: String = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
                .fetch_one(&mut *tx)
                .await
                .context("read the commit")?;
            // At least one sample: a request is a transaction around one
            // query, which `--hold-ms 0` reads as.
            let opened = Instant::now();
            loop {
                let started = Instant::now();
                let row: (i64, i64, i64, i64) = sqlx::query_as(sample_sql)
                    .fetch_one(&mut *tx)
                    .await
                    .context("sample")?;
                samples.push(json!({
                    "txn": txn, "at_ms": now_ms(), "head": head,
                    "ms": started.elapsed().as_millis() as u64,
                    "fts": row.0, "terms": row.1, "joined": row.2, "rows": row.3,
                }));
                if opened.elapsed() >= hold || until.exists() {
                    break;
                }
                tokio::time::sleep(interval).await;
            }
            tx.commit().await.context("end the read")?;
            tokio::time::sleep(interval).await;
            anyhow::Ok(())
        };
        if let Err(e) = window.await {
            errors.push(format!("window {txn}: {e:#}"));
        }
        txn += 1;
    }
    pool.close().await;
    Ok(json!({ "role": "terms-read", "samples": samples, "errors": errors }))
}

async fn main_tip(db: &Path) -> Result<String> {
    let pool = datalib_pin::open_reader(db).await?;
    let tip = sqlx::query_scalar::<_, String>("SELECT hash FROM dolt_branches WHERE name = 'main'")
        .fetch_one(&pool)
        .await;
    pool.close().await;
    Ok(tip?)
}

/// Run `sql` until it is not refused as busy or `budget` runs out; each try
/// also waits out the connection's busy timeout. Returns the last outcome and
/// how many tries it took.
async fn retry_busy(
    conn: &mut sqlx::SqliteConnection,
    sql: &str,
    budget: Duration,
) -> (Result<()>, u64) {
    let started = Instant::now();
    let mut attempts = 0u64;
    loop {
        attempts += 1;
        // Audited: every caller passes a literal or a branch name the test chose.
        match sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&mut *conn)
            .await
        {
            Ok(_) => return (Ok(()), attempts),
            Err(e) if e.to_string().contains("locked") && started.elapsed() < budget => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Err(e) => return (Err(e.into()), attempts),
        }
    }
}

/// A table or column name from the command line, held to the one shape
/// that is safe to splice into SQL.
fn identifier(name: &str) -> Result<&str> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        bail!("not a plain lowercase identifier: {name:?}");
    }
    Ok(name)
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
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

/// A render store's checkpoint or a `grid_index` pass: the rows go in one
/// SQL transaction that stays open for `hold`, then the seal.
async fn commit_a_chunk_in_a_held_transaction(
    pool: &sqlx::SqlitePool,
    chunk: usize,
    hold: Duration,
) -> Result<String> {
    let mut conn = pool.acquire().await.context("acquire")?;
    sqlx::query("BEGIN")
        .execute(&mut *conn)
        .await
        .context("BEGIN")?;
    for row in 0..2 {
        sqlx::query("INSERT OR REPLACE INTO entities (id, body) VALUES (?, ?)")
            .bind(format!("chunk-{chunk}-{row}"))
            .bind("x")
            .execute(&mut *conn)
            .await
            .with_context(|| format!("chunk {chunk}: insert"))?;
    }
    tokio::time::sleep(hold).await;
    sqlx::query("COMMIT")
        .execute(&mut *conn)
        .await
        .with_context(|| format!("chunk {chunk}: COMMIT"))?;
    drop(conn);
    doltlite_raw::commit_run(pool, &format!("chunk {chunk}"))
        .await?
        .ok_or_else(|| anyhow!("chunk {chunk} committed nothing"))
}

/// The plain SQLite terms file, laid out as `docs/dev/doltlite.md`
/// § "Full-text search (FTS5)" has it.
async fn terms_pool(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(format!("file:{}?doltlite_engine=sqlite", path.display()))
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    for ddl in [
        "CREATE TABLE IF NOT EXISTS terms (term_id INTEGER PRIMARY KEY, uuid TEXT, value TEXT)",
        "CREATE INDEX IF NOT EXISTS terms_by_uuid ON terms (uuid)",
        "CREATE VIRTUAL TABLE IF NOT EXISTS terms_fts USING fts5(value, content='', \
         contentless_delete=1, tokenize=\"unicode61 tokenchars '@.-_+:'\")",
    ] {
        sqlx::query(ddl).execute(&pool).await.context(ddl)?;
    }
    Ok(pool)
}

/// One transaction: a term for each of `ids`, and `replace`'s terms
/// deleted and written again.
async fn write_terms(pool: &SqlitePool, ids: &[String], replace: Option<&str>) -> Result<()> {
    let mut conn = pool.acquire().await.context("acquire")?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let mut all: Vec<&str> = ids.iter().map(String::as_str).collect();
    if let Some(old) = replace {
        sqlx::query(
            "DELETE FROM terms_fts WHERE rowid IN (SELECT term_id FROM terms WHERE uuid = ?)",
        )
        .bind(old)
        .execute(&mut *conn)
        .await?;
        sqlx::query("DELETE FROM terms WHERE uuid = ?")
            .bind(old)
            .execute(&mut *conn)
            .await?;
        all.push(old);
    }
    for id in all {
        let value = format!("email:{id}@example.com");
        let term_id: i64 =
            sqlx::query_scalar("INSERT INTO terms (uuid, value) VALUES (?, ?) RETURNING term_id")
                .bind(id)
                .bind(&value)
                .fetch_one(&mut *conn)
                .await?;
        sqlx::query("INSERT INTO terms_fts (rowid, value) VALUES (?, ?)")
            .bind(term_id)
            .bind(&value)
            .execute(&mut *conn)
            .await?;
    }
    sqlx::query("COMMIT").execute(&mut *conn).await?;
    Ok(())
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
