//! The Linear download against the fake Linear, whose GraphQL endpoint
//! validates every query against Linear's own public schema.

use std::collections::BTreeMap;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_linear::ingest::{self, FetchOptions, FetchSummary, RawDb};
use datalib_etl_linear::probe::probe;
use datalib_etl_linear_config::{LinearConfig, LinearSync};
use datalib_fake_services::{with_real_rate_limits, Fakes, Launch, LINEAR};
use datalib_probe::{ProbeAsk, ProbeList};
use serde_json::{json, Value};

const TOKEN: &str = "lin_api_fakestarfleettoken";
const PICARD: &str = "picard@enterprise.example";

fn seed() -> Value {
    json!({
        "anchor": "2026-03-21T00:00:00Z",
        "time_shift": "none",
        "rate_limits": "none",
        "tokens": [{"token": TOKEN, "user": PICARD}],
        "organization": {"name": "Starfleet", "url_key": "starfleet"},
        "teams": [
            {"key": "ENG", "name": "Engineering", "states": [
                {"name": "Todo", "type": "unstarted", "position": 1},
                {"name": "In Progress", "type": "started", "position": 2},
                {"name": "Done", "type": "completed", "position": 3},
            ]},
            {"key": "SCI", "name": "Science"},
        ],
        "users": [
            {"name": "Jean-Luc Picard", "email": PICARD, "admin": true},
            {"name": "Geordi La Forge", "email": "laforge@enterprise.example"},
        ],
        "labels": [{"name": "warp-core"}, {"name": "sensors", "team": "SCI"}],
        "projects": [{"name": "Warp core refit", "state": "started", "lead": PICARD}],
        "cycles": [{"team": "ENG", "name": "Cycle 7", "number": 7, "starts_at": "2026-03-16T00:00:00Z", "ends_at": "2026-03-30T00:00:00Z"}],
        "issues": [
            {"identifier": "ENG-1", "team": "ENG", "title": "Recalibrate the deflector array", "state": "Done", "assignee": "laforge@enterprise.example", "creator": PICARD, "project": "Warp core refit", "cycle": "Cycle 7", "labels": ["warp-core"], "estimate": 3, "priority": 2, "created_at": "2026-03-02T09:00:00Z", "updated_at": "2026-03-10T17:00:00Z"},
            {"identifier": "ENG-2", "team": "ENG", "title": "Reroute power to the forward shields", "state": "In Progress", "assignee": "laforge@enterprise.example", "creator": PICARD, "project": "Warp core refit", "created_at": "2026-03-12T09:00:00Z", "updated_at": "2026-03-19T12:00:00Z", "relations": [{"type": "blocked_by", "issue": "ENG-1"}]},
            {"identifier": "SCI-1", "team": "SCI", "title": "Survey the Rigel VII nebula", "state": "started", "creator": PICARD, "labels": ["sensors"], "created_at": "2026-03-15T09:00:00Z"},
        ],
        "comments": [
            {"issue": "ENG-2", "user": "laforge@enterprise.example", "body": "Bisected to the plasma conduits.", "created_at": "2026-03-19T12:00:00Z"},
        ]
    })
}

async fn run(db: &RawDb, sync: LinearSync) -> anyhow::Result<FetchSummary> {
    ingest::fetch(FetchOptions {
        sync,
        ..FetchOptions::new(db.clone())
    })
    .await
}

async fn ids(db: &RawDb, table: &str) -> Vec<String> {
    let sql = format!("SELECT id FROM {table} WHERE payload IS NOT NULL ORDER BY id");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(db.pool())
        .await
        .unwrap()
}

async fn payloads(db: &RawDb, table: &str) -> Vec<Value> {
    let sql = format!("SELECT json(payload) FROM {table} WHERE payload IS NOT NULL ORDER BY id");
    let texts: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(db.pool())
        .await
        .unwrap();
    texts
        .iter()
        .map(|t| serde_json::from_str(t).unwrap())
        .collect()
}

fn titles(issues: &[Value]) -> BTreeMap<String, String> {
    issues
        .iter()
        .map(|i| {
            (
                i["identifier"].as_str().unwrap().to_string(),
                i["title"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn config() -> LinearConfig {
    LinearConfig {
        api: Some(LinearSync::default()),
        ..LinearConfig::default()
    }
}

#[tokio::test]
async fn a_first_sync_copies_the_workspace_with_queries_linears_schema_accepts() {
    let fakes = Fakes::start(&BTreeMap::from([(LINEAR, seed())])).expect("the fakes serve");
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();

    let summary = run(&db, LinearSync::default())
        .await
        .expect("the download runs");

    let shown = serde_json::to_string(&summary).unwrap();
    let issues = payloads(&db, "issues").await;
    assert_eq!(
        titles(&issues),
        BTreeMap::from([
            (
                "ENG-1".to_string(),
                "Recalibrate the deflector array".to_string()
            ),
            (
                "ENG-2".to_string(),
                "Reroute power to the forward shields".to_string()
            ),
            (
                "SCI-1".to_string(),
                "Survey the Rigel VII nebula".to_string()
            ),
        ])
    );
    assert_eq!(ids(&db, "teams").await.len(), 2, "{shown}");
    assert_eq!(ids(&db, "users").await.len(), 2, "{shown}");
    assert_eq!(ids(&db, "projects").await.len(), 1, "{shown}");
    assert_eq!(ids(&db, "cycles").await.len(), 1, "{shown}");
    assert_eq!(ids(&db, "issue_labels").await.len(), 2, "{shown}");
    assert_eq!(ids(&db, "comments").await.len(), 1, "{shown}");
    assert!(summary.requests > 0, "{shown}");
    // Every request the download made was a query the real API's schema
    // takes, answered 200; none was a write.
    let statuses: Vec<i64> = fakes
        .access_log()
        .iter()
        .map(|line| line["status"].as_i64().unwrap())
        .collect();
    assert!(statuses.iter().all(|s| *s == 200), "{statuses:?}");
    let unsupported = fakes.unsupported_calls();
    assert!(
        !unsupported
            .keys()
            .any(|k| k.starts_with("linear:mutation") || k.starts_with("linear:filter")),
        "{unsupported:?}"
    );
    db.close().await;
}

#[tokio::test]
async fn a_second_sync_over_an_unchanged_workspace_writes_nothing() {
    let fakes = Fakes::start(&BTreeMap::from([(LINEAR, seed())])).expect("the fakes serve");
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    fakes.mark_phase("first").unwrap();
    let first = run(&db, LinearSync::default())
        .await
        .expect("the first download runs");
    db.commit_all("first").await.unwrap();

    fakes.mark_phase("second").unwrap();
    let second = run(&db, LinearSync::default())
        .await
        .expect("the second download runs");

    assert_eq!(
        second.written,
        0,
        "{}",
        serde_json::to_string(&second).unwrap()
    );
    assert!(
        second.requests <= first.requests,
        "second {} first {}",
        second.requests,
        first.requests
    );
    db.close().await;
}

#[tokio::test]
async fn an_edit_and_a_new_issue_are_what_a_later_sync_writes() {
    let fakes = Fakes::start(&BTreeMap::from([(LINEAR, seed())])).expect("the fakes serve");
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    fakes.mark_phase("first").unwrap();
    run(&db, LinearSync::default())
        .await
        .expect("the first download runs");
    db.commit_all("first").await.unwrap();
    fakes
        .add_records(
            LINEAR,
            &json!({
                "issues": [
                    {"identifier": "ENG-2", "team": "ENG", "title": "Reroute power to the aft shields", "state": "Done", "updated_at": "first+00:00:01"},
                    {"identifier": "ENG-3", "team": "ENG", "title": "Replace the dilithium matrix", "state": "Todo", "creator": PICARD, "created_at": "first+00:00:02"},
                ],
                "comments": [
                    {"issue": "ENG-3", "user": PICARD, "body": "Before the next mission.", "created_at": "first+00:00:03"},
                ]
            }),
            Some("first"),
        )
        .expect("the update lands");

    let second = run(&db, LinearSync::default())
        .await
        .expect("the second download runs");

    let shown = serde_json::to_string(&second).unwrap();
    let issues = payloads(&db, "issues").await;
    assert_eq!(
        titles(&issues)["ENG-2"],
        "Reroute power to the aft shields",
        "{shown}"
    );
    assert_eq!(
        titles(&issues)["ENG-3"],
        "Replace the dilithium matrix",
        "{shown}"
    );
    assert_eq!(ids(&db, "comments").await.len(), 2, "{shown}");
    assert_eq!(second.written, 3, "{shown}");
    db.close().await;
}

/// Linear's hourly request bucket drains at its rate and refuses a caller
/// past it with a 429 and a Retry-After; shrunk to three requests a
/// second here, so a sync of a dozen requests meets the refusal and has
/// only a second to wait out.
#[tokio::test]
async fn a_sync_past_linears_request_limit_waits_and_completes() {
    let launch = Launch {
        rate_limit_by_bucket: BTreeMap::from([("linear:requests".to_string(), 3)]),
        rate_limit_window_seconds: Some(1),
    };
    let fakes = Fakes::start_with(
        &BTreeMap::from([(LINEAR, with_real_rate_limits(seed()))]),
        &launch,
    )
    .expect("the fakes serve");
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();

    let summary = run(&db, LinearSync::default())
        .await
        .expect("the download runs");

    assert_eq!(
        ids(&db, "issues").await.len(),
        3,
        "{}",
        serde_json::to_string(&summary).unwrap()
    );
    let refused = fakes.health()["rate_limited"]["linear:requests"]
        .as_u64()
        .unwrap_or(0);
    assert!(refused >= 1, "{}", fakes.health());
    db.close().await;
}

#[tokio::test]
async fn the_probe_names_the_account_and_lists_teams_and_projects() {
    let _fakes = Fakes::start(&BTreeMap::from([(LINEAR, seed())])).expect("the fakes serve");

    let account = probe(&config(), ProbeAsk::Account, &|_| {})
        .await
        .expect("the probe runs");
    let teams = probe(&config(), ProbeAsk::List(ProbeList::Teams), &|_| {})
        .await
        .expect("the probe runs");
    let projects = probe(&config(), ProbeAsk::List(ProbeList::Projects), &|_| {})
        .await
        .expect("the probe runs");

    assert_eq!(
        account.account.address.as_deref(),
        Some(PICARD),
        "{:?}",
        account.account
    );
    let mut keys: Vec<&str> = teams.items.iter().map(|i| i.path.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["ENG", "SCI"]);
    assert_eq!(
        projects
            .items
            .iter()
            .map(|i| i.path.as_str())
            .collect::<Vec<_>>(),
        ["Warp core refit"]
    );
}
