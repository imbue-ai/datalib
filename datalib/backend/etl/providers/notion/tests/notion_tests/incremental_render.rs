//! The forward scan that narrows notion's render: which page and
//! thread buckets a change names, through the rows still there.

use datalib_etl_notion::ingest::db::{CommentUpsert, PageMarkdownUpsert, PageUpsert, RawDb};
use datalib_etl_notion_render::render::parse_api_dir;
use datalib_etl_render::inputs::RawRange;
use serde_json::json;
use std::collections::HashSet;
use tempfile::tempdir;

const A: &str = "aaaaaaaa-1111-2222-3333-444444444444";
const B: &str = "bbbbbbbb-1111-2222-3333-444444444444";

/// Bound, not interpolated — the statement stays `&'static str` and
/// needs no `AssertSqlSafe`.
async fn commit(db: &RawDb, msg: &str) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT dolt_commit('-Am', ?)")
        .bind(msg)
        .fetch_one(db.pool())
        .await
        .unwrap()
        .unwrap_or_default()
}

fn page(id: &str) -> PageUpsert {
    PageUpsert {
        id: id.into(),
        last_edited_time: Some("2026-09-07T00:00:00.000Z".into()),
        payload: Some(serde_json::to_string(&json!({"id": id, "object": "page"})).unwrap()),
        ..Default::default()
    }
}

fn body(id: &str, md: &str) -> PageMarkdownUpsert {
    PageMarkdownUpsert {
        id: id.into(),
        markdown: md.into(),
        ..Default::default()
    }
}

async fn seed(path: &std::path::Path) -> String {
    let db = RawDb::open(path).await.unwrap();
    db.upsert_pages(&[page(A), page(B)]).await.unwrap();
    db.upsert_page_markdown(&[body(A, "# A\n"), body(B, "# B\n")])
        .await
        .unwrap();
    let head = commit(&db, "seed").await;
    db.close().await;
    head
}

fn ids(pages: &[serde_json::Value]) -> Vec<&str> {
    pages.iter().map(|p| p["id"].as_str().unwrap()).collect()
}

fn set(keys: &[&str]) -> HashSet<String> {
    keys.iter().map(|k| k.to_string()).collect()
}

/// The range the driver hands a warm run: a cursor to diff from and
/// its own stale set — empty, when nothing declared moved.
fn warm<'a>(cursor: &'a str, stale: &'a HashSet<String>) -> RawRange<'a> {
    RawRange {
        cursor: Some(cursor),
        pin: None,
        stale: Some(stale),
    }
}

/// The whole point: a second render with nothing changed upstream is
/// handed no buckets at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_store_yields_no_pages_to_render() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    seed(&f).await;

    let cold = parse_api_dir(&f, RawRange::cold()).unwrap();
    assert_eq!(cold.pages.len(), 2, "a cold start renders everything");
    assert!(
        cold.render.is_none(),
        "no cursor means no diff was asked for"
    );
    let head = cold.head.clone().expect("a head to resume from");
    let none: HashSet<String> = HashSet::new();

    let warm = parse_api_dir(&f, warm(&head, &none)).unwrap();
    assert!(warm.pages.is_empty(), "nothing changed, so nothing renders");
    assert_eq!(warm.render, Some(HashSet::new()));
}

/// And a run where one page moved is handed exactly that page — not the
/// other, and not everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_changed_page_is_handed_to_render() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    let head = seed(&f).await;
    let none: HashSet<String> = HashSet::new();

    let db = RawDb::open(&f).await.unwrap();
    db.upsert_page_markdown(&[body(B, "# B\n\nnew paragraph\n")])
        .await
        .unwrap();
    commit(&db, "edit B").await;
    db.close().await;

    let parsed = parse_api_dir(&f, warm(&head, &none)).unwrap();
    assert_eq!(parsed.render, Some(set(&[B])));
    assert_eq!(ids(&parsed.pages), vec![B], "only the edited page");
    assert!(parsed.markdown_by_page.contains_key(B));
    assert!(!parsed.markdown_by_page.contains_key(A));
}

/// A new comment names its thread, and the thread brings its page
/// along for the title — as a page to read, not one to render.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_comment_names_its_thread() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    let head = seed(&f).await;
    let none: HashSet<String> = HashSet::new();

    let db = RawDb::open(&f).await.unwrap();
    db.upsert_comments(&[CommentUpsert {
        id: "c1".into(),
        discussion_id: Some("d1".into()),
        page_id: Some(A.into()),
        payload: serde_json::to_string(&json!({"id": "c1", "discussion_id": "d1"})).unwrap(),
        ..Default::default()
    }])
    .await
    .unwrap();
    commit(&db, "comment on A").await;
    db.close().await;

    let parsed = parse_api_dir(&f, warm(&head, &none)).unwrap();
    assert_eq!(parsed.render, Some(set(&["d1"])));
    assert_eq!(
        ids(&parsed.pages),
        vec![A],
        "the thread's page, for its title"
    );
    assert!(!parsed.renders(A), "the page itself did not change");
    assert_eq!(parsed.comments.len(), 1);
}

/// A page's own row changing names its threads too, since they carry
/// its title.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_page_names_its_threads() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");

    let db = RawDb::open(&f).await.unwrap();
    db.upsert_pages(&[page(A)]).await.unwrap();
    db.upsert_comments(&[CommentUpsert {
        id: "c1".into(),
        discussion_id: Some("d1".into()),
        page_id: Some(A.into()),
        payload: serde_json::to_string(&json!({"id": "c1", "discussion_id": "d1"})).unwrap(),
        ..Default::default()
    }])
    .await
    .unwrap();
    let head = commit(&db, "seed with a thread").await;
    let none: HashSet<String> = HashSet::new();
    db.upsert_pages(&[PageUpsert {
        last_edited_time: Some("2026-09-09T00:00:00.000Z".into()),
        ..page(A)
    }])
    .await
    .unwrap();
    commit(&db, "A edited").await;
    db.close().await;

    let parsed = parse_api_dir(&f, warm(&head, &none)).unwrap();
    assert_eq!(parsed.render, Some(set(&[A, "d1"])));
}

/// Deletion. A page the diff names whose row is gone is a bucket to
/// render with nothing in it: the processor declares it empty and the
/// driver drops its documents.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_page_is_a_bucket_with_no_rows() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    let head = seed(&f).await;
    let none: HashSet<String> = HashSet::new();

    let db = RawDb::open(&f).await.unwrap();
    sqlx::query("DELETE FROM pages WHERE id = ?")
        .bind(A)
        .execute(db.pool())
        .await
        .unwrap();
    commit(&db, "A deleted upstream").await;
    db.close().await;

    let parsed = parse_api_dir(&f, warm(&head, &none)).unwrap();
    assert_eq!(parsed.render, Some(set(&[A])));
    assert!(
        parsed.pages.is_empty(),
        "the deleted page has no rows left to render"
    );
}

/// A thread whose last comment went: the comment's row is gone, so the
/// forward scan cannot name the thread — the driver does, from the
/// `(comments, c1)` input the thread declared when it rendered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_whose_last_comment_went_is_the_drivers_to_name() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");

    let db = RawDb::open(&f).await.unwrap();
    db.upsert_pages(&[page(A)]).await.unwrap();
    db.upsert_page_markdown(&[body(A, "# A\n")]).await.unwrap();
    db.upsert_comments(&[CommentUpsert {
        id: "c1".into(),
        discussion_id: Some("d1".into()),
        page_id: Some(A.into()),
        payload: serde_json::to_string(&json!({"id": "c1", "discussion_id": "d1"})).unwrap(),
        ..Default::default()
    }])
    .await
    .unwrap();
    let head = commit(&db, "seed with a thread").await;
    let none: HashSet<String> = HashSet::new();
    db.close().await;

    let db = RawDb::open(&f).await.unwrap();
    sqlx::query("DELETE FROM comments WHERE id = 'c1'")
        .execute(db.pool())
        .await
        .unwrap();
    commit(&db, "thread resolved away").await;
    db.close().await;

    let forward = parse_api_dir(&f, warm(&head, &none)).unwrap();
    assert_eq!(forward.render, Some(HashSet::new()));

    let stale = set(&["d1"]);
    let parsed = parse_api_dir(&f, warm(&head, &stale)).unwrap();
    assert_eq!(parsed.render, Some(set(&["d1"])));
    assert!(
        parsed.comments.is_empty(),
        "nothing left to render under it"
    );
}
