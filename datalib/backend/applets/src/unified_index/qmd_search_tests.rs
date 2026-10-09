//! Free-text searches through the applet's handlers, against the TNG
//! fixture's grid and qmd indexes and a real `qmd mcp`: the rank order and
//! paging, the structured terms narrowing qmd's hits, and what a search
//! says when qmd cannot answer.

use std::collections::HashSet;
use std::sync::OnceLock;

use super::tabs::SearchTab;
use super::tests::{groups, groups_tab, index_over, search, search_tab, search_within, uuids};
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
pub(super) fn fixture_root() -> tempfile::TempDir {
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

fn qmd_index_missing(r: &SearchResponse) -> bool {
    r.query_echo["qmd_index_missing"]
        .as_bool()
        .expect("query_echo always says whether the qmd index is missing")
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
    assert!(!qmd_index_missing(&whole));
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

/// `source_id:` scopes qmd's retrieval, not only the rows the grid keeps:
/// given room for one hit, a source whose best hit ranks below another
/// source's still gets its own. Filtering qmd's global answer instead
/// would leave nothing.
#[tokio::test]
async fn source_id_scopes_qmd_before_its_limit() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let ranked = search(&s, QUERY, None, 1_000, None).await;
    let leader = &ranked.rows[0].source_id;
    let other = &ranked
        .rows
        .iter()
        .find(|r| &r.source_id != leader)
        .expect("the fixture's hits span sources")
        .source_id;

    let parsed = parse_query(&format!("{QUERY} source_id:{other}"));
    let top = qmd_ranking(&s.root, &s.repo, &s.qmd, &parsed, 1)
        .await
        .unwrap();
    assert_eq!(top.len(), 1, "{top:?}");
    let row = s.repo.rows_by_uuids(&[top[0].0.clone()]).await.unwrap();
    assert_eq!(
        &row[0].source_id, other,
        "qmd answered for {leader}, unscoped"
    );
}

/// A free-text search groups the rows qmd ranked, and nothing else: the
/// groups' counts add up to the ranking, and a group's rows are its own.
#[tokio::test]
async fn free_text_groups_the_ranked_rows() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let ranked = search(&s, QUERY, None, 1_000, None).await;

    let by_source = groups(&s, QUERY, "source_ref").await;
    assert_eq!(by_source.qmd_error, None);
    assert!(by_source.groups.len() > 1, "the hits span sources");
    let counted: u64 = by_source.groups.iter().map(|g| g.count).sum();
    assert_eq!(counted, ranked.total);

    let group = &by_source.groups[0];
    let source = group.values[0]
        .clone()
        .expect("a source group names its source");
    let within = serde_json::json!([["source_ref", source]]).to_string();
    let rows = search_within(&s, QUERY, &within, 1_000).await;
    assert_eq!(rows.total, group.count);
    assert!(rows.rows.iter().all(|r| r.source_id == source));
    assert!(
        rows.rows.iter().all(|r| r.score.is_some()),
        "a group keeps qmd's scores"
    );
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

/// A root no sync has built a qmd index for answers free text with no
/// rows and `qmd_index_missing`, on the grid and its groups: a state the
/// grid explains, not a failure it shows as an error, and not an empty
/// result that reads as "no matches". The map says why in its errors.
#[tokio::test]
async fn free_text_without_a_qmd_index_says_it_is_not_built() {
    stage_runtime_once();
    let root = tempfile::tempdir().unwrap();
    datalib_qmd_fixture::copy_grid_index(root.path());
    let s = index_over(root.path()).await;

    let r = search(&s, QUERY, None, 10, None).await;
    assert!(r.rows.is_empty());
    assert!(qmd_index_missing(&r), "{:?}", r.query_echo);
    assert_eq!(qmd_error(&r), None);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let g = groups(&s, QUERY, "kind").await;
    assert!(g.groups.is_empty());
    assert!(g.qmd_index_missing);
    assert_eq!(g.qmd_error, None);

    let structured = search(&s, "is:document", None, 10, None).await;
    assert!(!structured.rows.is_empty());
    assert!(!qmd_index_missing(&structured));

    let params = map::MatchParams {
        q: Some(QUERY.to_string()),
    };
    let matched = map::matches_handler(State(s.clone()), Query(params))
        .await
        .0;
    assert!(matched.markdown_uuids.is_empty());
    assert_eq!(matched.errors.len(), 1, "{:?}", matched.errors);
}

/// Each tab answers the same words its own way, and keeps its answer apart
/// from the others': Fields from the search terms file, each row saying which of
/// its terms matched; Words by BM25 from qmd's keyword index, best first;
/// Meaning from qmd's vectors alone.
#[tokio::test]
async fn each_tab_answers_free_text_its_own_way() {
    let root = fixture_root();
    let s = index_over(root.path()).await;

    let fields = search_tab(&s, "enterprise", Some(SearchTab::Fields), None, 1_000, None).await;
    assert_eq!(fields.query_echo["tab"], "fields");
    assert!(
        !fields.rows.is_empty(),
        "no row's terms start with the word"
    );
    for row in &fields.rows {
        let (kind, value) = row
            .snippet
            .split_once(": ")
            .expect("a fields hit names its term");
        assert!(
            datalib_schema::search_terms::SearchTermKind::parse(kind).is_some(),
            "{kind}"
        );
        assert!(value.to_lowercase().contains("enterprise"), "{value}");
    }

    let words = search_tab(&s, QUERY, Some(SearchTab::Words), None, 1_000, None).await;
    assert_eq!(qmd_error(&words), None);
    assert_eq!(words.query_echo["tab"], "words");
    let ranked = scores(&words);
    assert!(
        ranked.windows(2).all(|w| w[0] >= w[1]),
        "not best first: {ranked:?}"
    );
    assert!(
        ranked.first() > ranked.last(),
        "every score ties: {ranked:?}"
    );

    let meaning = search_tab(&s, QUERY, Some(SearchTab::Meaning), None, 1_000, None).await;
    assert_eq!(qmd_error(&meaning), None);
    assert_eq!(meaning.query_echo["tab"], "meaning");
    assert!(!meaning.rows.is_empty());
    assert_ne!(uuids(&words), uuids(&meaning), "two tabs gave one answer");
}

/// A pasted address finds the emails it was copied on, not only the ones
/// it wrote, and a label is a field like any other: both are terms the
/// email render supplies, beyond the row's own columns.
#[tokio::test]
async fn an_address_finds_the_emails_it_received_and_a_label_its_emails() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let snippets = |r: &SearchResponse| -> HashSet<String> {
        r.rows.iter().map(|row| row.snippet.clone()).collect()
    };

    let riker = search(&s, "riker@enterprise.starfleet", None, 1_000, None).await;
    let matched = snippets(&riker);
    assert!(
        matched.contains("cc: email:riker@enterprise.starfleet"),
        "{matched:?}"
    );
    assert!(
        matched.contains("from: email:riker@enterprise.starfleet"),
        "{matched:?}"
    );

    let inbox = search_tab(&s, "inbox", Some(SearchTab::Fields), None, 1_000, None).await;
    assert!(
        snippets(&inbox).contains("label: Inbox"),
        "{:?}",
        snippets(&inbox)
    );
}

/// A person is found where they were only blind-copied or mentioned:
/// Troi on the log Picard sent her a Bcc of, Geordi where an email
/// @-mentions him, Riker (by his number) where a Signal message does.
#[tokio::test]
async fn a_person_is_found_where_they_were_blind_copied_or_mentioned() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    for (q, matched) in [
        (
            "troi@enterprise.starfleet",
            "bcc: email:troi@enterprise.starfleet",
        ),
        (
            "laforge@enterprise.starfleet",
            "mention: email:laforge@enterprise.starfleet",
        ),
        ("+17015550101", "mention: tel:+17015550101"),
    ] {
        let found = search(&s, q, None, 1_000, None).await;
        let snippets: HashSet<&str> = found.rows.iter().map(|r| r.snippet.as_str()).collect();
        assert!(snippets.contains(matched), "{q}: {snippets:?}");
    }
}

/// The Words tab is scoped by `source_id:` like any search, and its groups
/// count the same rows its pages list.
#[tokio::test]
async fn the_words_tab_narrows_and_groups_like_a_search() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let narrowed = format!("{QUERY} source_id:slack");
    let slack = search_tab(&s, &narrowed, Some(SearchTab::Words), None, 1_000, None).await;
    assert!(slack.total > 0, "the fixture's slack source has the words");
    assert!(slack.rows.iter().all(|r| r.source_id == "slack"));

    let all = search_tab(&s, QUERY, Some(SearchTab::Words), None, 1_000, None).await;
    let by_source = groups_tab(&s, QUERY, "source_ref", Some(SearchTab::Words)).await;
    let counted: u64 = by_source.groups.iter().map(|g| g.count).sum();
    assert_eq!(counted, all.total);
}
