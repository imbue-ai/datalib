// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Discord download, and the check that a second run reads no
//! message.
//!
//! Not run by `bazel test`: every test here is named `live::<fn>`, and
//! the target skips that prefix. `bazel run
//! //datalib/backend/etl/providers/discord:discord_live` runs exactly
//! these, against the account latchkey holds for `discord`
//! (`DISCORD_TEST_ACCOUNT` names one when it holds several). It copies
//! the direct messages and `DISCORD_TEST_SERVERS` (ids, comma-separated)
//! since `DISCORD_TEST_SINCE`, a week unless set. The store is left on
//! disk, and its path printed, for inspection; nothing in it is printed
//! here but counts.

use datalib_etl_discord::ingest::{self, db_path_for, FetchSummary, RawDb};
use datalib_etl_discord_config::DiscordSync;
use datalib_etl_web::http::LatchkeySettings;
use datalib_time::IsoOffsetTimestamp;

async fn run(store: &std::path::Path, sync: &DiscordSync) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = ingest::fetch(ingest::FetchOptions {
        latchkey: LatchkeySettings {
            account: std::env::var("DISCORD_TEST_ACCOUNT").ok(),
        },
        sync: sync.clone(),
        ..ingest::FetchOptions::new(db.clone(), IsoOffsetTimestamp::now_local())
    })
    .await;
    let sealed = datalib_etl::doltlite_raw::commit_run(db.pool(), "discord_live").await;
    db.close().await;
    sealed.expect("seal the raw store");
    r.expect("discord fetch")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_then_a_second_run_that_reads_no_message() {
    let since = std::env::var("DISCORD_TEST_SINCE").unwrap_or_else(|_| "-7d".to_string());
    let servers: Option<Vec<String>> = std::env::var("DISCORD_TEST_SERVERS")
        .ok()
        .map(|s| s.split(',').map(str::to_string).collect());
    let sync = DiscordSync {
        since: Some(since),
        servers,
        ..Default::default()
    };
    let store = tempfile::TempDir::with_prefix("discord-live-")
        .unwrap()
        .keep();
    eprintln!("[test] store: {}", store.display());

    let first = run(&store, &sync).await;
    eprintln!("[test] first run: {}", first.line());
    assert!(first.conversations > 0, "{first:?}");

    let second = run(&store, &sync).await;
    eprintln!("[test] second run: {}", second.line());
    // A message that came in between the two runs fails this; run it again.
    assert_eq!(second.walked, 0, "{second:?}");
}
