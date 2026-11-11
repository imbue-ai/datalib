//! What a source's index step owes: the documents whose rendered text the
//! index does not hold, and the ones it holds that the source no longer has.

use std::collections::BTreeMap;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Owed {
    pub put: Vec<String>,
    pub remove: Vec<String>,
}

/// Both maps are `markdown_uuid → content_hash`: the source's documents as
/// rendered, and as the index last took them.
pub fn owed(rendered: &BTreeMap<String, String>, indexed: &BTreeMap<String, String>) -> Owed {
    Owed {
        put: rendered
            .iter()
            .filter(|(uuid, hash)| indexed.get(*uuid) != Some(*hash))
            .map(|(uuid, _)| uuid.clone())
            .collect(),
        remove: indexed
            .keys()
            .filter(|uuid| !rendered.contains_key(*uuid))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn new_and_changed_are_put_and_gone_are_removed() {
        let rendered = map(&[("kept", "h1"), ("changed", "h3"), ("new", "h4")]);
        let indexed = map(&[("kept", "h1"), ("changed", "h2"), ("gone", "h5")]);
        assert_eq!(
            owed(&rendered, &indexed),
            Owed {
                put: vec!["changed".into(), "new".into()],
                remove: vec!["gone".into()],
            }
        );
    }

    #[test]
    fn an_index_in_step_owes_nothing() {
        let both = map(&[("a", "h1"), ("b", "h2")]);
        assert_eq!(owed(&both, &both), Owed::default());
    }
}
