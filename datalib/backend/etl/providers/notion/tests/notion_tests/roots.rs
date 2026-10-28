//! Roots mode mirrors what hangs under each root, found by its parent
//! in the workspace's search rather than by a link in the root's body,
//! and the pages a root's body links. A database pasted as a root was
//! refused as "a database, not a page", and a root page's sub-pages
//! that its body did not link were never found.

use datalib_etl::doltlite_raw as dr;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_notion::ingest::official::BASE;
use datalib_etl_notion::ingest::schema_raw::PAGES;
use datalib_etl_notion::ingest::RawDb;
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::*;

const MISSIONS: &str = "1701d000-0000-4000-8000-000000000010";
const MISSIONS_DS: &str = "1701d000-0000-4000-8000-000000000011";
const AWAY_TEAM: &str = "1701d000-0000-4000-8000-000000000012";
const DEBRIEF: &str = "1701d000-0000-4000-8000-000000000013";
const TOGGLE: &str = "1701d000-0000-4000-8000-000000000020";
const READY_ROOM: &str = "1701d000-0000-4000-8000-000000000021";
const SICKBAY_CHART: &str = "1701d000-0000-4000-8000-000000000022";
const CREW: &str = "1701d000-0000-4000-8000-000000000030";
const CREW_DS: &str = "1701d000-0000-4000-8000-000000000031";
const UNSEEN: &str = "1701d000-0000-4000-8000-000000000032";

fn hanging_off(id: &str, parent: Value) -> Value {
    let mut p = page(id, EDITED);
    p["parent"] = parent;
    p
}

fn under_page(id: &str, parent: &str) -> Value {
    hanging_off(id, json!({"type": "page_id", "page_id": parent}))
}

fn mention(id: &str) -> String {
    format!(
        "<mention-page url=\"https://app.notion.com/p/{}\"/>",
        id.replace('-', "")
    )
}

/// The workspace: a database of missions with a row and a page under
/// it; the bridge, with a page inside a toggle on it and a mention of
/// sickbay, and links to things that are not pages (its toggle, the crew
/// database, something the search does not list); sickbay's own
/// sub-page, and the holodeck, which nothing reaches.
fn workspace(tape: &std::path::Path) {
    let results = json!([
        {
            "object": "data_source", "id": MISSIONS_DS, "last_edited_time": EDITED,
            "parent": {"type": "database_id", "database_id": MISSIONS},
            "database_parent": {"type": "workspace", "workspace": true},
        },
        hanging_off(
            AWAY_TEAM,
            json!({"type": "data_source_id", "data_source_id": MISSIONS_DS, "database_id": MISSIONS}),
        ),
        under_page(DEBRIEF, AWAY_TEAM),
        page(BRIDGE, EDITED),
        hanging_off(READY_ROOM, json!({"type": "block_id", "block_id": TOGGLE})),
        page(SICKBAY, EDITED),
        under_page(SICKBAY_CHART, SICKBAY),
        page(HOLODECK, EDITED),
        {
            "object": "data_source", "id": CREW_DS, "last_edited_time": EDITED,
            "parent": {"type": "database_id", "database_id": CREW},
            "database_parent": {"type": "workspace", "workspace": true},
        },
    ]);
    serve_search(tape, None, results.clone(), None);
    serve(
        tape,
        &format!("{BASE}/blocks/{TOGGLE}"),
        json!({"object": "block", "id": TOGGLE, "type": "toggle",
               "parent": {"type": "page_id", "page_id": BRIDGE}}),
    );
    let pages = results.as_array().unwrap().iter();
    for obj in pages.filter(|o| o["object"] == "page") {
        let id = obj["id"].as_str().unwrap();
        serve(tape, &format!("{BASE}/pages/{id}"), obj.clone());
        serve_comments(tape, id);
    }
    serve_body(
        tape,
        BRIDGE,
        &format!(
            "Senior staff to {}. See {}, {} and {}.\n",
            mention(SICKBAY),
            mention(TOGGLE),
            mention(CREW),
            mention(UNSEEN)
        ),
        false,
    );
    for id in [AWAY_TEAM, DEBRIEF, READY_ROOM, SICKBAY] {
        serve_body(tape, id, "", false);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn roots_mirror_what_hangs_under_them_and_what_a_root_links() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    workspace(&tape);
    let database_view = format!(
        "https://app.notion.com/p/enterprise/{}?v=1701d0000000400080000000000000ff",
        MISSIONS.replace('-', "")
    );
    let roots = [database_view.as_str(), BRIDGE];

    let first = run(&tape, &store, &roots).await.unwrap();
    assert!(
        problems(&store).await.is_empty(),
        "{:?}",
        problems(&store).await
    );
    let mut want = vec![AWAY_TEAM, BRIDGE, DEBRIEF, READY_ROOM, SICKBAY];
    want.sort();
    assert_eq!(
        stored_pages(&store).await,
        want,
        "the database's row and its sub-page, the root page and the page \
         in its toggle, and the page it mentions; not what hangs under that \
         one, not the links that are no page, and not the holodeck"
    );
    assert_eq!(first.new_pages, 5);

    let second = run(&tape, &store, &roots).await.unwrap();
    assert_eq!(
        second.official_requests, 1,
        "the search alone: every page is held at the stamp it lists, and \
         the toggle was placed once"
    );
}

/// A truncated body whose hole is an inline database asked Notion for
/// the database as a page of markdown, got a 400, and so was owed
/// forever and never stored. The hole is now the database's link.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_database_in_a_truncated_body_is_linked_not_fetched() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let crew = CREW.replace('-', "");
    serve_search(
        &tape,
        None,
        json!([
            page(BRIDGE, EDITED),
            {
                "object": "data_source", "id": CREW_DS, "last_edited_time": EDITED,
                "title": [{"plain_text": "Crew"}],
                "parent": {"type": "database_id", "database_id": CREW},
                "database_parent": {"type": "page_id", "page_id": BRIDGE},
            },
        ]),
        None,
    );
    serve_object(&tape, BRIDGE, EDITED);
    serve_comments(&tape, BRIDGE);
    serve_body(
        &tape,
        BRIDGE,
        &format!("Duty roster: <unknown url=\"https://app.notion.com/p/x#{crew}\"/>\n"),
        true,
    );

    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert!(
        problems(&store).await.is_empty(),
        "{:?}",
        problems(&store).await
    );
    let db = RawDb::open(&store).await.unwrap();
    let bodies = db.load_page_markdown().await.unwrap();
    db.close().await;
    assert_eq!(
        bodies,
        vec![(
            BRIDGE.to_string(),
            format!(
                "Duty roster: <database url=\"https://app.notion.com/p/{crew}\">Crew</database>\n"
            )
        )]
    );
}

/// A database pasted as a root was once asked for as a page, and the
/// 400 stood as an error on `pages:<root>` after the build that places
/// it as a database: nothing asked for that page again to clear it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_database_root_clears_the_page_error_an_older_build_left() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    workspace(&tape);
    let db = RawDb::open(&store).await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    dr::record_object_error(&mut tx, PAGES, MISSIONS, "HTTP 400: a database, not a page")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    db.commit_all("as an older build left it").await.unwrap();
    db.close().await;
    assert_eq!(
        problems(&store).await,
        vec![row(&format!("pages:{MISSIONS}"), "error")]
    );

    run(&tape, &store, &[MISSIONS]).await.unwrap();
    assert!(
        problems(&store).await.is_empty(),
        "{:?}",
        problems(&store).await
    );
    let db = RawDb::open(&store).await.unwrap();
    let stub: Option<String> = sqlx::query_scalar("SELECT id FROM pages WHERE id = ?")
        .bind(MISSIONS)
        .fetch_optional(db.pool())
        .await
        .unwrap();
    db.close().await;
    assert_eq!(stub, None, "the empty row the failed fetch seeded goes too");
}

/// A page moved under a root after the first run is mirrored by the
/// next, which lists it because moving it edited it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_moved_under_a_root_is_mirrored_by_the_next_run() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, EDITED), page(HOLODECK, EDITED)]),
        None,
    );
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(stored_pages(&store).await, vec![BRIDGE.to_string()]);

    let later = "2026-09-02T00:00:00.000Z";
    let mut moved = under_page(HOLODECK, BRIDGE);
    moved["last_edited_time"] = json!(later);
    serve_search(
        &tape,
        None,
        json!([moved.clone(), page(BRIDGE, EDITED)]),
        None,
    );
    serve(&tape, &format!("{BASE}/pages/{HOLODECK}"), moved);
    serve_body(&tape, HOLODECK, "Arch.\n", false);
    serve_comments(&tape, HOLODECK);
    run(&tape, &store, &[BRIDGE]).await.unwrap();
    assert_eq!(
        stored_pages(&store).await,
        vec![BRIDGE.to_string(), HOLODECK.to_string()]
    );
    assert!(problems(&store).await.is_empty());
}
