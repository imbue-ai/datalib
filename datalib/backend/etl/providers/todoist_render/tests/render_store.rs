//! A TNG account written into a real raw store the way the download
//! writes it, then read back and rendered: the store reads (JSONB
//! payloads, the diff that narrows a run to the projects that changed)
//! are what the unit tests cannot reach.

use datalib_etl::doltlite_raw::commit_run;
use datalib_etl_render::inputs::{Inputs, RawRange};
use datalib_etl_todoist::ingest::db::{self, Stamp};
use datalib_etl_todoist::ingest::schema_raw::{self, Kind};
use datalib_etl_todoist::ingest::RawDb;
use datalib_etl_todoist_render::render::build;
use datalib_etl_todoist_render::store;
use datalib_schema::grid_rows::GridRow;
use datalib_time::IsoOffsetTimestamp;
use serde_json::{json, Value};

const REFIT: &str = "6RefitProject";
const INBOX: &str = "6InboxPicard";
const AWAY: &str = "6AwayMissions";
const GONE: &str = "6ArchivedProject";

fn task(
    id: &str,
    project: &str,
    section: Option<&str>,
    parent: Option<&str>,
    content: &str,
    order: i64,
) -> Value {
    json!({"id": id, "user_id": "1701", "project_id": project, "content": content,
           "description": "", "priority": 1, "due": null, "deadline": null,
           "parent_id": parent, "child_order": order, "section_id": section, "labels": [],
           "added_by_uid": "1701", "responsible_uid": null, "checked": false,
           "is_deleted": false, "added_at": "2026-09-20T08:00:00.000000Z",
           "updated_at": "2026-09-20T08:00:00.000000Z", "completed_at": null})
}

fn account() -> Vec<(Kind, Vec<Value>)> {
    let mut coils = task(
        "6TaskCoils",
        REFIT,
        Some("6SecEngineering"),
        None,
        "Recalibrate the warp coils",
        1,
    );
    coils["priority"] = json!(4);
    coils["labels"] = json!(["engineering", "warp"]);
    coils["responsible_uid"] = json!("1702");
    coils["description"] = json!("Check the **dilithium** matrix first.");
    coils["due"] = json!({"date": "2026-10-03", "string": "Oct 3", "is_recurring": false});
    let mut tea = task("6TaskTea", INBOX, None, None, "Tea, Earl Grey, hot", 1);
    tea["description"] = json!("<script>x</script> & co");
    let mut farpoint = task(
        "6TaskFarpoint",
        AWAY,
        None,
        None,
        "Survey Farpoint station",
        1,
    );
    farpoint["is_deleted"] = json!(true);
    let mut chair = task(
        "6TaskChair",
        REFIT,
        Some("6SecBridge"),
        None,
        "Polish the captain's chair",
        1,
    );
    chair["checked"] = json!(true);
    chair["completed_at"] = json!("2026-09-25T10:00:00.000000Z");
    let project = |id: &str, name: &str| {
        json!({"id": id, "name": name, "is_deleted": false, "is_archived": false,
               "created_at": "2026-03-02T09:00:00Z", "updated_at": "2026-09-01T09:00:00Z"})
    };
    vec![
        (
            schema_raw::USER,
            vec![json!({"id": "1701", "email": "picard@enterprise.test",
                        "full_name": "Jean-Luc Picard", "joined_at": "2026-03-02T09:00:00Z"})],
        ),
        (
            schema_raw::PROJECTS,
            vec![
                project(INBOX, "Inbox"),
                project(REFIT, "Enterprise refit"),
                project(AWAY, "Away missions"),
            ],
        ),
        (
            schema_raw::SECTIONS,
            vec![
                json!({"id": "6SecBridge", "name": "Bridge", "project_id": REFIT, "section_order": 2,
                       "is_deleted": false, "is_archived": false}),
                json!({"id": "6SecEngineering", "name": "Engineering", "project_id": REFIT,
                       "section_order": 1, "is_deleted": false, "is_archived": false}),
            ],
        ),
        (
            schema_raw::ITEMS,
            vec![
                coils,
                task(
                    "6TaskConduit",
                    REFIT,
                    Some("6SecEngineering"),
                    Some("6TaskCoils"),
                    "Replace the plasma conduit",
                    1,
                ),
                chair,
                tea,
                farpoint,
            ],
        ),
        (
            schema_raw::NOTES,
            vec![
                json!({"id": "6NoteCoils1", "posted_uid": "1702", "item_id": "6TaskCoils",
                        "content": "The coils need a full day.", "is_deleted": false,
                        "posted_at": "2026-09-20T09:00:00.000000Z",
                        "file_attachment": {"file_name": "coil-specs.pdf",
                                            "file_url": "https://files.todoist.test/coil-specs.pdf"}}),
            ],
        ),
        (
            schema_raw::PROJECT_NOTES,
            vec![
                json!({"id": "6PNoteRefit", "posted_uid": 1701, "project_id": REFIT,
                        "content": "The refit begins at Utopia Planitia.", "is_deleted": false,
                        "posted_at": "2026-09-19T09:00:00.000000Z"}),
            ],
        ),
        (
            schema_raw::COLLABORATORS,
            vec![json!({"id": "1702", "email": "laforge@enterprise.test",
                        "full_name": "Geordi La Forge"})],
        ),
        (
            schema_raw::COMPLETED_ITEMS,
            vec![
                json!({"id": "6DonePhysical", "project_id": INBOX, "content": "Pass the Starfleet physical",
                       "checked": true, "is_deleted": false, "added_at": "2026-03-10T08:00:00Z",
                       "completed_at": "2026-09-12T15:00:00.000000Z",
                       "updated_at": "2026-09-12T15:00:00.000000Z"}),
                json!({"id": "6DoneOld", "project_id": GONE, "content": "Decommission the Stargazer",
                       "checked": true, "is_deleted": false, "added_at": "2026-03-10T08:00:00Z",
                       "completed_at": "2026-04-01T15:00:00.000000Z",
                       "updated_at": "2026-04-01T15:00:00.000000Z"}),
            ],
        ),
    ]
}

async fn write(db: &RawDb, records: Vec<(Kind, Vec<Value>)>) {
    let now = IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.unwrap();
    for (kind, rows) in records {
        db::write(&mut tx, kind, &rows, Stamp::Fetched, &now)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
}

fn doc_rows(rows: &[GridRow]) -> usize {
    rows.iter().filter(|r| r.is_document).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_project_renders_its_sections_tasks_and_comments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ingest").join("entities.doltlite_db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = RawDb::open(&path).await.unwrap();
    write(&db, account()).await;
    commit_run(db.pool(), "test fixture").await.unwrap();
    db.close().await;

    let db = RawDb::open_reader(&path, None)
        .await
        .unwrap()
        .expect("committed");
    let s = store::load(&db, RawRange::cold()).await.unwrap();
    db.close().await;
    assert_eq!(s.render, None, "a cold run renders everything");
    assert_eq!(s.project_ids(), [GONE, AWAY, INBOX, REFIT]);

    let refit = build("todoist", &s, REFIT, &Inputs::default());
    let md: String = refit.sections.iter().map(|x| x.md.as_str()).collect();
    let at = |needle: &str| {
        md.find(needle)
            .unwrap_or_else(|| panic!("no {needle:?} in:\n{md}"))
    };
    assert!(at("## Comments on the project") < at("The refit begins at Utopia Planitia."));
    assert!(at("## Engineering") < at("### ☐ Recalibrate the warp coils"));
    assert!(at("### ☐ Recalibrate the warp coils") < at("The coils need a full day."));
    assert!(at("The coils need a full day.") < at("#### ☐ Replace the plasma conduit"));
    assert!(at("#### ☐ Replace the plasma conduit") < at("## Bridge"));
    assert!(at("## Bridge") < at("<summary>✓ 1 completed</summary>"));
    assert!(at("<summary>✓ 1 completed</summary>") < at("### ☑ Polish the captain's chair"));
    at("p1 · due <time");
    at("@engineering @warp");
    at("assigned to [Geordi La Forge]");
    at("Check the **dilithium** matrix first.");
    at("📎 [coil-specs.pdf](https://files.todoist.test/coil-specs.pdf)");
    at("data-section-uuid=");

    let kinds: Vec<&str> = refit.rows.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "Todoist Project",
            "Todoist Comment",
            "Todoist Task",
            "Todoist Comment",
            "Todoist Task",
            "Todoist Task",
        ]
    );
    assert_eq!(doc_rows(&refit.rows), 1);
    assert_eq!(refit.rows[0].item_count, Some(5));
    assert_eq!(
        refit.rows[0].account.as_deref(),
        Some("picard@enterprise.test")
    );
    let comment = &refit.rows[3];
    assert_eq!(comment.author.as_deref(), Some("Geordi La Forge"));
    assert_eq!(comment.channel.as_deref(), Some("Engineering"));
    assert!(refit.problems.is_empty(), "{:?}", refit.problems);

    // Plain text from upstream is escaped where it becomes markup.
    let inbox = build("todoist", &s, INBOX, &Inputs::default());
    let md: String = inbox.sections.iter().map(|x| x.md.as_str()).collect();
    assert!(
        md.contains("&lt;script&gt;x&lt;/script&gt; &amp; co"),
        "{md}"
    );
    assert!(!md.contains("<script>"), "{md}");
    assert!(
        md.contains("Pass the Starfleet physical"),
        "a completed-only task renders: {md}"
    );

    // A deleted task is gone; the project stays.
    let away = build("todoist", &s, AWAY, &Inputs::default());
    let md: String = away.sections.iter().map(|x| x.md.as_str()).collect();
    assert!(!md.contains("Farpoint"), "{md}");
    assert_eq!(away.rows.len(), 1);

    // A project only a completed task names still gets its tasks a home.
    let gone = build("todoist", &s, GONE, &Inputs::default());
    let md: String = gone.sections.iter().map(|x| x.md.as_str()).collect();
    assert!(md.contains("Decommission the Stargazer"), "{md}");
    assert!(md.contains("did not send this project"), "{md}");
}

/// A new comment on one task names that task's project, and only it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_render_narrows_to_the_projects_whose_rows_moved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ingest").join("entities.doltlite_db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = RawDb::open(&path).await.unwrap();
    write(&db, account()).await;
    let first = commit_run(db.pool(), "first")
        .await
        .unwrap()
        .expect("a commit");
    write(
        &db,
        vec![(
            schema_raw::NOTES,
            vec![
                json!({"id": "6NoteConduit", "posted_uid": "1701", "item_id": "6TaskConduit",
                        "content": "Make it so.", "is_deleted": false,
                        "posted_at": "2026-09-21T09:00:00.000000Z"}),
            ],
        )],
    )
    .await;
    commit_run(db.pool(), "second").await.unwrap();
    db.close().await;

    let db = RawDb::open_reader(&path, None)
        .await
        .unwrap()
        .expect("committed");
    // As the driver hands it over: a cursor, and no bucket whose
    // declared rows moved.
    let stale = std::collections::HashSet::new();
    let range = RawRange {
        cursor: Some(&first),
        pin: None,
        stale: Some(&stale),
    };
    let s = store::load(&db, range).await;
    db.close().await;
    let s = s.unwrap();
    let render = s.render.expect("a cursor narrows the run");
    assert_eq!(render.into_iter().collect::<Vec<_>>(), [REFIT]);
}
