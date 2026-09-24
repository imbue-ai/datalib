//! One writer of a doltlite store that can be stopped inside a write, for
//! `tests/doltlite_interrupt.rs`: it opens the store as every download does
//! (`doltlite_raw::open`), applies one batch and seals it, and at the named
//! break point prints `at <point>` and waits to be killed. With no break
//! point it prints `done` and exits. A process because the question is what
//! a writer that *dies* leaves behind.
//!
//! The seal is `commit_run`'s two halves done by hand — `dolt_commit`, then
//! `publish_to_main` — so the test can stop it between them.
//!
//! usage: `<db> <none|opened|mid-tx|applied|committed|published> <op>…`,
//! an op being `put:<id>:<n>` or `del:<id>`.

use std::io::Write;
use std::path::Path;

use datalib_etl::doltlite_raw;

const ROWS_DDL: &str = "CREATE TABLE IF NOT EXISTS rows (id TEXT PRIMARY KEY, n INTEGER NOT NULL)";

fn at(point: &str, stop: &str) {
    if point != stop {
        return;
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, "at {point}").unwrap();
    out.flush().unwrap();
    // Until the test kills it.
    loop {
        // SAFETY: waits for a signal; there is no handler, so any it
        // sends ends the process.
        unsafe { libc::pause() };
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let [db, stop, ops @ ..] = argv.as_slice() else {
        panic!("usage: <db> <break point> <op>…");
    };
    let pool = doltlite_raw::open(Path::new(db), &[ROWS_DDL])
        .await
        .expect("open");
    // What `open` alone did to what a dead writer left.
    at("opened", stop);
    let mut tx = pool.begin().await.expect("begin");
    for (i, op) in ops.iter().enumerate() {
        match op.split(':').collect::<Vec<_>>().as_slice() {
            ["put", id, n] => {
                sqlx::query(
                    "INSERT INTO rows (id, n) VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET n = excluded.n",
                )
                .bind(*id)
                .bind(n.parse::<i64>().unwrap())
                .execute(&mut *tx)
                .await
                .expect("put");
            }
            ["del", id] => {
                sqlx::query("DELETE FROM rows WHERE id = ?")
                    .bind(*id)
                    .execute(&mut *tx)
                    .await
                    .expect("del");
            }
            _ => panic!("bad op {op}"),
        }
        // Half the batch in, inside the transaction.
        if i + 1 == ops.len().div_ceil(2) {
            at("mid-tx", stop);
        }
    }
    tx.commit().await.expect("commit the transaction");
    // In the working set, not committed.
    at("applied", stop);
    // As `commit_run` does: a clean working set is nothing to commit.
    match sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', 'batch')")
        .fetch_one(&pool)
        .await
    {
        Ok(_) => {}
        Err(e) if e.to_string().contains("nothing to commit") => {}
        Err(e) => panic!("dolt_commit: {e}"),
    }
    // Committed on the writer's branch, not yet on `main`.
    at("committed", stop);
    doltlite_raw::publish_to_main(&pool).await.expect("publish");
    at("published", stop);
    pool.close().await;
    let mut out = std::io::stdout().lock();
    writeln!(out, "done").unwrap();
    out.flush().unwrap();
}
