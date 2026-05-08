//! Verify every grid row produced from the checked-in TNG fixtures has a
//! non-empty timestamp.
//!
//! Loads the `dump.sql` artifact emitted by `//tests/fixtures:ingested_tng`
//! into in-memory SQLite and runs `grid_rows_with_conn` over it. The
//! check exists because tool_use / tool_result blocks routinely lack an
//! intrinsic `start_timestamp`; without the synthetic-timestamp fallback
//! they sort to the top of the grid and the user can't reason about
//! ordering.
//!
//! How the dump.sql is located:
//!   * Under `bazel test`: the test target sets `FRANKWEILER_TEST_DUMP_SQL`
//!     to the runfiles path of the genrule output.
//!   * Under plain `cargo test`: set the env var manually after building
//!     the fixture, e.g.
//!     bazelisk build //tests/fixtures:ingested_tng
//!     FRANKWEILER_TEST_DUMP_SQL=$(pwd)/bazel-bin/tests/fixtures/ingested/dump.sql \
//!     cargo test -p frankweiler-core --test fixture_timestamps
//!     If the env var is unset the test prints a skip notice and passes,
//!     so the inner `cargo test` loop stays unblocked.
//!
//! Why the dump text needs translation: `dump.sql` is emitted in the SQL
//! subset Dolt and MySQL share, where `\\` inside a string literal means
//! one backslash and `\"` means one quote. SQLite is the standard SQL
//! dialect — backslashes are literal — so values that round-trip cleanly
//! through Dolt come out double-backslashed in SQLite, corrupting any
//! JSON that contains an escaped quote (`json_extract` then panics with
//! "malformed JSON"). We undo MySQL's backslash interpretation before
//! handing the script to SQLite.

use frankweiler_core::db::grid_rows_with_conn;
use frankweiler_core::query::parse_query;
use rusqlite::Connection;
use std::path::PathBuf;

fn locate_dump_sql() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FRANKWEILER_TEST_DUMP_SQL") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Some(path);
        }
        // Bazel sometimes hands us a runfiles-relative path; resolve via
        // TEST_SRCDIR if so.
        if let Ok(srcdir) = std::env::var("TEST_SRCDIR") {
            let candidate = PathBuf::from(srcdir).join("_main").join(&p);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Translate MySQL-style backslash escapes inside single-quoted SQL
/// literals into their literal characters, so SQLite reads the same
/// values that Dolt/MySQL would.
///
/// Walks the script once, tracking whether we're inside a single-quoted
/// string. Inside such a string, `\\` → `\`, `\'` → `'`, `\"` → `"`,
/// `\n` → newline, `\r` → CR. SQL-standard `''` (a doubled single quote)
/// is left alone — SQLite handles it natively.
fn mysql_to_sqlite(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        if !in_str {
            if c == b'\'' {
                in_str = true;
            }
            out.push(c as char);
            i += 1;
            continue;
        }
        // In single-quoted string.
        if c == b'\\' && i + 1 < bytes.len() {
            let n = bytes[i + 1];
            match n {
                b'\\' => out.push('\\'),
                b'\'' => out.push('\''),
                b'"' => out.push('"'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'0' => out.push('\0'),
                // Unknown escape: drop the backslash, keep the next char
                // (mirrors MySQL's behavior for un-recognized sequences).
                _ => out.push(n as char),
            }
            i += 2;
            continue;
        }
        if c == b'\'' {
            // Possible string terminator or doubled-quote escape (`''`).
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                out.push_str("''");
                i += 2;
                continue;
            }
            in_str = false;
            out.push('\'');
            i += 1;
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}

fn load_dump(path: &PathBuf) -> Connection {
    let sql =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let translated = mysql_to_sqlite(&sql);
    let conn = Connection::open_in_memory().expect("open in-memory sqlite");
    conn.execute_batch(&translated)
        .unwrap_or_else(|e| panic!("load dump.sql: {}", e));
    conn
}

#[test]
fn every_grid_row_has_a_timestamp() {
    let Some(dump) = locate_dump_sql() else {
        eprintln!(
            "skipping: FRANKWEILER_TEST_DUMP_SQL not set or file missing. \
             Run via `bazel test` or build the fixture first."
        );
        return;
    };
    let conn = load_dump(&dump);

    // Empty query → resolves to RowType::All, exercising every push_*
    // branch (anthropic chats, anthropic messages, anthropic blocks,
    // openai chats, openai messages).
    let q = parse_query("");
    let rows = grid_rows_with_conn(&conn, &q, 100_000);
    assert!(
        !rows.is_empty(),
        "fixture produced zero rows — broken setup"
    );

    let missing: Vec<String> = rows
        .iter()
        .filter(|r| r.when.trim().is_empty())
        .map(|r| {
            format!(
                "  source={} kind={} sender={} conv={} msg_idx={:?} snippet={:?}",
                r.source,
                r.kind,
                r.sender,
                r.conversation_uuid,
                r.message_index,
                r.snippet.chars().take(80).collect::<String>()
            )
        })
        .collect();

    assert!(
        missing.is_empty(),
        "{} of {} rows have empty `when`:\n{}",
        missing.len(),
        rows.len(),
        missing.join("\n")
    );

    // Sanity: confirm we exercised both providers (otherwise the test
    // could pass trivially if one provider's data weren't loaded).
    let saw_claude = rows.iter().any(|r| r.source == "Claude");
    let saw_chatgpt = rows.iter().any(|r| r.source == "ChatGPT");
    assert!(saw_claude, "no Claude rows in fixture output");
    assert!(saw_chatgpt, "no ChatGPT rows in fixture output");

    // Sanity: confirm tool blocks are represented — they're the row type
    // most likely to lack an intrinsic timestamp.
    let saw_tool = rows.iter().any(|r| r.kind == "Tool Call");
    assert!(saw_tool, "no Tool Call rows in fixture output");
}
