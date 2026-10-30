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

/// A filtered Meaning search ranks a document only by the chunks over the
/// messages its filters keep (`from:worf` by what Worf wrote), so its hit
/// is one of those chunks rather than the document's nearest overall,
/// which the filter would then drop along with the document.
#[tokio::test]
async fn a_filtered_meaning_search_ranks_by_the_kept_messages() {
    use std::collections::{HashMap, HashSet};

    use datalib_unified_document_index::qmd::mapping::{
        parse_section_uuid, snippet_match_line, Place,
    };
    use datalib_unified_document_index::qmd::vectors::{
        chunk_lines, chunk_snippet, Kept, QueryVector,
    };
    use datalib_unified_document_index::qmd::{qmd_index_path, HeldIndex, QmdHit};
    use sqlx::{ConnectOptions, Connection};

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    materialize_root_with_grid(root);
    let vectors = read_document_vectors(root).await.unwrap().unwrap();

    let mut qmd = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(qmd_index_path(root))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let model: String = sqlx::query_scalar("SELECT model FROM content_vectors LIMIT 1")
        .fetch_one(&mut qmd)
        .await
        .unwrap();
    let chunked: Vec<(String, String)> = sqlx::query_as(
        "SELECT d.path, c.doc FROM documents d JOIN content c ON c.hash = d.hash \
          WHERE d.active = 1 \
            AND (SELECT count(*) FROM content_vectors v WHERE v.hash = d.hash) >= 3",
    )
    .fetch_all(&mut qmd)
    .await
    .unwrap();
    qmd.close().await.unwrap();

    let held = HeldIndex::default();
    let nearest = |document: String, kept: Kept, vector: Vec<f32>| {
        let held = &held;
        let model = &model;
        async move {
            let open = held.open(root).await.unwrap().unwrap();
            let among: HashMap<String, Kept> = [(document, kept)].into();
            let query = QueryVector {
                model,
                vector: &vector,
            };
            let hits: Vec<QmdHit> = open
                .reader
                .nearest_documents(&open.vectors, &query, None, Some(&among), 5)
                .await
                .unwrap();
            hits
        }
    };

    let mut checked = 0;
    for (path, body) in &chunked {
        let Some(at) = vectors.paths.iter().position(|p| p == path) else {
            continue;
        };
        let document = markdown_uuid_of_path(path).unwrap().to_string();
        let vector = vectors.vectors[at * vectors.dim..(at + 1) * vectors.dim].to_vec();
        // Each message's lines, 1-based, to where the next one starts.
        let lines: Vec<&str> = body.lines().collect();
        let starts: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| Some((i + 1, parse_section_uuid(l)?)))
            .collect();
        let span_of = |i: usize| {
            (
                starts[i].0,
                starts.get(i + 1).map_or(lines.len() + 1, |s| s.0),
            )
        };
        // The lines a hit's chunk spans.
        let chunk_of = |hit: &QmdHit| match hit.place {
            Some(Place::Chunk { pos, next }) => {
                let from = snippet_match_line(&chunk_snippet(body, pos)).unwrap();
                (from, from + chunk_lines(body, pos, next))
            }
            ref other => panic!("{path}: a Meaning hit placed at {other:?}"),
        };

        let whole = nearest(document.clone(), Kept::Whole, vector.clone()).await;
        let (from, to) = chunk_of(&whole[0]);
        // A message the document's nearest chunk does not reach.
        let Some(other) = (0..starts.len()).find(|&i| {
            let (a, b) = span_of(i);
            b <= from || to <= a
        }) else {
            continue;
        };
        let (a, b) = span_of(other);
        let kept = Kept::Sections(HashSet::from([starts[other].1.to_string()]));
        let narrowed = nearest(document.clone(), kept, vector).await;
        let hit = narrowed.first().unwrap_or_else(|| panic!("{path}: no hit"));
        let (from, to) = chunk_of(hit);
        assert!(
            from < b && a < to,
            "{path}: the hit at lines {from}..{to} misses the kept message at {a}..{b}"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "no fixture document has a message its nearest chunk misses"
    );
}
