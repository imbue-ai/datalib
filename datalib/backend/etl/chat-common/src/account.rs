//! What the grid's Account column shows for one login. Every provider
//! that knows who its mirror belongs to resolves the upstream id
//! through this, so the column reads the same way everywhere.

/// The email first — an account is a login, and two people in one org
/// can share a name — then the name, and the provider's own id only
/// when nothing better is stored. `None` when there is no id at all:
/// a blank cell says "unknown", an id says "this one, unresolved".
pub fn account_label(id: &str, email: Option<&str>, name: Option<&str>) -> Option<String> {
    [email, name, Some(id)]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::account_label;

    #[test]
    fn prefers_email_then_name_then_id() {
        assert_eq!(
            account_label("u1", Some("jlp@x.test"), Some("Picard")).as_deref(),
            Some("jlp@x.test")
        );
        assert_eq!(
            account_label("u1", None, Some("Picard")).as_deref(),
            Some("Picard")
        );
        assert_eq!(account_label("u1", None, None).as_deref(), Some("u1"));
    }

    /// A blank email has to fall through to the name, not stop there:
    /// `.or_else()` on an `Option<String>` only fires on `None`, which is
    /// the bug the Claude resolver shipped with.
    #[test]
    fn blank_is_absent() {
        assert_eq!(
            account_label("u1", Some("  "), Some("Data")).as_deref(),
            Some("Data")
        );
        assert_eq!(account_label("", Some(""), None), None);
    }
}
