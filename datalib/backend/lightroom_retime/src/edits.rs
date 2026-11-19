//! The edits file: JSON Lines, one photo per line (README.md §"The edits
//! file").

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, Timelike};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    id_global: String,
    expected: String,
    capture_time: String,
    #[serde(default)]
    why: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub line: usize,
    pub id_global: String,
    pub expected: String,
    pub target: DateTime<FixedOffset>,
    pub why: Option<String>,
}

/// Every edit, or every line's problem.
pub fn parse(text: &str) -> Result<Vec<Edit>, Vec<String>> {
    let mut edits = Vec::new();
    let mut problems = Vec::new();
    let mut first_line: HashMap<String, usize> = HashMap::new();
    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        if raw.trim().is_empty() {
            continue;
        }
        match parse_line(raw) {
            Ok((l, target)) => {
                if let Some(first) = first_line.insert(l.id_global.clone(), line) {
                    problems.push(format!(
                        "line {line}: {} is already edited on line {first}",
                        l.id_global
                    ));
                    continue;
                }
                edits.push(Edit {
                    line,
                    id_global: l.id_global,
                    expected: l.expected,
                    target,
                    why: l.why,
                });
            }
            Err(e) => problems.push(format!("line {line}: {e}")),
        }
    }
    if problems.is_empty() {
        Ok(edits)
    } else {
        Err(problems)
    }
}

fn parse_line(raw: &str) -> Result<(Line, DateTime<FixedOffset>), String> {
    let l: Line = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let target = DateTime::parse_from_rfc3339(&l.capture_time).map_err(|e| {
        format!(
            "capture_time {:?} is not a time with its offset, like \
             2024-07-01T14:00:00.000+02:00 ({e})",
            l.capture_time
        )
    })?;
    if target.nanosecond() % 1_000_000 != 0 {
        return Err(format!(
            "capture_time {:?} is finer than the millisecond Lightroom keeps",
            l.capture_time
        ));
    }
    Ok((l, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "FFBB4B7B-CD62-4012-8074-3B10D8CB3A6A";

    fn line(capture_time: &str) -> String {
        format!(
            r#"{{"id_global":"{ID}","expected":"2024-07-01T12:00:00.000","capture_time":"{capture_time}","why":"a reason"}}"#
        )
    }

    #[test]
    fn reads_a_line_and_skips_blank_ones() {
        let edits = parse(&format!("\n{}\n\n", line("2024-07-01T14:00:00.000+02:00"))).unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].line, 2);
        assert_eq!(edits[0].target.offset().local_minus_utc(), 7200);
        assert_eq!(edits[0].why.as_deref(), Some("a reason"));
    }

    #[test]
    fn a_time_without_its_offset_is_refused() {
        let problems = parse(&line("2024-07-01T14:00:00.000")).unwrap_err();
        assert!(problems[0].contains("with its offset"), "{problems:?}");
    }

    #[test]
    fn a_time_finer_than_a_millisecond_is_refused() {
        let problems = parse(&line("2024-07-01T14:00:00.0001+02:00")).unwrap_err();
        assert!(problems[0].contains("millisecond"), "{problems:?}");
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let problems = parse(&line("x").replace("\"why\"", "\"wyh\"")).unwrap_err();
        assert!(problems[0].contains("unknown field"), "{problems:?}");
    }

    #[test]
    fn a_photo_edited_twice_is_refused() {
        let one = line("2024-07-01T14:00:00+02:00");
        let problems = parse(&format!("{one}\n{one}\n")).unwrap_err();
        assert_eq!(
            problems,
            vec![format!("line 2: {ID} is already edited on line 1")]
        );
    }
}
