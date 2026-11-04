//! sqlx warns of every statement over a second, and a render over a big
//! store is one such read per page: a column of `warn` lines nobody can act
//! on. The runner keeps each at `debug` and says what they came to, once,
//! when the attempt ends. Here rather than in the step because the runner
//! sees every line of an attempt however it dies.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::events::LogLevel;

const SQLX_TARGET: &str = "sqlx::query";

/// How much of a statement a sentence quotes.
const QUOTED_CHARS: usize = 120;

/// sqlx logs a statement above `debug` only when it was slow.
pub fn is_slow_statement(level: LogLevel, target: Option<&str>) -> bool {
    level == LogLevel::Warn && target == Some(SQLX_TARGET)
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Spent {
    count: u64,
    secs: f64,
    slowest: f64,
}

impl Spent {
    fn add(&mut self, secs: f64) {
        self.count += 1;
        self.secs += secs;
        self.slowest = self.slowest.max(secs);
    }
}

/// One attempt's slow statements, by what they said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlowStatements {
    by_statement: BTreeMap<String, Spent>,
}

/// sqlx indents a statement and leaves `db.statement` empty for one it
/// summarizes, such as `COMMIT`.
fn statement_of(fields: &Map<String, Value>) -> String {
    let words = |key: &str| {
        fields
            .get(key)
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
    };
    words("db.statement")
        .or_else(|| words("summary"))
        .unwrap_or_else(|| "(a statement sqlx did not name)".to_string())
}

impl SlowStatements {
    pub fn note(&mut self, fields: Option<&Map<String, Value>>) {
        let empty = Map::new();
        let fields = fields.unwrap_or(&empty);
        let secs = fields
            .get("elapsed_secs")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        self.by_statement
            .entry(statement_of(fields))
            .or_default()
            .add(secs);
    }

    /// The line these come to, and its fields; `None` for an attempt with
    /// none.
    pub fn summary(&self) -> Option<(String, Value)> {
        let (top, top_spent) = self
            .by_statement
            .iter()
            .max_by(|a, b| a.1.secs.total_cmp(&b.1.secs))?;
        let mut all = Spent::default();
        for spent in self.by_statement.values() {
            all.count += spent.count;
            all.secs += spent.secs;
            all.slowest = all.slowest.max(spent.slowest);
        }
        let quoted: String = top.chars().take(QUOTED_CHARS).collect();
        let msg = format!(
            "{} slow statements took {:.1}s in all; {} of them, {:.1}s, were: {quoted}",
            all.count, all.secs, top_spent.count, top_spent.secs,
        );
        let fields = serde_json::json!({
            "slow_statements": all.count,
            "total_ms": (all.secs * 1000.0) as u64,
            "slowest_ms": (all.slowest * 1000.0) as u64,
            "kinds": self.by_statement.len(),
            "top_statement": top,
            "top_count": top_spent.count,
            "top_total_ms": (top_spent.secs * 1000.0) as u64,
        });
        Some((msg, fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(statement: &str, summary: &str, secs: f64) -> Map<String, Value> {
        serde_json::json!({
            "db.statement": statement,
            "summary": summary,
            "elapsed_secs": secs,
        })
        .as_object()
        .unwrap()
        .clone()
    }

    /// Guards the render that wrote 21 `warn` lines, nearly all one
    /// expected read: they come to one sentence naming that read.
    #[test]
    fn slow_statements_come_to_one_line_naming_where_the_time_went() {
        let mut slow = SlowStatements::default();
        assert_eq!(slow.summary(), None, "an attempt with none says nothing");
        let page = "\n\nSELECT id, json(payload)\n FROM records\n";
        slow.note(Some(&fields(page, "SELECT id", 3.0)));
        slow.note(Some(&fields(page, "SELECT id", 2.5)));
        slow.note(Some(&fields("", "COMMIT", 1.5)));

        let (msg, fields) = slow.summary().unwrap();
        assert_eq!(
            msg,
            "3 slow statements took 7.0s in all; 2 of them, 5.5s, were: \
             SELECT id, json(payload) FROM records"
        );
        assert_eq!(fields["slow_statements"], 3);
        assert_eq!(fields["total_ms"], 7000);
        assert_eq!(fields["slowest_ms"], 3000);
        assert_eq!(fields["kinds"], 2);
        assert_eq!(fields["top_count"], 2);
    }

    #[test]
    fn a_statement_with_no_text_goes_by_its_summary() {
        let mut slow = SlowStatements::default();
        slow.note(Some(&fields("", "COMMIT", 2.0)));
        let (msg, _) = slow.summary().unwrap();
        assert!(msg.ends_with("were: COMMIT"), "{msg}");
    }

    #[test]
    fn only_a_warning_from_sqlx_is_a_slow_statement() {
        assert!(is_slow_statement(LogLevel::Warn, Some("sqlx::query")));
        assert!(!is_slow_statement(LogLevel::Debug, Some("sqlx::query")));
        assert!(!is_slow_statement(LogLevel::Warn, Some("datalib_etl")));
        assert!(!is_slow_statement(LogLevel::Warn, None));
    }
}
