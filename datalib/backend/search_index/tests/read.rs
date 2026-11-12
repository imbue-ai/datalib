//! The reader against an index and cache the writers filled.

use std::collections::HashSet;

use datalib_search_index::{
    chunk::chunk_hash, words, Index, IndexDoc, IndexSection, InferenceCache, Reader,
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

/// The plan's join: words said in two messages of one thread find it, and
/// the hit lands on a message by its bytes in the document.
#[tokio::test]
async fn words_in_different_messages_find_the_thread_and_land_on_a_message() {
    let f = fixture(&[thread()], &[]).await;
    let warp = f
        .reader
        .matching("{title body} : (\"warp\")", None)
        .await
        .unwrap();
    let coolant = f
        .reader
        .matching("{title body} : (\"coolant\")", None)
        .await
        .unwrap();
    let hits = words::rank(&[warp, coolant], &HashSet::new(), None, 10);
    assert_eq!(hits.len(), 1, "{hits:?}");
    let text: String = thread().sections.iter().map(|s| s.text.as_str()).collect();
    let landed = &text[hits[0].start as usize..hits[0].end as usize];
    assert!(
        landed == "the warp core is stable\n" || landed == "deck twelve reports a coolant leak\n",
        "{landed:?}"
    );
    f.reader.close().await;
}

#[tokio::test]
async fn a_source_left_out_is_not_matched() {
    let other = doc("slack", "chat", &[(Some("c1"), "warp drive online\n")]);
    let f = fixture(&[thread(), other], &[]).await;
    let only_slack = ["slack".to_string()];
    let found = f
        .reader
        .matching("{title body} : (\"warp\")", Some(&only_slack))
        .await
        .unwrap();
    let docs: Vec<&str> = found.iter().map(|m| m.markdown_uuid.as_str()).collect();
    assert_eq!(docs, ["chat"]);
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
