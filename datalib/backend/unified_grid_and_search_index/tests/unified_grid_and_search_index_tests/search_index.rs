//! Meaning read from the search index: the chunks' vectors from the
//! inference cache, scored in memory, each document at its nearest chunk.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use datalib_runtime::layout;
use datalib_search_index::chunk::{chunk_hash, four_bytes_a_token};
use datalib_search_index::{model_key, ChunkedDoc, Index, IndexDoc, IndexSection, InferenceCache};
use datalib_unified_grid_and_search_index::hits::mapping::Place;
use datalib_unified_grid_and_search_index::hits::vectors::Kept;
use datalib_unified_grid_and_search_index::search_index::HeldVectors;

fn doc(source: &str, uuid: &str, messages: &[(&str, &str)]) -> IndexDoc {
    IndexDoc {
        source_id: source.into(),
        markdown_uuid: uuid.into(),
        content_hash: format!("h-{uuid}"),
        title: uuid.into(),
        head: None,
        sections: messages
            .iter()
            .map(|(m, text)| IndexSection {
                uuid: Some(m.to_string()),
                text: format!("{text}\n"),
                lead: None,
            })
            .collect(),
    }
}

/// Indexes `docs`, cuts each into chunks as its `embed` would, and
/// caches `vector` for each one's single chunk.
async fn write(root: &Path, docs: &[(IndexDoc, [f32; 3])]) {
    let only: Vec<IndexDoc> = docs.iter().map(|(d, _)| d.clone()).collect();
    let cache = index_and_cut(root, &only).await;
    let vectors: Vec<(String, Vec<f32>)> = docs
        .iter()
        .map(|(d, v)| {
            let text: String = d.sections.iter().map(|s| s.text.as_str()).collect();
            (chunk_hash(&d.title, &text), v.to_vec())
        })
        .collect();
    cache.put(&model_key(), &vectors).await.unwrap();
    cache.close().await;
}

/// `docs` in the keyword index and cut into chunks in the cache, which
/// is handed back open.
async fn index_and_cut(root: &Path, docs: &[IndexDoc]) -> InferenceCache {
    let index = Index::open(&layout::grid_index_db(root)).await.unwrap();
    index.put(docs).await.unwrap();
    index.close().await;
    let cache = InferenceCache::open(&layout::inference_cache_db(root))
        .await
        .unwrap();
    let cut: Vec<ChunkedDoc> = docs
        .iter()
        .map(|d| ChunkedDoc::from_prompted(d, &d.prompted(&four_bytes_a_token).unwrap()))
        .collect();
    cache.file_chunks(&cut).await.unwrap();
    cache
}

fn documents(
    hits: &[(
        String,
        datalib_unified_grid_and_search_index::hits::SearchHit,
    )],
) -> Vec<&str> {
    hits.iter().map(|(d, _)| d.as_str()).collect()
}

#[tokio::test]
async fn the_nearest_documents_come_first_of_the_sources_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        &[
            (doc("mail", "near", &[("n1", "warp core")]), [1.0, 0.1, 0.0]),
            (doc("mail", "far", &[("f1", "tea")]), [0.0, 1.0, 0.0]),
            (doc("slack", "chat", &[("c1", "warp")]), [0.9, 0.0, 0.1]),
        ],
    )
    .await;
    let held = HeldVectors::default();
    let query = [2.0, 0.0, 0.0];
    let all = held
        .nearest(root, &query, None, None, 10)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(documents(&all), ["near", "chat", "far"]);
    let (_, top) = &all[0];
    assert!((top.score - 0.995).abs() < 0.001, "cosine: {}", top.score);
    assert_eq!(
        top.place,
        Some(Place::Item {
            uuid: Some("n1".to_string())
        }),
        "a Meaning hit names the first item its chunk covers"
    );

    let mail = ["mail".to_string()];
    let only_mail = held
        .nearest(root, &query, Some(&mail), None, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(documents(&only_mail), ["near"]);
}

/// A search's other terms keep some messages of a document: a chunk that
/// covers none of them does not rank it.
#[tokio::test]
async fn a_chunk_covering_no_kept_message_does_not_rank_its_document() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        &[(
            doc("mail", "thread", &[("m1", "warp"), ("m2", "core")]),
            [1.0, 0.0, 0.0],
        )],
    )
    .await;
    let held = HeldVectors::default();
    let query = [1.0, 0.0, 0.0];
    let keep = |m: &str| {
        HashMap::from([(
            "thread".to_string(),
            Kept::Sections(HashSet::from([m.to_string()])),
        )])
    };
    for (kept, found) in [(keep("m2"), 1), (keep("someone-else"), 0)] {
        let hits = held
            .nearest(root, &query, None, Some(&kept), 10)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hits.len(), found, "{kept:?}");
    }
}

/// A document of two messages, each its own chunk: a search whose other
/// terms keep only the second ranks the document by that chunk, and lands
/// on it, rather than by its nearest chunk overall, which those terms drop.
#[tokio::test]
async fn a_filtered_meaning_search_ranks_by_the_kept_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let thread = IndexDoc {
        source_id: "mail".into(),
        markdown_uuid: "thread".into(),
        content_hash: "h-thread".into(),
        title: "thread".into(),
        head: None,
        sections: ["m1", "m2"]
            .into_iter()
            .map(|m| IndexSection {
                uuid: Some(m.to_string()),
                text: format!("words of {m}\n"),
                lead: Some(format!("**{m}** wrote")),
            })
            .collect(),
    };
    let chunks = thread.prompted(&four_bytes_a_token).unwrap();
    assert_eq!(chunks.len(), 2, "a message starts a chunk of its own");
    let cache = index_and_cut(root, std::slice::from_ref(&thread)).await;
    let vectors = vec![
        (chunks[0].hash.clone(), vec![1.0, 0.0, 0.0]),
        (chunks[1].hash.clone(), vec![0.0, 1.0, 0.0]),
    ];
    cache.put(&model_key(), &vectors).await.unwrap();
    cache.close().await;

    let held = HeldVectors::default();
    let query = [1.0, 0.0, 0.0];
    let top = |kept: Option<HashMap<String, Kept>>| {
        let held = &held;
        async move {
            let hits = held
                .nearest(root, &query, None, kept.as_ref(), 10)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(documents(&hits), ["thread"]);
            hits.into_iter().next().unwrap().1
        }
    };
    let item = |m: &str| {
        Some(Place::Item {
            uuid: Some(m.to_string()),
        })
    };

    let whole = top(Some(HashMap::from([("thread".to_string(), Kept::Whole)]))).await;
    assert_eq!(whole.place, item("m1"));
    assert!((whole.score - 1.0).abs() < 1e-3, "{}", whole.score);

    let second = top(Some(HashMap::from([(
        "thread".to_string(),
        Kept::Sections(HashSet::from(["m2".to_string()])),
    )])))
    .await;
    assert_eq!(second.place, item("m2"));
    assert!(second.score.abs() < 1e-3, "{}", second.score);
}

/// A document the keyword step removes stops being found by meaning,
/// though the cache still holds its chunk's vector.
#[tokio::test]
async fn a_removed_document_is_no_longer_found() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        &[
            (doc("mail", "near", &[("n1", "warp")]), [1.0, 0.0, 0.0]),
            (doc("mail", "far", &[("f1", "tea")]), [0.0, 1.0, 0.0]),
        ],
    )
    .await;
    let held = HeldVectors::default();
    let ask = || held.nearest(root, &[1.0, 0.0, 0.0], None, None, 10);
    assert_eq!(documents(&ask().await.unwrap().unwrap()), ["near", "far"]);

    let index = Index::open(&layout::grid_index_db(root)).await.unwrap();
    index.remove(&["near".to_string()]).await.unwrap();
    index.close().await;
    assert_eq!(documents(&ask().await.unwrap().unwrap()), ["far"]);
}

/// What is held is read again once the files move.
#[tokio::test]
async fn held_vectors_are_read_again_when_the_index_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let held = HeldVectors::default();
    let ask = || held.nearest(root, &[1.0, 0.0, 0.0], None, None, 10);
    assert!(ask().await.unwrap().is_none());
    write(
        root,
        &[(doc("mail", "first", &[("a", "one")]), [1.0, 0.0, 0.0])],
    )
    .await;
    assert_eq!(ask().await.unwrap().unwrap().len(), 1);
    write(
        root,
        &[(doc("mail", "second", &[("b", "two")]), [1.0, 0.0, 0.0])],
    )
    .await;
    assert_eq!(ask().await.unwrap().unwrap().len(), 2);
}

/// The index shares the grid index's file, which the grid writes far more
/// often than the index moves. A write that changes no document of the
/// index reads none again: every document held is the one held before.
#[tokio::test]
async fn a_write_that_changes_no_document_reads_none_again() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        &[(doc("mail", "first", &[("a", "one")]), [1.0, 0.0, 0.0])],
    )
    .await;
    let held = HeldVectors::default();
    let before = held.current(root).await.unwrap().unwrap();

    let grid = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(layout::grid_index_db(root)),
        )
        .await
        .unwrap();
    sqlx::query("CREATE TABLE grid_rows (uuid TEXT)")
        .execute(&grid)
        .await
        .unwrap();
    grid.close().await;
    let after = held.current(root).await.unwrap().unwrap();
    assert!(!Arc::ptr_eq(&before, &after), "the file moved: read again");
    assert!(Arc::ptr_eq(&before.docs["first"], &after.docs["first"]));
}
