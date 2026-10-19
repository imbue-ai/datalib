//! What a render reports about a record it read from someone else's
//! export: the keys it left unread, by shape, and findings that cost the
//! record nothing. A sample is the value's shape, never its content, so a
//! person can read the problems table of a private export.

use datalib_schema::problems::{Problem, Reason, Severity};
use serde_json::Value;

/// A warning for each key of `v` this render does not read, so a run over
/// a real export says what it left out. A key holding nothing (null, an
/// empty string, list or object) loses nothing and is not reported.
pub fn unread_keys(v: &Value, read: &[&str], path: &str) -> Vec<Problem> {
    let Some(map) = v.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(k, v)| !read.contains(&k.as_str()) && !is_empty_value(v))
        .map(|(k, v)| {
            Problem::field(k.clone(), Reason::UncoveredType, &shape_of(v))
                .at(format!("{path}/{k}"))
                .severity(Severity::Warning)
        })
        .collect()
}

pub fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// What a value is, without what it says: `string(12 chars)`,
/// `array(3)`, `object{a,b}`, `int`, `bool:true`.
pub fn shape_of(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool:{b}"),
        Value::Number(n) if n.is_i64() || n.is_u64() => "int".to_string(),
        Value::Number(_) => "float".to_string(),
        Value::String(s) => format!("string({} chars)", s.chars().count()),
        Value::Array(a) => format!("array({})", a.len()),
        Value::Object(m) => {
            let keys: Vec<&str> = m.keys().map(String::as_str).collect();
            format!("object{{{}}}", keys.join(","))
        }
    }
}

/// A finding about a record that cost it nothing, worth counting on a
/// real export.
pub fn noted(field: &str, explanation: &str) -> Problem {
    Problem::explained(Reason::Noted, Some(field.to_string()), explanation).severity(Severity::Info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_unread_key_is_reported_by_its_shape_alone() {
        let v = json!({"content": "private words", "call_duration": 42, "x": {"a": 1}, "gone": ""});
        let problems = unread_keys(&v, &["content"], "/message");
        let mut got: Vec<(String, String, String)> = problems
            .iter()
            .map(|p| {
                (
                    p.field.clone().unwrap(),
                    p.sample.clone(),
                    p.path.clone().unwrap(),
                )
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                (
                    "call_duration".into(),
                    "int".into(),
                    "/message/call_duration".into()
                ),
                ("x".into(), "object{a}".into(), "/message/x".into()),
            ]
        );
        assert_eq!(shape_of(&json!("private words")), "string(13 chars)");
    }
}
