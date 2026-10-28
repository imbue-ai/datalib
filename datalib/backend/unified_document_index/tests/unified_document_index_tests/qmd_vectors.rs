//! The document vectors read out of the real qmd index the TNG fixture
//! builds, and the join the embedding map depends on: each one's path
//! names a document the grid knows.

use std::collections::BTreeSet;
use std::sync::Arc;

use datalib_schema::grid_rows::markdown_uuid_of_path;
use datalib_unified_document_index::dolt_repo::DoltRepo;
use datalib_unified_document_index::qmd::vectors::read_document_vectors;
use datalib_unified_document_index::qmd::QmdIndexReader;
use datalib_unified_document_index::query::parse_query;
use datalib_unified_document_index::repo::IndexRepo;

use datalib_qmd_fixture::materialize_root_with_grid;

#[tokio::test]
async fn every_embedded_document_has_one_unit_vector_and_a_grid_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root_with_grid(root);

    let got = read_document_vectors(root)
        .await
        .expect("read vectors")
        .expect("the fixture has a qmd index");
    assert_eq!(got.dim, 768, "embeddinggemma's width");
    assert_eq!(got.vectors.len(), got.paths.len() * got.dim);

    // The same count the index's own bookkeeping gives: every document
    // with a complete vector set, and none without.
    let summary = QmdIndexReader::open(root)
        .await
        .expect("open")
        .expect("index")
        .summary()
        .await
        .expect("summary");
    let distinct: BTreeSet<&String> = got.paths.iter().collect();
    assert_eq!(distinct.len(), got.paths.len(), "a path appears once");
    assert!(got.paths.len() as u64 >= summary.embedded, "{summary:?}");
    assert_eq!(got.unembedded, 0, "the fixture embeds everything");

    for (i, v) in got.vectors.chunks(got.dim).enumerate() {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "{}: norm {norm}", got.paths[i]);
    }

    let repo = DoltRepo::open(Arc::new(root.to_path_buf()))
        .await
        .expect("open the grid");
    let documents: BTreeSet<String> = repo
        .document_rows()
        .await
        .expect("document rows")
        .into_iter()
        .map(|r| r.markdown_uuid)
        .collect();
    let orphans: Vec<&String> = got
        .paths
        .iter()
        .filter(|p| markdown_uuid_of_path(p).is_none_or(|id| !documents.contains(id)))
        .collect();
    assert!(
        orphans.is_empty(),
        "vectors with no document row: {orphans:?}"
    );
}

/// A filter on the map is answered by `matching_documents`: the
/// documents behind every row the structured terms match, and nothing
/// else.
#[tokio::test]
async fn matching_documents_follows_the_structured_terms() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root_with_grid(root);
    let repo = DoltRepo::open(Arc::new(root.to_path_buf()))
        .await
        .expect("open the grid");
    let docs = repo.document_rows().await.expect("document rows");
    let source = docs
        .iter()
        .find(|d| d.source_id != "datalib")
        .expect("a document from a source")
        .source_id
        .clone();

    let all = repo
        .matching_documents(&parse_query(""))
        .await
        .expect("match all");
    for d in &docs {
        assert!(
            all.contains(&d.markdown_uuid),
            "{} unmatched",
            d.markdown_uuid
        );
    }

    let only = repo
        .matching_documents(&parse_query(&format!("source_id:{source}")))
        .await
        .expect("match one source");
    assert!(!only.is_empty());
    for d in &docs {
        assert_eq!(
            only.contains(&d.markdown_uuid),
            d.source_id == source,
            "{} under source_id:{source}",
            d.markdown_uuid
        );
    }
}

/// A held index answers Meaning from what it read before, until another
/// connection commits: then it reads the documents again, so one the
/// keyword step made inactive stops being found.
#[tokio::test]
async fn a_held_index_rereads_the_documents_after_another_connection_commits() {
    use datalib_unified_document_index::qmd::vectors::QueryVector;
    use datalib_unified_document_index::qmd::{qmd_index_path, HeldIndex};
    use sqlx::{ConnectOptions, Connection};

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root_with_grid(root);
    let vectors = read_document_vectors(root).await.unwrap().unwrap();
    let (path, vector) = (&vectors.paths[0], &vectors.vectors[..vectors.dim]);

    let mut writer = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(qmd_index_path(root))
        .connect()
        .await
        .unwrap();
    let model: String = sqlx::query_scalar("SELECT model FROM content_vectors LIMIT 1")
        .fetch_one(&mut writer)
        .await
        .unwrap();
    let held = HeldIndex::default();
    let open = held.open(root).await.unwrap().unwrap();
    let nearest = || async {
        let open = held.open(root).await.unwrap().unwrap();
        let query = QueryVector {
            model: &model,
            vector,
        };
        open.reader
            .nearest_documents(&open.vectors, &query, None, None, 5)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.path)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        nearest().await.first(),
        Some(path),
        "a document is nearest itself"
    );
    assert!(
        Arc::ptr_eq(&open, &held.open(root).await.unwrap().unwrap()),
        "the index is held, not opened again"
    );

    sqlx::query("UPDATE documents SET active = 0 WHERE path = ?")
        .bind(path)
        .execute(&mut writer)
        .await
        .unwrap();
    writer.close().await.unwrap();

    assert!(
        !nearest().await.contains(path),
        "an inactive document is not found"
    );
}
