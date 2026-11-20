use super::*;
use crate::{IndexDoc, IndexSection};
use sqlx::AssertSqlSafe;

const SENTENCE: &str = "the quarterly budget review moved to thursday afternoon";

fn doc(uuid: &str, hash: &str, title: &str, sections: &[(Option<&str>, &str)]) -> IndexDoc {
    IndexDoc {
        source_id: "mail".into(),
        markdown_uuid: uuid.into(),
        content_hash: hash.into(),
        title: title.into(),
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

fn thread(hash: &str) -> IndexDoc {
    doc(
        "d1",
        hash,
        "Planning",
        &[
            (None, "# Planning\n"),
            (Some("m1"), "lunch on friday\n"),
            (Some("m2"), &format!("{SENTENCE}\n")),
        ],
    )
}

async fn open() -> (tempfile::TempDir, Index) {
    let dir = tempfile::tempdir().unwrap();
    let index = Index::open(&dir.path().join("search_index/index.sqlite"))
        .await
        .unwrap();
    (dir, index)
}

/// Each matching section as (document, message), in document order.
async fn hits(index: &Index, query: &str) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT d.markdown_uuid, s.uuid FROM search_fts f \
           JOIN search_sections s ON s.section_id = f.rowid \
           JOIN search_documents d ON d.doc_id = s.doc_id \
          WHERE search_fts MATCH ? ORDER BY d.markdown_uuid, s.seq",
    )
    .bind(query)
    .fetch_all(index.pool())
    .await
    .unwrap()
}

/// Rows of the keyword index itself that match, whether or not a section
/// still names them: a posting left behind would be found again by a
/// section that reuses its rowid.
async fn postings(index: &Index, query: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM search_fts WHERE search_fts MATCH ?")
        .bind(query)
        .fetch_one(index.pool())
        .await
        .unwrap()
}

async fn count(index: &Index, table: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(index.pool())
        .await
        .unwrap()
}

fn hit(d: &str, m: Option<&str>) -> (String, Option<String>) {
    (d.to_string(), m.map(str::to_string))
}

#[tokio::test]
async fn a_word_lands_on_the_message_that_says_it() {
    let (_dir, index) = open().await;
    index.put(&[thread("h1")]).await.unwrap();
    assert_eq!(hits(&index, "thursday").await, [hit("d1", Some("m2"))]);
    assert_eq!(hits(&index, "lunch").await, [hit("d1", Some("m1"))]);
    assert_eq!(
        hits(&index, "title : planning").await,
        [hit("d1", None)],
        "the title is indexed once, on the first section"
    );
}

#[tokio::test]
async fn sections_are_byte_spans_of_the_document() {
    let (_dir, index) = open().await;
    index.put(&[thread("h1")]).await.unwrap();
    let sections: Vec<(Option<String>, i64, i64)> =
        sqlx::query_as("SELECT uuid, start_byte, end_byte FROM search_sections ORDER BY seq")
            .fetch_all(index.pool())
            .await
            .unwrap();
    let m2_end = 11 + 16 + SENTENCE.len() as i64 + 1;
    assert_eq!(
        sections,
        [
            (None, 0, 11),
            (Some("m1".into()), 11, 27),
            (Some("m2".into()), 27, m2_end)
        ]
    );
}

#[tokio::test]
async fn a_document_with_no_text_is_found_by_its_title() {
    let (_dir, index) = open().await;
    index
        .put(&[doc("d2", "h1", "Offsite agenda", &[])])
        .await
        .unwrap();
    assert_eq!(hits(&index, "offsite").await, [hit("d2", None)]);
}

/// A changed document drops its old words and sections.
#[tokio::test]
async fn a_changed_hash_replaces_a_document() {
    let (_dir, index) = open().await;
    index.put(&[thread("h1")]).await.unwrap();
    let changed = doc(
        "d1",
        "h2",
        "Planning",
        &[(Some("m3"), "dinner on saturday")],
    );
    index.put(&[changed]).await.unwrap();
    assert_eq!(count(&index, "search_sections").await, 1);
    assert_eq!(postings(&index, "thursday").await, 0);
    assert_eq!(hits(&index, "dinner").await, [hit("d1", Some("m3"))]);
    assert_eq!(
        index.indexed("mail").await.unwrap(),
        BTreeMap::from([("d1".to_string(), "h2".to_string())])
    );
}

#[tokio::test]
async fn a_removed_document_leaves_nothing_behind() {
    let (_dir, index) = open().await;
    let other = doc("d2", "h1", "Other", &[(Some("m9"), "budget again")]);
    index.put(&[thread("h1"), other]).await.unwrap();
    index.remove(&["d1".to_string()]).await.unwrap();
    assert_eq!(hits(&index, "budget").await, [hit("d2", Some("m9"))]);
    assert_eq!(postings(&index, "lunch").await, 0);
    assert_eq!(count(&index, "search_documents").await, 1);
    assert_eq!(count(&index, "search_sections").await, 1);
}

#[tokio::test]
async fn indexed_names_only_its_own_source() {
    let (_dir, index) = open().await;
    let mut chat = doc("c1", "h9", "", &[(Some("x"), "hello")]);
    chat.source_id = "chat".into();
    index.put(&[thread("h1"), chat]).await.unwrap();
    assert_eq!(
        index.indexed("chat").await.unwrap(),
        BTreeMap::from([("c1".to_string(), "h9".to_string())])
    );
}

/// The point of the index: the words are searchable and the text is not
/// in any of its files.
#[tokio::test]
async fn the_files_hold_no_copy_of_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.sqlite");
    let index = Index::open(&path).await.unwrap();
    index.put(&[thread("h1")]).await.unwrap();
    assert_eq!(hits(&index, "afternoon").await.len(), 1);
    index.close().await;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(!bytes
            .windows(SENTENCE.len())
            .any(|w| w == SENTENCE.as_bytes()));
    }
}

/// The index shares the grid index's file: a change of the index's shape
/// empties its own tables and leaves every other table in the file as it
/// was, and the file's own shape and kind are the grid's to write.
#[tokio::test]
async fn an_index_in_another_shape_empties_only_its_own_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db.sqlite");
    let index = Index::open(&path).await.unwrap();
    index.put(&[thread("h1")]).await.unwrap();
    for sql in [
        "CREATE TABLE grid_rows (uuid TEXT)",
        "INSERT INTO grid_rows VALUES ('m1')",
    ] {
        sqlx::query(sql).execute(index.pool()).await.unwrap();
    }
    index.close().await;

    let index = Index::open(&path).await.unwrap();
    assert_eq!(index.indexed("mail").await.unwrap().len(), 1);
    sqlx::query(
        "UPDATE _datalib_meta SET value = 'an older shape' \
          WHERE key = 'search_index_schema_hash'",
    )
    .execute(index.pool())
    .await
    .unwrap();
    index.close().await;

    let index = Index::open(&path).await.unwrap();
    assert!(index.indexed("mail").await.unwrap().is_empty());
    assert!(hits(&index, "lunch").await.is_empty());
    assert_eq!(count(&index, "grid_rows").await, 1);
    let mut conn = index.pool().acquire().await.unwrap();
    let recorded = datalib_store_meta::part_schema_hash_in(
        &mut conn,
        datalib_store_meta::StoreKind::SearchIndex,
    )
    .await
    .unwrap();
    assert_eq!(recorded, Some(shape()));
    drop(conn);
    let file = datalib_store_meta::read(index.pool()).await.unwrap();
    assert_eq!(
        file, None,
        "the file's own shape is the grid index's to write"
    );
}

/// A write that changes what the index holds raises its generation; a put
/// of what it already holds does not, so a reader keeps what it read.
#[tokio::test]
async fn only_a_change_raises_the_generation() {
    let (_dir, index) = open().await;
    let generation = || async {
        sqlx::query_scalar::<_, i64>("SELECT generation FROM search_state")
            .fetch_one(index.pool())
            .await
            .unwrap()
    };
    let start = generation().await;
    index.put(&[thread("h1")]).await.unwrap();
    let put = generation().await;
    assert!(put > start);
    index.put(&[thread("h1")]).await.unwrap();
    assert_eq!(generation().await, put);
    index.remove(&["d1".to_string()]).await.unwrap();
    assert!(generation().await > put);
}

/// A source taken out of the config leaves the index.
#[tokio::test]
async fn a_source_no_longer_configured_leaves() {
    let (_dir, index) = open().await;
    let mut chat = doc("c1", "h9", "Chat", &[(Some("x"), "hello there")]);
    chat.source_id = "chat".into();
    index.put(&[thread("h1"), chat]).await.unwrap();
    assert_eq!(index.keep_sources(&["mail".to_string()]).await.unwrap(), 1);
    assert!(index.indexed("chat").await.unwrap().is_empty());
    assert_eq!(index.indexed("mail").await.unwrap().len(), 1);
}

/// A large put commits in parts, each about `PUT_BYTES` of text, so the
/// file's other writers wait between them rather than for the whole batch;
/// a document larger than that is still one part, never split.
#[tokio::test]
async fn a_large_put_commits_in_parts() {
    let (_dir, index) = open().await;
    let generation = || async {
        sqlx::query_scalar::<_, i64>("SELECT generation FROM search_state WHERE id = 1")
            .fetch_one(&index.pool)
            .await
            .unwrap()
    };
    let before = generation().await;
    let big = "warp ".repeat(PUT_BYTES / 5 * 3 / 5);
    let docs: Vec<IndexDoc> = (0..3)
        .map(|i| doc(&format!("d{i}"), "h", "T", &[(Some("m"), big.as_str())]))
        .collect();
    index.put(&docs).await.unwrap();
    assert_eq!(generation().await - before, 3, "one commit per part");
    assert_eq!(count(&index, "search_documents").await, 3);
}

/// The search index writes the grid index's file too, so its connection
/// cuts the `-wal` back as the grid's does.
#[tokio::test]
async fn the_search_index_cuts_the_files_wal_back() {
    let (_dir, index) = open().await;
    let limit: i64 = sqlx::query_scalar("PRAGMA journal_size_limit")
        .fetch_one(index.pool())
        .await
        .unwrap();
    assert_eq!(
        limit.to_string(),
        datalib_runtime::plain_sqlite::WAL_SIZE_LIMIT_BYTES
    );
    index.close().await;
}

/// What the index holds of each source is read by the source, once per
/// source on every pass. Asked as `?1 IS NULL OR source_id = ?1`, each
/// source's read walked every document of every source.
#[tokio::test]
async fn a_sources_documents_are_read_by_its_index() {
    let (_dir, index) = open().await;
    // Audited: a constant statement of this crate.
    let explain = format!(
        "EXPLAIN QUERY PLAN {}",
        crate::read::held_documents_sql(Some("mail"))
    );
    let plan: Vec<String> = sqlx::query_as::<_, (i64, i64, i64, String)>(AssertSqlSafe(explain))
        .bind("mail")
        .fetch_all(index.pool())
        .await
        .unwrap()
        .into_iter()
        .map(|(_, _, _, detail)| detail)
        .collect();
    assert!(
        plan.iter().any(|d| d.starts_with(
            "SEARCH search_documents USING INDEX search_documents_by_source (source_id=?)"
        )),
        "{plan:?}"
    );
    index.close().await;
}
