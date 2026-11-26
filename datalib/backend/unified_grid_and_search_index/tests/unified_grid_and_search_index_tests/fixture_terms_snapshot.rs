//! Golden of what every TNG row answers to beyond its columns: each row's
//! search terms (`from:`, `to:`, `with:`, `label:`, `pictured:`, …), one
//! snapshot per source, and the handles' names and the accounts' own
//! handles, which `me` and a name read. Over `:ingested_tng`, and over
//! each source synced on its own (`tests/fixtures/tng_source.bzl`). `search_finds_itself_tests` shows
//! a search finds what these hold; this pins what they hold, so a row
//! filed under the wrong person, or no longer under the right one, fails
//! here by name.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use datalib_schema::providers::Provider;
use datalib_schema::search_terms::SearchTermKind;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::fixture_db_snapshot::fixture_db_path;
use datalib_sql::test_sql;

#[derive(sqlx::FromRow)]
struct TermRow {
    source_id: String,
    uuid: String,
    kind: String,
    term_kind: Option<i64>,
    value: Option<String>,
}

struct FiledRow {
    uuid: String,
    kind: String,
    terms: Vec<(SearchTermKind, String)>,
}

/// The rows of one source, each with its terms, in the order the query
/// read them.
#[derive(Default)]
struct Source {
    rows: Vec<FiledRow>,
}

impl Source {
    fn render(&self) -> String {
        let mut out = String::new();
        for FiledRow { uuid, kind, terms } in &self.rows {
            writeln!(out, "{uuid}  {kind}").unwrap();
            for (term_kind, value) in terms {
                // The row's own id is the line above.
                if *term_kind == SearchTermKind::Id && value == uuid {
                    continue;
                }
                writeln!(out, "  {}: {value}", term_kind.as_str()).unwrap();
            }
        }
        out
    }
}

#[tokio::test]
async fn snapshot_search_terms_per_source() {
    snapshot_index(&fixture_db_path(), "fixture_terms_people").await;
}

#[tokio::test]
async fn snapshot_search_terms_of_lightroom() {
    snapshot_index(&lightroom_index_path(), "lightroom_terms_people").await;
}

/// The grid index `//tests/fixtures:ingested_lightroom` built.
pub(crate) fn lightroom_index_path() -> PathBuf {
    let rel = std::env::var("LIGHTROOM_INDEX_RLOC").expect("LIGHTROOM_INDEX_RLOC on the test rule");
    runfiles::Runfiles::create()
        .expect("runfiles")
        .rlocation(&rel)
        .unwrap_or_else(|| panic!("{rel} not in runfiles"))
}

/// One snapshot per source of `path`'s rows and their terms, and one,
/// `people`, of its handles' names and its sources' own handles.
async fn snapshot_index(path: &Path, people_snapshot: &str) {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(SqliteConnectOptions::new().filename(path).read_only(true))
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));

    // A storage report measures a source and is filed under `datalib`;
    // `fixture_db_snapshot` digests those rows and so does this, by
    // leaving them out.
    let read: Vec<TermRow> = test_sql!(
        "SELECT g.source_id, g.uuid, g.kind, t.kind AS term_kind, v.value \
           FROM grid_rows g \
           LEFT JOIN terms t ON t.row_id = g.rowid \
           LEFT JOIN vals v ON v.val_id = t.val_id \
          WHERE g.provider != ? \
          ORDER BY g.source_id, g.markdown_uuid, g.is_document DESC, g.message_index, g.uuid, \
                   t.kind, v.value"
    )
    .query_as()
    .bind(Provider::Datalib.as_str())
    .fetch_all(&pool)
    .await
    .expect("read the terms");

    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    for r in read {
        let source = sources.entry(r.source_id).or_default();
        let new_row = source.rows.last().is_none_or(|row| row.uuid != r.uuid);
        if new_row {
            source.rows.push(FiledRow {
                uuid: r.uuid.clone(),
                kind: r.kind.clone(),
                terms: Vec::new(),
            });
        }
        if let (Some(code), Some(value)) = (r.term_kind, r.value) {
            let kind = SearchTermKind::from_code(code)
                .unwrap_or_else(|| panic!("a term kind this build does not know: {code}"));
            source.rows.last_mut().unwrap().terms.push((kind, value));
        }
    }
    assert!(!sources.is_empty(), "the fixture holds rows");
    for (source_id, source) in &sources {
        insta::assert_snapshot!(format!("fixture_terms__{source_id}"), source.render());
    }

    let names: Vec<(String, String)> =
        test_sql!("SELECT handle, name FROM names ORDER BY handle, name")
            .query_as()
            .fetch_all(&pool)
            .await
            .expect("read the names");
    let accounts: Vec<(String, String)> =
        test_sql!("SELECT source_id, handle FROM account_handles ORDER BY source_id, handle")
            .query_as()
            .fetch_all(&pool)
            .await
            .expect("read the account handles");
    let mut people = String::from("# each handle and the names it was seen under\n");
    for (handle, name) in names {
        writeln!(people, "{handle}\t{name}").unwrap();
    }
    people.push_str("\n# each source's own handles, what `me` finds\n");
    for (source_id, handle) in accounts {
        writeln!(people, "{source_id}\t{handle}").unwrap();
    }
    insta::assert_snapshot!(people_snapshot, people);
    pool.close().await;
}
