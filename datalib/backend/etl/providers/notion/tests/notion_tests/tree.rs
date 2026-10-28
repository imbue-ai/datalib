//! Where a page sits: the databases and blocks above it land in `tree`,
//! from search where it describes them and one request each otherwise,
//! and ordinary pages are fetched before a database's rows.

use datalib_etl_notion::ingest::db::RawDb;
use datalib_etl_notion::ingest::official::BASE;
use datalib_etl_notion::ingest::schema_raw::TreeKind;
use datalib_etl_web::owed::Listed;
use serde_json::{json, Value};
use tempfile::tempdir;

use crate::support::*;

const DB: &str = "1701d000-0000-4000-8000-0000000000db";
const COLUMN: &str = "1701d000-0000-4000-8000-0000000000c1";

/// A row of the crew database, edited at `edited`.
fn crew_row(id: &str, edited: &str) -> Value {
    json!({
        "object": "page", "id": id, "last_edited_time": edited,
        "parent": {"type": "data_source_id", "data_source_id": "ds-1", "database_id": DB},
    })
}

async fn tree(store: &std::path::Path) -> Vec<(String, Option<TreeKind>, Option<String>)> {
    let db = RawDb::open(store).await.unwrap();
    let mut rows: Vec<_> = db
        .load_tree()
        .await
        .unwrap()
        .into_iter()
        .map(|(id, r)| (id, r.kind, r.title))
        .collect();
    db.close().await;
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Search describes the database a row sits in, so its title costs no
/// request; the column it sits in and the page above that cost one
/// each, and the page this credential cannot list still has its title.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_places_a_row_under_its_database_and_the_pages_above() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let data_source = json!({
        "object": "data_source", "id": "ds-1", "last_edited_time": EDITED,
        "title": [{"plain_text": "Crew manifest"}],
        "parent": {"type": "database_id", "database_id": DB},
        "database_parent": {"type": "block_id", "block_id": COLUMN},
    });
    serve_search(
        &tape,
        None,
        json!([crew_row(SICKBAY, EDITED), data_source]),
        None,
    );
    serve(
        &tape,
        &format!("{BASE}/blocks/{COLUMN}"),
        json!({"object": "block", "id": COLUMN, "type": "column", "column": {},
               "parent": {"type": "page_id", "page_id": BRIDGE}}),
    );
    serve(
        &tape,
        &format!("{BASE}/blocks/{BRIDGE}"),
        json!({"object": "block", "id": BRIDGE, "type": "child_page",
               "child_page": {"title": "Bridge"},
               "parent": {"type": "workspace", "workspace": true}}),
    );
    serve_body(&tape, SICKBAY, "", false);
    serve_comments(&tape, SICKBAY);

    let s = run(&tape, &store, &[]).await.unwrap();
    assert_eq!(s.ancestors_resolved, 2, "the column and the page above it");
    assert!(problems(&store).await.is_empty());
    assert_eq!(
        tree(&store).await,
        [
            (BRIDGE.into(), Some(TreeKind::Block), Some("Bridge".into())),
            (SICKBAY.into(), Some(TreeKind::Page), None),
            (COLUMN.into(), Some(TreeKind::Block), None),
            (
                DB.into(),
                Some(TreeKind::Database),
                Some("Crew manifest".into())
            ),
        ]
    );
}

/// Outside search, a database is asked for by id; a parent Notion will
/// not show is kept with nothing known of it and not asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_database_is_fetched_once_and_a_hidden_parent_is_asked_once() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    // The search lists the row, not its database's data source.
    serve_search(&tape, None, json!([crew_row(SICKBAY, EDITED)]), None);
    serve(
        &tape,
        &format!("{BASE}/pages/{SICKBAY}"),
        crew_row(SICKBAY, EDITED),
    );
    serve_body(&tape, SICKBAY, "", false);
    serve_comments(&tape, SICKBAY);
    serve(
        &tape,
        &format!("{BASE}/databases/{DB}"),
        json!({"object": "database", "id": DB, "title": [{"plain_text": "Crew manifest"}],
               "parent": {"type": "page_id", "page_id": BRIDGE}}),
    );
    serve_status(&tape, &format!("{BASE}/blocks/{BRIDGE}"), 404);

    run(&tape, &store, &[SICKBAY]).await.unwrap();
    assert!(problems(&store).await.is_empty());
    let rows = tree(&store).await;
    assert!(rows.contains(&(
        DB.into(),
        Some(TreeKind::Database),
        Some("Crew manifest".into())
    )));
    assert!(rows.contains(&(BRIDGE.into(), Some(TreeKind::Block), None)));

    unserve(&tape, &format!("{BASE}/databases/{DB}"));
    unserve(&tape, &format!("{BASE}/blocks/{BRIDGE}"));
    let again = run(&tape, &store, &[SICKBAY]).await.unwrap();
    assert_eq!(again.ancestors_resolved, 0);
    assert!(
        problems(&store).await.is_empty(),
        "neither was asked for again"
    );
}

/// A workspace is mostly database rows, and fetching bodies newest
/// first reached its pages only after days of rows. A page comes before
/// every row, however recently the row was edited.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_are_owed_bodies_before_database_rows() {
    let d = tempdir().unwrap();
    let store = d.path().join("s.doltlite_db");
    let db = RawDb::open(&store).await.unwrap();
    let mut tx = db.pool().begin().await.unwrap();
    let rows: Vec<_> = [
        crew_row(SICKBAY, "2026-09-03T00:00:00.000Z"),
        page(BRIDGE, "2026-09-01T00:00:00.000Z"),
        crew_row(HOLODECK, "2026-09-02T00:00:00.000Z"),
        page(TEN_FORWARD, "2026-08-01T00:00:00.000Z"),
    ]
    .iter()
    .map(|p| datalib_etl_notion::ingest::db::PageUpsert::from_object(p).unwrap())
    .collect();
    db.store_pages(&mut tx, &rows).await.unwrap();
    tx.commit().await.unwrap();
    let order: Vec<String> = db
        .pages_listed()
        .await
        .unwrap()
        .into_iter()
        .map(|l: Listed| l.key)
        .collect();
    db.close().await;
    assert_eq!(order, [BRIDGE, TEN_FORWARD, SICKBAY, HOLODECK]);
}
