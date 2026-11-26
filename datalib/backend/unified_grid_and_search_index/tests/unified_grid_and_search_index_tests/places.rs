//! A library's tree read from the grid index a level at a time: a source's
//! top, a workspace's channels, an account's labels.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datalib_etl::stop::StopFlag;
use datalib_etl_render::grid_index::{build_grid_index_for, close_index, RenderedMarkdown};
use datalib_etl_render::indexed_markdown::IndexedMarkdownStore;
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::filings::FilingRow;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::nodes::TREE_INDEXES;
use datalib_schema::providers::Provider;
use datalib_unified_grid_and_search_index::at_node::FACTS_SQL;
use datalib_unified_grid_and_search_index::dolt_repo::DoltRepo;
use datalib_unified_grid_and_search_index::places::{
    level_sql, Key, Parent, Place, PlacesPage, SOURCES_WITH_PLACES_SQL,
};
use datalib_unified_grid_and_search_index::repo::IndexRepo;

use crate::dolt_backend_integration::{unique_db_path, writer};
use datalib_sql::{test_sql_id, Sql};

pub(crate) const SOURCE: &str = "slack";

pub(crate) fn stamp(minute: u32) -> String {
    format!("2364-02-11T09:{minute:02}:00+00:00")
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

async fn page(repo: &DoltRepo, parent: &str, after: &Key, limit: usize) -> PlacesPage {
    repo.places(&Parent::parse(parent), after, limit)
        .await
        .unwrap_or_else(|e| panic!("places in {parent}: {e}"))
}

/// The whole of a level, read `limit` places at a time.
async fn level(repo: &DoltRepo, parent: &str, limit: usize) -> Vec<Place> {
    let (mut listed, mut after) = (Vec::new(), Key::default());
    loop {
        let p = page(repo, parent, &after, limit).await;
        assert!(p.places.len() <= limit, "{p:?}");
        listed.extend(p.places);
        let Some(next) = p.after else { break listed };
        after = next;
    }
}

async fn uuids(repo: &DoltRepo, parent: &str) -> Vec<String> {
    let one_by_one = level(repo, parent, 1).await;
    assert_eq!(one_by_one, level(repo, parent, 10).await, "{parent}");
    one_by_one.into_iter().map(|p| p.uuid).collect()
}

/// A place that arrives after a page was read, before the page's last
/// place, is on no later page, and one after it is on the next: the cursor
/// is a key, never a count.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_place_moves_no_page() {
    let library = seeded().await;
    let repo = &library.repo;
    let first = page(repo, "ws", &Key::default(), 1).await;
    assert_eq!(first.places[0].uuid, "bridge");
    assert!(first.at.is_some(), "a page names the commit it was read at");
    let after = first.after.expect("more after bridge");

    library
        .sync(
            vec![
                thread_in("ws", "academy", "t6", 6),
                thread_in("ws", "medical", "t7", 7),
            ],
            vec![],
        )
        .await;
    let rest = page(repo, "ws", &after, 10).await;
    let rest: Vec<&str> = rest.places.iter().map(|p| p.uuid.as_str()).collect();
    assert_eq!(rest, ["medical", "ops"]);
    assert_eq!(
        uuids(repo, "ws").await,
        ["academy", "bridge", "medical", "ops"]
    );
}

/// Documents that place a channel differently leave it one child, where
/// its newest says, wherever it is listed from; when that document goes,
/// where the next newest says. A listing of places once held it in both
/// its places, and on two pages of one.
#[tokio::test(flavor = "multi_thread")]
async fn a_container_its_documents_place_differently_is_in_one_place() {
    let library = seeded().await;
    let repo = &library.repo;
    assert_eq!(uuids(repo, "ws").await, ["bridge", "ops"]);

    let mut t4 = thread("t4", 4);
    t4.data_nodes[1] = DataNodeRow::new("t4", "bridge", SOURCE, "Slack channel")
        .named(Some("#main-bridge".to_string()))
        .place()
        .sort_key("9".to_string())
        .under(&Lineage::top().below(&t4.data_nodes[0]));
    library.sync(vec![t4], vec![]).await;
    assert_eq!(
        uuids(repo, "ws").await,
        ["ops", "bridge"],
        "once, at its newest document's key"
    );
    let p = level(repo, "ws", 10).await;
    assert_eq!(p[1].name.as_deref(), Some("#main-bridge"));

    let mut t5 = thread("t5", 5);
    t5.data_nodes[1] = DataNodeRow::new("t5", "bridge", SOURCE, "Slack channel").place();
    library.sync(vec![t5], vec![]).await;
    assert_eq!(uuids(repo, "ws").await, ["ops"], "moved out of ws");
    assert_eq!(uuids(repo, "source:slack").await, ["bridge", "ws"]);

    library.sync(vec![], vec!["t5", "t4"]).await;
    assert_eq!(uuids(repo, "ws").await, ["bridge", "ops"]);
    assert_eq!(uuids(repo, "source:slack").await, ["ws"]);
}

/// A level holds places alone, each once though every document carries it:
/// not the threads and messages that live there, nor what is filed there.
/// Each says what it is and whether places lie below it, and one that is a
/// document names it. A label whose account no document carries is under
/// that account, not at its source's top, and another source's top holds
/// none of these.
#[tokio::test(flavor = "multi_thread")]
async fn a_level_holds_only_places() {
    let library = seeded().await;
    let repo = &library.repo;
    let top = level(repo, "source:slack", 10).await;
    assert_eq!(
        top,
        [Place {
            uuid: "ws".into(),
            sort_key: String::new(),
            kind: "Slack workspace".into(),
            name: Some("#ws".into()),
            preview: Some(String::new()),
            markdown_uuid: None,
            places_below: true,
        }]
    );
    assert_eq!(
        uuids(repo, "ws").await,
        ["bridge", "ops"],
        "t2 is filed there but no place"
    );
    let channels = level(repo, "ws", 10).await;
    assert!(
        channels.iter().all(|c| !c.places_below),
        "threads are not places: {channels:?}"
    );
    assert_eq!(uuids(repo, "bridge").await, Vec::<String>::new());
    assert_eq!(uuids(repo, "acct").await, ["lbl"]);
    assert_eq!(uuids(repo, "source:email").await, Vec::<String>::new());
    assert_eq!(
        repo.sources_with_places().await.expect("read the top"),
        ["slack"]
    );

    let mut wiki = thread("wiki", 8);
    wiki.data_nodes[2].is_place = true;
    library.sync(vec![wiki], vec![]).await;
    let page = level(repo, "bridge", 10).await;
    assert_eq!(page.len(), 1, "{page:?}");
    assert_eq!(page[0].markdown_uuid.as_deref(), Some("wiki"));
    assert_eq!(page[0].preview.as_deref(), Some("about wiki"));
}

pub(crate) async fn plan(w: &sqlx::SqlitePool, sql: &str, binds: usize) -> Vec<String> {
    let mut query = Sql::dynamic(test_sql_id!(), format!("EXPLAIN QUERY PLAN {sql}")).query();
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

/// A level, of a place or at a source's top, is one range of `nodes_places`
/// read in order with no sort, and so is each place's question of whether
/// more lie below it; nothing reads a table whole. And every index made
/// for the tree is one some read of it plans with.
#[tokio::test(flavor = "multi_thread")]
async fn every_read_of_the_tree_is_served_by_its_index() {
    let library = seeded().await;
    let w = writer(&library.root).await;
    let level = "SEARCH n USING INDEX nodes_places (parent_uuid=";
    let below = "SEARCH b USING COVERING INDEX nodes_places (parent_uuid=";
    let top = "SEARCH nodes USING COVERING INDEX nodes_places (parent_uuid=";
    let mut used = std::collections::BTreeSet::new();
    let mut unserved = Vec::new();
    for (sql, binds, wanted) in [
        (level_sql(&Parent::parse("ws")), 4, vec![level, below]),
        (
            level_sql(&Parent::parse("source:slack")),
            4,
            vec![level, below],
        ),
        (SOURCES_WITH_PLACES_SQL.to_string(), 0, vec![top]),
        (FACTS_SQL.to_string(), 1, vec![]),
    ] {
        let plan = plan(&w, &sql, binds).await;
        let searched = wanted.iter().all(|w| plan.iter().any(|d| d.starts_with(w)));
        let sorts = plan.iter().any(|d| d.contains("FOR ORDER BY"));
        if !searched || sorts || reads_a_whole_table(&plan) {
            unserved.push(format!("{sql}: {plan:?}"));
        }
        used.extend(index_names(&plan));
    }
    assert!(unserved.is_empty(), "{}", unserved.join("\n"));
    let unused: Vec<&str> = TREE_INDEXES
        .iter()
        .filter_map(|ddl| ddl.split_whitespace().nth(5))
        .filter(|name| !used.contains(*name))
        .collect();
    assert!(unused.is_empty(), "no read plans with these: {unused:?}");
}

/// `SCAN CONSTANT ROW` is a `SELECT` of no table.
fn reads_a_whole_table(plan: &[String]) -> bool {
    plan.iter()
        .any(|d| d.starts_with("SCAN") && d != "SCAN CONSTANT ROW")
}

fn index_names(plan: &[String]) -> Vec<String> {
    plan.iter()
        .filter_map(|d| d.split("INDEX ").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(String::from)
        .collect()
}
