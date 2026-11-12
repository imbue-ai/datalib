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

fn words(terms: &[&str]) -> Vec<String> {
    terms
        .iter()
        .map(|t| format!("{{title body}} : (\"{t}\")"))
        .collect()
}

/// The plan's join: words said in two messages of one thread find it.
#[tokio::test]
async fn words_in_different_messages_find_the_thread_and_land_on_a_message() {
    let f = fixture(&[thread()], &[]).await;
    let hits = f
        .reader
        .words(&words(&["warp", "coolant"]), &[], None, None, 10)
        .await
        .unwrap();
    let found: Vec<&str> = hits.iter().map(|h| h.markdown_uuid.as_str()).collect();
    assert_eq!(found, ["thread"]);
    f.reader.close().await;
}

/// The rarest term is read first and the rest only within the documents
/// it found; whichever order the terms come in, a document that lacks one
/// is dropped, and so is one an excluded term names. A word in more than
/// one message in twenty is left out: here, the filler every other
/// document says.
#[tokio::test]
async fn every_term_must_match_whatever_the_order_and_an_excluded_one_drops() {
    let ten = doc(
        "mail",
        "ten",
        &[
            (Some("a"), "warp core warp core\n"),
            (Some("b"), "coolant\n"),
        ],
    );
    let only_warp = doc("mail", "only", &[(Some("c"), "warp warp warp\n")]);
    let vented = doc(
        "mail",
        "vented",
        &[(Some("d"), "warp coolant\n"), (Some("e"), "venting\n")],
    );
    let dry = doc("mail", "dry", &[(Some("f"), "a coolant leak\n")]);
    let mut docs = vec![ten, only_warp, vented, dry];
    docs.extend((0..100).map(|n| {
        let id = format!("filler-{n}");
        doc(
            "mail",
            &id,
            &[(Some(id.as_str()), "filler about the weather\n")],
        )
    }));
    let f = fixture(&docs, &[]).await;
    let ids = |hits: &[datalib_search_index::words::WordsHit]| {
        let mut ids: Vec<String> = hits.iter().map(|h| h.markdown_uuid.clone()).collect();
        ids.sort();
        ids
    };
    for terms in [["warp", "coolant"], ["coolant", "warp"]] {
        let hits = f
            .reader
            .words(&words(&terms), &[], None, None, 10)
            .await
            .unwrap();
        assert_eq!(ids(&hits), ["ten", "vented"], "{terms:?}");
    }
    let kept = f
        .reader
        .words(
            &words(&["warp", "coolant"]),
            &words(&["venting"]),
            None,
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(ids(&kept), ["ten"]);
    let among = HashSet::from(["vented".to_string()]);
    let narrowed = f
        .reader
        .words(&words(&["warp", "coolant"]), &[], None, Some(&among), 10)
        .await
        .unwrap();
    assert_eq!(ids(&narrowed), ["vented"]);
    let common = f
        .reader
        .words(&words(&["coolant", "filler"]), &[], None, None, 10)
        .await
        .unwrap();
    assert_eq!(
        ids(&common),
        ["dry", "ten", "vented"],
        "the common word is left out"
    );
    let none = f
        .reader
        .words(&words(&["warp", "absent"]), &[], None, None, 10)
        .await
        .unwrap();
    assert!(none.is_empty());
    f.reader.close().await;
}

#[tokio::test]
async fn a_source_left_out_is_not_matched() {
    let other = doc("slack", "chat", &[(Some("c1"), "warp drive online\n")]);
    let f = fixture(&[thread(), other], &[]).await;
    let only_slack = ["slack".to_string()];
    let found = f
        .reader
        .words(&words(&["warp"]), &[], Some(&only_slack), None, 10)
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
