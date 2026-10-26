//! `docs/user/raw_stores.md` names every table and column of the raw
//! stores it covers, and those names are checked here against
//! `raw_shapes/current.json`, which `migrate.rs` keeps equal to what the
//! code creates. A column added, renamed or dropped in a covered source
//! fails this until the doc says what it holds.
//!
//! The doc's shape, which the parser below reads: a source is a `## …
//! (`<type>`)` heading; each table under it is a `### `<table>`` heading
//! followed by rows `| `<column>` | … |`. The tables every store shares
//! sit under a `##` heading that says "every raw store", and
//! `<table>_bookkeeping` there stands for each table's sidecar.

use std::collections::{BTreeMap, BTreeSet};

type Columns = BTreeSet<String>;
type Tables = BTreeMap<String, Columns>;

const SIDECAR: &str = "<table>_bookkeeping";

#[derive(Debug, Default)]
struct Doc {
    shared: Tables,
    sources: BTreeMap<String, Tables>,
}

enum Section {
    Other,
    Shared,
    Source(String),
}

fn first_code_span(line: &str) -> Option<&str> {
    let (_, rest) = line.split_once('`')?;
    rest.split_once('`').map(|(name, _)| name)
}

fn documented(markdown: &str) -> Doc {
    let mut doc = Doc::default();
    let mut section = Section::Other;
    let mut table: Option<String> = None;
    for line in markdown.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            table = None;
            section = match heading.trim_end().strip_suffix("`)") {
                Some(rest) => match rest.rsplit_once("(`") {
                    Some((_, source_type)) => Section::Source(source_type.to_string()),
                    None => Section::Other,
                },
                None if heading.contains("every raw store") => Section::Shared,
                None => Section::Other,
            };
        } else if let Some(heading) = line.strip_prefix("### ") {
            table = first_code_span(heading).map(str::to_string);
            let tables = match &section {
                Section::Shared => &mut doc.shared,
                Section::Source(t) => doc.sources.entry(t.clone()).or_default(),
                Section::Other => continue,
            };
            if let Some(name) = &table {
                tables.entry(name.clone()).or_default();
            }
        } else if let (Some(name), Some(column)) = (&table, line.strip_prefix("| `")) {
            let column = column.split('`').next().unwrap_or_default().to_string();
            let tables = match &section {
                Section::Shared => &mut doc.shared,
                Section::Source(t) => doc.sources.entry(t.clone()).or_default(),
                Section::Other => continue,
            };
            tables.entry(name.clone()).or_default().insert(column);
        }
    }
    doc
}

/// The tables a store's `CREATE TABLE` statements declare, with their
/// columns. Constraints (`PRIMARY KEY(…)`, `CHECK(…)`) are not columns.
fn declared(statements: &[String]) -> Tables {
    let mut tables = Tables::new();
    for sql in statements {
        let Some(rest) = sql.strip_prefix("CREATE TABLE ") else {
            continue;
        };
        let rest = rest.trim_start_matches("IF NOT EXISTS ");
        let (Some(open), Some(close)) = (rest.find('('), rest.rfind(')')) else {
            continue;
        };
        let name = rest[..open].trim().trim_matches('"').to_string();
        let body = &rest[open + 1..close];
        let mut columns = Columns::new();
        let mut depth = 0;
        let mut start = 0;
        for (i, c) in body.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => {
                    columns.extend(column_of(&body[start..i]));
                    start = i + 1;
                }
                _ => {}
            }
        }
        columns.extend(column_of(&body[start..]));
        tables.insert(name, columns);
    }
    tables
}

fn column_of(definition: &str) -> Option<String> {
    let word = definition.split_whitespace().next()?;
    let keyword = word.split('(').next().unwrap_or(word).to_ascii_uppercase();
    let constraint =
        ["PRIMARY", "CHECK", "UNIQUE", "FOREIGN", "CONSTRAINT"].contains(&keyword.as_str());
    (!constraint).then(|| word.trim_matches(|c| c == '"' || c == '`').to_string())
}

fn compare(out: &mut Vec<String>, what: &str, said: &Columns, real: &Columns) {
    for c in real.difference(said) {
        out.push(format!("{what}: column `{c}` is not documented"));
    }
    for c in said.difference(real) {
        out.push(format!(
            "{what}: documents `{c}`, which the table does not have"
        ));
    }
}

/// What the doc says that the stores do not bear out, one line each.
fn disagreements(doc: &Doc, shapes: &BTreeMap<String, Tables>) -> Vec<String> {
    let mut out = Vec::new();
    let mut shared_seen = BTreeSet::new();
    for (source_type, said) in &doc.sources {
        let Some(real) = shapes.get(source_type) else {
            out.push(format!(
                "`{source_type}` is not a source type with a raw store"
            ));
            continue;
        };
        for (table, columns) in real {
            let what = format!("{source_type}.{table}");
            if table.ends_with("_bookkeeping") {
                shared_seen.insert(SIDECAR.to_string());
                match doc.shared.get(SIDECAR) {
                    Some(said) => compare(&mut out, &what, said, columns),
                    None => out.push(format!("{what}: no `{SIDECAR}` section")),
                }
            } else if let Some(said_shared) = doc.shared.get(table) {
                shared_seen.insert(table.clone());
                compare(&mut out, &what, said_shared, columns);
            } else if let Some(said) = said.get(table) {
                compare(&mut out, &what, said, columns);
            } else {
                out.push(format!("{what}: table is not documented"));
            }
        }
        for table in said.keys().filter(|t| !real.contains_key(*t)) {
            out.push(format!(
                "{source_type}.{table}: documented, but the store has no such table"
            ));
        }
    }
    for table in doc.shared.keys().filter(|t| !shared_seen.contains(*t)) {
        out.push(format!("shared `{table}`: in no documented source's store"));
    }
    out
}

fn current_shapes() -> BTreeMap<String, Tables> {
    let rel = "_main/datalib/backend/datalib_step/raw_shapes/current.json";
    let path = runfiles::Runfiles::create()
        .expect("runfiles tree")
        .rlocation(rel)
        .unwrap_or_else(|| panic!("rlocation for {rel}"));
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    json.as_object()
        .expect("current.json is an object by source type")
        .iter()
        .map(|(source_type, shape)| {
            let sql: Vec<String> = serde_json::from_value(shape["sql"].clone()).unwrap();
            (source_type.clone(), declared(&sql))
        })
        .collect()
}

#[test]
fn the_raw_stores_doc_names_every_table_and_column_of_what_it_covers() {
    let doc = documented(crate::help::RAW_STORES);
    for covered in ["email", "slack", "calendar"] {
        assert!(
            doc.sources.get(covered).is_some_and(|t| !t.is_empty()),
            "raw_stores.md has no tables under a `## … (`{covered}`)` heading; \
             did the headings change shape?"
        );
    }
    let wrong = disagreements(&doc, &current_shapes());
    assert!(
        wrong.is_empty(),
        "docs/user/raw_stores.md disagrees with raw_shapes/current.json:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn a_declared_table_is_its_columns_without_its_constraints() {
    let tables = declared(&[
        "CREATE TABLE t( a TEXT NOT NULL, b INT, PRIMARY KEY(a, b), CHECK(b IS NULL OR length(a) = 64))"
            .to_string(),
        "CREATE INDEX t_by_b ON t(b)".to_string(),
    ]);
    let columns: Vec<&str> = tables["t"].iter().map(String::as_str).collect();
    assert_eq!(columns, ["a", "b"]);
    assert_eq!(tables.len(), 1);
}

#[test]
fn a_column_missing_from_the_doc_or_extra_in_it_is_named() {
    let doc = documented(
        "## Mail (`email`)\n\n### `emails`\n\n| column | what |\n|---|---|\n\
         | `id` | the id |\n| `gone` | nothing |\n",
    );
    let shapes = BTreeMap::from([(
        "email".to_string(),
        declared(&["CREATE TABLE emails( id TEXT PRIMARY KEY, payload TEXT NULL)".to_string()]),
    )]);
    assert_eq!(
        disagreements(&doc, &shapes),
        [
            "email.emails: column `payload` is not documented",
            "email.emails: documents `gone`, which the table does not have",
        ]
    );
}
