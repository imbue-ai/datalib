//! Free-text searches through the applet's handlers, against the TNG
//! fixture's grid and qmd indexes and a real `qmd mcp`: the rank order and
//! paging, the structured terms narrowing qmd's hits, and what a search
//! says when qmd cannot answer.

use std::collections::HashSet;
use std::sync::OnceLock;

use super::tests::{index_over, search, uuids};
use super::*;

/// A word the fixture's corpus uses across several sources.
const QUERY: &str = "the enterprise";

/// qmd finds its node and package through `DATALIB_RUNTIME_DIR`, which
/// every test in this binary shares; the BUILD rule runs them one at a
/// time (`RUST_TEST_THREADS=1`), so setting it once, before any qmd runs,
/// races nothing.
fn stage_runtime_once() {
    static STAGED: OnceLock<()> = OnceLock::new();
    STAGED.get_or_init(|| {
        let work = tempfile::tempdir().unwrap().keep();
        let runtime = datalib_qmd_fixture::stage_runtime(&work);
        // SAFETY: see above; no other thread is running a test.
        unsafe { std::env::set_var("DATALIB_RUNTIME_DIR", &runtime) };
    });
}

/// The fixture's root: its grid index, its qmd index and qmd's model.
fn fixture_root() -> tempfile::TempDir {
    stage_runtime_once();
    let root = tempfile::tempdir().unwrap();
    datalib_qmd_fixture::materialize_root_with_grid(root.path());
    datalib_qmd_fixture::stage_models(root.path(), &root.path().join("_work"));
    root
}

fn scores(r: &SearchResponse) -> Vec<f64> {
    r.rows
        .iter()
        .map(|row| row.score.expect("every free-text row has qmd's score"))
        .collect()
}

fn qmd_error(r: &SearchResponse) -> Option<&str> {
    r.query_echo["qmd_error"].as_str()
}

/// A free-text search comes best first by qmd's score, and its pages are
/// consecutive slices of that one ranking; each row shows the words its
/// hit matched.
#[tokio::test]
async fn free_text_pages_follow_qmd_rank_and_show_the_matched_words() {
    let root = fixture_root();
    let s = index_over(root.path()).await;

    let whole = search(&s, QUERY, None, 1_000, None).await;
    assert_eq!(qmd_error(&whole), None);
    assert!(whole.errors.is_empty(), "{:?}", whole.errors);
    let ranked = scores(&whole);
    assert!(
        ranked.windows(2).all(|w| w[0] >= w[1]),
        "not best first: {ranked:?}"
    );
    assert!(
        ranked.first() > ranked.last(),
        "every score ties, so the order is untested: {ranked:?}"
    );

    let first = search(&s, QUERY, None, 5, None).await;
    assert_eq!(first.total, whole.total);
    assert_eq!(first.next_offset, Some(5));
    let second = search(&s, QUERY, first.next_offset, 5, None).await;
    assert_eq!(second.at, first.at);
    let paged: Vec<&str> = uuids(&first).into_iter().chain(uuids(&second)).collect();
    assert_eq!(paged, uuids(&whole)[..10]);
    assert!(first.rows.iter().all(|row| !row.snippet.is_empty()));
}

/// Structured terms narrow qmd's hits, over the whole ranking rather than
/// one page of it, and a sort reorders what is left.
#[tokio::test]
async fn structured_terms_narrow_the_ranking_and_a_sort_reorders_it() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let everything = search(&s, QUERY, None, 1_000, None).await;

    let narrowed = format!("{QUERY} source_id:slack");
    let slack = search(&s, &narrowed, None, 1_000, None).await;
    assert!(slack.total > 0, "the fixture's slack source matches");
    assert!(
        slack.total < everything.total,
        "the filter narrowed nothing"
    );
    assert!(slack.rows.iter().all(|row| row.source_id == "slack"));

    let oldest = search(&s, &narrowed, None, 1_000, Some("created_at:asc")).await;
    let newest = search(&s, &narrowed, None, 1_000, Some("created_at:desc")).await;
    let mut reversed = uuids(&newest);
    reversed.reverse();
    assert_eq!(uuids(&oldest), reversed);
    assert_ne!(uuids(&oldest), uuids(&slack), "the sort changed nothing");

    let worst_first = search(&s, &narrowed, None, 1_000, Some("score:asc")).await;
    let mut best_first = uuids(&slack);
    best_first.reverse();
    assert_eq!(uuids(&worst_first), best_first);
}

/// The embedding map lights up the documents behind the same hits.
#[tokio::test]
async fn the_map_matches_the_documents_the_grid_finds() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let params = map::MatchParams {
        q: Some(QUERY.to_string()),
    };
    let matched = map::matches_handler(State(s.clone()), Query(params))
        .await
        .0;
    assert!(matched.errors.is_empty(), "{:?}", matched.errors);
    assert!(!matched.markdown_uuids.is_empty());

    let grid = search(&s, QUERY, None, 1_000, None).await;
    let found: HashSet<&str> = grid
        .rows
        .iter()
        .filter_map(|row| row.markdown_uuid.as_deref())
        .collect();
    for doc in &matched.markdown_uuids {
        assert!(
            found.contains(doc.as_str()),
            "{doc} is lit on the map but not in the grid"
        );
    }
}

/// `qmd_vsearch:` asks qmd's vector search alone, and says so.
#[tokio::test]
async fn a_vector_only_search_ranks_by_meaning() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let r = search(&s, &format!("qmd_vsearch:\"{QUERY}\""), None, 10, None).await;
    assert_eq!(qmd_error(&r), None);
    assert_eq!(r.query_echo["free_text_mode"], "vsearch");
    assert!(!r.rows.is_empty());
    assert!(r.rows.iter().all(|row| row.score.is_some()));
}

/// Structured terms alone light up the map without asking qmd, which a
/// root without a qmd index shows.
#[tokio::test]
async fn the_map_matches_structured_terms_without_qmd() {
    let root = tempfile::tempdir().unwrap();
    datalib_qmd_fixture::copy_grid_index(root.path());
    let s = index_over(root.path()).await;
    let params = map::MatchParams {
        q: Some("source_id:slack".to_string()),
    };
    let matched = map::matches_handler(State(s), Query(params)).await.0;
    assert!(matched.errors.is_empty(), "{:?}", matched.errors);
    assert!(!matched.markdown_uuids.is_empty());
}

/// A root with no qmd index answers free text with the reason, on the grid
/// and on the map, not with an empty result that reads as "no matches".
#[tokio::test]
async fn free_text_without_a_qmd_index_says_why() {
    stage_runtime_once();
    let root = tempfile::tempdir().unwrap();
    datalib_qmd_fixture::copy_grid_index(root.path());
    let s = index_over(root.path()).await;

    let r = search(&s, QUERY, None, 10, None).await;
    assert!(r.rows.is_empty());
    assert!(qmd_error(&r).is_some(), "{:?}", r.query_echo);

    let params = map::MatchParams {
        q: Some(QUERY.to_string()),
    };
    let matched = map::matches_handler(State(s.clone()), Query(params))
        .await
        .0;
    assert!(matched.markdown_uuids.is_empty());
    assert_eq!(matched.errors.len(), 1, "{:?}", matched.errors);
}
