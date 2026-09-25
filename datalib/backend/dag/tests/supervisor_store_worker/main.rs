//! One process writing or reading `system/supervisor.sqlite` the way one
//! of its real users does, for `supervisor_contention_test`:
//! `<root> mailbox|record|reader <tag> <n>`.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use datalib_dag::supervisor::record::{Record, StepRecord};
use datalib_dag::supervisor::store::Store;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [root, role, tag, n] = args.as_slice() else {
        bail!("usage: <root> mailbox|record|reader <tag> <n>");
    };
    let root = PathBuf::from(root);
    let n: usize = n.parse().context("n")?;
    let store = Store::open(&root).await?;
    match role.as_str() {
        // A person at the controls: a sync, and now and then a pause.
        "mailbox" => {
            for k in 0..n {
                store.open_request(&[format!("{tag}/{k}")], tag).await?;
                if k % 10 == 0 {
                    store.pause(&format!("{tag}/{k}"), tag).await?;
                    store.resume(&format!("{tag}/{k}")).await?;
                }
            }
        }
        // The loop saving its record, one step's row more each time.
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
        // The server answering the Manage screen.
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
