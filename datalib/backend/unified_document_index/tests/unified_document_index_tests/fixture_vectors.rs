//! The document vectors the TNG fixture's search index holds, and the
//! joins the embedding map depends on: each vector names a document the
//! grid knows, and a filter on the map matches the documents its terms do.

use std::collections::BTreeSet;
use std::sync::Arc;

use datalib_runtime::layout::{grid_index_db, inference_cache_db};
use datalib_search_index::{model_key, Reader};
use datalib_unified_document_index::dolt_repo::DoltRepo;
use datalib_unified_document_index::query::parse_query;
use datalib_unified_document_index::repo::IndexRepo;

async fn document_markdowns(repo: &DoltRepo) -> BTreeSet<String> {
    repo.document_rows()
        .await
        .expect("document rows")
        .into_iter()
        .map(|r| r.markdown_uuid)
        .collect()
}

#[tokio::test]
async fn every_embedded_document_has_one_unit_vector_and_a_grid_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    datalib_search_fixture::materialize_root(root);

    let reader = Reader::open(&grid_index_db(root), &inference_cache_db(root))
        .await
        .expect("open the search index")
        .expect("the fixture has a search index");
    let model = model_key();
    let held = reader.held(&model).await.expect("held vectors");
    let (documents, embedded) = reader.coverage(&model).await.expect("coverage");
    reader.close().await;

    assert_eq!(held.dim, 768, "embeddinggemma's width");
    let (ids, vectors) = held.document_vectors();
    assert!(!ids.is_empty(), "the fixture embeds its documents");
    assert_eq!(vectors.len(), ids.len() * held.dim);
    let distinct: BTreeSet<&String> = ids.iter().collect();
    assert_eq!(distinct.len(), ids.len(), "a document appears once");
    assert_eq!(embedded, documents, "the fixture embeds everything");
    assert_eq!(ids.len() as u64, embedded);

    for (i, v) in vectors.chunks(held.dim).enumerate() {
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "{}: norm {norm}", ids[i]);
    }

    let repo = DoltRepo::open(Arc::new(root.to_path_buf()))
        .await
        .expect("open the grid");
    let grid = document_markdowns(&repo).await;
    let orphans: Vec<&String> = ids.iter().filter(|id| !grid.contains(*id)).collect();
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
    datalib_search_fixture::copy_grid_index(root);
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
        .matching_documents(&parse_query(&format!("source:{source}")))
        .await
        .expect("match one source");
    assert!(!only.is_empty());
    for d in &docs {
        assert_eq!(
            only.contains(&d.markdown_uuid),
            d.source_id == source,
            "{} under source:{source}",
            d.markdown_uuid
        );
    }
}
