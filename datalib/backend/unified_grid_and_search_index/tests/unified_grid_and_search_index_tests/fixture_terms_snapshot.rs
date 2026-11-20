//! Golden of what every TNG row answers to beyond its columns: each row's
//! search terms (`from:`, `to:`, `with:`, `label:`, `pictured:`, …), one
//! snapshot per source, and the handles' names and the accounts' own
//! handles, which `me` and a name read. `search_finds_itself_tests` shows
//! a search finds what these hold; this pins what they hold, so a row
//! filed under the wrong person, or no longer under the right one, fails
//! here by name.

use std::collections::BTreeMap;
use std::fmt::Write;

use datalib_schema::providers::Provider;
use datalib_schema::search_terms::SearchTermKind;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::fixture_db_snapshot::fixture_db_path;

#[derive(sqlx::FromRow)]
struct TermRow {
    source_id: String,
    uuid: String,
    kind: String,
    term_kind: Option<i64>,
    value: Option<String>,
}

/// The rows of one source, each with its terms, in the order the query
/// read them.
#[derive(Default)]
struct Source {
    rows: Vec<(String, String, Vec<(SearchTermKind, String)>)>,
}

impl Source {
    fn render(&self) -> String {
        let mut out = String::new();
        for (uuid, kind, terms) in &self.rows {
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
    let path = fixture_db_path();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&path).read_only(true))
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));

    // A storage report measures a source and is filed under `datalib`;
    // `fixture_db_snapshot` digests those rows and so does this, by
    // leaving them out.
    let read: Vec<TermRow> = sqlx::query_as(
        "SELECT g.source_id, g.uuid, g.kind, t.kind AS term_kind, v.value \
           FROM grid_rows g \
           LEFT JOIN terms t ON t.row_id = g.rowid \
           LEFT JOIN vals v ON v.val_id = t.val_id \
          WHERE g.provider != ? \
          ORDER BY g.source_id, g.markdown_uuid, g.is_document DESC, g.message_index, g.uuid, \
                   t.kind, v.value",
    )
    .bind(Provider::Datalib.as_str())
    .fetch_all(&pool)
    .await
    .expect("read the terms");

    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    for r in read {
        let source = sources.entry(r.source_id).or_default();
        let new_row = source
            .rows
            .last()
            .is_none_or(|(uuid, _, _)| *uuid != r.uuid);
        if new_row {
            source.rows.push((r.uuid.clone(), r.kind.clone(), Vec::new()));
        }
        if let (Some(code), Some(value)) = (r.term_kind, r.value) {
            let kind = SearchTermKind::from_code(code)
                .unwrap_or_else(|| panic!("a term kind this build does not know: {code}"));
            source.rows.last_mut().unwrap().2.push((kind, value));
        }
    }
    assert!(!sources.is_empty(), "the fixture holds rows");
    for (source_id, source) in &sources {
        insta::assert_snapshot!(format!("fixture_terms__{source_id}"), source.render());
    }

    let names: Vec<(String, String)> =
        sqlx::query_as("SELECT handle, name FROM names ORDER BY handle, name")
            .fetch_all(&pool)
            .await
            .expect("read the names");
    let accounts: Vec<(String, String)> = sqlx::query_as(
        "SELECT source_id, handle FROM account_handles ORDER BY source_id, handle",
    )
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
    insta::assert_snapshot!("fixture_terms_people", people);
    pool.close().await;
}
