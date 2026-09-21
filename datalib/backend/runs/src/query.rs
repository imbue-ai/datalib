//! The run log read through the shared search-bar grammar: which keys
//! name a `log` column, and free text as a substring of the line.

use std::path::Path;

use app_schema::runs::LogLevel;
use datalib_query::Token;

use crate::runs_path;
use crate::store::{log_line_from, open_existing, LogLine, LOG_LINE_COLUMNS};

/// One read of the log. `run` and `step` narrow it the way the panel
/// does; `q` is what was typed; `after_seq` is the tail cursor.
pub struct LogQuery<'a> {
    pub run: Option<&'a str>,
    pub step: Option<&'a str>,
    pub q: &'a str,
    pub after_seq: i64,
    pub limit: i64,
}

/// A query the vocabulary cannot answer. Worded for the search bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryError(pub String);

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The keys a log query understands, each the column it names — on the
/// line (`l`), or on the process that wrote it (`p`).
const KEYS: &[(&str, &str)] = &[
    ("run", "l.run_id"),
    ("process", "p.process"),
    ("step", "l.step"),
    ("level", "l.level"),
    ("stream", "l.stream"),
    ("target", "l.target"),
    ("thread", "l.thread"),
    ("msg", "l.msg"),
];

/// `commit:0fc29cb` — the process's commit, by prefix, the way git
/// names one. Not in [`KEYS`]: a prefix, not a column's text.
const COMMIT_KEY: &str = "commit";

/// `min_level:warn` — this level and above. Not in [`KEYS`]: its value
/// is a rank, not a column's text. A level word this build does not
/// know ranks above every known one, so a line from a newer build is
/// shown rather than hidden.
const MIN_LEVEL_KEY: &str = "min_level";
const LEVEL_RANK: &str = "CASE l.level WHEN 'trace' THEN 0 WHEN 'debug' THEN 1 WHEN 'info' THEN 2 \
     WHEN 'warn' THEN 3 WHEN 'error' THEN 4 ELSE 5 END";

fn level_rank(level: LogLevel) -> i64 {
    match level {
        LogLevel::Trace => 0,
        LogLevel::Debug => 1,
        LogLevel::Info => 2,
        LogLevel::Warn => 3,
        LogLevel::Error => 4,
    }
}

/// Where free text is looked for: the message, and the two columns a
/// reader would otherwise have to open a line to see.
const FREE_TEXT_COLUMNS: &[&str] = &["l.msg", "l.target", "l.fields"];

#[derive(Debug)]
enum Bound {
    Text(String),
    Int(i64),
}

#[derive(Debug)]
struct Compiled {
    clauses: Vec<String>,
    binds: Vec<Bound>,
}

fn compile(q: &LogQuery<'_>) -> Result<Compiled, QueryError> {
    let mut c = Compiled {
        clauses: vec!["l.seq > ?".to_string()],
        binds: vec![Bound::Int(q.after_seq)],
    };
    if let Some(run) = q.run {
        c.clauses.push("l.run_id = ?".to_string());
        c.binds.push(Bound::Text(run.to_string()));
    }
    if let Some(step) = q.step {
        c.clauses.push("l.step = ?".to_string());
        c.binds.push(Bound::Text(step.to_string()));
    }
    for tok in datalib_query::parse(q.q) {
        match tok {
            Token::Term(t) if t.key == COMMIT_KEY => {
                c.clauses.push(if t.negate {
                    "(p.git_hash IS NULL OR p.git_hash NOT LIKE ? ESCAPE '\\')".to_string()
                } else {
                    "p.git_hash LIKE ? ESCAPE '\\'".to_string()
                });
                c.binds.push(Bound::Text(format!(
                    "{}%",
                    escape_like(&t.value.to_ascii_lowercase())
                )));
            }
            Token::Term(t) if t.key == MIN_LEVEL_KEY => {
                let Some(level) = LogLevel::parse(&t.value) else {
                    return Err(QueryError(format!(
                        "`{MIN_LEVEL_KEY}:` wants a level — trace, debug, info, warn or error — not `{}`",
                        t.value
                    )));
                };
                c.clauses.push(if t.negate {
                    format!("{LEVEL_RANK} < ?")
                } else {
                    format!("{LEVEL_RANK} >= ?")
                });
                c.binds.push(Bound::Int(level_rank(level)));
            }
            Token::Term(t) => {
                let Some((_, col)) = KEYS.iter().find(|(k, _)| *k == t.key) else {
                    let known: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
                    return Err(QueryError(format!(
                        "`{}:` is not something a log line has; try one of {}, {COMMIT_KEY}, {MIN_LEVEL_KEY}",
                        t.key,
                        known.join(", ")
                    )));
                };
                // A negated term keeps the lines with no value in the
                // column: they are not the value being excluded.
                c.clauses.push(if t.negate {
                    format!("({col} IS NULL OR {col} != ?)")
                } else {
                    format!("{col} = ?")
                });
                c.binds.push(Bound::Text(t.value));
            }
            Token::Free(raw) => {
                let (text, negate) = datalib_query::free_text(&raw);
                if text.is_empty() {
                    continue;
                }
                let any: Vec<String> = FREE_TEXT_COLUMNS
                    .iter()
                    .map(|col| format!("{col} LIKE ? ESCAPE '\\'"))
                    .collect();
                let any = any.join(" OR ");
                c.clauses.push(if negate {
                    format!("NOT ({any})")
                } else {
                    format!("({any})")
                });
                let pattern = format!("%{}%", escape_like(&text));
                for _ in FREE_TEXT_COLUMNS {
                    c.binds.push(Bound::Text(pattern.clone()));
                }
            }
        }
    }
    Ok(c)
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Log lines matching `q`, oldest first, at most `limit`. Empty when the
/// store does not exist yet; an error only for a query the vocabulary
/// cannot read.
pub async fn log_query(data_root: &Path, q: &LogQuery<'_>) -> Result<Vec<LogLine>, QueryError> {
    let compiled = compile(q)?;
    let path = runs_path(data_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let Ok(pool) = open_existing(&path).await else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "SELECT {LOG_LINE_COLUMNS} FROM log l LEFT JOIN processes p USING (process_id) \
         WHERE {} ORDER BY l.seq LIMIT ?",
        compiled.clauses.join(" AND ")
    );
    // Audited: every clause is assembled from the `&'static str` column
    // names in KEYS and FREE_TEXT_COLUMNS with `?` placeholders; every
    // value the user typed is bound below, never interpolated.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for b in &compiled.binds {
        query = match b {
            Bound::Text(s) => query.bind(s),
            Bound::Int(n) => query.bind(*n),
        };
    }
    let rows = query
        .bind(q.limit)
        .fetch_all(&pool)
        .await
        .unwrap_or_default();
    pool.close().await;
    Ok(rows.iter().map(log_line_from).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(s: &str) -> LogQuery<'_> {
        LogQuery {
            run: None,
            step: None,
            q: s,
            after_seq: 0,
            limit: 10,
        }
    }

    #[test]
    fn terms_bind_columns_and_negation_keeps_nulls() {
        let c = compile(&q("level:warn -target:sqlx")).unwrap();
        assert_eq!(
            c.clauses,
            vec![
                "l.seq > ?",
                "l.level = ?",
                "(l.target IS NULL OR l.target != ?)"
            ]
        );
    }

    #[test]
    fn free_text_searches_the_line_and_escapes_like() {
        let c = compile(&q("50%")).unwrap();
        assert_eq!(c.clauses.len(), 2);
        assert!(c.clauses[1].starts_with("(l.msg LIKE ?"));
        assert!(matches!(&c.binds[1], Bound::Text(s) if s == "%50\\%%"));
    }

    /// `commit:` is a prefix: the column shows ten characters and git
    /// names a commit by any unambiguous start.
    #[test]
    fn commit_matches_a_prefix() {
        let c = compile(&q("commit:0FC29cb")).unwrap();
        assert_eq!(c.clauses[1], "p.git_hash LIKE ? ESCAPE '\\'");
        assert!(matches!(&c.binds[1], Bound::Text(s) if s == "0fc29cb%"));
        let c = compile(&q("-commit:abc")).unwrap();
        assert!(c.clauses[1].starts_with("(p.git_hash IS NULL OR"));
    }

    /// `min_level:` is a rank on the level word, and refuses a word
    /// that is not a level rather than matching nothing.
    #[test]
    fn min_level_ranks_the_level_word() {
        let c = compile(&q("min_level:warn")).unwrap();
        assert!(c.clauses[1].ends_with("END >= ?"), "{}", c.clauses[1]);
        assert!(matches!(c.binds[1], Bound::Int(3)));
        let c = compile(&q("-min_level:info")).unwrap();
        assert!(c.clauses[1].ends_with("END < ?"), "{}", c.clauses[1]);
        let e = compile(&q("min_level:loud")).unwrap_err();
        assert!(e.0.contains("`loud`"), "{e}");
    }

    /// `process:` names the program, which is the process's column,
    /// not the line's.
    #[test]
    fn the_process_key_reads_the_process_row() {
        let c = compile(&q("process:http")).unwrap();
        assert_eq!(c.clauses[1], "p.process = ?");
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let e = compile(&q("author:thad")).unwrap_err();
        assert!(e.0.contains("`author:`"), "{e}");
        assert!(e.0.contains("run, process, step, level"), "{e}");
        assert!(e.0.ends_with("min_level"), "{e}");
    }
}
