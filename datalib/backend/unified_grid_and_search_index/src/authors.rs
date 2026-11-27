//! Everyone the grid's rows were written by, and how much each wrote: the
//! one count of what someone wrote, which `from:` finds too
//! (docs/dev/contacts.md §"How much someone wrote"). The contacts list
//! sorts by it, and its Unfiled tab is everyone here who is on no contact.

use std::collections::{BTreeMap, BTreeSet};

use datalib_contact_schema::Seen;
use datalib_handle::{Handle, HandleKind};
use serde::Serialize;

/// What the index answers: how many rows `handle` wrote under `author`,
/// and the newest one's stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorRow {
    pub handle: String,
    pub author: Option<String>,
    pub items: u64,
    pub last_at: Option<String>,
}

/// One handle that wrote something, under the name it wrote the most as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Author {
    pub handle: String,
    pub name: Option<String>,
    pub items: u64,
    pub last_at: Option<String>,
}

/// Each handle once, its rows summed over the names it wrote under; its
/// name the one with the most rows, a tie going to the name that sorts
/// first. A name shown with its own address (`Will Riker
/// <riker@enterprise.org>`) is the name before it.
pub fn authors(rows: Vec<AuthorRow>) -> Vec<Author> {
    let mut by_handle: BTreeMap<String, (Author, BTreeMap<String, u64>)> = BTreeMap::new();
    for row in rows {
        let (author, names) = by_handle.entry(row.handle.clone()).or_insert_with(|| {
            let author = Author {
                handle: row.handle.clone(),
                name: None,
                items: 0,
                last_at: None,
            };
            (author, BTreeMap::new())
        });
        author.items += row.items;
        author.last_at = author.last_at.take().max(row.last_at);
        if let Some(name) = row.author.as_deref().map(|n| bare_name(n, &row.handle)) {
            if !name.is_empty() {
                *names.entry(name.to_string()).or_insert(0) += row.items;
            }
        }
    }
    by_handle
        .into_values()
        .map(|(mut author, names)| {
            author.name = names
                .into_iter()
                .max_by(|(a, na), (b, nb)| na.cmp(nb).then_with(|| b.cmp(a)))
                .map(|(name, _)| name);
            author
        })
        .collect()
}

fn bare_name<'a>(shown: &'a str, handle: &str) -> &'a str {
    let shown = shown.trim();
    let value = handle.split_once(':').map_or(handle, |(_, v)| v);
    let name = match shown.strip_suffix('>').and_then(|s| s.rsplit_once('<')) {
        Some((before, inside)) if inside.eq_ignore_ascii_case(value) => before.trim(),
        _ => shown,
    };
    name.strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .unwrap_or(name)
}

/// How much `handles` wrote between them, `None` for nothing.
pub fn wrote<'a>(
    handles: impl IntoIterator<Item = &'a str>,
    by_handle: &BTreeMap<&str, &Author>,
) -> Option<Seen> {
    let mut seen = Seen {
        items: 0,
        last_at: None,
    };
    for h in handles {
        if let Some(a) = by_handle.get(h) {
            seen.items += a.items;
            seen.last_at = seen.last_at.take().max(a.last_at.clone());
        }
    }
    (seen.items > 0).then_some(seen)
}

/// What says someone is filed already, or is nobody to file.
#[derive(Debug, Default)]
pub struct Filed {
    /// Every handle on a contact.
    pub held: BTreeSet<String>,
    /// The handles of the library's own accounts: the person whose
    /// library this is.
    pub accounts: BTreeSet<String>,
    /// Handles a person or an agent said are not real (spam).
    pub not_real: BTreeSet<String>,
}

/// One author on no contact, and whether they were marked not real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unfiled<'a> {
    pub author: &'a Author,
    pub not_real: bool,
}

/// The authors on no contact: not the library's owner, nor a model or
/// the stand-in `me`, and, unless `with_not_real`, nobody marked not
/// real. Most rows first, then the newest, then by handle.
pub fn unfiled<'a>(authors: &'a [Author], filed: &Filed, with_not_real: bool) -> Vec<Unfiled<'a>> {
    let mut out: Vec<Unfiled<'a>> = authors
        .iter()
        .filter(|a| {
            let kind = Handle::parse(&a.handle).map(|h| h.kind());
            !matches!(kind, Some(HandleKind::Me | HandleKind::AiModel))
                && !filed.held.contains(&a.handle)
                && !filed.accounts.contains(&a.handle)
        })
        .map(|a| Unfiled {
            author: a,
            not_real: filed.not_real.contains(&a.handle),
        })
        .filter(|u| with_not_real || !u.not_real)
        .collect();
    out.sort_by(|a, b| {
        b.author
            .items
            .cmp(&a.author.items)
            .then_with(|| b.author.last_at.cmp(&a.author.last_at))
            .then_with(|| a.author.handle.cmp(&b.author.handle))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(handle: &str, author: &str, items: u64, last_at: &str) -> AuthorRow {
        AuthorRow {
            handle: handle.into(),
            author: Some(author.into()),
            items,
            last_at: Some(last_at.into()),
        }
    }

    const RIKER: &str = "email:riker@enterprise.org";

    #[test]
    fn a_handle_is_summed_over_its_names_and_goes_by_the_one_most_written_under() {
        let got = authors(vec![
            row(RIKER, "Will Riker <riker@enterprise.org>", 3, "2369-01-01"),
            row(RIKER, "Number One", 2, "2369-05-01"),
            row(RIKER, "\"Will Riker\"", 1, "2368-01-01"),
        ]);
        assert_eq!(
            got,
            [Author {
                handle: RIKER.into(),
                name: Some("Will Riker".into()),
                items: 6,
                last_at: Some("2369-05-01".into()),
            }]
        );
    }

    /// Unfiled is who wrote and is nobody's yet: not anyone on a contact,
    /// not the library's owner or a model, and a sender marked not real
    /// only when asked for.
    #[test]
    fn unfiled_leaves_out_contacts_the_owner_models_and_spam() {
        let all = authors(vec![
            row(RIKER, "Will Riker", 3, "2369-01-01"),
            row("email:troi@enterprise.org", "Deanna Troi", 5, "2369-01-01"),
            row(
                "email:picard@enterprise.org",
                "Jean-Luc Picard",
                9,
                "2369-01-01",
            ),
            row(
                "email:prizes@quarks.example",
                "Quark's Bar",
                7,
                "2369-01-01",
            ),
            row("email:worf@enterprise.org", "Worf", 5, "2370-01-01"),
            row("me:self", "Me", 40, "2369-01-01"),
            row("ai_model:claude", "Claude", 30, "2369-01-01"),
        ]);
        let filed = Filed {
            held: [RIKER.to_string()].into(),
            accounts: ["email:picard@enterprise.org".to_string()].into(),
            not_real: ["email:prizes@quarks.example".to_string()].into(),
        };
        let shown = |with_not_real| -> Vec<(String, bool)> {
            unfiled(&all, &filed, with_not_real)
                .into_iter()
                .map(|u| (u.author.handle.clone(), u.not_real))
                .collect()
        };
        assert_eq!(
            shown(false),
            [
                ("email:worf@enterprise.org".to_string(), false),
                ("email:troi@enterprise.org".to_string(), false),
            ],
            "most rows first, the newest breaking a tie"
        );
        assert_eq!(
            shown(true)[0],
            ("email:prizes@quarks.example".to_string(), true)
        );
    }

    #[test]
    fn handles_wrote_between_them_what_each_did() {
        let all = authors(vec![
            row(RIKER, "Will Riker", 3, "2369-01-01"),
            row("slack:T1/U_RIKER", "Riker", 4, "2369-06-01"),
        ]);
        let by_handle: BTreeMap<&str, &Author> =
            all.iter().map(|a| (a.handle.as_str(), a)).collect();
        assert_eq!(
            wrote([RIKER, "slack:T1/U_RIKER", "tel:+15550101"], &by_handle),
            Some(Seen {
                items: 7,
                last_at: Some("2369-06-01".into())
            })
        );
        assert_eq!(wrote(["tel:+15550101"], &by_handle), None);
    }
}
