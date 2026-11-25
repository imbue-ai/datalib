//! A container's children read from the grid index around an anchor and
//! paged both ways: a workspace's channels, a channel's threads, a
//! thread's messages and a label's filed threads.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datalib_etl::stop::StopFlag;
use datalib_etl_render::grid_index::{build_grid_index_for, close_index, RenderedMarkdown};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::filings::FilingRow;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::nodes::CHILDREN_INDEXES;
use datalib_schema::providers::Provider;
use datalib_unified_grid_and_search_index::children::{
    child_key_sql, children_sql, Anchor, Ask, Child, ChildrenError, ChildrenFailure, ChildrenPage,
    Key, Scope, Side, Via, NODE_FACTS_SQL, PLACES_BELOW_SQL, ROW_FACTS_SQL,
    SOURCES_WITH_PLACES_SQL,
};
use datalib_unified_grid_and_search_index::dolt_repo::DoltRepo;
use datalib_unified_grid_and_search_index::repo::IndexRepo;

use crate::dolt_backend_integration::{unique_db_path, writer};

pub(crate) const SOURCE: &str = "slack";

pub(crate) fn stamp(minute: u32) -> String {
    format!("2364-02-11T09:{minute:02}:00+00:00")
}

/// `stamp(minute)` as a sort key spells it.
fn key_at(minute: u32) -> String {
    format!("2364-02-11T09:{minute:02}:00.000000Z")
}

pub(crate) fn row(uuid: &str, parent: &str, markdown: &str, created_at: &str) -> GridRow {
    let document = uuid == markdown;
    GridRow::builder()
        .uuid(uuid)
        .provider(Provider::Slack)
        .kind(if document { "Slack Thread" } else { "Message" })
        .source_label("Slack")
        .is_document(document)
        .item_count(document.then_some(1))
        .created_at(Some(created_at.to_string()))
        .author(Some("Geordi La Forge".to_string()))
        .conversation_uuid(markdown)
        .entire_chat(format!("/chat/{markdown}"))
        .body(format!("about {uuid}"))
        .markdown_uuid(Some(markdown.to_string()))
        .parent_uuid(Some(parent.to_string()))
        .build()
        .expect("a valid row")
}

fn thread(t: &str, minute: u32) -> RenderedMarkdown {
    thread_in("ws", "bridge", t, minute)
}

/// Thread `t` at `minute`, carrying the channel it is in and the workspace
/// above, as Slack's renderer does.
pub(crate) fn thread_in(ws: &str, channel: &str, t: &str, minute: u32) -> RenderedMarkdown {
    let workspace = DataNodeRow::new(t, ws, SOURCE, "Slack workspace")
        .named(Some(format!("#{ws}")))
        .place();
    let in_workspace = Lineage::top().below(&workspace);
    let channel = DataNodeRow::new(t, channel, SOURCE, "Slack channel")
        .named(Some(format!("#{channel}")))
        .place()
        .under(&in_workspace);
    let in_channel = in_workspace.below(&channel);
    let thread = DataNodeRow::new(t, t, SOURCE, "Slack thread")
        .named(Some(format!("#{t}")))
        .read_together()
        .under(&in_channel);
    let mut own = row(t, &channel.uuid, t, &stamp(minute));
    own.place_uuid = in_channel.place_uuid.clone();
    RenderedMarkdown {
        searchable: true,
        content_hash: None,
        markdown_uuid: t.to_string(),
        source_id: SOURCE.to_string(),
        upstream_cursor: None,
        render_unit_key: None,
        render_version: 1,
        rows: vec![own],
        sections: Vec::new(),
        front: Default::default(),
        edges: Vec::new(),
        contacts: Vec::new(),
        data_nodes: vec![workspace, channel, thread],
        filings: Vec::new(),
        photos: Vec::new(),
        problems: Vec::new(),
    }
}

/// A workspace `ws` with two channels, `bridge` and `ops`, each a place no
/// document holds a row for; `bridge` holds threads `t1`..`t3`, each a
/// document, and every thread's document carries the channel and workspace
/// again. `t1` holds messages `m1`..`m5` a minute apart from minute 10. A
/// label `lbl`, in an account no document carries, has `t2` and `t3` filed
/// under it; `t2` is filed under the workspace too.
fn documents() -> Vec<RenderedMarkdown> {
    let (mut t1, mut t2, mut t3) = (thread("t1", 1), thread("t2", 2), thread("t3", 3));
    for i in 1..=5 {
        let m = format!("m{i}");
        t1.rows.push(row(&m, "t1", "t1", &stamp(9 + i)));
    }
    t1.data_nodes.push(
        DataNodeRow::new("t1", "ops", SOURCE, "Slack channel")
            .named(Some("#ops".to_string()))
            .place()
            .under(&Lineage::top().below(&t1.data_nodes[0])),
    );
    let account = DataNodeRow::new("t2", "acct", SOURCE, "Account").place();
    t2.data_nodes.push(
        DataNodeRow::new("t2", "lbl", SOURCE, "Label")
            .named(Some("#lbl".to_string()))
            .place()
            .under(&Lineage::top().below(&account)),
    );
    for under in ["lbl", "ws"] {
        t2.filings.push(FilingRow::new("t2", "t2", under));
    }
    t3.filings.push(FilingRow::new("t3", "t3", "lbl"));
    vec![t1, t2, t3]
}

/// A library holding `documents()`, rendered and indexed as a sync does.
struct Library {
    root: Arc<PathBuf>,
    repo: DoltRepo,
}

async fn seeded() -> Library {
    let root = Arc::new(unique_db_path().parent().unwrap().to_path_buf());
    sync(&root, documents(), vec![]).await;
    let repo = DoltRepo::open(root.clone()).await.expect("open the repo");
    Library { root, repo }
}

impl Library {
    async fn sync(&self, docs: Vec<RenderedMarkdown>, gone: Vec<&str>) {
        sync(&self.root, docs, gone).await;
    }
}

/// Render `docs`, drop the documents `gone`, and index what moved.
pub(crate) async fn sync(root: &Path, docs: Vec<RenderedMarkdown>, gone: Vec<&str>) {
    let rendered = datalib_etl::layout::render_markdown_root(root, SOURCE);
    let gone: Vec<String> = gone.into_iter().map(String::from).collect();
    tokio::task::spawn_blocking(move || {
        let store = IndexedMarkdownStore::open(&rendered).expect("open the render store");
        for d in &docs {
            store.put_document(d).expect("render a document");
        }
        store.remove_documents(&gone).expect("drop documents");
        store.commit("test render").expect("commit");
        store.close();
    })
    .await
    .expect("render");
    let pool = writer(root).await;
    build_grid_index_for(
        &pool,
        root,
        &[SOURCE.to_string()],
        |_| {},
        None,
        &StopFlag::new(),
    )
    .await
    .expect("index");
    close_index(pool).await.expect("close the index");
}

async fn page(repo: &DoltRepo, parent: &str, ask: Ask, limit: usize) -> ChildrenPage {
    scoped(repo, &Scope::all(parent), ask, limit).await
}

async fn scoped(repo: &DoltRepo, scope: &Scope, ask: Ask, limit: usize) -> ChildrenPage {
    match repo.children(scope, &ask, limit).await {
        Ok(page) => page,
        Err(e) => panic!("children of {scope:?}: {e:?}"),
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
#[tokio::test(flavor = "multi_thread")]
async fn the_window_around_an_item_holds_it_and_its_neighbours() {
    let Library { repo, .. } = seeded().await;
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
#[tokio::test(flavor = "multi_thread")]
async fn the_window_around_a_moment_and_around_now() {
    let Library { repo, .. } = seeded().await;
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
#[tokio::test(flavor = "multi_thread")]
async fn paging_both_ways_reaches_each_end_once() {
    let Library { repo, .. } = seeded().await;
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
#[tokio::test(flavor = "multi_thread")]
async fn a_container_carried_many_times_or_with_a_row_is_one_child() {
    let Library { repo, .. } = seeded().await;
    let p = page(&repo, "ws", Ask::Around(Anchor::Start), 10).await;
    assert_eq!(uuids(&p), ["bridge", "ops", "t2"]);
    assert!(p.children[..2]
        .iter()
        .all(|c| c.container && c.kind == "Slack channel"));
    assert_eq!(p.children[0].name.as_deref(), Some("#bridge"));
    assert_eq!(
        p.children[0].markdown_uuid, None,
        "no document holds a channel"
    );

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
#[tokio::test(flavor = "multi_thread")]
async fn what_is_filed_under_a_label_is_its_children() {
    let Library { repo, .. } = seeded().await;
    let p = page(&repo, "lbl", Ask::Around(Anchor::Start), 10).await;
    assert_eq!(uuids(&p), ["t2", "t3"]);
    assert_eq!(p.children[1].sort_key, key_at(3));
    let p = page(&repo, "lbl", item("t3"), 1).await;
    assert_eq!(uuids(&p), ["t2", "t3"]);
}

/// A message that arrives after a page was read, older than the page's
/// last row, lands in its place: the next page past the cursor neither
/// repeats nor skips a row, and a window around it shows it between its
/// neighbours.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_message_lands_where_it_belongs() {
    let library = seeded().await;
    let repo = &library.repo;
    let first = page(repo, "t1", Ask::Around(Anchor::Start), 2).await;
    assert_eq!(uuids(&first), ["m1", "m2"]);
    let cursor = first.after.clone().expect("more after m2");

    // One older than the cursor, one between the cursor and the next row.
    let mut t1 = documents().remove(0);
    for (m, at) in [
        ("m1b", "2364-02-11T09:10:30+00:00"),
        ("m2b", "2364-02-11T09:11:30+00:00"),
    ] {
        t1.rows.push(row(m, "t1", "t1", at));
    }
    library.sync(vec![t1], vec![]).await;

    let next = page(repo, "t1", Ask::After(cursor), 2).await;
    assert_eq!(uuids(&next), ["m2b", "m3"]);
    let around = page(repo, "t1", item("m1b"), 1).await;
    assert_eq!(uuids(&around), ["m1", "m1b", "m2"]);
}

/// Documents that place a channel differently leave it one child, where
/// its newest says, wherever it is listed from; when that document goes,
/// where the next newest says. A listing of places once held it in both
/// its places, and on two pages of one.
#[tokio::test(flavor = "multi_thread")]
async fn a_container_its_documents_place_differently_is_in_one_place() {
    let library = seeded().await;
    let repo = &library.repo;
    let places = |parent: &'static str| {
        let scope = Scope::places(parent);
        async move {
            let mut listed: Vec<String> = Vec::new();
            let mut ask = Ask::Around(Anchor::Start);
            loop {
                let p = scoped(repo, &scope, ask, 1).await;
                listed.extend(p.children.iter().map(|c| c.uuid.clone()));
                let Some(after) = p.after else { break listed };
                ask = Ask::After(after);
            }
        }
    };
    assert_eq!(places("ws").await, ["bridge", "ops"]);

    let mut t4 = thread("t4", 4);
    t4.data_nodes[1] = DataNodeRow::new("t4", "bridge", SOURCE, "Slack channel")
        .named(Some("#main-bridge".to_string()))
        .place()
        .sort_key("9".to_string())
        .under(&Lineage::top().below(&t4.data_nodes[0]));
    library.sync(vec![t4], vec![]).await;
    assert_eq!(
        places("ws").await,
        ["ops", "bridge"],
        "once, at its newest document's key"
    );
    let p = page(repo, "ws", item("bridge"), 1).await;
    assert_eq!(p.children[1].name.as_deref(), Some("#main-bridge"));

    let mut t5 = thread("t5", 5);
    t5.data_nodes[1] = DataNodeRow::new("t5", "bridge", SOURCE, "Slack channel").place();
    library.sync(vec![t5], vec![]).await;
    assert_eq!(places("ws").await, ["ops"], "moved out of ws");
    assert_eq!(places("source:slack").await, ["bridge", "ws"]);

    library.sync(vec![], vec!["t5", "t4"]).await;
    assert_eq!(places("ws").await, ["bridge", "ops"]);
    assert_eq!(places("source:slack").await, ["ws"]);
}

/// An anchor that is not among the container's children is refused by
/// name, even when it is a child of something else.
#[tokio::test(flavor = "multi_thread")]
async fn an_anchor_elsewhere_in_the_tree_is_refused() {
    let Library { repo, .. } = seeded().await;
    match repo.children(&Scope::all("bridge"), &item("m3"), 5).await {
        Err(ChildrenFailure::Refused(ChildrenError::NotAChild { parent, child })) => {
            assert_eq!((parent.as_str(), child.as_str()), ("bridge", "m3"));
        }
        other => panic!("expected m3 refused as not a child of bridge, got {other:?}"),
    }
}

/// What a source holds that names no parent is its top: the workspace,
/// once though every document carries it. A label whose account no
/// document carries is under that account, not at the top.
#[tokio::test(flavor = "multi_thread")]
async fn a_sources_top_is_what_names_no_parent() {
    let Library { repo, .. } = seeded().await;
    let p = scoped(
        &repo,
        &Scope::all("source:slack"),
        Ask::Around(Anchor::Start),
        10,
    )
    .await;
    assert_eq!(uuids(&p), ["ws"]);
    assert!(p.children[0].container);
    assert_eq!(
        p.children[0].places_below, None,
        "only a listing of places says"
    );
    let p = scoped(
        &repo,
        &Scope::all("source:email"),
        Ask::Around(Anchor::Start),
        10,
    )
    .await;
    assert_eq!(
        uuids(&p),
        Vec::<&str>::new(),
        "another source's top holds none of slack's"
    );
}

/// Listing places leaves out rows, the containers that are not places and
/// what is filed there, and says which have places below them.
#[tokio::test(flavor = "multi_thread")]
async fn a_listing_of_places_holds_only_places() {
    let Library { repo, .. } = seeded().await;
    let p = scoped(&repo, &Scope::places("ws"), Ask::Around(Anchor::Start), 10).await;
    assert_eq!(
        uuids(&p),
        ["bridge", "ops"],
        "t2 is filed there but no place"
    );
    assert!(
        p.children.iter().all(|c| c.places_below == Some(false)),
        "{:?}",
        p.children
    );

    let p = scoped(
        &repo,
        &Scope::places("source:slack"),
        Ask::Around(Anchor::Start),
        10,
    )
    .await;
    assert_eq!(uuids(&p), ["ws"]);
    assert_eq!(p.children[0].places_below, Some(true));

    let p = scoped(
        &repo,
        &Scope::places("bridge"),
        Ask::Around(Anchor::Start),
        10,
    )
    .await;
    assert_eq!(
        uuids(&p),
        Vec::<&str>::new(),
        "threads and messages are not places"
    );

    let p = scoped(&repo, &Scope::places("ws"), item("ops"), 1).await;
    assert_eq!(uuids(&p), ["bridge", "ops"], "a place anchors a window");

    assert_eq!(
        repo.sources_with_places().await.expect("read the top"),
        ["slack"]
    );
}

pub(crate) async fn plan(w: &sqlx::SqlitePool, sql: &str, binds: usize) -> Vec<String> {
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

/// A page of rows or places is one range of its `(parent, sort_key, uuid)`
/// index read in order, either way, with no sort, for a container and for a
/// source's top; what is filed is found by its label's index and sorted
/// after (its key is on another table). And every index made for children
/// is one some read plans with.
#[tokio::test(flavor = "multi_thread")]
async fn every_children_read_is_served_by_its_index() {
    let library = seeded().await;
    let w = writer(&library.root).await;
    let mut used = std::collections::BTreeSet::new();
    let mut unserved = Vec::new();
    let scopes = [
        Scope::all("ws"),
        Scope::all("source:slack"),
        Scope::places("ws"),
        Scope::places("source:slack"),
    ];
    for scope in &scopes {
        for via in scope.vias() {
            for side in [Side::Before, Side::After] {
                for at_is_after in [true, false] {
                    let sql = children_sql(scope, via, side, at_is_after).expect("a read");
                    let plan = plan(&w, &sql, 4).await;
                    let wanted = match (via, scope == &Scope::all("ws")) {
                        (Via::Row, true) => {
                            "SEARCH grid_rows USING COVERING INDEX grid_rows_by_parent "
                        }
                        // A source's top checks a column the index does not
                        // hold, so it reads the table beside it, in the
                        // index's order still.
                        (Via::Row, false) => "SEARCH grid_rows USING INDEX grid_rows_by_parent ",
                        (Via::Node, _) => "SEARCH nodes USING COVERING INDEX nodes_places ",
                        (Via::Filed, _) => "SEARCH f USING COVERING INDEX filings_by_under ",
                    };
                    let searched = plan.iter().any(|d| d.starts_with(wanted));
                    let sorts = plan.iter().any(|d| d.contains("TEMP B-TREE"));
                    if !searched || (sorts && via != Via::Filed) {
                        unserved.push(format!(
                            "{scope:?} {via:?} {side:?} {at_is_after}: {plan:?}"
                        ));
                    }
                    used.extend(index_names(&plan));
                }
            }
        }
        let sql = child_key_sql(scope);
        let plan = plan(&w, &sql, 2).await;
        if reads_a_whole_table(&plan) {
            unserved.push(format!("{sql}: {plan:?}"));
        }
        used.extend(index_names(&plan));
    }
    // A level asks each of these once, however many places are in it.
    for (sql, binds) in [
        (ROW_FACTS_SQL, 1),
        (NODE_FACTS_SQL, 1),
        (PLACES_BELOW_SQL, 1),
        (SOURCES_WITH_PLACES_SQL, 0),
    ] {
        let plan = plan(&w, sql, binds).await;
        if reads_a_whole_table(&plan) {
            unserved.push(format!("{sql}: {plan:?}"));
        }
        used.extend(index_names(&plan));
    }
    for sql in [PLACES_BELOW_SQL, SOURCES_WITH_PLACES_SQL] {
        let plan = plan(&w, sql, sql.matches('?').count()).await;
        assert!(
            plan.iter()
                .any(|d| d.contains("USING COVERING INDEX nodes_places (parent_uuid=")),
            "{sql}: {plan:?}"
        );
    }
    assert!(unserved.is_empty(), "{}", unserved.join("\n"));
    let unused: Vec<&str> = CHILDREN_INDEXES
        .iter()
        .filter_map(|ddl| ddl.split_whitespace().nth(5))
        .filter(|name| !used.contains(*name))
        .collect();
    assert!(unused.is_empty(), "no read plans with these: {unused:?}");
}

/// A bound array is read whole, and so are the rows of a subquery.
fn reads_a_whole_table(plan: &[String]) -> bool {
    plan.iter().any(|d| {
        d.starts_with("SCAN") && !d.starts_with("SCAN json_each") && !d.starts_with("SCAN (")
    })
}

fn index_names(plan: &[String]) -> Vec<String> {
    plan.iter()
        .filter_map(|d| d.split("INDEX ").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(String::from)
        .collect()
}
