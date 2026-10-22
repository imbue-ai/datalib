//! A download end to end against the fake: what the first run stores,
//! what a later run costs and writes, and how a walk cut short carries
//! on.

use datalib_etl::doltlite_raw as dr;
use datalib_etl_linear::ingest::{self, schema_raw, FetchOptions, FetchSummary, RawDb};
use datalib_etl_linear_config::LinearSync;
use serde_json::{json, Value};

use crate::fake::FakeLinear;

fn at(minute: u32) -> String {
    format!("2026-09-01T{:02}:{:02}:00.000Z", minute / 60, minute % 60)
}

fn issue(n: u32, minute: u32) -> Value {
    let (team, project) = match n % 3 {
        0 => ("t-eng", Value::Null),
        1 => ("t-sci", Value::Null),
        _ => ("t-sci", json!({"id": "p-probe"})),
    };
    json!({
        "id": format!("i-{n:03}"),
        "identifier": format!("ENG-{n}"),
        "title": format!("Recalibrate deflector array {n}"),
        "updatedAt": at(minute),
        "sortOrder": n as f64,
        "labelIds": ["l-b", "l-a"],
        "team": {"id": team},
        "project": project,
    })
}

/// A workspace of two teams, two projects and `issues` issues, the
/// newest updated last.
fn workspace(issues: u32) -> FakeLinear {
    FakeLinear::with(vec![
        (
            "teams",
            vec![
                json!({"id": "t-eng", "key": "ENG", "name": "Engineering", "updatedAt": at(0),
                   "issueCount": 40}),
                json!({"id": "t-sci", "key": "SCI", "name": "Science", "updatedAt": at(0),
                   "issueCount": 80}),
            ],
        ),
        (
            "projects",
            vec![
                json!({"id": "p-warp", "name": "Warp core refit", "updatedAt": at(0),
                   "teams": {"nodes": [{"id": "t-eng"}]}, "issueCountHistory": [1.0]}),
                json!({"id": "p-probe", "name": "Probe survey", "updatedAt": at(0),
                   "teams": {"nodes": [{"id": "t-sci"}]}, "issueCountHistory": [2.0]}),
            ],
        ),
        (
            "users",
            vec![
                json!({"id": "u-picard", "name": "Jean-Luc Picard", "updatedAt": at(0),
                   "lastSeen": at(1)}),
                json!({"id": "u-crusher", "name": "Beverly Crusher", "updatedAt": at(0)}),
            ],
        ),
        ("issues", (1..=issues).map(|n| issue(n, n)).collect()),
        (
            "comments",
            vec![
                json!({"id": "c-1", "issueId": "i-001", "body": "Make it so.", "updatedAt": at(5),
                   "issue": {"team": {"id": "t-sci"}}}),
            ],
        ),
        (
            "documents",
            vec![
                json!({"id": "d-1", "title": "Away team protocol", "updatedAt": at(3),
                   "team": {"id": "t-eng"}}),
            ],
        ),
        (
            "projectUpdates",
            vec![
                json!({"id": "pu-1", "body": "On course.", "updatedAt": at(4),
                   "project": {"id": "p-probe"}, "isStale": false}),
            ],
        ),
        (
            "attachments",
            vec![
                json!({"id": "a-eng", "title": "Sensor log", "updatedAt": at(6),
                   "issue": {"id": "i-003", "team": {"id": "t-eng"}, "project": null}}),
                json!({"id": "a-sci", "title": "Star chart", "updatedAt": at(7),
                   "issue": {"id": "i-001", "team": {"id": "t-sci"}, "project": null}}),
            ],
        ),
    ])
}

async fn run(fake: &FakeLinear, db: &RawDb, sync: LinearSync) -> FetchSummary {
    ingest::fetch_with(
        fake,
        FetchOptions {
            sync,
            ..FetchOptions::new(db.clone())
        },
    )
    .await
    .expect("the download runs")
}

/// Seals what the run wrote and names the commit.
async fn seal(db: &RawDb, msg: &str) -> String {
    dr::commit_run(db.pool(), msg).await.unwrap();
    dr::head_commit(db.pool()).await.unwrap().unwrap()
}

/// The record tables whose rows differ between two commits. `coverage`
/// moves whenever a walk reads something newer, and `sqlite_sequence`
/// with every run's `sync_runs` row; neither is a record.
async fn records_changed(db: &RawDb, from: &str, to: &str) -> Vec<String> {
    let records: Vec<&str> = schema_raw::KINDS.iter().map(|k| k.table).collect();
    dr::content_tables_changed(db.pool(), from, to)
        .await
        .unwrap()
        .into_iter()
        .filter(|t| records.contains(&t.as_str()))
        .collect()
}

async fn ids(db: &RawDb, table: &str) -> Vec<String> {
    let sql = format!("SELECT id FROM {table} WHERE payload IS NOT NULL ORDER BY id");
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .fetch_all(db.pool())
        .await
        .unwrap()
}

async fn payload(db: &RawDb, table: &str, id: &str) -> Value {
    let sql = format!("SELECT json(payload) FROM {table} WHERE id = ?");
    let text: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

async fn volatile(db: &RawDb, table: &str, id: &str) -> Value {
    let sql = format!("SELECT json(volatile_payload) FROM {table}_bookkeeping WHERE id = ?");
    let text: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    text.map(|t| serde_json::from_str(&t).unwrap())
        .unwrap_or(Value::Null)
}

const WALKS: &[&str] = &[
    "issues",
    "comments",
    "documents",
    "projectUpdates",
    "attachments",
];

/// The incremental promise: once a workspace is mirrored, a run over it
/// unchanged asks each walked connection for one page and writes no
/// content at all.
#[tokio::test]
async fn a_run_over_an_unchanged_workspace_reads_one_page_per_walk_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    let fake = workspace(120);

    let first = run(&fake, &db, LinearSync::default()).await;
    assert_eq!(ids(&db, "issues").await.len(), 120);
    assert_eq!(first.written, 120 + 1 + 1 + 1 + 2);
    assert_eq!(fake.requests_for("issues"), 3, "120 issues, 50 a page");
    let c1 = seal(&db, "first").await;

    fake.reset_counts();
    let second = run(&fake, &db, LinearSync::default()).await;
    for root in WALKS {
        assert_eq!(fake.requests_for(root), 1, "{root}");
    }
    assert_eq!(second.written, 0, "{second:?}");
    let c2 = seal(&db, "second").await;
    assert_eq!(records_changed(&db, &c1, &c2).await, Vec::<String>::new());
    db.close().await;
}

#[tokio::test]
async fn an_edit_is_the_one_record_a_later_run_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    let fake = workspace(120);
    run(&fake, &db, LinearSync::default()).await;
    let c1 = seal(&db, "first").await;

    let mut edited = issue(5, 500);
    edited["title"] = json!("Reroute power to the forward shields");
    fake.put("issues", edited);
    let second = run(&fake, &db, LinearSync::default()).await;
    assert_eq!(second.written, 1, "{second:?}");
    assert_eq!(
        payload(&db, "issues", "i-005").await["title"],
        "Reroute power to the forward shields"
    );
    let c2 = seal(&db, "second").await;
    assert_eq!(records_changed(&db, &c1, &c2).await, ["issues"]);
    db.close().await;
}

/// Dragging an issue on a board moves its sort order and its
/// `updatedAt` and nothing else; both live in the sidecar, so the
/// issue's content does not change.
#[tokio::test]
async fn a_board_drag_changes_no_content() {
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    let fake = workspace(10);
    run(&fake, &db, LinearSync::default()).await;
    let c1 = seal(&db, "first").await;
    let stored = payload(&db, "issues", "i-004").await;
    assert!(stored.get("updatedAt").is_none() && stored.get("sortOrder").is_none());
    assert_eq!(
        stored["labelIds"],
        json!(["l-a", "l-b"]),
        "an id bag is sorted"
    );
    assert_eq!(volatile(&db, "issues", "i-004").await["sortOrder"], 4.0);

    let mut dragged = issue(4, 600);
    dragged["sortOrder"] = json!(-17.5);
    fake.put("issues", dragged);
    run(&fake, &db, LinearSync::default()).await;
    assert_eq!(volatile(&db, "issues", "i-004").await["sortOrder"], -17.5);
    let c2 = seal(&db, "second").await;
    assert_eq!(records_changed(&db, &c1, &c2).await, Vec::<String>::new());
    db.close().await;
}

/// The run that died on its second page of issues kept the first, newest
/// page. The next run reads what is new since, then finishes the history
/// under where the first one stopped, and nothing in between is lost.
#[tokio::test]
async fn a_walk_cut_short_finishes_on_the_next_run_after_what_is_new() {
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    let fake = workspace(120);
    *fake.fail.lock().unwrap() = Some(("issues".into(), 2));

    run(&fake, &db, LinearSync::default()).await;
    let held = ids(&db, "issues").await;
    assert_eq!(held.len(), 50);
    assert_eq!(
        held.first().map(String::as_str),
        Some("i-071"),
        "the newest fifty"
    );

    fake.put("issues", issue(121, 700));
    fake.reset_counts();
    run(&fake, &db, LinearSync::default()).await;
    assert_eq!(ids(&db, "issues").await.len(), 121);
    assert_eq!(
        fake.served_for("issues"),
        2 + 72,
        "the two at or above what is held, then the 71 under it with the record at the \
         page boundary read twice — not the whole workspace again"
    );
    db.close().await;
}

/// Each page of a walk was its own SQL transaction, and a doltlite
/// transaction leaves behind every page of the tree it rewrote, so a
/// first run over randomly keyed records grew the store to several times
/// what it keeps: 1.2 GB for about 92 MB of records on a real workspace,
/// 494 MB after `dolt_gc()`. The pages between two seals now share one
/// transaction, so there is little for gc to reclaim.
#[tokio::test]
async fn a_first_run_leaves_little_for_gc_to_reclaim() {
    let dir = tempfile::tempdir().unwrap();
    let path = ingest::db_path_for(dir.path());
    let db = RawDb::open(&path).await.unwrap();
    let fake = workspace(0);
    let log = "Sensor sweep of the Neutral Zone logged and filed. ".repeat(20);
    let issues: Vec<Value> = (1..=1500u32)
        .map(|n| {
            let mut i = issue(n, n);
            // Linear's ids are uuidv4s: in no order the walk could follow.
            i["id"] = json!(format!("{:08x}", n.wrapping_mul(2_654_435_761)));
            i["description"] = json!(log);
            i
        })
        .collect();
    fake.set("issues", issues);
    run(&fake, &db, LinearSync::default()).await;
    seal(&db, "first").await;
    db.close().await;
    let written = std::fs::metadata(&path).unwrap().len();

    let db = RawDb::open(&path).await.unwrap();
    sqlx::query("SELECT dolt_gc()")
        .execute(db.pool())
        .await
        .unwrap();
    db.close().await;
    let kept = std::fs::metadata(&path).unwrap().len();
    assert!(
        kept * 10 >= written * 7,
        "gc took the store from {written} to {kept} bytes"
    );
}

/// History beyond the fifty entries that come with an issue is fetched
/// for an issue that changed, and not again for one that did not.
#[tokio::test]
async fn history_past_what_comes_with_an_issue_is_fetched_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    let fake = workspace(3);
    let entries: Vec<Value> = (0..60u32)
        .rev()
        .map(|n| {
            json!({"id": format!("h-{n:02}"), "updatedAt": at(n), "issue": {"id": "i-002"},
                   "toStateId": "s-done"})
        })
        .collect();
    fake.history.lock().unwrap().insert("i-002".into(), entries);

    let first = run(&fake, &db, LinearSync::default()).await;
    assert_eq!(ids(&db, "issue_history").await.len(), 60);
    assert_eq!(first.history, 60);
    assert_eq!(fake.requests_for("issue_history"), 1);

    fake.reset_counts();
    run(&fake, &db, LinearSync::default()).await;
    assert_eq!(fake.requests_for("issue_history"), 0);
    db.close().await;
}

/// A team brings every issue in it; a project brings its issues from any
/// team; an issue in neither is left out, and so is an attachment on it.
#[tokio::test]
async fn a_team_brings_its_issues_and_a_project_its_others() {
    let dir = tempfile::tempdir().unwrap();
    let db = RawDb::open(&ingest::db_path_for(dir.path())).await.unwrap();
    let fake = workspace(9);
    let sync = LinearSync {
        teams: vec!["eng".into()],
        projects: vec!["Probe survey".into()],
    };
    run(&fake, &db, sync).await;
    assert_eq!(
        ids(&db, "issues").await,
        ["i-002", "i-003", "i-005", "i-006", "i-008", "i-009"],
        "ENG's issues (n % 3 == 0) and Probe survey's (n % 3 == 2)"
    );
    assert_eq!(ids(&db, "projects").await, ["p-probe", "p-warp"]);
    assert_eq!(ids(&db, "attachments").await, ["a-eng"]);
    assert_eq!(
        payload(&db, "attachments", "a-eng").await["issue"],
        json!({"id": "i-003"}),
        "the team and project asked for to decide were not stored"
    );
    db.close().await;
}
