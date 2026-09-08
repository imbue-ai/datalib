//! The `dolt_diff` scan that narrows notion's render, and the deletions
//! it has to notice now that render no longer walks everything.

use datalib_etl_notion::download::db::{CommentUpsert, PageMarkdownUpsert, PageUpsert, RawDb};
use datalib_etl_notion::render::parse_api_dir;
use serde_json::json;
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

/// The whole point: a second render with nothing changed upstream is
/// handed no pages at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_store_yields_no_pages_to_render() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    seed(&f).await;

    let cold = parse_api_dir(&f, None).unwrap();
    assert_eq!(cold.pages.len(), 2, "a cold start renders everything");
    assert!(
        cold.scan.changed_buckets.is_none(),
        "no cursor means no diff was asked for"
    );
    let head = cold.scan.new_head.clone().expect("a head to resume from");

    let warm = parse_api_dir(&f, Some(&head)).unwrap();
    assert!(warm.pages.is_empty(), "nothing changed, so nothing renders");
    assert_eq!(warm.scan.changed_buckets.as_ref().unwrap().len(), 0);
}

/// And a run where one page moved is handed exactly that page — not the
/// other, and not everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_changed_page_is_handed_to_render() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    let head = seed(&f).await;

    let db = RawDb::open(&f).await.unwrap();
    db.upsert_page_markdown(&[body(B, "# B\n\nnew paragraph\n")])
        .await
        .unwrap();
    commit(&db, "edit B").await;
    db.close().await;

    let parsed = parse_api_dir(&f, Some(&head)).unwrap();
    let ids: Vec<&str> = parsed
        .pages
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![B], "only the edited page");
    assert!(parsed.markdown_by_page.contains_key(B));
    assert!(!parsed.markdown_by_page.contains_key(A));
}

/// A comment reaches its page's bucket through `comments.page_id`, with
/// no join — which is why the download side records it there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_comment_marks_its_page_changed() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    let head = seed(&f).await;

    let db = RawDb::open(&f).await.unwrap();
    db.upsert_comments(&[CommentUpsert {
        id: "c1".into(),
        discussion_id: Some("d1".into()),
        page_id: Some(A.into()),
        payload: serde_json::to_string(&json!({"id": "c1"})).unwrap(),
        ..Default::default()
    }])
    .await
    .unwrap();
    commit(&db, "comment on A").await;
    db.close().await;

    let parsed = parse_api_dir(&f, Some(&head)).unwrap();
    let ids: Vec<&str> = parsed
        .pages
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![A]);
}

/// Deletion. Render no longer walks the whole store, so absence from a
/// run means nothing — the page has to be named. This is the half that
/// `retain_documents` used to cover for free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_page_is_named_as_vanished() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    let head = seed(&f).await;

    let db = RawDb::open(&f).await.unwrap();
    sqlx::query("DELETE FROM pages WHERE id = ?")
        .bind(A)
        .execute(db.pool())
        .await
        .unwrap();
    commit(&db, "A deleted upstream").await;
    db.close().await;

    let parsed = parse_api_dir(&f, Some(&head)).unwrap();
    assert_eq!(parsed.vanished_pages, vec![A.to_string()]);
    assert!(
        parsed.pages.is_empty(),
        "the deleted page has no rows left to render"
    );
}

/// A page that merely has no body yet must never read as deleted — its
/// row is there, and `load_pages` filtering on `payload IS NOT NULL` is
/// exactly the trap `buckets_without_rows` exists to avoid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_awaiting_its_body_is_not_vanished() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");
    let head = seed(&f).await;

    let db = RawDb::open(&f).await.unwrap();
    // Discovery-shaped upsert: the row exists, the payload does not.
    db.upsert_pages(&[PageUpsert {
        id: "cccccccc-1111-2222-3333-444444444444".into(),
        last_edited_time: Some("2026-09-08T00:00:00.000Z".into()),
        payload: None,
        ..Default::default()
    }])
    .await
    .unwrap();
    commit(&db, "discovered C").await;
    db.close().await;

    let parsed = parse_api_dir(&f, Some(&head)).unwrap();
    assert!(
        parsed.vanished_pages.is_empty(),
        "a body-less page is pending, not deleted: {:?}",
        parsed.vanished_pages
    );
}

/// A thread whose last comment went, on a page that survived. The page
/// and its threads are separate documents with separate
/// `conversation_uuid`s, so removing the page would not have caught it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_whose_last_comment_went_is_named_as_vanished() {
    let d = tempdir().unwrap();
    let f = d.path().join("notion.doltlite_db");

    let db = RawDb::open(&f).await.unwrap();
    db.upsert_pages(&[page(A)]).await.unwrap();
    db.upsert_page_markdown(&[body(A, "# A\n")]).await.unwrap();
    db.upsert_comments(&[CommentUpsert {
        id: "c1".into(),
        discussion_id: Some("d1".into()),
        page_id: Some(A.into()),
        payload: serde_json::to_string(&json!({"id": "c1"})).unwrap(),
        ..Default::default()
    }])
    .await
    .unwrap();
    let head = commit(&db, "seed with a thread").await;
    db.close().await;

    let db = RawDb::open(&f).await.unwrap();
    sqlx::query("DELETE FROM comments WHERE id = 'c1'")
        .execute(db.pool())
        .await
        .unwrap();
    commit(&db, "thread resolved away").await;
    db.close().await;

    let parsed = parse_api_dir(&f, Some(&head)).unwrap();
    assert_eq!(parsed.vanished_discussions, vec!["d1".to_string()]);
    assert!(
        parsed.vanished_pages.is_empty(),
        "the page itself is still there"
    );
}
