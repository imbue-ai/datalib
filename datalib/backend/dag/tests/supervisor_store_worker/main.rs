//! One process writing or reading `system/supervisor.sqlite` the way one
//! of its real users does, for `supervisor_contention_test`:
//! `<root> mailbox|record|reader <tag> <n>`, or `<root> try-write`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use datalib_dag::supervisor::record::{Record, StepRecord};
use datalib_dag::supervisor::store::Store;
use datalib_sql::sql;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [root, role] = args.as_slice() {
        if role == "try-write" {
            return try_write(&PathBuf::from(root)).await;
        }
    }
    let [root, role, tag, n] = args.as_slice() else {
        bail!("usage: <root> mailbox|record|reader <tag> <n>, or <root> try-write");
    };
    let root = PathBuf::from(root);
    let n: usize = n.parse().context("n")?;
    let store = Store::open(&root).await?;
    match role.as_str() {
        // A person at the controls: a sync, and now and then a switch flipped off and on.
        "mailbox" => {
            for k in 0..n {
                store.open_request(&[format!("{tag}/{k}")], tag).await?;
                if k % 10 == 0 {
                    store.turn_off(&format!("{tag}/{k}"), tag).await?;
                    store.turn_on(&format!("{tag}/{k}")).await?;
                }
            }
        }
        // The loop saving its `Record`, one step's row more each time.
        "record" => {
            let mut prev = Record::default();
            for k in 0..n {
                let mut next = prev.clone();
                next.steps.insert(
                    format!("{tag}/{k}"),
                    StepRecord {
                        fingerprint: k.to_string(),
                        succeeded: true,
                        ..Default::default()
                    },
                );
                store.save_record(&prev, &next).await?;
                prev = next;
            }
        }
        // The server reading the store for the Manage screen.
        "reader" => {
            for _ in 0..n {
                store.load_record().await?;
                store.open_requests().await?;
                store.recent_requests(100).await?;
                store.taken_on("nobody").await?;
            }
        }
        other => bail!("no role {other:?}"),
    }
    store.close().await;
    Ok(())
}

/// Take the store's write lock without waiting, and say whether another
/// process held it: prints `held` or `got it`.
async fn try_write(root: &std::path::Path) -> Result<()> {
    use sqlx::Connection;
    let db = datalib_runtime::layout::supervisor_db(root);
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(&db))
        .busy_timeout(std::time::Duration::ZERO);
    let mut conn = sqlx::SqliteConnection::connect_with(&opts).await?;
    let answer = match sql!("dag.tests.try_begin_immediate", "BEGIN IMMEDIATE")
        .query()
        .execute(&mut conn)
        .await
    {
        Ok(_) => "got it",
        Err(_) => "held",
    };
    std::io::Write::write_all(&mut std::io::stdout(), answer.as_bytes())?;
    conn.close().await?;
    Ok(())
}
