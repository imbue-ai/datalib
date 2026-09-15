//! The run log read through the shared search-bar grammar: which keys
//! name a `log` column, and free text as a substring of the line.

use std::path::Path;

use app_schema::runs::LogRow;
use datalib_query::Token;

use crate::runs_path;
use crate::store::{log_row_from, open_existing};

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

/// The keys a log query understands, each the column it names.
const KEYS: &[(&str, &str)] = &[
    ("run", "run_id"),
    ("process", "process"),
    ("step", "step"),
    ("level", "level"),
    ("stream", "stream"),
    ("target", "target"),
    ("thread", "thread"),
    ("msg", "msg"),
];

/// Where free text is looked for: the message, and the two columns a
/// reader would otherwise have to open a line to see.
const FREE_TEXT_COLUMNS: &[&str] = &["msg", "target", "fields"];

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
        clauses: vec!["seq > ?".to_string()],
        binds: vec![Bound::Int(q.after_seq)],
    };
    if let Some(run) = q.run {
        c.clauses.push("run_id = ?".to_string());
        c.binds.push(Bound::Text(run.to_string()));
    }
    if let Some(step) = q.step {
        c.clauses.push("step = ?".to_string());
        c.binds.push(Bound::Text(step.to_string()));
    }
    for tok in datalib_query::parse(q.q) {
        match tok {
            Token::Term(t) => {
                let Some((_, col)) = KEYS.iter().find(|(k, _)| *k == t.key) else {
                    let known: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
                    return Err(QueryError(format!(
                        "`{}:` is not something a log line has; try one of {}",
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
pub async fn log_query(data_root: &Path, q: &LogQuery<'_>) -> Result<Vec<LogRow>, QueryError> {
    let compiled = compile(q)?;
    let path = runs_path(data_root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let Ok(pool) = open_existing(&path).await else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "SELECT seq, run_id, process, step, attempt, ts_utc, tz_offset, stream, level, target, \
         thread, msg, fields FROM log WHERE {} ORDER BY seq LIMIT ?",
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
    Ok(rows.iter().map(log_row_from).collect())
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
            vec!["seq > ?", "level = ?", "(target IS NULL OR target != ?)"]
        );
    }

    #[test]
    fn free_text_searches_the_line_and_escapes_like() {
        let c = compile(&q("50%")).unwrap();
        assert_eq!(c.clauses.len(), 2);
        assert!(c.clauses[1].starts_with("(msg LIKE ?"));
        assert!(matches!(&c.binds[1], Bound::Text(s) if s == "%50\\%%"));
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let e = compile(&q("author:thad")).unwrap_err();
        assert!(e.0.contains("`author:`"), "{e}");
        assert!(e.0.contains("run, process, step, level"), "{e}");
    }
}
