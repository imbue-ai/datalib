//! What the search index needs from SQLite's FTS5, measured on the engine we
//! link (doltlite, writing a plain SQLite file): a keyword index that keeps
//! no copy of the text, still ranks with `bm25()`, and deletes a row by its
//! rowid. Where a fact holds only because of an option, a control table
//! without the option shows the check would fail.

use datalib_sql::{test_sql, test_sql_id, Sql};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::{Connection, Row};

const SENTENCE: &str = "the quarterly budget review moved to thursday afternoon";

const CONTENTLESS: &str = "CREATE VIRTUAL TABLE t USING fts5(title, body, \
     content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 2')";

async fn open(dir: &tempfile::TempDir) -> (SqliteConnection, std::path::PathBuf) {
    let path = dir.path().join("index.sqlite");
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(&path))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete);
    (SqliteConnection::connect_with(&opts).await.unwrap(), path)
}

async fn exec(c: &mut SqliteConnection, sql: &str) -> Result<(), String> {
    Sql::dynamic(test_sql_id!(), sql)
        .query()
        .execute(c)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

async fn ok(c: &mut SqliteConnection, sql: &str) {
    exec(c, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

async fn insert(c: &mut SqliteConnection, table: &str, rowid: i64, title: &str, body: &str) {
    Sql::dynamic(
        test_sql_id!(),
        format!("INSERT INTO {table}(rowid, title, body) VALUES (?, ?, ?)"),
    )
    .query()
    .bind(rowid)
    .bind(title)
    .bind(body)
    .execute(c)
    .await
    .unwrap();
}

async fn matching(c: &mut SqliteConnection, table: &str, query: &str) -> Vec<i64> {
    Sql::dynamic(
        test_sql_id!(),
        format!("SELECT rowid FROM {table} WHERE {table} MATCH ? ORDER BY rowid"),
    )
    .query_scalar()
    .bind(query)
    .fetch_all(c)
    .await
    .unwrap()
}

#[tokio::test]
async fn the_engine_is_new_enough_for_contentless_delete() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, _) = open(&dir).await;
    let version: String = test_sql!("SELECT sqlite_version()")
        .query_scalar()
        .fetch_one(&mut c)
        .await
        .unwrap();
    let parts: Vec<u32> = version.split('.').map(|p| p.parse().unwrap()).collect();
    assert!(
        (parts[0], parts[1]) >= (3, 43),
        "contentless_delete needs SQLite 3.43; the linked engine is {version}"
    );
}

#[tokio::test]
async fn the_index_file_is_plain_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, path) = open(&dir).await;
    ok(&mut c, CONTENTLESS).await;
    insert(&mut c, "t", 1, "", SENTENCE).await;
    c.close().await.unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..16], b"SQLite format 3\0");
}

/// The point of the index: a contentless table holds the words' postings
/// and not the text. A table with content, in the same file, does hold it,
/// which is what the byte search would find if the option were dropped.
#[tokio::test]
async fn a_contentless_table_keeps_no_copy_of_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, path) = open(&dir).await;
    ok(&mut c, CONTENTLESS).await;
    insert(&mut c, "t", 1, "budget", SENTENCE).await;

    let body: Option<String> = test_sql!("SELECT body FROM t WHERE rowid = 1")
        .query_scalar()
        .fetch_one(&mut c)
        .await
        .unwrap();
    assert_eq!(body, None, "a contentless column reads back NULL");
    assert_eq!(matching(&mut c, "t", "thursday").await, vec![1]);
    c.close().await.unwrap();
    let holds = |bytes: &[u8]| {
        bytes
            .windows(SENTENCE.len())
            .any(|w| w == SENTENCE.as_bytes())
    };
    assert!(!holds(&std::fs::read(&path).unwrap()));

    let (mut c, path) = open(&dir).await;
    ok(
        &mut c,
        "CREATE VIRTUAL TABLE with_content USING fts5(title, body, tokenize='porter unicode61')",
    )
    .await;
    insert(&mut c, "with_content", 1, "budget", SENTENCE).await;
    c.close().await.unwrap();
    assert!(holds(&std::fs::read(&path).unwrap()));
}

/// Ranking reads only the postings and the column sizes, so it survives the
/// text going: a short row that says the word twice beats a long one that
/// says it once, and a column weight moves a title match up.
#[tokio::test]
async fn bm25_ranks_a_contentless_table() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, _) = open(&dir).await;
    ok(&mut c, CONTENTLESS).await;
    insert(&mut c, "t", 1, "", "budget budget").await;
    insert(&mut c, "t", 2, "", SENTENCE).await;
    insert(&mut c, "t", 3, "", "lunch on friday").await;
    insert(&mut c, "t", 4, "budget", "nothing about it in the body").await;

    let ranked = |title_weight: f64| {
        let sql = Sql::dynamic(
            test_sql_id!(),
            format!(
                "SELECT rowid, bm25(t, {title_weight}, 1.0) AS s FROM t \
              WHERE t MATCH 'budget' ORDER BY s"
            ),
        );
        sql.query()
    };
    let rows = ranked(1.0).fetch_all(&mut c).await.unwrap();
    let order: Vec<i64> = rows.iter().map(|r| r.get("rowid")).collect();
    assert_eq!(order[0], 1, "the short row saying it twice ranks first");
    assert!(!order.contains(&3));
    assert!(rows.iter().all(|r| r.get::<f64, _>("s") < 0.0));

    let weighted = ranked(10.0).fetch_all(&mut c).await.unwrap();
    assert_eq!(weighted[0].get::<i64, _>("rowid"), 4);
}

/// `contentless_delete=1` is what lets a changed or retired message leave
/// the index by rowid; without it a contentless table refuses the delete.
#[tokio::test]
async fn a_contentless_row_deletes_by_rowid_only_with_the_option() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, _) = open(&dir).await;
    ok(&mut c, CONTENTLESS).await;
    insert(&mut c, "t", 1, "", SENTENCE).await;
    insert(&mut c, "t", 2, "", "the budget again").await;
    ok(&mut c, "DELETE FROM t WHERE rowid = 1").await;
    assert_eq!(matching(&mut c, "t", "budget").await, vec![2]);
    assert_eq!(matching(&mut c, "t", "thursday").await, Vec::<i64>::new());

    ok(
        &mut c,
        "CREATE VIRTUAL TABLE no_delete USING fts5(title, body, content='')",
    )
    .await;
    insert(&mut c, "no_delete", 1, "", SENTENCE).await;
    assert!(exec(&mut c, "DELETE FROM no_delete WHERE rowid = 1")
        .await
        .is_err());
}

/// Positions are kept, so a quoted phrase matches only words side by side.
#[tokio::test]
async fn a_phrase_matches_only_adjacent_words() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, _) = open(&dir).await;
    ok(&mut c, CONTENTLESS).await;
    insert(&mut c, "t", 1, "", SENTENCE).await;
    insert(&mut c, "t", 2, "", "a review of the quarterly budget").await;
    assert_eq!(matching(&mut c, "t", "\"budget review\"").await, vec![1]);
    assert_eq!(matching(&mut c, "t", "budget review").await, vec![1, 2]);
}

/// The index matches a word as written, so a quoted phrase is exact: another
/// form of the word is not it, while case and accents fold. A stemming
/// table, the control, matches the other form.
#[tokio::test]
async fn a_word_matches_as_written_with_case_and_accents_folded() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, _) = open(&dir).await;
    ok(&mut c, CONTENTLESS).await;
    insert(&mut c, "t", 1, "", "two meetings were moved to the Café").await;
    assert_eq!(matching(&mut c, "t", "meeting").await, Vec::<i64>::new());
    assert_eq!(matching(&mut c, "t", "moving").await, Vec::<i64>::new());
    assert_eq!(matching(&mut c, "t", "\"MEETINGS were\"").await, vec![1]);
    assert_eq!(matching(&mut c, "t", "cafe").await, vec![1]);

    ok(
        &mut c,
        "CREATE VIRTUAL TABLE stemmed USING fts5(title, body, tokenize='porter unicode61')",
    )
    .await;
    insert(&mut c, "stemmed", 1, "", "two meetings were moved").await;
    assert_eq!(matching(&mut c, "stemmed", "meeting").await, vec![1]);
}
