//! A statement's name as it travels: a comment that opens the statement's
//! text, `/* sql:<name> */`. SQLite hands a statement's text, and nothing
//! else, to the hook that counts its work (`datalib_sql_work`), so that is
//! where the name has to be.
//!
//! A name is lowercase words joined by `_` and `.`. One statement run over
//! many tables may add the table in brackets, `etl.create_table[slack_messages]`.

/// What opens a statement's name.
pub const OPEN: &str = "/* sql:";

/// What closes it.
pub const CLOSE: &str = " */ ";

/// What a statement our code did not write is filed under, with its first
/// word: sqlx's own `BEGIN` and `PRAGMA`s, a statement a person typed into
/// the SQL card.
pub const UNTAGGED: &str = "untagged";

/// The name `sql` opens with, if it opens with one.
pub fn of(sql: &str) -> Option<&str> {
    let rest = sql.trim_start().strip_prefix(OPEN)?;
    let name = &rest[..rest.find(CLOSE.trim_end())?];
    is_name(name).then_some(name)
}

/// `sql` without the name it opens with, if it opens with one.
pub fn after_name(sql: &str) -> &str {
    let trimmed = sql.trim_start();
    match of(trimmed) {
        Some(name) => &trimmed[OPEN.len() + name.len() + CLOSE.trim_end().len()..],
        None => sql,
    }
}

/// The name `sql` is filed under: its own, or for one without, `untagged:`
/// and its first word, lowercased.
pub fn filed_under(sql: &str) -> std::borrow::Cow<'_, str> {
    if let Some(name) = of(sql) {
        return name.into();
    }
    let first: String = sql
        .trim_start_matches(|c: char| !c.is_ascii_alphabetic())
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    format!("{UNTAGGED}:{first}").into()
}

/// The table, index, view or trigger a schema statement makes, drops or
/// alters, as it is spelled there, quotes taken off.
pub fn object_of(ddl: &str) -> Option<&str> {
    let words: Vec<&str> = ddl.split_whitespace().take(8).collect();
    let verb = words.first()?.to_ascii_uppercase();
    if !matches!(verb.as_str(), "CREATE" | "DROP" | "ALTER") {
        return None;
    }
    let mut rest = words[1..].iter().map(|w| w.trim_end_matches('('));
    let mut word = rest.next()?;
    while matches!(
        word.to_ascii_uppercase().as_str(),
        "UNIQUE"
            | "TEMP"
            | "TEMPORARY"
            | "VIRTUAL"
            | "TABLE"
            | "INDEX"
            | "VIEW"
            | "TRIGGER"
            | "IF"
            | "NOT"
            | "EXISTS"
    ) {
        word = rest.next()?;
    }
    let name = word.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']' | '\''));
    let name = name.rsplit('.').next().unwrap_or(name);
    is_qualifier(name).then_some(name)
}

/// Whether `name`, brackets and all, is a name a statement may carry.
pub fn is_name(name: &str) -> bool {
    match name.split_once('[') {
        None => is_id(name),
        Some((id, rest)) => is_id(id) && rest.strip_suffix(']').is_some_and(is_qualifier),
    }
}

/// What may go in a name's brackets: a table's name, never anything that
/// could close the comment or the brackets.
pub fn is_qualifier(q: &str) -> bool {
    !q.is_empty()
        && q.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

/// Whether `id` is lowercase words joined by `_` and `.`. A `const fn`, so
/// `sql!` refuses a bad name when it compiles.
pub const fn is_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;
    let mut word_start = true;
    while i < b.len() {
        let c = b[i];
        if word_start {
            if !c.is_ascii_lowercase() {
                return false;
            }
            word_start = false;
        } else if c == b'.' {
            word_start = true;
        } else if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_') {
            return false;
        }
        i += 1;
    }
    !word_start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_is_filed_under_the_name_it_opens_with() {
        assert_eq!(
            of("/* sql:runs.latest_metric */ SELECT 1"),
            Some("runs.latest_metric")
        );
        assert_eq!(of("\n   /* sql:a_b.c2 */\n SELECT 1"), Some("a_b.c2"));
        assert_eq!(
            of("/* sql:etl.upsert[slack_messages] */ INSERT INTO slack_messages"),
            Some("etl.upsert[slack_messages]")
        );
    }

    #[test]
    fn a_name_is_lowercase_words() {
        for bad in [
            "Runs",
            "runs latest",
            ".runs",
            "runs.",
            "runs..x",
            "9runs",
            "runs[]",
            "runs[a b]",
            "runs[a*/]",
        ] {
            assert!(!is_name(bad), "{bad}");
        }
        assert!(is_name("runs.latest_metric_2"));
        assert!(is_name("etl.create_table[grid-rows]"));
    }

    #[test]
    fn only_the_opening_comment_names_a_statement() {
        assert_eq!(of("SELECT 1 /* sql:runs */"), None);
        assert_eq!(of("/* other */ /* sql:runs */ SELECT 1"), None);
        assert_eq!(of("/* sql:runs*/ SELECT 1"), None);
    }

    #[test]
    fn the_text_after_the_name_is_the_statement() {
        assert_eq!(after_name("/* sql:x.y */ SELECT 1"), " SELECT 1");
        assert_eq!(after_name("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn a_statement_without_a_name_is_filed_by_its_first_word() {
        assert_eq!(filed_under("BEGIN IMMEDIATE"), "untagged:begin");
        assert_eq!(filed_under("  pragma foreign_keys = ON"), "untagged:pragma");
        assert_eq!(filed_under("/* sql:x.y */ SELECT 1"), "x.y");
    }
}
