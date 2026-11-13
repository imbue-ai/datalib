//! Free-text searches through the applet's handlers, against the TNG
//! fixture's grid and search indexes and the real embedding model: the
//! rank order and paging, the structured terms narrowing the ranking, and
//! what a search says when the index cannot answer.

use std::collections::HashSet;

use super::tests::{groups, index_over, search, search_part, search_within, uuids, Part};
use super::*;
use sqlx::ConnectOptions;

/// A word the fixture's corpus uses across several sources.
const QUERY: &str = "the enterprise";

/// The fixture's root: its rendered documents, grid and search indexes,
/// vectors and embedding model.
pub(super) fn fixture_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::materialize_root(root.path());
    root
}

fn scores(r: &SearchResponse) -> Vec<f64> {
    r.rows
        .iter()
        .map(|row| row.score.expect("every free-text row has a score"))
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

/// A free-text search comes best first by its score, and its pages are
/// consecutive slices of that one ranking; each row shows the words its
/// hit matched.
#[tokio::test]
async fn free_text_pages_follow_the_rank_and_show_the_matched_words() {
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

/// Structured terms narrow the hits, over the whole ranking rather than
/// one page of it, and a sort reorders what is left.
#[tokio::test]
async fn structured_terms_narrow_the_ranking_and_a_sort_reorders_it() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let everything = search(&s, QUERY, None, 1_000, None).await;

    let narrowed = format!("{QUERY} source:slack");
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

/// Warming reads every vector before anyone searches, so the first search
/// does not wait for them.
#[tokio::test]
async fn warming_reads_what_the_first_searches_would_wait_for() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    assert_eq!(s.held_vectors.held().await, 0);

    super::warm::warm_once(&s).await;

    assert!(s.held_vectors.held().await > 0, "no vector was read");
}

/// A source the search index holds and the grid does not, as a turned-off
/// one is once its render store falls behind a change of shape: a search
/// draws nothing from it, so its hits take no place in the ranking's cut.
#[tokio::test]
async fn a_search_reads_only_the_sources_the_grid_holds() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let plain = parse_query(QUERY);
    let gone = search(&s, QUERY, None, 1_000, None).await.rows[0]
        .source_id
        .clone();
    let named = parse_query(&format!("source:{gone} {QUERY}"));
    assert!(
        !word_hits(&s, &named).await.unwrap().is_empty(),
        "the source has hits to lose"
    );

    let db = datalib_runtime::layout::grid_index_db(root.path());
    let pool = datalib_etl_render::grid_index::open_index(&db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM grid_rows WHERE source_id = ?")
        .bind(&gone)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE store_generation SET generation = generation + 1")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let hits = word_hits(&s, &plain).await.unwrap();
    assert!(!hits.is_empty(), "the other sources still answer");
    let documents: Vec<String> = hits.iter().map(|(d, _)| d.clone()).collect();
    let with_rows = s
        .repo
        .documents_kept(&parse_query(""), &documents)
        .await
        .unwrap();
    assert_eq!(documents.len(), hits.len());
    for document in &documents {
        assert!(with_rows.contains(document), "no row for {document}");
    }
    assert!(
        word_hits(&s, &named).await.unwrap().is_empty(),
        "naming the source reads no more of it"
    );

    let fresh = index_over(root.path()).await;
    let near = meaning_ranking(&fresh, &plain).await.unwrap();
    assert!(!near.is_empty(), "the other sources answer by meaning");
}

/// `source:` scopes the ranking, not only the rows the grid keeps: a
/// source whose hits rank below another source's still answers in full.
#[tokio::test]
async fn source_id_scopes_the_ranking_before_its_cut() {
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

    let scoped = search(&s, &format!("{QUERY} source:{other}"), None, 1, None).await;
    assert_eq!(scoped.rows.len(), 1, "{:?}", scoped.errors);
    assert_eq!(&scoped.rows[0].source_id, other);
}

/// A free-text search groups the rows it ranked, and nothing else: the
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
        "a group keeps the ranking's scores"
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

/// `qmd_vsearch:` and `qmd:` are older spellings of free text, kept for the
/// searches saved with them: each answers as the bare words do.
#[tokio::test]
async fn the_old_spellings_of_free_text_answer_as_free_text() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let word = "warp";
    let bare = search(&s, word, None, 1_000, None).await;
    for q in [format!("qmd_vsearch:{word}"), format!("qmd:{word}")] {
        let r = search(&s, &q, None, 1_000, None).await;
        assert_eq!(qmd_error(&r), None, "{q}");
        assert_eq!(uuids(&r), uuids(&bare), "{q}");
    }
}

/// Structured terms alone light up the map without a search index, which
/// a root no `keyword_index` has run on shows.
#[tokio::test]
async fn the_map_matches_structured_terms_without_a_search_index() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::copy_grid_index(root.path());
    let s = index_over(root.path()).await;
    let params = map::MatchParams {
        q: Some("source:slack".to_string()),
    };
    let matched = map::matches_handler(State(s), Query(params)).await.0;
    assert!(matched.errors.is_empty(), "{:?}", matched.errors);
    assert!(!matched.markdown_uuids.is_empty());
}

/// A root no sync has built a search index for still answers what the grid's
/// own terms hold, and says the rest is missing. Words the terms do not hold
/// get no rows and `qmd_index_missing`, on the grid and its groups: a state
/// the grid explains, not a failure it shows as an error, and not an empty
/// result that reads as "no matches". The map says why in its errors.
#[tokio::test]
async fn free_text_without_a_search_index_says_it_is_not_built() {
    let root = tempfile::tempdir().unwrap();
    datalib_search_fixture::copy_grid_index(root.path());
    let s = index_over(root.path()).await;

    let named = search(&s, "enterprise", None, 10, None).await;
    assert!(!named.rows.is_empty(), "the terms answer without the index");
    assert!(
        named.query_echo["partial"]
            .as_str()
            .is_some_and(|why| why.contains("search index")),
        "{:?}",
        named.query_echo
    );

    const NOWHERE: &str = "xyzzyplugh";
    let r = search(&s, NOWHERE, None, 10, None).await;
    assert!(r.rows.is_empty());
    assert!(qmd_index_missing(&r), "{:?}", r.query_echo);
    assert_eq!(qmd_error(&r), None);
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    let g = groups(&s, NOWHERE, "kind").await;
    assert!(g.groups.is_empty());
    assert!(g.qmd_index_missing);
    assert_eq!(g.qmd_error, None);

    let structured = search(&s, "is:document", None, 10, None).await;
    assert!(!structured.rows.is_empty());
    assert!(!qmd_index_missing(&structured));

    let params = map::MatchParams {
        q: Some(NOWHERE.to_string()),
    };
    let matched = map::matches_handler(State(s.clone()), Query(params))
        .await
        .0;
    assert!(matched.markdown_uuids.is_empty());
    assert_eq!(matched.errors.len(), 1, "{:?}", matched.errors);
}

/// Each part of a search answers the same words its own way: fields from the
/// search terms, each row saying which of its terms matched unless its title
/// did; words by BM25 over the search index's messages, best first;
/// meaning from its vectors alone.
#[tokio::test]
async fn each_part_answers_free_text_its_own_way() {
    let root = fixture_root();
    let s = index_over(root.path()).await;

    let fields = search_part(&s, "enterprise", Part::Fields, 0, 1_000).await;
    assert!(
        !fields.rows.is_empty(),
        "no row's terms start with the word"
    );
    for row in &fields.rows {
        // A row its title matched is already called by it, and keeps its
        // own opening words.
        let by_term = row.snippet.split_once(": ").filter(|(kind, _)| {
            datalib_schema::search_terms::SearchTermKind::parse(kind).is_some()
        });
        match by_term {
            Some((_, value)) => assert!(value.to_lowercase().contains("enterprise"), "{value}"),
            None => assert!(
                row.conversation_name.to_lowercase().contains("enterprise"),
                "{:?} names no term and its title does not match",
                row.snippet
            ),
        }
    }

    let words = search_part(&s, QUERY, Part::Words, 0, 1_000).await;
    assert_eq!(qmd_error(&words), None);
    let ranked = scores(&words);
    assert!(
        ranked.windows(2).all(|w| w[0] >= w[1]),
        "not best first: {ranked:?}"
    );
    assert!(
        ranked.first() > ranked.last(),
        "every score ties: {ranked:?}"
    );

    let meaning = search_part(&s, QUERY, Part::Meaning, 0, 1_000).await;
    assert_eq!(qmd_error(&meaning), None);
    assert!(!meaning.rows.is_empty());
    assert_ne!(uuids(&words), uuids(&meaning), "two parts gave one answer");
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

    let inbox = search_part(&s, "inbox", Part::Fields, 0, 1_000).await;
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

/// With meaning left out, a search is scoped by `source:` like any other,
/// and its groups count the same rows its pages list.
#[tokio::test]
async fn a_search_narrows_and_groups_with_meaning_left_out() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let words = format!("{QUERY} semantic:off");
    let slack = search(&s, &format!("{words} source:slack"), None, 1_000, None).await;
    assert!(slack.total > 0, "the fixture's slack source has the words");
    assert!(slack.rows.iter().all(|r| r.source_id == "slack"));

    let all = search(&s, &words, None, 1_000, None).await;
    let by_source = groups(&s, &words, "source_ref").await;
    let counted: u64 = by_source.groups.iter().map(|g| g.count).sum();
    assert_eq!(counted, all.total);
}

/// A search is one list, however it matched: what the words find is in it,
/// and what is near in meaning joins it above the cutoff. Every document has
/// some similarity, so without a cutoff every search finds them all.
#[tokio::test]
async fn the_cutoff_decides_what_meaning_adds_to_the_words() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let total = |q: String| {
        let s = &s;
        async move {
            let r = search(s, &q, None, 1_000, None).await;
            assert_eq!(qmd_error(&r), None, "{q}");
            assert!(r.refused.is_empty(), "{q}: {:?}", r.refused);
            r.total
        }
    };
    let off = total(format!("{QUERY} semantic:off")).await;
    let strict = total(format!("{QUERY} semantic:1")).await;
    let default = total(QUERY.to_string()).await;
    let everything = total(format!("{QUERY} semantic:0")).await;
    assert!(off > 0, "the words find something");
    assert_eq!(
        strict, off,
        "nothing is that near, so only the words answer"
    );
    assert!(default >= off, "the words stay whatever the cutoff");
    assert!(
        default < everything,
        "the default cutoff drops the far documents: {default} of {everything}"
    );

    let echoed = search(&s, &format!("{QUERY} semantic:0.4"), None, 1, None).await;
    assert_eq!(echoed.query_echo["semantic_cutoff"], 0.4);
    let echoed = search(&s, &format!("{QUERY} semantic:off"), None, 1, None).await;
    assert_eq!(
        echoed.query_echo["semantic_cutoff"],
        serde_json::Value::Null
    );
}

/// Words nothing holds are not near anything either: the search says it
/// found nothing rather than list the library.
#[tokio::test]
async fn nonsense_finds_little_rather_than_everything() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let everything = search(&s, "xyzzy plugh semantic:0", None, 1_000, None).await;
    let default = search(&s, "xyzzy plugh", None, 1_000, None).await;
    assert!(
        default.total * 4 < everything.total,
        "{} of {}",
        default.total,
        everything.total
    );
}

/// Above the results a person is told how many of the documents this search
/// could find have no vectors. It counts the documents the filters leave, and
/// only where meaning was asked for.
#[tokio::test]
async fn a_search_says_how_many_candidates_have_no_vectors() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let count = |r: &SearchResponse| r.query_echo["without_vectors"]["total"].as_u64();

    let all = search(&s, QUERY, None, 1, None).await;
    let all_count = count(&all).expect("a search by meaning counts");
    let again = search(&s, QUERY, None, 1, None).await;
    assert_eq!(
        count(&again),
        Some(all_count),
        "the same search, the same count"
    );

    let by_source = &all.query_echo["without_vectors"]["by_source"];
    let summed: u64 = by_source
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| pair[1].as_u64().unwrap())
        .sum();
    assert_eq!(summed, all_count);

    let narrowed = search(&s, &format!("{QUERY} source:slack"), None, 1, None).await;
    assert!(count(&narrowed).expect("it counts") <= all_count);

    for (q, why) in [
        (format!("{QUERY} semantic:off"), "meaning was not asked for"),
        ("source:slack".to_string(), "there are no words to match"),
        (
            "picard@enterprise.starfleet".to_string(),
            "an address is looked up, not searched by meaning",
        ),
    ] {
        let r = search(&s, &q, None, 1, None).await;
        assert_eq!(count(&r), None, "{q}: {why}");
    }
    let later = search(&s, QUERY, Some(1), 1, None).await;
    assert_eq!(count(&later), None, "a later page has nothing to add");
}

/// A document filed for the grid alone, Notion's table of contents, is kept
/// out of the search index on purpose, so it must not read as vectors still owed: every
/// Notion library would carry a warning nothing could clear. A document that
/// did lose its vectors is counted, under its source.
#[tokio::test]
async fn only_a_document_meant_to_have_vectors_counts_as_lacking_them() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let r = search(&s, QUERY, None, 1, None).await;
    assert_eq!(
        r.query_echo["without_vectors"]["total"], 0,
        "the fixture embeds all it means to: {:?}",
        r.query_echo["without_vectors"]
    );
    drop(s);

    // A Slack document's chunks, forgotten by the inference cache.
    let mut grid = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(datalib_runtime::layout::grid_index_db(root.path()))
        .connect()
        .await
        .unwrap();
    let hashes: Vec<String> = sqlx::query_scalar(
        "SELECT c.hash FROM search_chunks c JOIN search_documents d ON d.doc_id = c.doc_id \
          WHERE d.markdown_uuid = (SELECT d2.markdown_uuid FROM search_documents d2 \
            JOIN grid_rows g ON g.uuid = d2.markdown_uuid \
           WHERE d2.source_id = 'slack' AND g.is_document = 1 ORDER BY 1 LIMIT 1)",
    )
    .fetch_all(&mut grid)
    .await
    .unwrap();
    sqlx::Connection::close(grid).await.unwrap();
    assert!(!hashes.is_empty(), "the fixture indexes its slack");

    let mut cache = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(datalib_runtime::layout::inference_cache_db(root.path()))
        .connect()
        .await
        .unwrap();
    let removed = sqlx::query("DELETE FROM vectors WHERE hash IN (SELECT value FROM json_each(?))")
        .bind(serde_json::to_string(&hashes).unwrap())
        .execute(&mut cache)
        .await
        .unwrap();
    assert!(removed.rows_affected() > 0, "the fixture embeds its slack");
    sqlx::Connection::close(cache).await.unwrap();

    let s = index_over(root.path()).await;
    let r = search(&s, QUERY, None, 1, None).await;
    let missing = &r.query_echo["without_vectors"];
    assert!(missing["total"].as_u64().unwrap() >= 1, "{missing:?}");
    assert_eq!(missing["by_source"][0][0], "slack", "{missing:?}");
}

/// The first launch downloads the embedding model, and a search then must
/// not lose its word matches to it, nor hide why meaning is missing.
#[tokio::test]
async fn a_search_keeps_its_words_when_meaning_cannot_answer_and_says_so() {
    let root = fixture_root();
    let mut s = index_over(root.path()).await;
    let elsewhere = tempfile::tempdir().unwrap();
    s.local_embedder = std::sync::Arc::new(super::local_embedder::LocalEmbedder::new(
        std::sync::Arc::new(super::models::ModelGate::new(elsewhere.path())),
        std::time::Duration::from_millis(1),
    ));

    let r = search(&s, QUERY, None, 1_000, None).await;
    assert_eq!(qmd_error(&r), None);
    assert!(!r.rows.is_empty(), "the words still answer");
    let partial = r.query_echo["partial"].as_str().expect("and it says so");
    assert!(partial.contains("could not answer"), "{partial}");
    assert!(
        super::unanswered(&r).iter().any(|why| why == partial),
        "and a script hears it"
    );

    let off = search(&s, &format!("{QUERY} semantic:off"), None, 1_000, None).await;
    assert_eq!(off.query_echo["partial"], serde_json::Value::Null);
    assert_eq!(off.total, r.total, "the same words, with nothing missing");
}

/// A script still naming a tab is told, not handed a different list.
#[tokio::test]
async fn a_search_naming_a_tab_is_refused() {
    let root = fixture_root();
    let s = index_over(root.path()).await;
    let params = super::SearchParams {
        q: Some(QUERY.to_string()),
        limit: Some(1),
        offset: None,
        sort: None,
        through: None,
        among: None,
        within: None,
        tab: Some("words".to_string()),
    };
    let r = super::answer_search(&s, params).await;
    assert!(r.rows.is_empty());
    assert!(r.refused[0].contains("semantic:off"), "{:?}", r.refused);
}
