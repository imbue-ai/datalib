//! Every row of the TNG fixture, searched for by what it holds, is found:
//! by each search key's value (exactly the rows holding that value), by
//! each key that reads the search terms (`from:`, `to:`, `with:`, …), by
//! each of its search terms on the Fields tab, and each document by its
//! rarest word on the Words tab and, nearly always, by its opening words
//! on the Meaning tab. Which source each search reached is a snapshot, so
//! a fixture change that leaves a source unchecked by a search shows in
//! review. A new key or term kind is checked with no change here.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use datalib_query::table::SearchTable;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::search_terms::SearchTermKind;
use datalib_unified_index::terms_keys::{self, TermsValue, TERMS_KEYS};
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection, SqliteConnection};

use super::qmd_search_tests::fixture_root;
use super::tabs::SearchTab;
use super::tests::{index_over, search_tab};
use super::*;

/// Which sources each search was checked on, and with how many rows.
#[derive(Default)]
struct Coverage(BTreeMap<String, BTreeMap<String, usize>>);

impl Coverage {
    fn checked(&mut self, search: &str, source: &str) {
        *self
            .0
            .entry(search.to_string())
            .or_default()
            .entry(source.to_string())
            .or_default() += 1;
    }

    /// One line per search: the sources it reached, then the ones it never
    /// did.
    fn render(&self, sources: &BTreeSet<String>) -> String {
        let mut out = String::new();
        for (search, by_source) in &self.0 {
            let reached: Vec<String> = by_source
                .iter()
                .map(|(source, n)| format!("{source} {n}"))
                .collect();
            let missed: Vec<&str> = sources
                .iter()
                .filter(|s| !by_source.contains_key(*s))
                .map(String::as_str)
                .collect();
            out.push_str(&format!("{search}\n  checked: {}\n", reached.join(", ")));
            if !missed.is_empty() {
                out.push_str(&format!("  never: {}\n", missed.join(", ")));
            }
        }
        out
    }
}

/// Every row a search holds, reading every page.
async fn every_uuid(s: &Index, q: &str, tab: Option<SearchTab>) -> Vec<SearchRow> {
    let mut rows = Vec::new();
    let mut offset = None;
    loop {
        let page = search_tab(s, q, tab, offset, results::MAX_PAGE, None).await;
        assert!(page.errors.is_empty(), "{q}: {:?}", page.errors);
        assert!(page.refused.is_empty(), "{q}: {:?}", page.refused);
        rows.extend(page.rows);
        match page.next_offset {
            Some(next) => offset = Some(next),
            None => return rows,
        }
    }
}

async fn open_read_only(path: &std::path::Path) -> SqliteConnection {
    SqliteConnectOptions::new()
        .filename(path)
        .read_only(true)
        .connect()
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
}

/// A grid row's source and document.
struct Row {
    source_id: String,
    markdown_uuid: Option<String>,
}

/// Every grid row, and each document's text: its rendered file, which is
/// what qmd indexed.
async fn read_rows(root: &std::path::Path) -> (HashMap<String, Row>, HashMap<String, String>) {
    let mut grid = open_read_only(&datalib_runtime::layout::grid_index_db(root)).await;
    #[derive(sqlx::FromRow)]
    struct Read {
        uuid: String,
        source_id: String,
        markdown_uuid: Option<String>,
        is_document: bool,
        qmd_path: Option<String>,
    }
    let read: Vec<Read> = sqlx::query_as(
        "SELECT uuid, source_id, markdown_uuid, is_document, qmd_path FROM grid_rows",
    )
    .fetch_all(&mut grid)
    .await
    .unwrap();
    grid.close().await.unwrap();
    let mut rows = HashMap::new();
    let mut texts: HashMap<String, String> = HashMap::new();
    for r in read {
        if let (true, Some(md), Some(path)) = (r.is_document, &r.markdown_uuid, &r.qmd_path) {
            let file = root.join(path);
            let text = std::fs::read_to_string(&file)
                .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
            texts.insert(md.clone(), text);
        }
        rows.insert(
            r.uuid,
            Row {
                source_id: r.source_id,
                markdown_uuid: r.markdown_uuid,
            },
        );
    }
    (rows, texts)
}

/// Each value a key's column holds, with the rows holding it.
async fn values_of(root: &std::path::Path, column: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut grid = open_read_only(&datalib_runtime::layout::grid_index_db(root)).await;
    // Audited: `column` is a `GridRowColumn`'s name.
    let pairs: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT uuid, CAST({column} AS TEXT) FROM grid_rows \
         WHERE {column} IS NOT NULL AND CAST({column} AS TEXT) != ''"
    )))
    .fetch_all(&mut grid)
    .await
    .unwrap();
    grid.close().await.unwrap();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (uuid, value) in pairs {
        out.entry(value).or_default().insert(uuid);
    }
    out
}

/// Each `(kind, value)` in the search terms file, with the rows holding it.
async fn terms_of(root: &std::path::Path) -> BTreeMap<(String, String), BTreeSet<String>> {
    let path = datalib_runtime::layout::search_terms_db(root);
    let mut terms = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(&path))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let read: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT r.uuid, t.kind, v.value FROM terms t \
         JOIN vals v ON v.val_id = t.val_id JOIN rows r ON r.row_id = t.row_id",
    )
    .fetch_all(&mut terms)
    .await
    .unwrap();
    terms.close().await.unwrap();
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (uuid, code, value) in read {
        let kind = SearchTermKind::from_code(code).expect("a kind this build knows");
        out.entry((kind.as_str().to_string(), value))
            .or_default()
            .insert(uuid);
    }
    out
}

/// Each handle with the names it was seen under, from the search terms
/// file.
async fn names_of(root: &std::path::Path) -> HashMap<String, Vec<String>> {
    let path = datalib_runtime::layout::search_terms_db(root);
    let mut terms = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(&path))
        .read_only(true)
        .connect()
        .await
        .unwrap();
    let read: Vec<(String, String)> = sqlx::query_as("SELECT handle, name FROM names")
        .fetch_all(&mut terms)
        .await
        .unwrap();
    terms.close().await.unwrap();
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (handle, name) in read {
        out.entry(handle).or_default().push(name);
    }
    out
}

/// The words a document's text holds, cut where qmd's tokenizer cuts
/// them (at anything not a letter or digit), keeping the ones of four or
/// more letters and nothing else, lowercased.
fn words(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4 && w.chars().all(|c| c.is_ascii_alphabetic()))
        .map(str::to_lowercase)
        .collect()
}

/// For each document, the word of its text the fewest documents hold
/// (ties to the first alphabetically): the one most likely to find it.
fn rarest_words(texts: &HashMap<String, String>) -> BTreeMap<String, String> {
    let by_doc: BTreeMap<&String, BTreeSet<String>> =
        texts.iter().map(|(md, text)| (md, words(text))).collect();
    let mut documents_with: HashMap<&str, usize> = HashMap::new();
    for ws in by_doc.values() {
        for w in ws {
            *documents_with.entry(w.as_str()).or_default() += 1;
        }
    }
    by_doc
        .iter()
        .filter_map(|(md, ws)| {
            ws.iter()
                .min_by_key(|w| (documents_with[w.as_str()], w.as_str()))
                .map(|w| ((*md).clone(), w.clone()))
        })
        .collect()
}

fn listed(uuids: &BTreeSet<&str>) -> String {
    let shown: Vec<&str> = uuids.iter().take(5).copied().collect();
    let more = uuids.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{} and {more} more", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

#[tokio::test]
async fn every_row_is_found_by_what_it_holds() {
    let root = fixture_root();
    let (rows, texts) = read_rows(root.path()).await;
    let s = index_over(root.path()).await;
    let source_of = |uuid: &str| rows[uuid].source_id.clone();
    let sources: BTreeSet<String> = rows.values().map(|r| r.source_id.clone()).collect();
    let mut coverage = Coverage::default();
    let mut failures: Vec<String> = Vec::new();

    // A key compares its column exactly, so it finds exactly the rows
    // holding the value: none missing, none extra.
    for key in GridRow::KEYS {
        for (value, holding) in values_of(root.path(), key.column.as_str()).await {
            let q = datalib_query::term(key.key, &value, false);
            let got: BTreeSet<String> = every_uuid(&s, &q, None)
                .await
                .into_iter()
                .map(|r| r.uuid)
                .collect();
            let missing: BTreeSet<&str> = holding.difference(&got).map(String::as_str).collect();
            let extra: BTreeSet<&str> = got.difference(&holding).map(String::as_str).collect();
            if !missing.is_empty() || !extra.is_empty() {
                failures.push(format!(
                    "`{q}` missed [{}] and found [{}]",
                    listed(&missing),
                    listed(&extra)
                ));
            }
            for uuid in &holding {
                coverage.checked(&format!("{}:", key.key), &source_of(uuid));
            }
        }
    }

    // A key that reads the search terms, given a value whole, finds
    // exactly the rows holding it in one of its kinds, case-blind; a
    // handle-shaped value its handle's rows too (`+1701…` shown as a name
    // is the `tel:` handle), and a name the rows of every handle seen
    // under it. A bare value matches in part, which the
    // applet's own tests pin down.
    let terms = terms_of(root.path()).await;
    let names = names_of(root.path()).await;
    for key in TERMS_KEYS {
        let kinds: Vec<&str> = key.kinds().iter().map(|k| k.as_str()).collect();
        let mut by_value: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for ((kind, value), holding) in &terms {
            if kinds.contains(&kind.as_str()) {
                by_value
                    .entry(value.as_str())
                    .or_default()
                    .extend(holding.iter().cloned());
            }
        }
        for (value, holding) in &by_value {
            let q = datalib_query::exact_term(key.key, value, false);
            let got: BTreeSet<String> = every_uuid(&s, &q, None)
                .await
                .into_iter()
                .map(|r| r.uuid)
                .collect();
            let TermsValue::Exact { values, by_name } = terms_keys::value_of(key, value, true, "*")
            else {
                panic!("a quoted value is matched whole: {q}");
            };
            // A name also reaches the handles seen under it.
            let seen_under = |handle: &str| {
                by_name.as_deref().is_some_and(|name| {
                    names
                        .get(handle)
                        .is_some_and(|ns| ns.iter().any(|n| n.eq_ignore_ascii_case(name)))
                })
            };
            let expected: BTreeSet<String> = by_value
                .iter()
                .filter(|(v, _)| values.iter().any(|w| w.eq_ignore_ascii_case(v)) || seen_under(v))
                .flat_map(|(_, rows)| rows.iter().cloned())
                .collect();
            let missing: BTreeSet<&str> = expected.difference(&got).map(String::as_str).collect();
            let extra: BTreeSet<&str> = got.difference(&expected).map(String::as_str).collect();
            if !missing.is_empty() || !extra.is_empty() {
                failures.push(format!(
                    "`{q}` missed [{}] and found [{}]",
                    listed(&missing),
                    listed(&extra)
                ));
            }
            for uuid in holding {
                coverage.checked(&format!("terms: {}:", key.key), &source_of(uuid));
            }
        }
    }

    // A term is found by its value on the Fields tab, among whatever else
    // answers to the same words.
    for ((kind, value), holding) in terms {
        let q = format!("\"{value}\"");
        let got: HashSet<String> = every_uuid(&s, &q, Some(SearchTab::Fields))
            .await
            .into_iter()
            .map(|r| r.uuid)
            .collect();
        let missing: BTreeSet<&str> = holding
            .iter()
            .filter(|u| !got.contains(*u))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            failures.push(format!(
                "the Fields tab's `{q}` ({kind}) missed [{}]",
                listed(&missing)
            ));
        }
        for uuid in &holding {
            coverage.checked(&format!("fields: {kind}"), &source_of(uuid));
        }
    }

    // A document is found on the Words tab by the word of its text the
    // fewest documents hold.
    for (md, word) in rarest_words(&texts) {
        let got = every_uuid(&s, &word, Some(SearchTab::Words)).await;
        if !got
            .iter()
            .any(|r| r.markdown_uuid.as_deref() == Some(md.as_str()))
        {
            failures.push(format!("the Words tab's `{word}` missed document {md}"));
        }
        let source = rows
            .values()
            .find(|r| r.markdown_uuid.as_deref() == Some(md.as_str()))
            .map(|r| r.source_id.clone())
            .expect("a document has rows");
        coverage.checked("words: its rarest word", &source);
    }

    // A document is among the ten nearest its own opening words on the
    // Meaning tab. Vectors are near, not exact: a document with a twin
    // (two chats between the same people) may lose its place to it, so
    // this asks for nearly every document and some of every source's.
    let mut meaning: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut lost: Vec<String> = Vec::new();
    for (md, text) in &texts {
        let q = opening(text, 30);
        if q.is_empty() {
            continue;
        }
        let got = search_tab(&s, &q, Some(SearchTab::Meaning), None, 10, None).await;
        assert!(got.errors.is_empty(), "{q}: {:?}", got.errors);
        let found = got
            .rows
            .iter()
            .any(|r| r.markdown_uuid.as_deref() == Some(md.as_str()));
        let source = rows
            .values()
            .find(|r| r.markdown_uuid.as_deref() == Some(md.as_str()))
            .map(|r| r.source_id.clone())
            .expect("a document has rows");
        let tally = meaning.entry(source.clone()).or_default();
        tally.0 += usize::from(found);
        tally.1 += 1;
        if !found {
            lost.push(format!("{md} ({source})"));
        }
        coverage.checked("meaning: its opening words", &source);
    }
    let (found, asked) = meaning
        .values()
        .fold((0, 0), |(f, a), (found, asked)| (f + found, a + asked));
    if found * 10 < asked * 9 {
        failures.push(format!(
            "the Meaning tab found {found} of {asked} documents by their opening words; \
             lost {lost:?}"
        ));
    }
    for (source, (found, asked)) in &meaning {
        if found * 2 < *asked {
            failures.push(format!(
                "the Meaning tab found {found} of {source}'s {asked} documents \
                 by their opening words"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} searches did not find what they hold:\n{}",
        failures.len(),
        failures.join("\n")
    );
    insta::assert_snapshot!("search_finds_itself_coverage", coverage.render(&sources));
}

/// The opening of a document's text, as words a person might paste: the
/// frontmatter and markup dropped.
fn opening(text: &str, n: usize) -> String {
    let body = match text
        .strip_prefix("---\n")
        .and_then(|t| t.split_once("\n---\n"))
    {
        Some((_, rest)) => rest,
        None => text,
    };
    let mut plain = String::new();
    let mut in_tag = false;
    for c in body.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => plain.push(c),
            _ => {}
        }
    }
    plain
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && w.chars().all(char::is_alphabetic))
        .take(n)
        .collect::<Vec<_>>()
        .join(" ")
}
