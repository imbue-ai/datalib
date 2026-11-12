//! The reader against an index and cache the writers filled.

use std::collections::HashSet;

use datalib_search_index::{
    chunk::chunk_hash, Index, IndexDoc, IndexSection, InferenceCache, Reader,
};

fn doc(source: &str, uuid: &str, sections: &[(Option<&str>, &str)]) -> IndexDoc {
    IndexDoc {
        source_id: source.into(),
        markdown_uuid: uuid.into(),
        content_hash: format!("hash-{uuid}"),
        title: "Ten Forward".into(),
        sections: sections
            .iter()
            .map(|(uuid, text)| IndexSection {
                uuid: uuid.map(str::to_string),
                text: text.to_string(),
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
    index.put(docs).await.unwrap();
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

/// A hit is a message holding every word, and lands on it by its bytes: a
/// thread that says the words in different messages is not one.
#[tokio::test]
async fn a_hit_is_a_message_holding_every_word() {
    let t = thread();
    let text: String = t.sections.iter().map(|s| s.text.as_str()).collect();
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
        &text[thread_hit.start as usize..thread_hit.end as usize],
        "deck twelve reports a coolant leak\n"
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
    let text: String = many.sections.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        &text[best.start as usize..best.end as usize],
        "warp warp warp\n"
    );
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

#[tokio::test]
async fn no_index_reads_as_none() {
    let dir = tempfile::tempdir().unwrap();
    let reader = Reader::open(&dir.path().join("a"), &dir.path().join("b"))
        .await
        .unwrap();
    assert!(reader.is_none());
}
