//! What the store changes in a line before it keeps it: the person's
//! home directory becomes `~`, so a path in a message, a field or a
//! step's error names no user (`docs/dev/logging.md` § "What a line may
//! carry"). Both writers take every row through here, so this is the
//! one place it happens.

use app_schema::runs::{LogRow, StepRunRow};

/// The home directory to hide, read once when a writer starts.
#[derive(Debug, Clone, Default)]
pub(crate) struct Redactor {
    home: Option<String>,
}

impl Redactor {
    pub(crate) fn from_env() -> Self {
        Self::for_home(std::env::var("HOME").ok().as_deref())
    }

    /// `/` or a relative path would turn every path into `~`; neither
    /// is a home worth hiding.
    pub(crate) fn for_home(home: Option<&str>) -> Self {
        let home = home
            .map(|h| h.trim_end_matches('/'))
            .filter(|h| h.starts_with('/') && h.len() > 1)
            .map(str::to_string);
        Self { home }
    }

    pub(crate) fn log(&self, mut row: LogRow) -> LogRow {
        self.in_place(&mut row.msg);
        if let Some(fields) = row.fields.as_mut() {
            self.in_place(fields);
        }
        row
    }

    pub(crate) fn step(&self, mut row: StepRunRow) -> StepRunRow {
        for text in [row.error.as_mut(), row.msg.as_mut()].into_iter().flatten() {
            self.in_place(text);
        }
        row
    }

    fn in_place(&self, text: &mut String) {
        if let Some(home) = &self.home {
            if let Some(hidden) = home_to_tilde(text, home) {
                *text = hidden;
            }
        }
    }
}

/// `text` with every `home` that is a whole path prefix written as `~`;
/// `None` when there is none. `/Users/al` in `/Users/alice` or in
/// `/mnt/Users/al` is a different path and stays.
pub(crate) fn home_to_tilde(text: &str, home: &str) -> Option<String> {
    let is_path_char = |c: char| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    let mut out = String::new();
    let mut copied = 0;
    for (at, _) in text.match_indices(home) {
        let end = at + home.len();
        let starts_a_path = !text[..at].chars().next_back().is_some_and(is_path_char);
        let ends_the_home = text[end..]
            .chars()
            .next()
            .is_none_or(|c| c == '/' || !is_path_char(c));
        if starts_a_path && ends_the_home {
            out.push_str(&text[copied..at]);
            out.push('~');
            copied = end;
        }
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/al";

    fn hidden(text: &str) -> String {
        home_to_tilde(text, HOME).unwrap_or_else(|| text.to_string())
    }

    #[test]
    fn a_path_under_home_starts_with_a_tilde() {
        assert_eq!(hidden("/Users/al/data/x.toml"), "~/data/x.toml");
        assert_eq!(hidden("opened /Users/al/data: ok"), "opened ~/data: ok");
        assert_eq!(
            hidden(r#"{"path":"/Users/al/a","dir":"/Users/al"}"#),
            r#"{"path":"~/a","dir":"~"}"#
        );
        assert_eq!(hidden("'/Users/al/a b' and /Users/al/c"), "'~/a b' and ~/c");
    }

    #[test]
    fn a_longer_name_or_a_deeper_path_is_not_home() {
        assert_eq!(home_to_tilde("/Users/alice/data", HOME), None);
        assert_eq!(home_to_tilde("/mnt/Users/al/data", HOME), None);
        assert_eq!(home_to_tilde("/Users/al.bak/data", HOME), None);
        assert_eq!(home_to_tilde("no path here", HOME), None);
    }

    #[test]
    fn root_and_relative_homes_hide_nothing() {
        let row = LogRow {
            msg: "/x/y".into(),
            ..Default::default()
        };
        assert_eq!(Redactor::for_home(Some("/")).log(row.clone()).msg, "/x/y");
        assert_eq!(Redactor::for_home(Some("x")).log(row.clone()).msg, "/x/y");
        assert_eq!(Redactor::for_home(None).log(row).msg, "/x/y");
    }

    #[test]
    fn a_rows_message_fields_and_step_error_are_all_covered() {
        let r = Redactor::for_home(Some("/Users/al/"));
        let line = r.log(LogRow {
            msg: "read /Users/al/x".into(),
            fields: Some(r#"{"path":"/Users/al/x"}"#.into()),
            ..Default::default()
        });
        assert_eq!(line.msg, "read ~/x");
        assert_eq!(line.fields.as_deref(), Some(r#"{"path":"~/x"}"#));
        let step = r.step(StepRunRow {
            error: Some("no such file /Users/al/x".into()),
            msg: Some("at /Users/al".into()),
            ..Default::default()
        });
        assert_eq!(step.error.as_deref(), Some("no such file ~/x"));
        assert_eq!(step.msg.as_deref(), Some("at ~"));
    }
}
