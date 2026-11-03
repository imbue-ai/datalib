//! `list_every_hours` decides how soon a run reads the user list again:
//! the six hours it was fixed at are now a setting. The second sync's
//! tape has no `users.list`, so a run that asks for it records a failed
//! listing, and one that does not, none.

use datalib_etl_slack::ingest::{db_path_for, FetchOptions, RawDb};
use datalib_etl_slack::recorded::History;
use serde_json::json;

use crate::support::{fetch_into, msg, record_general, serve, Tree};

async fn users_listing_failed(out: &std::path::Path) -> bool {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM problems WHERE scope_key LIKE '%users.list%'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    db.close().await;
    n > 0
}

async fn second_sync_asks_for_users(list_every_hours: i64) -> bool {
    let t = Tree::new();
    record_general(&t.api);
    History::cold("C1")
        .record(&t.api, json!([msg("1735689600.000100", "hello")]))
        .unwrap();
    t.serve();
    fetch_into(&t.playback, &t.out, |o| FetchOptions {
        list_every_hours,
        ..o
    })
    .await
    .unwrap();

    // An hour later, against a tape with no user list.
    let later = tempfile::tempdir().unwrap();
    let api = later.path().join("api");
    let playback = later.path().join("playback");
    std::fs::create_dir_all(&api).unwrap();
    for entry in std::fs::read_dir(t.api.join("raw_api")).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "users.list" {
            let to = api.join("raw_api").join(entry.file_name());
            std::fs::create_dir_all(&to).unwrap();
            for f in std::fs::read_dir(entry.path()).unwrap() {
                let f = f.unwrap();
                std::fs::copy(f.path(), to.join(f.file_name())).unwrap();
            }
        }
    }
    serve(&api, &playback);
    fetch_into(&playback, &t.out, |o| FetchOptions {
        list_every_hours,
        now: o.now + chrono::Duration::hours(1),
        ..o
    })
    .await
    .unwrap();
    users_listing_failed(&t.out).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_user_list_is_read_again_only_after_list_every_hours() {
    assert!(
        !second_sync_asks_for_users(6).await,
        "an hour on, a six-hour setting does not read the user list again"
    );
    assert!(
        second_sync_asks_for_users(0).await,
        "0 reads it on every sync"
    );
}
