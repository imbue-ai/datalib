//! The reader against an index and cache the writers filled.

use std::collections::{HashMap, HashSet};

use datalib_search_index::{
    chunk::chunk_hash, Index, IndexDoc, IndexSection, InferenceCache, Reader,
};

fn doc(source: &str, uuid: &str, sections: &[(Option<&str>, &str)]) -> IndexDoc {
    IndexDoc {
        source_id: source.into(),
        markdown_uuid: uuid.into(),
        content_hash: format!("hash-{uuid}"),
        title: "Ten Forward".into(),
        head: None,
        sections: sections
            .iter()
            .map(|(uuid, text)| IndexSection {
                uuid: uuid.map(str::to_string),
                text: text.to_string(),
                lead: None,
            })
            .collect(),
    }
}

fn thread() -> IndexDoc {
    doc(
        "mail",
        "thread",
        &[
            (None, "# Ten Forward\n"),
            (Some("m1"), "the warp core is stable\n"),
            (Some("m2"), "deck twelve reports a coolant leak\n"),
        ],
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    reader: Reader,
}

async fn fixture(docs: &[IndexDoc], vectors: &[(String, Vec<f32>)]) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let index_path = dir.path().join("index.sqlite");
    let cache_path = dir.path().join("cache.sqlite");
    let index = Index::open(&index_path).await.unwrap();
    index
        .put(docs, &datalib_search_index::chunk::four_bytes_a_token)
        .await
        .unwrap();
    index.close().await;
    let cache = InferenceCache::open(&cache_path).await.unwrap();
    cache
        .put(&datalib_search_index::model_key(), vectors)
        .await
        .unwrap();
    cache.close().await;
    let reader = Reader::open(&index_path, &cache_path)
        .await
        .unwrap()
        .unwrap();
    Fixture { _dir: dir, reader }
}

/// Every word, each whole, in one message.
fn all_of(words: &[&str]) -> String {
    let terms: Vec<String> = words.iter().map(|w| format!("\"{w}\"")).collect();
    format!("{{title body}} : ({})", terms.join(" AND "))
}

fn ids(hits: &[datalib_search_index::MessageHit]) -> Vec<&str> {
    hits.iter().map(|h| h.markdown_uuid.as_str()).collect()
}

/// A hit is a message holding every word, and names that message: a
/// thread that says the words in different messages is not one.
#[tokio::test]
async fn a_hit_is_a_message_holding_every_word() {
    let t = thread();
    let both = doc(
        "mail",
        "both",
        &[
            (Some("b0"), "tea\n"),
            (Some("b1"), "the warp core coolant\n"),
        ],
    );
    let f = fixture(&[t, both], &[]).await;
    let spread = f
        .reader
        .words(&all_of(&["warp", "coolant"]), None, None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&spread), ["both"]);
    let one = f
        .reader
        .words(&all_of(&["coolant"]), None, None, 10)
        .await
        .unwrap();
    let thread_hit = one.iter().find(|h| h.markdown_uuid == "thread").unwrap();
    assert_eq!(
        thread_hit.uuid.as_deref(),
        Some("m2"),
        "the message saying it"
    );
    f.reader.close().await;
}

/// A document answers once, at its best message, and the cut counts
/// documents; `among` and an excluded word narrow what is ranked.
#[tokio::test]
async fn a_document_answers_once_at_its_best_message() {
    let many = doc(
        "mail",
        "many",
        &[
            (Some("m1"), "warp\n"),
            (Some("m2"), "warp warp warp\n"),
            (Some("m3"), "warp and a breach\n"),
        ],
    );
    let other = doc("mail", "other", &[(Some("o1"), "warp drive\n")]);
    let f = fixture(&[many.clone(), other], &[]).await;
    let hits = f
        .reader
        .words(&all_of(&["warp"]), None, None, 10)
        .await
        .unwrap();
    let mut found = ids(&hits);
    found.sort();
    assert_eq!(found, ["many", "other"]);
    let best = hits.iter().find(|h| h.markdown_uuid == "many").unwrap();
    assert_eq!(best.uuid.as_deref(), Some("m2"), "warp warp warp");
    assert_eq!(
        f.reader
            .words(&all_of(&["warp"]), None, None, 1)
            .await
            .unwrap()
            .len(),
        1
    );
    let among = HashSet::from(["other".to_string()]);
    let narrowed = f
        .reader
        .words(&all_of(&["warp"]), None, Some(&among), 10)
        .await
        .unwrap();
    assert_eq!(ids(&narrowed), ["other"]);
    let not_breach = f
        .reader
        .words("{title body} : (\"breach\" NOT \"warp\")", None, None, 10)
        .await
        .unwrap();
    assert!(not_breach.is_empty(), "the breach is said with warp");
    f.reader.close().await;
}

#[tokio::test]
async fn a_source_left_out_is_not_matched() {
    let other = doc("slack", "chat", &[(Some("c1"), "warp drive online\n")]);
    let f = fixture(&[thread(), other], &[]).await;
    let only_slack = ["slack".to_string()];
    let found = f
        .reader
        .words(&all_of(&["warp"]), Some(&only_slack), None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&found), ["chat"]);
    f.reader.close().await;
}

/// A chunk with a cached vector is held, unit length, with the messages it
/// covers; one the cache lacks is not, and neither is a document left with
/// no chunk.
#[tokio::test]
async fn held_vectors_are_the_cached_chunks_with_their_messages() {
    let t = thread();
    let text: String = t.sections.iter().map(|s| s.text.as_str()).collect();
    let hash = chunk_hash(&t.title, &text);
    let unembedded = doc("mail", "draft", &[(Some("d1"), "not yet embedded\n")]);
    let f = fixture(&[t, unembedded], &[(hash, vec![3.0, 4.0])]).await;
    let held = f
        .reader
        .held(&datalib_search_index::model_key())
        .await
        .unwrap();
    assert_eq!(held.dim, 2);
    assert_eq!(held.docs.len(), 1);
    let d = &held.docs[0];
    assert_eq!(d.markdown_uuid, "thread");
    assert_eq!(held.vector(0), [0.6, 0.8]);
    let covered: Vec<&str> = held.covered(d, d.chunks.start).collect();
    assert_eq!(covered, ["m1", "m2"]);
    let other_model = f.reader.held("another-model").await.unwrap();
    assert!(other_model.docs.is_empty());
    f.reader.close().await;
}

/// A document is embedded when every chunk of it has a vector; one the
/// index does not hold is not answered, and the whole index is counted.
#[tokio::test]
async fn a_document_is_embedded_when_every_chunk_has_a_vector() {
    let t = thread();
    let text: String = t.sections.iter().map(|s| s.text.as_str()).collect();
    let hash = chunk_hash(&t.title, &text);
    let unembedded = doc("mail", "draft", &[(Some("d1"), "not yet embedded\n")]);
    let f = fixture(&[t, unembedded], &[(hash, vec![3.0, 4.0])]).await;
    let model = datalib_search_index::model_key();
    let asked = ["thread", "draft", "never-indexed"].map(String::from);
    let states = f.reader.states(&model, &asked).await.unwrap();
    assert_eq!(states.len(), 2, "{states:?}");
    assert!(states["thread"]);
    assert!(!states["draft"]);
    assert_eq!(f.reader.coverage(&model).await.unwrap(), (2, 1));
    assert_eq!(f.reader.coverage("another-model").await.unwrap(), (2, 0));
    f.reader.close().await;
}

/// A message of `text`, drawn with an envelope: it starts a chunk of its
/// own.
fn message(uuid: &str, text: &str) -> IndexSection {
    IndexSection {
        uuid: Some(uuid.into()),
        text: format!("{text}\n"),
        lead: Some(format!("**{uuid}** wrote")),
    }
}

/// A document is embedded only once every chunk of it has a vector, not
/// when some do; and every source's documents are answered and counted.
#[tokio::test]
async fn a_document_is_embedded_only_once_every_chunk_is() {
    let mut two = doc("mail", "two", &[]);
    two.sections = vec![message("m1", "warp core"), message("m2", "coolant leak")];
    let halves = two
        .prompted(&datalib_search_index::chunk::four_bytes_a_token)
        .unwrap();
    assert_eq!(halves.len(), 2, "a message starts a chunk of its own");
    let chat = doc("chat", "chat", &[(Some("c1"), "tea, Earl Grey\n")]);
    let chat_chunk = chat
        .prompted(&datalib_search_index::chunk::four_bytes_a_token)
        .unwrap()[0]
        .hash
        .clone();
    let model = datalib_search_index::model_key();
    let asked = ["two", "chat"].map(String::from);

    let first = (halves[0].hash.clone(), vec![1.0, 0.0]);
    let tea = (chat_chunk, vec![0.0, 1.0]);
    let f = fixture(&[two.clone(), chat.clone()], &[first.clone(), tea.clone()]).await;
    let states = f.reader.states(&model, &asked).await.unwrap();
    assert_eq!(
        states,
        HashMap::from([("two".to_string(), false), ("chat".to_string(), true)])
    );
    assert_eq!(f.reader.coverage(&model).await.unwrap(), (2, 1));
    f.reader.close().await;

    let second = (halves[1].hash.clone(), vec![0.6, 0.8]);
    let f = fixture(&[two, chat], &[first, second, tea]).await;
    let states = f.reader.states(&model, &asked).await.unwrap();
    assert!(states.values().all(|embedded| *embedded), "{states:?}");
    assert_eq!(f.reader.coverage(&model).await.unwrap(), (2, 2));
    f.reader.close().await;
}

/// A page's rows can ask about more documents than one statement binds:
/// those past the first batch are answered too.
#[tokio::test]
async fn states_are_answered_past_the_first_batch() {
    let held = ["held-a", "held-b"];
    let docs: Vec<IndexDoc> = held
        .iter()
        .map(|u| doc("mail", u, &[(Some("m"), "warp\n")]))
        .collect();
    let f = fixture(&docs, &[]).await;
    let mut asked: Vec<String> = (0..10_005).map(|i| format!("absent-{i}")).collect();
    asked.extend(held.map(String::from));
    let states = f
        .reader
        .states(&datalib_search_index::model_key(), &asked)
        .await
        .unwrap();
    assert_eq!(
        states,
        HashMap::from([("held-a".to_string(), false), ("held-b".to_string(), false)])
    );
    f.reader.close().await;
}

#[tokio::test]
async fn no_index_reads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let reader = Reader::open(&dir.path().join("a"), &dir.path().join("b"))
        .await
        .unwrap();
    assert!(reader.is_none());
}

/// The grid index's file is there before any keyword pass has run: the
/// reader takes it as no index yet, not as an error.
#[tokio::test]
async fn a_grid_index_without_the_search_tables_is_no_index_yet() {
    let dir = tempfile::tempdir().unwrap();
    let grid = dir.path().join("db.sqlite");
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&grid)
        .create_if_missing(true);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE grid_rows (uuid TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let reader = Reader::open(&grid, &dir.path().join("cache.sqlite"))
        .await
        .unwrap();
    assert!(reader.is_none());
}

fn page_term(word: &str) -> String {
    format!("{{title body}} : (\"{word}\")")
}

fn page_terms(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| page_term(w)).collect()
}

/// A page's heading and its rows: no one piece says every word.
fn page(uuid: &str) -> IndexDoc {
    doc(
        "airvisual",
        uuid,
        &[
            (None, "# Air quality\n"),
            (Some("kitchen"), "Kitchen: 12 µg/m³\n"),
            (Some("bedroom"), "Bedroom: 8 µg/m³ air\n"),
        ],
    )
}

/// The same words said as messages, each with its envelope.
fn conversation() -> IndexDoc {
    let mut c = page("chat");
    c.source_id = "slack".into();
    for s in &mut c.sections {
        s.lead = Some("Data — Tue 3 Mar 2026 14:05".into());
    }
    c
}

/// A page holds the words between its pieces, and lands on the piece
/// holding the most of them; a
/// conversation that says them in different messages is not found.
#[tokio::test]
async fn a_page_holds_the_words_between_its_pieces() {
    let f = fixture(&[page("page"), conversation()], &[]).await;
    let hits = f
        .reader
        .page_words(
            &page_terms(&["bedroom", "air", "quality"]),
            &[],
            None,
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["page"]);
    assert_eq!(hits[0].uuid.as_deref(), Some("bedroom"));
    let missing = f
        .reader
        .page_words(&page_terms(&["bedroom", "tribble"]), &[], None, None, 10)
        .await
        .unwrap();
    assert!(missing.is_empty(), "a page lacking a word: {missing:?}");
    f.reader.close().await;
}

/// An excluded word anywhere on a page drops it; `sources` and `among`
/// narrow what is read.
#[tokio::test]
async fn an_excluded_word_anywhere_drops_the_page() {
    let f = fixture(&[page("page"), page("other")], &[]).await;
    let terms = page_terms(&["bedroom", "quality"]);
    let excluded = f
        .reader
        .page_words(&terms, &[page_term("kitchen")], None, None, 10)
        .await
        .unwrap();
    assert!(excluded.is_empty(), "{excluded:?}");
    let among: HashSet<String> = ["other".to_string()].into();
    let narrowed = f
        .reader
        .page_words(&terms, &[], None, Some(&among), 10)
        .await
        .unwrap();
    assert_eq!(ids(&narrowed), ["other"]);
    let elsewhere = f
        .reader
        .page_words(&terms, &[], Some(&["slack".to_string()]), None, 10)
        .await
        .unwrap();
    assert!(elsewhere.is_empty(), "{elsewhere:?}");
    f.reader.close().await;
}
