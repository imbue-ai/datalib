//! `at:<node>` over an index a sync wrote: a workspace, its channels and
//! their threads, a label with threads filed under it, and a page that is a
//! document and holds another. And each kind of node is read off the index
//! made for it.

use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::filings::FilingRow;
use datalib_schema::nodes::PLACE_INDEX;
use datalib_unified_grid_and_search_index::at_node::{Arms, FACTS_SQL};
use datalib_unified_grid_and_search_index::dolt_repo::{listing_sql, DoltRepo};
use datalib_unified_grid_and_search_index::query::parse_query;
use datalib_unified_grid_and_search_index::repo::IndexRepo;

use crate::children::{plan, row, stamp, sync, thread_in, SOURCE};
use crate::dolt_backend_integration::{unique_db_path, writer};

const WS: &str = "00000000-0000-8000-a000-000000000001";
const BRIDGE: &str = "00000000-0000-8000-a000-000000000002";
const OPS: &str = "00000000-0000-8000-a000-000000000003";
const ACCT: &str = "00000000-0000-8000-a000-000000000004";
const LBL: &str = "00000000-0000-8000-a000-000000000005";
const T1: &str = "00000000-0000-8000-a000-000000000011";
const T2: &str = "00000000-0000-8000-a000-000000000012";
const T3: &str = "00000000-0000-8000-a000-000000000013";
const M1: &str = "00000000-0000-8000-a000-000000000021";
const KEYWORD: &str = "00000000-0000-8000-a000-000000000006";
const PHOTO: &str = "00000000-0000-8000-a000-000000000014";
const PAGE: &str = "00000000-0000-8000-a000-000000000031";
const SUB: &str = "00000000-0000-8000-a000-000000000032";
const SUBSUB: &str = "00000000-0000-8000-a000-000000000033";

/// `t1` (with a message) and `t2` in the bridge, `t3` in ops; `t2` and `t3`
/// filed under a label of an account; a page holding a page holding a page; and a photo, in
/// no place and no container itself, filed under a keyword.
fn documents() -> Vec<RenderedMarkdown> {
    let mut t1 = thread_in(WS, BRIDGE, T1, 1);
    let mut m1 = row(M1, T1, T1, &stamp(20));
    m1.place_uuid = Some(BRIDGE.to_string());
    t1.rows.push(m1);
    let mut t2 = thread_in(WS, BRIDGE, T2, 2);
    let mut t3 = thread_in(WS, OPS, T3, 3);
    for t in [&mut t2, &mut t3] {
        let md = t.markdown_uuid.clone();
        let account = DataNodeRow::new(&md, ACCT, SOURCE, "Account").place();
        let in_account = Lineage::top().below(&account);
        t.data_nodes.push(account);
        t.data_nodes.push(
            DataNodeRow::new(&md, LBL, SOURCE, "Label")
                .place()
                .under(&in_account),
        );
        t.filings.push(FilingRow::new(&md, &md, LBL));
    }
    let mut photo = page_under(PHOTO, None, 6);
    photo.data_nodes = vec![DataNodeRow::new(PHOTO, KEYWORD, SOURCE, "Keyword").place()];
    photo.filings.push(FilingRow::new(PHOTO, PHOTO, KEYWORD));
    vec![
        t1,
        t2,
        t3,
        page_under(PAGE, None, 4),
        page_under(SUB, Some(PAGE), 5),
        page_under(SUBSUB, Some(SUB), 7),
        photo,
    ]
}

/// A page is a document, read together and a place.
fn page_under(uuid: &str, parent: Option<&str>, minute: u32) -> RenderedMarkdown {
    let mut doc = thread_in(WS, BRIDGE, uuid, minute);
    let mut page = DataNodeRow::new(uuid, uuid, SOURCE, "Page")
        .place()
        .read_together();
    page.parent_uuid = parent.map(String::from);
    page.place_uuid = parent.map(String::from);
    doc.rows[0].parent_uuid = parent.map(String::from);
    doc.rows[0].place_uuid = parent.map(String::from);
    doc.rows[0].read_uuid = None;
    doc.data_nodes = vec![page];
    doc
}

async fn library() -> DoltRepo {
    let root = std::sync::Arc::new(unique_db_path().parent().unwrap().to_path_buf());
    sync(&root, documents(), vec![]).await;
    DoltRepo::open(root).await.expect("open the repo")
}

/// What `q` finds, in uuid order.
async fn found(repo: &DoltRepo, q: &str) -> Vec<String> {
    let parsed = parse_query(q);
    assert_eq!(parsed.refusal(), None, "{q}");
    let mut uuids = repo
        .ordered_uuids(&parsed, &[], &[])
        .await
        .expect("search")
        .uuids;
    uuids.sort();
    uuids
}

/// What is directly under a node, and the document it is when it is one: a
/// channel's threads and their messages, a label's filed threads, a page
/// and the page in it, a thread with its messages. A workspace holds its
/// channels, and an account its labels, so neither's documents are at it.
#[tokio::test(flavor = "multi_thread")]
async fn at_a_node_is_what_lives_there_what_is_filed_there_and_its_document() {
    let repo = library().await;
    for (node, want) in [
        (WS, vec![]),
        (BRIDGE, vec![T1, T2]),
        (OPS, vec![T3]),
        (LBL, vec![T2, T3]),
        (ACCT, vec![]),
        (KEYWORD, vec![PHOTO]),
        (PAGE, vec![PAGE, SUB]),
        (SUB, vec![SUB, SUBSUB]),
        (SUBSUB, vec![SUBSUB]),
        (T1, vec![T1]),
    ] {
        let q = format!("at:{node} is:document");
        assert_eq!(found(&repo, &q).await, want, "{q}");
    }
    assert_eq!(found(&repo, &format!("at:{T1}")).await, [T1, M1]);
    assert_eq!(
        found(&repo, &format!("at:{BRIDGE}")).await,
        [T1, T2, M1],
        "a message's nearest place is its channel"
    );
    assert_eq!(found(&repo, &format!("at:{WS}")).await, [BRIDGE, OPS]);
    assert_eq!(
        found(&repo, &format!("at:{BRIDGE} at:{LBL} is:document")).await,
        [T2],
        "two nodes keep what is at both"
    );
    assert_eq!(
        found(&repo, &format!("-at:{BRIDGE} is:document")).await,
        [T3, PHOTO, PAGE, SUB, SUBSUB],
        "a page in no place is not at the bridge"
    );
    assert_eq!(
        found(&repo, &format!("at:datalib:node/{OPS} is:document")).await,
        [T3],
        "the link a node chip copies names it too"
    );
    assert_eq!(
        found(&repo, "at:00000000-0000-8000-a000-0000000000ff").await,
        Vec::<String>::new()
    );
    let why = parse_query("at:bridge").refusal().expect("no id");
    assert!(why.contains("`at:`"), "{why}");
}

/// A search with words ranks within what `at:` keeps, which it reads as
/// rows and documents.
#[tokio::test(flavor = "multi_thread")]
async fn a_ranked_search_is_held_to_what_is_at_the_node() {
    let repo = library().await;
    let q = parse_query(&format!("at:{LBL}"));
    let mut documents: Vec<String> = repo
        .matching_documents(&q)
        .await
        .expect("read the documents")
        .into_iter()
        .collect();
    documents.sort();
    assert_eq!(documents, [T2, T3]);
    let mut rows = repo.matching_rows(&q).await.expect("read the rows");
    rows.sort();
    let own = |t: &str| (t.to_string(), t.to_string(), true);
    assert_eq!(rows, [own(T2), own(T3)]);
}

/// Listing a node that has one way to hold rows reads one index and no row
/// of the table: fetching each row of a busy channel took 1.9 s. Learning
/// which ways it has reads no table whole.
#[tokio::test(flavor = "multi_thread")]
async fn at_is_read_off_its_indexes() {
    let root = std::sync::Arc::new(unique_db_path().parent().unwrap().to_path_buf());
    sync(&root, documents(), vec![]).await;
    let w = writer(&root).await;
    let place_index = PLACE_INDEX.split_whitespace().nth(5).expect("its name");
    let none = Arms {
        held: false,
        filed: false,
        own: false,
    };
    for (arms, index) in [
        (Arms { held: true, ..none }, place_index),
        (
            Arms {
                filed: true,
                ..none
            },
            "grid_rows_by_conversation",
        ),
        (Arms { own: true, ..none }, "grid_rows_by_conversation"),
    ] {
        let q = parse_query(&format!("at:{BRIDGE} is:document")).with_arms(
            &std::collections::HashMap::from([(BRIDGE.to_string(), arms)]),
        );
        let (sql, params) = listing_sql(&q, &[], &[]);
        let plan = plan(&w, &sql, params.len()).await;
        let covered = format!("SEARCH grid_rows USING COVERING INDEX {index} ");
        assert!(
            plan.iter().any(|d| d.starts_with(&covered)),
            "{arms:?}: {plan:?}"
        );
        assert!(
            plan.iter().all(|d| !d.starts_with("SCAN")),
            "{arms:?}: {plan:?}"
        );
        assert!(
            !arms.held || plan.iter().all(|d| d != "USE TEMP B-TREE FOR ORDER BY"),
            "a place's rows come newest first off its index: {plan:?}"
        );
    }
    let facts = plan(&w, FACTS_SQL, 1).await;
    assert_eq!(
        facts.iter().filter(|d| d.starts_with("SEARCH ")).count(),
        3,
        "{facts:?}"
    );
    assert!(
        facts
            .iter()
            .all(|d| !d.contains("SCAN grid_rows") && !d.contains("SCAN filings")),
        "{facts:?}"
    );
}
