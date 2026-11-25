//! One run after another over the tapes in [`crate::world`].

use std::path::Path;

use datalib_etl::doltlite_raw::commit_run;
use datalib_etl_discord::ingest::{self, db_path_for, schema_raw, snowflake, FetchSummary, RawDb};
use datalib_etl_discord_config::DiscordSync;
use datalib_etl_web::playback;
use datalib_time::IsoOffsetTimestamp;
use serde_json::{json, Value};

use crate::world::{self, *};

/// Every table a run's result is compared on.
pub const TABLES: &[&str] = &[
    "account",
    "users",
    "guilds",
    "members",
    "roles",
    "channels",
    "messages",
    "discord_attachments",
    "coverage",
];

pub fn sync() -> DiscordSync {
    DiscordSync {
        servers: Some(vec!["uss enterprise".into()]),
        ..Default::default()
    }
}

pub fn options(db: RawDb, sync: DiscordSync, now: IsoOffsetTimestamp) -> ingest::FetchOptions {
    ingest::FetchOptions {
        sync,
        ..ingest::FetchOptions::new(db, now)
    }
}

/// `now` a day after the first run, when listings and refreshes are due.
pub fn later() -> IsoOffsetTimestamp {
    datalib_time::parse_with_assumed_utc("2024-06-02T00:00:00Z").unwrap()
}

async fn run_at(
    store: &Path,
    tape: &Path,
    sync: DiscordSync,
    now: IsoOffsetTimestamp,
) -> anyhow::Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = playback::scope(tape, ingest::fetch(options(db.clone(), sync, now))).await;
    commit_run(db.pool(), "test").await.unwrap();
    db.close().await;
    r
}

async fn run(store: &Path, tape: &Path) -> anyhow::Result<FetchSummary> {
    run_at(store, tape, sync(), world::now()).await
}

async fn count(store: &Path, sql: &'static str) -> i64 {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let n = sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap();
    db.close().await;
    n
}

async fn payload(store: &Path, table: &str, id: &str) -> Option<Value> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let sql = format!("SELECT json(payload) FROM {table} WHERE id = ?");
    let got: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .unwrap();
    db.close().await;
    got.map(|s| serde_json::from_str(&s).unwrap())
}

async fn problems(store: &Path) -> Vec<String> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let got = sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    got
}

fn message_key(channel: &str, minute: u64) -> String {
    schema_raw::message_key(channel, at(minute))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_copies_every_conversation_the_account_can_read() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let tape = World::before().tape(d.path(), "before");
    let s = run(&store, &tape).await.unwrap();

    // The direct message, the bridge, its thread and the forum post.
    assert_eq!((s.conversations, s.walked), (4, 4), "{s:?}");
    assert_eq!(s.messages, BRIDGE_MESSAGES + 1 + 2 + 2, "{s:?}");
    assert_eq!(s.files, 1, "{s:?}");
    assert_eq!(problems(&store).await, [] as [&str; 0]);

    // The ready room is listed, and never asked for: a request with no
    // tape would be a problem.
    assert!(payload(&store, "channels", READY_ROOM).await.is_some());
    let unread = count(
        &store,
        "SELECT COUNT(*) FROM messages WHERE channel_id = '7002'",
    );
    assert_eq!(unread.await, 0);
    assert_eq!(
        payload(&store, "guilds", ACADEMY).await,
        None,
        "a server not named is not copied"
    );
    let m = payload(&store, "messages", &message_key(BRIDGE, 0)).await;
    assert_eq!(m.unwrap()["author"], json!({"id": RIKER}));
    assert_eq!(
        payload(&store, "users", RIKER).await.unwrap()["global_name"],
        "Will Riker"
    );
    let held = count(
        &store,
        "SELECT COUNT(*) FROM discord_attachments WHERE blake3 IS NOT NULL",
    );
    assert_eq!(held.await, 1);
}

/// Nothing new: the listings, and not one page of messages, whether the
/// messages are older than the refresh window or inside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sync_with_nothing_new_only_lists() {
    for days in [7, 60] {
        let d = tempfile::tempdir().unwrap();
        let store = d.path().join("store");
        let tape = World::before().tape(d.path(), "before");
        let sync = || DiscordSync {
            refresh_window_days: Some(days),
            ..sync()
        };
        let first = run_at(&store, &tape, sync(), world::now()).await.unwrap();
        assert_eq!(first.walked, 4, "{first:?}");
        let s = run_at(&store, &tape, sync(), world::now()).await.unwrap();
        // The account, the direct messages, the servers, one server's channels.
        assert_eq!((s.walked, s.messages, s.requests), (0, 0, 4), "{s:?}");
    }
}

/// A conversation quiet for longer than the window, then spoken in: what
/// the window holds of it has just been read, and is not read again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_first_read_by_a_new_message_is_not_read_again() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    run(&store, &World::before().tape(d.path(), "before"))
        .await
        .unwrap();
    let mut w = World::before();
    // 2024-05-31, a day before the runs' now and weeks after the rest.
    let spoken = at(30 * 24 * 60);
    let dm = w.value["messages"][RIKER_DM].as_array_mut().unwrap();
    dm.push(message(RIKER_DM, spoken, RIKER, "Awake?"));
    w.value["dms"][0]["last_message_id"] = json!(spoken.to_string());
    let tape = w.tape(d.path(), "spoken");
    let s = run(&store, &tape).await.unwrap();
    assert_eq!(s.walked, 1, "{s:?}");
    let s = run(&store, &tape).await.unwrap();
    assert_eq!((s.walked, s.requests), (0, 4), "{s:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_reads_what_is_new_and_notices_edits_and_deletions() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    run(&store, &World::before().tape(d.path(), "before"))
        .await
        .unwrap();
    let tape = World::after().tape(d.path(), "after");
    // The window that takes in all of May.
    let sync = DiscordSync {
        refresh_window_days: Some(60),
        ..sync()
    };
    let s = run_at(&store, &tape, sync.clone(), later()).await.unwrap();
    assert_eq!(s.pruned, 1, "{s:?}");
    let gone = payload(&store, "messages", &message_key(BRIDGE, 225)).await;
    assert_eq!(gone, None, "deleted upstream");
    let edited = payload(&store, "messages", &message_key(BRIDGE, 220)).await;
    assert_eq!(edited.unwrap()["content"], "Log entry 220, corrected.");
    let new = BRIDGE_MESSAGES as u64 + 60;
    assert!(payload(&store, "messages", &message_key(BRIDGE, new))
        .await
        .is_some());
    // The reply in the forum post, whose newest message moved.
    let post = at(50).to_string();
    assert!(payload(&store, "messages", &message_key(&post, 400))
        .await
        .is_some());
    assert_eq!(problems(&store).await, [] as [&str; 0]);

    // Read again an hour later, the window is not due.
    let soon = datalib_time::parse_with_assumed_utc("2024-06-02T01:00:00Z").unwrap();
    let s = run_at(&store, &tape, sync, soon).await.unwrap();
    assert_eq!((s.walked, s.pruned), (0, 0), "{s:?}");
}

/// With the window off, a deletion is not noticed and nothing is judged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_refresh_window_a_deleted_message_stays() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    run(&store, &World::before().tape(d.path(), "before"))
        .await
        .unwrap();
    let tape = World::after().tape(d.path(), "after");
    let sync = DiscordSync {
        refresh_window_days: Some(0),
        ..sync()
    };
    let s = run_at(&store, &tape, sync, later()).await.unwrap();
    assert_eq!(s.pruned, 0, "{s:?}");
    assert!(payload(&store, "messages", &message_key(BRIDGE, 225))
        .await
        .is_some());
}

/// `since` bounds every walk, and moving it earlier later fetches the
/// rest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_earlier_since_fetches_what_it_now_covers() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let mut world = World::before();
    // Where the first run stopped, which the second goes on under.
    let edge = snowflake::floor_at(snowflake::unix_ms(at(150)));
    world.value["befores"] = json!({
        (BRIDGE): [edge.to_string()],
        (RIKER_DM): [edge.to_string()],
    });
    let tape = world.tape(d.path(), "before");
    let from = |since: Option<String>| DiscordSync { since, ..sync() };
    let s = run_at(&store, &tape, from(Some(stamp(at(150)))), world::now())
        .await
        .unwrap();
    assert_eq!(s.messages, (BRIDGE_MESSAGES - 150) + 2, "{s:?}");
    let older = count(
        &store,
        "SELECT COUNT(*) FROM messages WHERE channel_id = '7001'",
    );
    assert_eq!(older.await, BRIDGE_MESSAGES as i64 - 150);
    run_at(&store, &tape, from(None), world::now())
        .await
        .unwrap();
    assert_eq!(problems(&store).await, [] as [&str; 0]);
    let all = count(&store, "SELECT COUNT(*) FROM messages");
    assert_eq!(all.await, BRIDGE_MESSAGES as i64 + 5);
}

/// A file whose link ran out has its message read again for one that
/// works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_whose_link_ran_out_is_fetched_by_a_new_one() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let tape = World::before().tape(d.path(), "before");
    let no_media = DiscordSync {
        media: false,
        ..sync()
    };
    let s = run_at(&store, &tape, no_media, world::now()).await.unwrap();
    assert_eq!(s.files, 0);

    // A week on, the stored link no longer works; Discord signs another.
    let week = datalib_time::parse_with_assumed_utc("2024-06-08T00:00:00Z").unwrap();
    let mut w = World::before();
    let fresh = link("log.txt", week.to_unix_millis() / 1000 + 86_400);
    let bridge = w.value["messages"][BRIDGE].as_array_mut().unwrap();
    bridge[200]["attachments"][0]["url"] = json!(fresh);
    w.files = [(fresh, b"Stardate.".to_vec())].into();
    let tape = w.tape(d.path(), "week");
    let s = run_at(&store, &tape, sync(), week).await.unwrap();
    assert_eq!(s.files, 1, "{s:?}");
    assert_eq!(problems(&store).await, [] as [&str; 0]);
}

/// A file over the limit is a warning on it, and is fetched once the
/// limit is raised.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_over_the_limit_waits_for_a_higher_one() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let tape = World::before().tape(d.path(), "before");
    for (limit, files, rows) in [(Some(5), 0, 1), (None, 1, 0)] {
        let db = RawDb::open(&db_path_for(&store)).await.unwrap();
        let opts = ingest::FetchOptions {
            blob_size_limit_bytes: limit,
            ..options(db.clone(), sync(), world::now())
        };
        let s = playback::scope(&tape, ingest::fetch(opts)).await.unwrap();
        commit_run(db.pool(), "test").await.unwrap();
        db.close().await;
        assert_eq!(s.files, files, "{limit:?}");
        assert_eq!(problems(&store).await.len(), rows, "{limit:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_discord_does_not_have_is_a_problem_and_costs_only_itself() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let tape = World::before().tape(d.path(), "before");
    let sync = DiscordSync {
        servers: Some(vec!["USS Enterprise".into(), "Deep Space 9".into()]),
        channels: Some(vec!["bridge".into(), "holodeck".into()]),
        threads: false,
        ..sync()
    };
    let s = run_at(&store, &tape, sync, world::now()).await.unwrap();
    assert_eq!(s.conversations, 2, "the bridge and the direct message");
    let got = problems(&store).await;
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.iter().any(|k| k.contains("Deep Space 9")), "{got:?}");
    assert!(got.iter().any(|k| k.contains("holodeck")), "{got:?}");
}

/// The account was let out of a channel between the listing and the read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_channel_that_refuses_costs_only_that_channel() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let tape = World::before().tape(d.path(), "before");
    let req = ingest::api::messages_request(BRIDGE, None);
    let refused = datalib_etl_web::http::HttpResponse {
        status: 403,
        headers: Default::default(),
        body: br#"{"message": "Missing Access", "code": 50001}"#.to_vec(),
        duration_ms: 0,
    };
    datalib_etl_web::synthesize::write_fixture(&tape, &req, &refused).unwrap();
    let s = run(&store, &tape).await.unwrap();
    assert_eq!(s.messages, 5, "{s:?}");
    assert_eq!(problems(&store).await, ["listing:messages of 7001"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_credential_fails_the_run_and_says_how_to_sign_in() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let tape = d.path().join("refused");
    let refused = datalib_etl_web::http::HttpResponse {
        status: 401,
        headers: Default::default(),
        body: br#"{"message": "401: Unauthorized", "code": 0}"#.to_vec(),
        duration_ms: 0,
    };
    datalib_etl_web::synthesize::write_fixture(&tape, &ingest::api::me_request(), &refused)
        .unwrap();
    let e = format!("{:#}", run(&store, &tape).await.unwrap_err());
    assert!(e.contains("latchkey auth browser discord"), "{e}");
}

/// A source signed in as someone else is that account's copy from then on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signing_in_as_another_account_leaves_one() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    let mut w = World::before();
    run(&store, &w.tape(d.path(), "picard")).await.unwrap();
    w.value["me"]["id"] = json!(RIKER);
    run(&store, &w.tape(d.path(), "riker")).await.unwrap();
    let accounts = count(&store, "SELECT count(*) FROM account").await;
    let sidecars = count(&store, "SELECT count(*) FROM account_bookkeeping").await;
    assert_eq!((accounts, sidecars), (1, 1));
    assert!(payload(&store, "account", RIKER).await.is_some());
}
