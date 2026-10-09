//! The search bar's keys that read the search terms rather than a
//! `grid_rows` column: a person in one role or in any, and a label
//! (docs/dev/plans/search_autocomplete.md § "The keys"). The terms file
//! is attached to the grid's reader under [`ATTACHED_AS`], so a term on
//! one of these keys is one more clause of the grid's own query.

use datalib_handle::Handle;
use datalib_schema::search_terms::SearchTermKind as Kind;

/// The schema name the search terms file is attached under.
pub const ATTACHED_AS: &str = "search_terms";

/// Which kinds of term a key reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kinds {
    These(&'static [Kind]),
    /// A person in any role, or an author by the name they were shown
    /// under.
    AnyPerson,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TermsKey {
    pub key: &'static str,
    /// Other spellings it reads: older keys people have typed and saved.
    pub aliases: &'static [&'static str],
    pub kinds: Kinds,
    /// Its values are people: a handle matches exactly and is drawn as a
    /// chip.
    pub person: bool,
}

impl TermsKey {
    pub fn kinds(&self) -> Vec<Kind> {
        use strum::VariantArray;
        match self.kinds {
            Kinds::These(kinds) => kinds.to_vec(),
            Kinds::AnyPerson => Kind::VARIANTS
                .iter()
                .copied()
                .filter(|k| k.is_person() || *k == Kind::Author)
                .collect(),
        }
    }
}

const fn person(key: &'static str, aliases: &'static [&'static str], kinds: Kinds) -> TermsKey {
    TermsKey {
        key,
        aliases,
        kinds,
        person: true,
    }
}

/// `author:` and `author_handle:` were `grid_rows` columns' keys, exact
/// on the name or the handle; they are `from:` now, which reads both.
pub const TERMS_KEYS: &[TermsKey] = &[
    person(
        "from",
        &["author", "author_handle"],
        Kinds::These(&[Kind::From, Kind::Author]),
    ),
    person("to", &[], Kinds::These(&[Kind::To])),
    person("cc", &[], Kinds::These(&[Kind::Cc])),
    person("recipient", &[], Kinds::These(&[Kind::To, Kind::Cc])),
    person("with", &["involves"], Kinds::AnyPerson),
    TermsKey {
        key: "label",
        aliases: &[],
        kinds: Kinds::These(&[Kind::Label]),
        person: false,
    },
];

/// The terms key a person typed, by its name or an alias.
pub fn key(typed: &str) -> Option<&'static TermsKey> {
    TERMS_KEYS
        .iter()
        .find(|k| k.key == typed || k.aliases.contains(&typed))
}

/// How a value on a terms key matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermsValue {
    /// `*`: any term of the key's kinds.
    Any,
    /// A handle, as `datalib_handle` spells it, and the value as typed:
    /// the handle, or an author shown under exactly that text (an address
    /// a source had no handle for).
    Exact { handle: String, typed: String },
    /// Anything else: a value holding it, case-blind.
    Partial(String),
}

pub fn value_of(key: &TermsKey, value: &str, any: &str) -> TermsValue {
    if value == any {
        return TermsValue::Any;
    }
    match handle_of(value).filter(|_| key.person) {
        Some(handle) => TermsValue::Exact {
            handle,
            typed: value.to_string(),
        },
        None => TermsValue::Partial(value.to_string()),
    }
}

/// `word` as a handle, when it is one: spelled as one (`email:…`,
/// `slack:T/U`), an email address, or a number with its country code.
pub fn handle_of(word: &str) -> Option<String> {
    let handle = if word.contains(':') {
        Handle::rebuild(word)
    } else if word.contains('@') {
        Handle::email(word)
    } else if word.starts_with('+') {
        Handle::tel(word)
    } else {
        None
    };
    handle.map(|h| h.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_author_keys_are_from_now() {
        for typed in ["from", "author", "author_handle"] {
            assert_eq!(key(typed).map(|k| k.key), Some("from"), "{typed}");
        }
        assert_eq!(key("involves").map(|k| k.key), Some("with"));
        assert_eq!(key("channel"), None);
    }

    /// `with:` is every person kind, and an author by name.
    #[test]
    fn with_reads_every_person_kind() {
        let kinds = key("with").unwrap().kinds();
        assert!(kinds.contains(&Kind::From) && kinds.contains(&Kind::Cc));
        assert!(kinds.contains(&Kind::Author));
        assert!(!kinds.contains(&Kind::Label) && !kinds.contains(&Kind::Name));
        assert_eq!(key("recipient").unwrap().kinds(), [Kind::To, Kind::Cc]);
    }

    #[test]
    fn a_handle_matches_exactly_and_anything_else_in_part() {
        let from = key("from").unwrap();
        assert_eq!(
            value_of(from, "Ann@Example.com", "*"),
            TermsValue::Exact {
                handle: "email:ann@example.com".into(),
                typed: "Ann@Example.com".into()
            }
        );
        assert!(matches!(
            value_of(from, "email:ann@example.com", "*"),
            TermsValue::Exact { handle, .. } if handle == "email:ann@example.com"
        ));
        assert_eq!(
            value_of(from, "Riker", "*"),
            TermsValue::Partial("Riker".into())
        );
        assert_eq!(value_of(from, "*", "*"), TermsValue::Any);
        let label = key("label").unwrap();
        assert_eq!(
            value_of(label, "a@b.c", "*"),
            TermsValue::Partial("a@b.c".into()),
            "a label is never a person"
        );
    }
}
