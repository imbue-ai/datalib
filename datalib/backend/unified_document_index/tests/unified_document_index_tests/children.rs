//! A container's children read from the grid index around an anchor and
//! paged both ways: a workspace's channels, a channel's threads, a
//! thread's messages and a label's filed threads.

use std::sync::Arc;

use datalib_schema::data_nodes::{CHILDREN_INDEXES, DDL as DATA_NODES_DDL};
use datalib_schema::filings::DDL as FILINGS_DDL;
use datalib_schema::grid_rows::{GridRow, DDL as GRID_DDL};
use datalib_schema::providers::Provider;
use datalib_unified_document_index::children::{
    children_sql, Anchor, Ask, Child, ChildrenError, ChildrenFailure, ChildrenPage, Key, Side, Via,
    CHILD_KEY_SQL, NODE_FACTS_SQL,
};
use datalib_unified_document_index::dolt_repo::DoltRepo;
use datalib_unified_document_index::repo::IndexRepo;

use crate::dolt_backend_integration::{commit, insert_rows, unique_db_path, writer};

/// The tables the tree lives in, with the indexes the grid index makes
/// for listing children.
pub(crate) async fn create_tree(writer: &sqlx::SqlitePool) {
    let tables = GRID_DDL
        .iter()
        .chain(DATA_NODES_DDL.iter())
        .chain(FILINGS_DDL.iter())
        .map(|(_t, ddl)| *ddl);
    for ddl in tables.chain(CHILDREN_INDEXES.iter().copied()) {
        sqlx::query(ddl).execute(writer).await.expect("create");
    }
}

fn stamp(minute: u32) -> String {
    format!("2364-02-11T09:{minute:02}:00+00:00")
}

/// `stamp(minute)` as a sort key spells it.
fn key_at(minute: u32) -> String {
    format!("2364-02-11T09:{minute:02}:00.000000Z")
}

fn row(uuid: &str, parent: &str, markdown: &str, minute: u32, document: bool) -> GridRow {
    GridRow::builder()
        .uuid(uuid)
        .provider(Provider::Slack)
        .kind(if document { "Slack Thread" } else { "Message" })
        .source_label("Slack")
        .is_document(document)
        .created_at(Some(stamp(minute)))
        .author(Some("Geordi La Forge".to_string()))
        .conversation_uuid(markdown)
        .entire_chat(format!("/chat/{markdown}"))
        .body(format!("about {uuid}"))
        .markdown_uuid(Some(markdown.to_string()))
        .parent_uuid(Some(parent.to_string()))
        .build()
        .expect("a valid row")
}

async fn node(
    w: &sqlx::SqlitePool,
    carried_by: &str,
    uuid: &str,
    kind: &str,
    parent: Option<&str>,
    sort_key: &str,
) {
    sqlx::query(
        "INSERT INTO data_nodes \
         (markdown_uuid, uuid, source_id, kind, name, parent_uuid, sort_key, read_together, is_place) \
         VALUES (?, ?, 'slack', ?, ?, ?, ?, 0, 0)",
    )
    .bind(carried_by)
    .bind(uuid)
    .bind(kind)
    .bind(format!("#{uuid}"))
    .bind(parent)
    .bind(sort_key)
    .execute(w)
    .await
    .expect("insert a data node");
}

async fn file(w: &sqlx::SqlitePool, carried_by: &str, node: &str, under: &str) {
    sqlx::query("INSERT INTO filings (markdown_uuid, node_uuid, under_uuid) VALUES (?, ?, ?)")
        .bind(carried_by)
        .bind(node)
        .bind(under)
        .execute(w)
        .await
        .expect("insert a filing");
}

/// A workspace `ws` with two channels, `bridge` and `ops`, each a
/// container with no row; `bridge` holds threads `t1`..`t3`, each a
/// container with its own document row, and every thread's document
/// carries the channel and workspace again. `t1` holds messages `m1`..`m5`
/// a minute apart from minute 10. A label `lbl` has `t2`, `t3` and `m5`
/// filed under it.
async fn seeded() -> (DoltRepo, sqlx::SqlitePool) {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let w = writer(&root).await;
    create_tree(&w).await;
    let threads = [("t1", 1), ("t2", 2), ("t3", 3)];
    let mut rows = Vec::new();
    for (t, minute) in threads {
        rows.push(row(t, "bridge", t, minute, true));
        node(&w, t, t, "Slack thread", Some("bridge"), &key_at(minute)).await;
        node(&w, t, "bridge", "Slack channel", Some("ws"), "").await;
        node(&w, t, "ws", "Slack workspace", None, "").await;
    }
    node(&w, "t1", "ops", "Slack channel", Some("ws"), "").await;
    node(&w, "t2", "lbl", "Label", Some("acct"), "").await;
    file(&w, "t2", "t2", "lbl").await;
    file(&w, "t3", "t3", "lbl").await;
    // A row filed with no container of its own, as a photo is under a
    // keyword.
    file(&w, "t1", "m5", "lbl").await;
    for i in 1..=5 {
        rows.push(row(&format!("m{i}"), "t1", "t1", 9 + i, false));
    }
    insert_rows(&w, &rows).await;
    commit(&w, "seed").await;
    let repo = DoltRepo::open(root).await.expect("open the repo");
    (repo, w)
}

async fn page(repo: &DoltRepo, parent: &str, ask: Ask, limit: usize) -> ChildrenPage {
    match repo.children(parent, &ask, limit).await {
        Ok(page) => page,
        Err(e) => panic!("children of {parent}: {e:?}"),
    }
}

fn uuids(page: &ChildrenPage) -> Vec<&str> {
    page.children.iter().map(|c| c.uuid.as_str()).collect()
}

fn item(uuid: &str) -> Ask {
    Ask::Around(Anchor::Item(uuid.to_string()))
}

/// Around an item: `limit` before it, the item, and `limit` after it,
/// with a cursor at each end that has more past it.
#[tokio::test]
async fn the_window_around_an_item_holds_it_and_its_neighbours() {
    let (repo, _w) = seeded().await;
    let p = page(&repo, "t1", item("m3"), 1).await;
    assert_eq!(uuids(&p), ["m2", "m3", "m4"]);
    assert_eq!(p.before, Some(Key::new(key_at(11), "m2")));
    assert_eq!(p.after, Some(Key::new(key_at(13), "m4")));
    let p = page(&repo, "t1", item("m3"), 2).await;
    assert_eq!(uuids(&p), ["m1", "m2", "m3", "m4", "m5"]);
    assert_eq!((p.before, p.after), (None, None), "m1 and m5 are the ends");
    assert!(p.at.is_some(), "a page names the commit it was read at");
}

/// A moment anchors at the first child at or after it; a moment past
/// every child ("now") opens at the newest end.
#[tokio::test]
async fn the_window_around_a_moment_and_around_now() {
    let (repo, _w) = seeded().await;
    let between = "2364-02-11T09:12:30.000000Z".to_string();
    let p = page(&repo, "t1", Ask::Around(Anchor::Moment(between)), 1).await;
    assert_eq!(uuids(&p), ["m3", "m4"]);
    let exactly = Ask::Around(Anchor::Moment(key_at(12)));
    assert_eq!(uuids(&page(&repo, "t1", exactly, 1).await), ["m2", "m3"]);
    let now = Ask::Around(Anchor::Moment("2400-01-01T00:00:00.000000Z".to_string()));
    let p = page(&repo, "t1", now, 2).await;
    assert_eq!(uuids(&p), ["m4", "m5"]);
    assert_eq!(p.after, None);
    assert_eq!(p.before, Some(Key::new(key_at(13), "m4")));
}

/// Paging on from either cursor reaches each end, every child once, in
/// order.
#[tokio::test]
async fn paging_both_ways_reaches_each_end_once() {
    let (repo, _w) = seeded().await;
    let mut seen = Vec::new();
    let mut p = page(&repo, "t1", Ask::Around(Anchor::Start), 2).await;
    loop {
        seen.extend(uuids(&p).into_iter().map(String::from));
        let Some(next) = p.after.clone() else { break };
        p = page(&repo, "t1", Ask::After(next), 2).await;
    }
    assert_eq!(seen, ["m1", "m2", "m3", "m4", "m5"]);

    let now = Ask::Around(Anchor::Moment("2400-01-01T00:00:00.000000Z".to_string()));
    let mut p = page(&repo, "t1", now, 2).await;
    let mut seen: Vec<String> = uuids(&p).into_iter().map(String::from).collect();
    while let Some(next) = p.before.clone() {
        p = page(&repo, "t1", Ask::Before(next), 2).await;
        let mut earlier: Vec<String> = uuids(&p).into_iter().map(String::from).collect();
        earlier.append(&mut seen);
        seen = earlier;
    }
    assert_eq!(seen, ["m1", "m2", "m3", "m4", "m5"]);
}

/// A channel carried by every thread's document is one child of its
/// workspace; a thread that is a container with a row is listed once, as
/// its container's kind with its row's author.
#[tokio::test]
async fn a_container_carried_many_times_or_with_a_row_is_one_child() {
    let (repo, _w) = seeded().await;
    let p = page(&repo, "ws", Ask::Around(Anchor::Start), 10).await;
    assert_eq!(uuids(&p), ["bridge", "ops"]);
    assert!(p
        .children
        .iter()
        .all(|c| c.container && c.kind == "Slack channel"));
    assert_eq!(p.children[0].name.as_deref(), Some("#bridge"));
    assert_eq!(p.children[0].markdown_uuid, None, "a channel has no row");

    let p = page(&repo, "bridge", Ask::Around(Anchor::Start), 10).await;
    assert_eq!(uuids(&p), ["t1", "t2", "t3"]);
    let t1: &Child = &p.children[0];
    assert!(t1.container);
    assert_eq!(t1.kind, "Slack thread");
    assert_eq!(t1.author.as_deref(), Some("Geordi La Forge"));
    assert_eq!(t1.markdown_uuid.as_deref(), Some("t1"));
    assert_eq!(t1.sort_key, key_at(1));
}

/// What is filed under a label is its children, in its rows' order.
#[tokio::test]
async fn what_is_filed_under_a_label_is_its_children() {
    let (repo, _w) = seeded().await;
    let p = page(&repo, "lbl", Ask::Around(Anchor::Start), 10).await;
    assert_eq!(uuids(&p), ["t2", "t3", "m5"]);
    assert_eq!(p.children[2].sort_key, key_at(14));
    assert!(!p.children[2].container);
    let p = page(&repo, "lbl", item("t3"), 1).await;
    assert_eq!(uuids(&p), ["t2", "t3", "m5"]);
}

/// A message that arrives after a page was read, older than the page's
/// last row, lands in its place: the next page past the cursor neither
/// repeats nor skips a row, and a window around it shows it between its
/// neighbours.
#[tokio::test]
async fn a_late_message_lands_where_it_belongs() {
    let (repo, w) = seeded().await;
    let first = page(&repo, "t1", Ask::Around(Anchor::Start), 2).await;
    assert_eq!(uuids(&first), ["m1", "m2"]);
    let cursor = first.after.clone().expect("more after m2");

    // One older than the cursor, one between the cursor and the next row.
    let mut early = row("m1b", "t1", "t1", 10, false);
    early.created_at = Some("2364-02-11T09:10:30+00:00".to_string());
    early.sort_key = "2364-02-11T09:10:30.000000Z".to_string();
    let mut between = row("m2b", "t1", "t1", 11, false);
    between.created_at = Some("2364-02-11T09:11:30+00:00".to_string());
    between.sort_key = "2364-02-11T09:11:30.000000Z".to_string();
    insert_rows(&w, &[early, between]).await;
    commit(&w, "late messages").await;

    let next = page(&repo, "t1", Ask::After(cursor), 2).await;
    assert_eq!(uuids(&next), ["m2b", "m3"]);
    let around = page(&repo, "t1", item("m1b"), 1).await;
    assert_eq!(uuids(&around), ["m1", "m1b", "m2"]);
}

/// An anchor that is not among the container's children is refused by
/// name, even when it is a child of something else.
#[tokio::test]
async fn an_anchor_elsewhere_in_the_tree_is_refused() {
    let (repo, _w) = seeded().await;
    match repo.children("bridge", &item("m3"), 5).await {
        Err(ChildrenFailure::Refused(ChildrenError::NotAChild { parent, child })) => {
            assert_eq!((parent.as_str(), child.as_str()), ("bridge", "m3"));
        }
        other => panic!("expected m3 refused as not a child of bridge, got {other:?}"),
    }
}

async fn plan(w: &sqlx::SqlitePool, sql: &str, binds: usize) -> Vec<String> {
    let mut query = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
    for _ in 0..binds {
        query = query.bind("x");
    }
    query
        .fetch_all(w)
        .await
        .expect("explain")
        .iter()
        .map(|r| sqlx::Row::get::<String, _>(r, "detail"))
        .collect()
}

/// A page of rows or containers is one range of its `(parent, sort_key,
/// uuid)` index read in order, either way, with no sort; what is filed is
/// found by its label's index and sorted after (its key is on another
/// table). And every index made for children is one some read plans with.
#[tokio::test]
async fn every_children_read_is_served_by_its_index() {
    let (_repo, w) = seeded().await;
    let mut used = std::collections::BTreeSet::new();
    let mut unserved = Vec::new();
    for via in Via::ALL {
        for side in [Side::Before, Side::After] {
            for at_is_after in [true, false] {
                let sql = children_sql(via, side, at_is_after);
                let plan = plan(&w, &sql, 4).await;
                let wanted = match via {
                    Via::Row => "SEARCH grid_rows USING COVERING INDEX grid_rows_by_parent ",
                    Via::Node => "SEARCH n USING COVERING INDEX data_nodes_by_parent ",
                    Via::Filed => "SEARCH f USING COVERING INDEX filings_by_under ",
                };
                let searched = plan.iter().any(|d| d.starts_with(wanted));
                let sorts = plan.iter().any(|d| d.contains("TEMP B-TREE"));
                if !searched || (sorts && via != Via::Filed) {
                    unserved.push(format!("{via:?} {side:?} {at_is_after}: {plan:?}"));
                }
                used.extend(index_names(&plan));
            }
        }
    }
    for (sql, binds) in [(CHILD_KEY_SQL, 2), (NODE_FACTS_SQL, 1)] {
        used.extend(index_names(&plan(&w, sql, binds).await));
    }
    assert!(unserved.is_empty(), "{}", unserved.join("\n"));
    let unused: Vec<&str> = CHILDREN_INDEXES
        .iter()
        .filter_map(|ddl| ddl.split_whitespace().nth(5))
        .filter(|name| !used.contains(*name))
        .collect();
    assert!(unused.is_empty(), "no read plans with these: {unused:?}");
}

fn index_names(plan: &[String]) -> Vec<String> {
    plan.iter()
        .filter_map(|d| d.split("INDEX ").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(String::from)
        .collect()
}
