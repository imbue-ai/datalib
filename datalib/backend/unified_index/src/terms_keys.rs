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
    /// A person in any role, an author by the name they were shown
    /// under, or the person a contact's card is about.
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
                .filter(|k| k.is_person() || matches!(k, Kind::Author | Kind::About))
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
    person("bcc", &[], Kinds::These(&[Kind::Bcc])),
    person(
        "recipient",
        &[],
        Kinds::These(&[Kind::To, Kind::Cc, Kind::Bcc]),
    ),
    person("mention", &[], Kinds::These(&[Kind::Mention])),
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
    /// Any of `values` whole, case-blind: a handle (`datalib_handle`'s
    /// spelling, and as typed, for an author shown under an address a
    /// source had no handle for), or a quoted value. On a person key a
    /// quoted name also reaches every handle seen under it (`by_name`).
    Exact {
        values: Vec<String>,
        by_name: Option<String>,
    },
    /// A value holding `text`, case-blind; on a person key, also every
    /// handle seen under a name holding it.
    Partial { text: String, by_name: bool },
    /// A contact in the contacts store, by id: the handles it reaches,
    /// read when the search runs ([`FilterTerm::handles`]).
    ///
    /// [`FilterTerm::handles`]: crate::query::FilterTerm::handles
    Contact(String),
}

/// The prefix of a value naming a contact: `from:contact:<id>`.
pub const CONTACT: &str = "contact:";

/// How a term's value matches: `*` anything; `contact:<id>` that contact;
/// a handle, or a quoted value, whole; anything else in part (`from:Data`
/// finds "Lt. Cmdr. Data", `from:"Data"` only "Data"). A name on a person
/// key also finds the handles that went by it.
pub fn value_of(key: &TermsKey, value: &str, quoted: bool, any: &str) -> TermsValue {
    if value == any && !quoted {
        return TermsValue::Any;
    }
    if let Some(id) = value.strip_prefix(CONTACT).filter(|_| key.person) {
        return TermsValue::Contact(id.to_string());
    }
    let exact =
        |values: Vec<String>, by_name: Option<String>| TermsValue::Exact { values, by_name };
    match handle_of(value).filter(|_| key.person) {
        Some(handle) if handle == value => exact(vec![handle], None),
        Some(handle) => exact(vec![handle, value.to_string()], None),
        None if quoted => exact(
            vec![value.to_string()],
            key.person.then(|| value.to_string()),
        ),
        None => TermsValue::Partial {
            text: value.to_string(),
            by_name: key.person,
        },
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

    /// `with:` is every person kind, an author by name, and who a
    /// contact's card is about; `from:` is not the card.
    #[test]
    fn with_reads_every_person_kind() {
        let kinds = key("with").unwrap().kinds();
        assert!(kinds.contains(&Kind::From) && kinds.contains(&Kind::Cc));
        assert!(kinds.contains(&Kind::Author) && kinds.contains(&Kind::About));
        assert!(!key("from").unwrap().kinds().contains(&Kind::About));
        assert!(!kinds.contains(&Kind::Label) && !kinds.contains(&Kind::Name));
        assert!(kinds.contains(&Kind::Bcc) && kinds.contains(&Kind::Mention));
        assert_eq!(
            key("recipient").unwrap().kinds(),
            [Kind::To, Kind::Cc, Kind::Bcc]
        );
    }

    #[test]
    fn a_handle_matches_exactly_and_anything_else_in_part() {
        let from = key("from").unwrap();
        let exact = |v: &[&str], by_name: Option<&str>| TermsValue::Exact {
            values: v.iter().map(|s| s.to_string()).collect(),
            by_name: by_name.map(String::from),
        };
        assert_eq!(
            value_of(from, "Ann@Example.com", false, "*"),
            exact(&["email:ann@example.com", "Ann@Example.com"], None)
        );
        assert_eq!(
            value_of(from, "email:ann@example.com", false, "*"),
            exact(&["email:ann@example.com"], None)
        );
        assert_eq!(
            value_of(from, "Riker", false, "*"),
            TermsValue::Partial {
                text: "Riker".into(),
                by_name: true
            }
        );
        assert_eq!(
            value_of(from, "Riker", true, "*"),
            exact(&["Riker"], Some("Riker"))
        );
        assert_eq!(value_of(from, "*", false, "*"), TermsValue::Any);
        assert_eq!(value_of(from, "*", true, "*"), exact(&["*"], Some("*")));
        assert_eq!(
            value_of(from, "contact:c-1", false, "*"),
            TermsValue::Contact("c-1".into())
        );
        let label = key("label").unwrap();
        assert_eq!(
            value_of(label, "a@b.c", false, "*"),
            TermsValue::Partial {
                text: "a@b.c".into(),
                by_name: false
            },
            "a label is never a person"
        );
        assert_eq!(value_of(label, "Work", true, "*"), exact(&["Work"], None));
        assert_eq!(
            value_of(label, "contact:c-1", false, "*"),
            TermsValue::Partial {
                text: "contact:c-1".into(),
                by_name: false
            }
        );
    }
}
