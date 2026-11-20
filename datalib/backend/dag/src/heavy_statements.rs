//! A step's heavy statements (`datalib_sql_work`): kept at `debug` one by
//! one, as slow ones are, and summed once when the attempt ends, naming the
//! statement that did most of the work and the file it read.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::events::LogLevel;

const QUOTED_CHARS: usize = 120;

/// An attempt whose heavy statements did less work than this in all says so
/// at `debug`: SQLite walks some three million rows for it, a second or two.
const WORTH_A_LINE_STEPS: u64 = 10_000_000;

pub fn is_heavy_statement(level: LogLevel, target: Option<&str>) -> bool {
    level == LogLevel::Warn && target == Some(datalib_sql_work::TARGET)
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Did {
    count: u64,
    vm_steps: u64,
    rows: u64,
    file: String,
    params: String,
}

/// One attempt's heavy statements, by statement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeavyStatements {
    by_statement: BTreeMap<String, Did>,
}

impl HeavyStatements {
    pub fn note(&mut self, fields: Option<&Map<String, Value>>) {
        let empty = Map::new();
        let fields = fields.unwrap_or(&empty);
        let text = |key: &str| fields.get(key).and_then(Value::as_str).unwrap_or_default();
        let number = |key: &str| fields.get(key).and_then(Value::as_u64).unwrap_or(0);
        let did = self
            .by_statement
            .entry(text("statement").to_string())
            .or_default();
        did.count += 1;
        did.vm_steps += number("vm_steps");
        did.rows += number("rows");
        did.file = text("file").to_string();
        did.params = text("params").to_string();
    }

    /// The line these come to, at its level, with its fields; `None` for an
    /// attempt with none.
    pub fn summary(&self) -> Option<(LogLevel, String, Value)> {
        let (top, top_did) = self
            .by_statement
            .iter()
            .max_by_key(|(_, did)| did.vm_steps)?;
        let (count, vm_steps, rows) = self.by_statement.values().fold((0, 0, 0), |a, d| {
            (a.0 + d.count, a.1 + d.vm_steps, a.2 + d.rows)
        });
        let quoted: String = top.chars().take(QUOTED_CHARS).collect();
        let file = std::path::Path::new(&top_did.file)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let steps = millions(vm_steps);
        let msg = if count == 1 {
            format!("a heavy statement did {steps} steps for {rows} rows, on {file}: {quoted}")
        } else {
            let which = if top_did.count == count {
                "every one".to_string()
            } else {
                format!(
                    "most of it {} runs doing {}",
                    top_did.count,
                    millions(top_did.vm_steps)
                )
            };
            format!("{count} heavy statements did {steps} steps for {rows} rows, {which} on {file}: {quoted}")
        };
        let level = if vm_steps < WORTH_A_LINE_STEPS {
            LogLevel::Debug
        } else {
            LogLevel::Info
        };
        let fields = serde_json::json!({
            "heavy_statements": count,
            "vm_steps": vm_steps,
            "rows": rows,
            "kinds": self.by_statement.len(),
            "top_statement": top,
            "top_file": top_did.file,
            "top_params": top_did.params,
            "top_count": top_did.count,
            "top_vm_steps": top_did.vm_steps,
        });
        Some((level, msg, fields))
    }
}

fn millions(steps: u64) -> String {
    format!("{:.1}M", steps as f64 / 1e6)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(statement: &str, vm_steps: u64, rows: u64) -> Map<String, Value> {
        serde_json::json!({
            "statement": statement,
            "file": "/root/unified_grid_and_search_index/db.sqlite",
            "params": "text(12)",
            "vm_steps": vm_steps,
            "rows": rows,
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn heavy_statements_come_to_one_line_naming_the_heaviest() {
        let mut heavy = HeavyStatements::default();
        assert_eq!(heavy.summary(), None, "an attempt with none says nothing");
        let nodes = "SELECT n.uuid FROM data_nodes n";
        heavy.note(Some(&fields(nodes, 30_000_000, 0)));
        heavy.note(Some(&fields(nodes, 25_000_000, 1)));
        heavy.note(Some(&fields("DELETE FROM vectors", 2_000_000, 1)));

        let (level, msg, fields) = heavy.summary().unwrap();
        assert_eq!(level, LogLevel::Info);
        assert_eq!(
            msg,
            "3 heavy statements did 57.0M steps for 2 rows, most of it 2 runs doing 55.0M \
             on db.sqlite: SELECT n.uuid FROM data_nodes n"
        );
        assert_eq!(fields["top_params"], "text(12)");
        assert_eq!(fields["top_count"], 2);
        assert_eq!(fields["kinds"], 2);
    }

    #[test]
    fn a_little_heavy_work_is_a_quiet_line() {
        let mut heavy = HeavyStatements::default();
        heavy.note(Some(&fields("SELECT 1", 2_000_000, 0)));
        let (level, msg, _) = heavy.summary().unwrap();
        assert_eq!(level, LogLevel::Debug);
        assert_eq!(
            msg,
            "a heavy statement did 2.0M steps for 0 rows, on db.sqlite: SELECT 1"
        );
    }

    #[test]
    fn only_a_warning_from_the_counter_is_a_heavy_statement() {
        assert!(is_heavy_statement(
            LogLevel::Warn,
            Some("datalib::sql_work")
        ));
        assert!(!is_heavy_statement(
            LogLevel::Debug,
            Some("datalib::sql_work")
        ));
        assert!(!is_heavy_statement(LogLevel::Warn, Some("sqlx::query")));
    }
}
