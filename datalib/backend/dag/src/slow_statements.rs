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

/// An attempt that spent less than this in slow statements says so at
/// `debug`: a second or two here and there is no one's question, and a
/// fan-in that spent them would say so on every pass.
const WORTH_A_LINE_SECS: f64 = 5.0;

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

    /// The line these come to, at its level, with its fields; `None` for an
    /// attempt with none.
    pub fn summary(&self) -> Option<(LogLevel, String, Value)> {
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
        let msg = if all.count == 1 {
            format!("a slow statement took {:.1}s: {quoted}", all.secs)
        } else if top_spent.count == all.count {
            format!(
                "{} slow statements took {:.1}s in all, every one: {quoted}",
                all.count, all.secs
            )
        } else {
            format!(
                "{} slow statements took {:.1}s in all, most of it {} taking {:.1}s: {quoted}",
                all.count, all.secs, top_spent.count, top_spent.secs,
            )
        };
        let level = if all.secs < WORTH_A_LINE_SECS {
            LogLevel::Debug
        } else {
            LogLevel::Info
        };
        let fields = serde_json::json!({
            "slow_statements": all.count,
            "total_ms": (all.secs * 1000.0) as u64,
            "slowest_ms": (all.slowest * 1000.0) as u64,
            "kinds": self.by_statement.len(),
            "top_statement": top,
            "top_count": top_spent.count,
            "top_total_ms": (top_spent.secs * 1000.0) as u64,
        });
        Some((level, msg, fields))
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

        let (level, msg, fields) = slow.summary().unwrap();
        assert_eq!(level, LogLevel::Info);
        assert_eq!(
            msg,
            "3 slow statements took 7.0s in all, most of it 2 taking 5.5s: \
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
        let (_, msg, _) = slow.summary().unwrap();
        assert!(msg.ends_with(": COMMIT"), "{msg}");
    }

    /// Guards `1 slow statements took 1.7s in all; 1 of them, 1.7s, were:`,
    /// which a fan-in wrote on most of its passes at `info`.
    #[test]
    fn a_second_or_two_is_a_quiet_singular_line() {
        let mut slow = SlowStatements::default();
        slow.note(Some(&fields("SELECT 1", "SELECT 1", 1.7)));
        let (level, msg, _) = slow.summary().unwrap();
        assert_eq!(level, LogLevel::Debug);
        assert_eq!(msg, "a slow statement took 1.7s: SELECT 1");

        slow.note(Some(&fields("SELECT 1", "SELECT 1", 4.0)));
        let (level, msg, _) = slow.summary().unwrap();
        assert_eq!(level, LogLevel::Info, "over the line in all");
        assert_eq!(
            msg,
            "2 slow statements took 5.7s in all, every one: SELECT 1"
        );
    }

    #[test]
    fn only_a_warning_from_sqlx_is_a_slow_statement() {
        assert!(is_slow_statement(LogLevel::Warn, Some("sqlx::query")));
        assert!(!is_slow_statement(LogLevel::Debug, Some("sqlx::query")));
        assert!(!is_slow_statement(LogLevel::Warn, Some("datalib_etl")));
        assert!(!is_slow_statement(LogLevel::Warn, None));
    }
}
