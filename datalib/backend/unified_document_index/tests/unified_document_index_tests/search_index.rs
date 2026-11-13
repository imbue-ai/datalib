//! Meaning read from the search index: the chunks' vectors from the
//! inference cache, scored in memory, each document at its nearest chunk.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use datalib_runtime::layout;
use datalib_search_index::chunk::chunk_hash;
use datalib_search_index::{model_key, Index, IndexDoc, IndexSection, InferenceCache};
use datalib_unified_document_index::qmd::mapping::Place;
use datalib_unified_document_index::qmd::vectors::{Freshness, Kept};
use datalib_unified_document_index::search_index::HeldVectors;

fn doc(source: &str, uuid: &str, messages: &[(&str, &str)]) -> IndexDoc {
    IndexDoc {
        source_id: source.into(),
        markdown_uuid: uuid.into(),
        content_hash: format!("h-{uuid}"),
        title: uuid.into(),
        sections: messages
            .iter()
            .map(|(m, text)| IndexSection {
                uuid: Some(m.to_string()),
                text: format!("{text}\n"),
            })
            .collect(),
    }
}

/// Indexes `docs` and caches `vector` for each one's single chunk.
async fn write(root: &Path, docs: &[(IndexDoc, [f32; 3])]) {
    let index = Index::open(&layout::grid_index_db(root)).await.unwrap();
    let only: Vec<IndexDoc> = docs.iter().map(|(d, _)| d.clone()).collect();
    index.put(&only).await.unwrap();
    index.close().await;
    let cache = InferenceCache::open(&layout::inference_cache_db(root))
        .await
        .unwrap();
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

fn documents(hits: &[(String, datalib_unified_document_index::qmd::QmdHit)]) -> Vec<&str> {
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
        .nearest(root, &query, None, None, 10, Freshness::Current)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(documents(&all), ["near", "chat", "far"]);
    let (_, top) = &all[0];
    assert!((top.score - 0.995).abs() < 0.001, "cosine: {}", top.score);
    assert!(matches!(top.place, Some(Place::Bytes { start: 0, .. })));
    assert!(top.path.is_empty(), "a page looks the file up");

    let mail = ["mail".to_string()];
    let only_mail = held
        .nearest(root, &query, Some(&mail), None, 1, Freshness::Current)
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
            .nearest(root, &query, None, Some(&kept), 10, Freshness::Current)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hits.len(), found, "{kept:?}");
    }
}

/// What is held is read again once the files move, unless the search asks
/// for what is held, as one does while a sync writes them.
#[tokio::test]
async fn held_vectors_are_read_again_when_the_index_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let held = HeldVectors::default();
    assert!(held
        .nearest(root, &[1.0, 0.0, 0.0], None, None, 10, Freshness::Current)
        .await
        .unwrap()
        .is_none());
    write(
        root,
        &[(doc("mail", "first", &[("a", "one")]), [1.0, 0.0, 0.0])],
    )
    .await;
    let ask = |fresh| held.nearest(root, &[1.0, 0.0, 0.0], None, None, 10, fresh);
    assert_eq!(ask(Freshness::Current).await.unwrap().unwrap().len(), 1);
    write(
        root,
        &[(doc("mail", "second", &[("b", "two")]), [1.0, 0.0, 0.0])],
    )
    .await;
    assert_eq!(ask(Freshness::AsHeld).await.unwrap().unwrap().len(), 1);
    assert_eq!(ask(Freshness::Current).await.unwrap().unwrap().len(), 2);
}

/// The index shares the grid index's file, which the grid writes far more
/// often than the index moves, and reading every vector again takes
/// seconds on a real root. What is held follows the index's generation:
/// a write to the file that leaves the generation alone, as the grid's
/// do, is not read.
#[tokio::test]
async fn held_vectors_follow_the_index_not_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        &[(doc("mail", "first", &[("a", "one")]), [1.0, 0.0, 0.0])],
    )
    .await;
    let held = HeldVectors::default();
    let ask = || held.nearest(root, &[1.0, 0.0, 0.0], None, None, 10, Freshness::Current);
    assert_eq!(ask().await.unwrap().unwrap().len(), 1);

    let grid = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(layout::grid_index_db(root)),
        )
        .await
        .unwrap();
    for sql in [
        "CREATE TABLE grid_rows (uuid TEXT)",
        "DELETE FROM search_chunks",
    ] {
        sqlx::query(sql).execute(&grid).await.unwrap();
    }
    grid.close().await;
    assert_eq!(
        ask().await.unwrap().unwrap().len(),
        1,
        "read again though the generation did not move"
    );
}
