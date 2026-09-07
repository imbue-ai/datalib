//! What every store in this build actually declares, as one golden.
//!
//! A table or column name is a contract: it is in hand-written SQL, in
//! prose under `docs/`, and in whatever a user types at the doltlite
//! shell. Nothing here enforces those uses — the point is narrower and
//! cheaper. Renaming a table, adding one, or dropping a column moves
//! this snapshot, so the change arrives in review as an explicit diff
//! instead of as a silent divergence someone notices months later.
//!
//! It is also the one current answer to "what tables are there?", which
//! is what `docs/dev/grid_rows.md` should be checked against: that file
//! spent an unknown stretch naming `openai_conversations`,
//! `claude_conversations` and `slack_workspaces`, none of which have
//! ever existed.
//!
//! Regenerate with:
//!   bazel run //datalib/backend/schema_inventory:inventory.update

use std::collections::BTreeMap;

/// Pull `CREATE TABLE <name> (...)` out of a DDL statement and return
/// the table plus its declared column names, in declaration order.
///
/// Deliberately a small hand parser rather than a SQL crate: the input
/// is our own generated DDL, and a dependency that understood all of
/// SQLite would make the golden depend on that crate's opinions.
/// `CREATE INDEX` and anything else returns `None`.
fn parse_create_table(ddl: &str) -> Option<(String, Vec<String>)> {
    let ddl = ddl.trim();
    let rest = ddl
        .strip_prefix("CREATE TABLE IF NOT EXISTS ")
        .or_else(|| ddl.strip_prefix("CREATE TABLE "))?;
    let (name, body) = rest.split_once('(')?;
    let name = name.trim().trim_matches('`').trim_matches('"').to_string();

    let body = body.trim_end().trim_end_matches(')');
    let mut cols = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => {
                push_column(&mut cols, &current);
                current.clear();
            }
            _ => current.push(c),
        }
    }
    push_column(&mut cols, &current);
    Some((name, cols))
}

/// Take the column name off one comma-separated clause of a CREATE
/// TABLE body, skipping table-level constraints (`PRIMARY KEY (...)`,
/// `UNIQUE (...)`, `FOREIGN KEY ...`) which name no column of their own.
fn push_column(cols: &mut Vec<String>, clause: &str) {
    let clause = clause.trim();
    let Some(first) = clause.split_whitespace().next() else {
        return;
    };
    let upper = first.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "PRIMARY" | "UNIQUE" | "FOREIGN" | "CHECK" | "CONSTRAINT"
    ) {
        return;
    }
    let name = first.trim_matches('`').trim_matches('"');
    if !name.is_empty() {
        cols.push(name.to_string());
    }
}

/// Every store, as `(store label, its DDL statements)`.
///
/// The label is the *store*, not the provider tag: `email` writes one
/// raw store whichever of its three download modes ran, and the two
/// Claude source types share one. Providers absent from this list
/// declare no raw store of their own.
fn stores() -> Vec<(&'static str, Vec<String>)> {
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        (
            "beeper/raw",
            datalib_etl_beeper::download::schema_raw::full_ddl(),
        ),
        (
            "chatgpt/raw",
            datalib_etl_chatgpt::download::schema_raw::full_ddl(),
        ),
        (
            "claude/raw",
            datalib_etl_claude::download::schema_raw::full_ddl(),
        ),
        (
            "contacts/raw",
            datalib_etl_contacts::download::schema_raw::full_ddl(),
        ),
        (
            "email/raw",
            datalib_etl_email::download::schema_raw::full_ddl(),
        ),
        (
            "fsindex/raw",
            datalib_etl_fsindex::download::schema_raw::full_ddl(),
        ),
        (
            "github/raw",
            datalib_etl_github::download::schema_raw::full_ddl(),
        ),
        (
            "gitlab/raw",
            datalib_etl_gitlab::download::schema_raw::full_ddl(),
        ),
        (
            "google_takeout/raw",
            datalib_etl_google_takeout::download::schema_raw::full_ddl(),
        ),
        (
            "media/raw",
            datalib_etl_media::download::schema_raw::full_ddl(),
        ),
        (
            "notion/raw",
            datalib_etl_notion::download::schema_raw::full_ddl(),
        ),
        ("pdf/raw", datalib_etl_pdf::download::schema_raw::full_ddl()),
        (
            "signal/raw",
            datalib_etl_signal::download::schema_raw::full_ddl(),
        ),
        (
            "slack/raw",
            datalib_etl_slack::download::schema_raw::full_ddl(),
        ),
        (
            "sms_backup_restore/raw",
            datalib_etl_sms_backup_restore::download::schema_raw::full_ddl(),
        ),
        (
            "whatsapp/raw",
            owned(datalib_etl_whatsapp::schema_raw::ALL_DDL),
        ),
        (
            "yolink/raw",
            datalib_etl_yolink::download::schema_raw::full_ddl(),
        ),
        // The shared render/index and app stores, from `PortableTable`.
        (
            "unified_index/grid",
            portable(&[
                datalib_schema::grid_rows::DDL,
                datalib_schema::markdowns::DDL,
                datalib_schema::edges::DDL,
                datalib_schema::render_problems::DDL,
                datalib_schema::source_cursors::DDL,
                datalib_schema::measurements::DDL,
            ]),
        ),
        (
            "system",
            portable(&[
                app_schema::feedback::DDL,
                app_schema::sync_jobs::DDL,
                app_schema::disk_usage::DDL,
            ]),
        ),
    ]
}

fn portable(groups: &[&[(&str, &str)]]) -> Vec<String> {
    groups
        .iter()
        .flat_map(|g| g.iter().map(|(_, ddl)| (*ddl).to_string()))
        .collect()
}

#[test]
fn schema_inventory() {
    let mut out = String::new();
    for (store, ddl) in stores() {
        let mut tables: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for stmt in &ddl {
            if let Some((name, cols)) = parse_create_table(stmt) {
                tables.insert(name, cols);
            }
        }
        assert!(
            !tables.is_empty(),
            "{store} declared no CREATE TABLE — did its DDL entry point move?"
        );
        out.push_str(&format!("== {store} ==\n"));
        for (table, cols) in tables {
            out.push_str(&format!("{table}\n"));
            for c in cols {
                out.push_str(&format!("    {c}\n"));
            }
        }
        out.push('\n');
    }
    insta::assert_snapshot!("schema_inventory", out);
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn reads_a_table_and_skips_table_level_constraints() {
        let (name, cols) = parse_create_table(
            "CREATE TABLE IF NOT EXISTS t (a TEXT NOT NULL, b INT, PRIMARY KEY (a, b))",
        )
        .expect("a CREATE TABLE");
        assert_eq!(name, "t");
        assert_eq!(
            cols,
            vec!["a", "b"],
            "the PRIMARY KEY clause names no column"
        );
    }

    /// A column whose type carries its own parentheses must not end the
    /// clause early — `VARCHAR(36)` is one column, not two.
    #[test]
    fn a_parenthesised_type_is_one_column() {
        let (_, cols) = parse_create_table("CREATE TABLE t (a VARCHAR(36), b DECIMAL(10, 2))")
            .expect("a CREATE TABLE");
        assert_eq!(cols, vec!["a", "b"]);
    }

    #[test]
    fn an_index_is_not_a_table() {
        assert!(parse_create_table("CREATE INDEX foo ON t(a)").is_none());
    }
}
