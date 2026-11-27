//! Who a handle is, as the index knows it: every source contact for the
//! person holding it, one per source and key, ranked. A source that is about
//! people (an address book, LinkedIn) says more than a chat that only
//! saw a name, so it comes first; among chats, the one where they wrote
//! the most. The contacts app's own answer ranks above all of these, and
//! is joined in by the UI, never here.
//!
//! How much someone wrote is the grid's rows, the one count: what `from:`
//! finds (docs/dev/contacts.md §"How much someone wrote"). A chat
//! describes a person once per document, and its name is decided across
//! all of them by how much was written under each, so one message, forged
//! or not, cannot rename the person everywhere.

use std::collections::{BTreeMap, HashMap};

use datalib_contact_schema::{NormalizedContact, Seen};
use datalib_handle::Handle;
use serde::Serialize;

/// One of the index's contact records reached through one of its handles.
#[derive(Clone)]
pub struct HandleRow {
    pub handle: String,
    pub contact: NormalizedContact,
    /// What its first name is credited with: the grid's rows its handles
    /// wrote in each document that carries it, and at least one for a
    /// document they wrote nothing in.
    pub credit: u64,
    /// A document that carries it is a contact's own page: the record is a
    /// source about people, an address book's card or a profile.
    pub about: bool,
}

/// How much one handle wrote in one source: its rows there, and the
/// newest one's stamp.
pub type Authored = HashMap<(String, String), Seen>;

/// A source's record of a person, with how much of that source they
/// wrote, as `POST /people` answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Counted {
    #[serde(flatten)]
    pub contact: NormalizedContact,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen: Option<Seen>,
    /// For ranking; the record's documents say it, not the record.
    #[serde(skip)]
    about: bool,
}

/// What the index read about some people, one row per distinct record.
#[derive(Default)]
pub struct PeopleRows {
    /// The records holding each handle.
    pub holding: Vec<HandleRow>,
    /// The records that write a number asked about without its country
    /// code, for that number: a likely match, by its digits alone.
    pub same_digits: Vec<HandleRow>,
    /// The record of each contact's page, for the page's id.
    pub pages: Vec<HandleRow>,
    /// Every full number any of these records' numbers may be.
    pub full_numbers: Vec<Handle>,
    /// How much each of these records' handles wrote in its source.
    pub authored: Authored,
}

/// Who each handle and each contact's page is, as `POST /people` answers.
#[derive(Debug, Default, Serialize)]
pub struct People {
    /// Each handle's records, ranked. One no source mentions is absent.
    pub people: BTreeMap<String, Vec<Counted>>,
    /// Each number's records that write it without its country code.
    pub same_digits: BTreeMap<String, Vec<Counted>>,
    /// Each contact's page, by its id.
    pub pages: BTreeMap<String, Counted>,
    /// Each number a record above writes without its country code, as it
    /// wrote it, with the full numbers a source knows that it may be.
    pub full_numbers: BTreeMap<String, Vec<Handle>>,
}

impl People {
    pub fn of(rows: PeopleRows) -> Self {
        let mut full_numbers = BTreeMap::new();
        for row in [&rows.holding, &rows.same_digits, &rows.pages]
            .into_iter()
            .flatten()
        {
            for h in &row.contact.handles {
                let Some(digits) = h.home_digits() else {
                    continue;
                };
                let may_be: Vec<Handle> = rows
                    .full_numbers
                    .iter()
                    .filter(|full| full.may_be(&digits))
                    .cloned()
                    .collect();
                if !may_be.is_empty() {
                    full_numbers.insert(h.value.clone(), may_be);
                }
            }
        }
        Self {
            people: merge_and_rank(rows.holding, &rows.authored),
            same_digits: merge_and_rank(rows.same_digits, &rows.authored),
            pages: merge_and_rank(rows.pages, &rows.authored)
                .into_iter()
                .filter_map(|(page, contacts)| Some((page, contacts.into_iter().next()?)))
                .collect(),
            full_numbers,
        }
    }
}

/// The records reached by digits a number ends in, each given the handles
/// of `numbers` its own digits may be, less any the record holds outright.
pub fn for_each_number(numbers: &[Handle], by_digits: Vec<HandleRow>) -> Vec<HandleRow> {
    by_digits
        .iter()
        .flat_map(|row| {
            numbers
                .iter()
                .filter(|number| number.may_be(&row.handle) && !row.contact.holds(number))
                .map(|number| HandleRow {
                    handle: number.as_str().to_string(),
                    ..row.clone()
                })
        })
        .collect()
}

struct Merged {
    contact: NormalizedContact,
    /// How many items were written under each name.
    weight: BTreeMap<String, u64>,
    about: bool,
}

/// Records for the same handle, source and person — a chat's differ by
/// the names it showed — become one source contact, with how much of the
/// source its handles wrote; then each handle's are ranked.
fn merge_and_rank(rows: Vec<HandleRow>, authored: &Authored) -> BTreeMap<String, Vec<Counted>> {
    let mut merged: BTreeMap<(String, String, String), Merged> = BTreeMap::new();
    for HandleRow {
        handle,
        mut contact,
        credit,
        about,
    } in rows
    {
        contact.names = own_names(&handle, &contact);
        let key = (handle, contact.source_id.clone(), contact.key.clone());
        let entry = merged.entry(key).or_insert_with(|| Merged {
            contact: NormalizedContact {
                names: Vec::new(),
                ..contact.clone()
            },
            weight: BTreeMap::new(),
            about: false,
        });
        entry.about |= about;
        add(entry, contact, credit);
    }
    let mut out: BTreeMap<String, Vec<Counted>> = BTreeMap::new();
    for (
        (handle, _, _),
        Merged {
            mut contact,
            weight,
            about,
        },
    ) in merged
    {
        let mut names: Vec<(String, u64)> = weight.into_iter().collect();
        // Most written under first; a tie by the name itself, so the
        // answer does not hang on the order the rows were read in.
        names.sort_by(|(a, wa), (b, wb)| wb.cmp(wa).then_with(|| a.cmp(b)));
        contact.names = names.into_iter().map(|(n, _)| n).collect();
        let seen = seen_in_source(&contact, authored);
        out.entry(handle).or_default().push(Counted {
            contact,
            seen,
            about,
        });
    }
    for contacts in out.values_mut() {
        contacts.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.contact.source_id.cmp(&b.contact.source_id))
        });
    }
    out
}

/// How much of its source the record's handles wrote; `None` for nothing.
fn seen_in_source(contact: &NormalizedContact, authored: &Authored) -> Option<Seen> {
    let mut seen: Option<Seen> = None;
    for h in contact.handles.iter().filter_map(|h| h.handle.as_ref()) {
        let key = (h.as_str().to_string(), contact.source_id.clone());
        let Some(wrote) = authored.get(&key) else {
            continue;
        };
        seen = Some(match seen {
            None => wrote.clone(),
            Some(s) => Seen {
                items: s.items + wrote.items,
                last_at: s.last_at.max(wrote.last_at.clone()),
            },
        });
    }
    seen.filter(|s| s.items > 0)
}

/// One record of the person. Its names come most used first (chat-common's
/// `baseline_contacts`), so its first takes the credit; the rest are kept,
/// credited nothing.
fn add(into: &mut Merged, other: NormalizedContact, credit: u64) {
    for (i, name) in other.names.into_iter().enumerate() {
        let credit = if i == 0 { credit } else { 0 };
        *into.weight.entry(name).or_insert(0) += credit;
    }
}

/// The names that are names: one that reads as an address or another
/// handle the record does not hold (`"picard@enterprise.org"
/// <q@continuum.test>`) would draw the person as someone else.
/// The render drops these too (`datalib_etl_render::names`); this keeps
/// a store rendered before it from showing them.
fn own_names(handle: &str, contact: &NormalizedContact) -> Vec<String> {
    let held = |h: &Handle| h.as_str() == handle || contact.holds(h);
    contact
        .names
        .iter()
        .filter(|name| !Handle::spelled_by(name).is_some_and(|h| !held(&h)))
        .cloned()
        .collect()
}

/// Lower ranks first: a source about people, then the one where they
/// wrote the most.
fn rank(c: &Counted) -> (bool, std::cmp::Reverse<u64>) {
    let items = c.seen.as_ref().map_or(0, |s| s.items);
    (!c.about, std::cmp::Reverse(items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_contact_schema::{ContactHandle, ContactKind};

    fn source_contact(source: &str, key: &str, name: &str) -> NormalizedContact {
        let mut c = NormalizedContact::new(source, key, ContactKind::Person);
        c.names = vec![name.to_string()];
        if let Some(h) = Handle::parse(key) {
            c.handles = vec![ContactHandle::of(h)];
        }
        c
    }

    /// A chat's record, its first name credited with `credit`.
    fn row(handle: &str, c: NormalizedContact, credit: u64) -> HandleRow {
        HandleRow {
            handle: handle.to_string(),
            credit,
            about: false,
            contact: c,
        }
    }

    fn wrote(entries: &[(&str, &str, u64, &str)]) -> Authored {
        entries
            .iter()
            .map(|(h, source, items, at)| {
                let seen = Seen {
                    items: *items,
                    last_at: Some(at.to_string()),
                };
                ((h.to_string(), source.to_string()), seen)
            })
            .collect()
    }

    /// One record per name a chat showed is one source contact, its count
    /// the grid's rows by its handle in that source.
    #[test]
    fn one_account_per_source_counted_from_the_grid() {
        let h = "email:riker@enterprise.org";
        let got = merge_and_rank(
            vec![
                row(h, source_contact("mail", h, "Will Riker"), 2),
                row(h, source_contact("mail", h, "Number One"), 3),
            ],
            &wrote(&[(h, "mail", 5, "2369-05-01"), (h, "slack", 9, "2370-01-01")]),
        );
        let mail = &got[h][0];
        assert_eq!(
            mail.contact.names,
            ["Number One", "Will Riker"],
            "most written first"
        );
        assert_eq!(
            mail.seen,
            Some(Seen {
                items: 5,
                last_at: Some("2369-05-01".into())
            }),
            "this source's rows only"
        );
    }

    #[test]
    fn a_source_about_people_outranks_chats_and_chats_rank_by_volume() {
        let h = "tel:+15550101010";
        let mut book = source_contact("address_book", "Bridge#troi", "Deanna Troi");
        book.handles = vec![ContactHandle::of(Handle::parse(h).unwrap())];
        let got = merge_and_rank(
            vec![
                row(h, source_contact("whatsapp", h, "Deanna"), 1),
                row(h, source_contact("signal", h, "Counselor"), 9),
                HandleRow {
                    about: true,
                    ..row(h, book, 1)
                },
            ],
            &wrote(&[(h, "whatsapp", 1, "a"), (h, "signal", 9, "b")]),
        );
        let order: Vec<&str> = got[h]
            .iter()
            .map(|c| c.contact.source_id.as_str())
            .collect();
        assert_eq!(order, ["address_book", "signal", "whatsapp"]);
    }

    /// One forged message in a mailbox (a spam folder's) must not rename
    /// the sender on all of their mail: ten documents under the real name
    /// outweigh one under another, whichever document is read first.
    #[test]
    fn one_message_cannot_rename_a_sender() {
        let h = "email:riker@enterprise.org";
        let genuine = |_document| row(h, source_contact("mail", h, "William Riker"), 1);
        let forged = || row(h, source_contact("mail", h, "PAY INVOICE NOW - Riker"), 1);
        for forged_at in [0, 5, 10] {
            let mut rows: Vec<HandleRow> = (0..10).map(genuine).collect();
            rows.insert(forged_at, forged());
            let got = merge_and_rank(rows, &wrote(&[(h, "mail", 11, "2369-09-09")]));
            assert_eq!(
                got[h][0].contact.names,
                ["William Riker", "PAY INVOICE NOW - Riker"],
                "forged row read at {forged_at}"
            );
        }
    }

    /// A tie is settled by the name, not by which row came back first.
    #[test]
    fn a_tie_does_not_hang_on_row_order() {
        let h = "email:riker@enterprise.org";
        let a = || row(h, source_contact("mail", h, "Will Riker"), 2);
        let b = || row(h, source_contact("mail", h, "Number One"), 2);
        let one = merge_and_rank(vec![a(), b()], &Authored::new());
        let other = merge_and_rank(vec![b(), a()], &Authored::new());
        assert_eq!(one[h][0].contact.names, other[h][0].contact.names);
        assert_eq!(one[h][0].contact.names, ["Number One", "Will Riker"]);
    }

    /// A display name that is someone else's address is not a name: a
    /// stranger shown as `"picard@enterprise.org" <q@continuum.test>`
    /// must not be labelled as the account owner. The handle's own
    /// address, or another the record holds, may stand as a name.
    #[test]
    fn a_name_that_is_another_handle_is_dropped() {
        let q = "email:q@continuum.test";
        let mut stranger = source_contact("mail", q, "picard@enterprise.org");
        stranger.names.push("mailto:picard@enterprise.org".into());
        stranger.names.push("+1 202 555 0101".into());
        stranger.names.push("q@continuum.test".into());
        let got = merge_and_rank(vec![row(q, stranger, 25)], &Authored::new());
        assert_eq!(got[q][0].contact.names, ["q@continuum.test"]);

        let troi = "email:troi@enterprise.org";
        let mut book = source_contact("book", "Bridge#troi", "deanna@betazed.test");
        book.handles = vec![
            ContactHandle::email(None, "troi@enterprise.org"),
            ContactHandle::email(None, "deanna@betazed.test"),
        ];
        let got = merge_and_rank(vec![row(troi, book, 1)], &Authored::new());
        assert_eq!(
            got[troi][0].contact.names,
            ["deanna@betazed.test"],
            "a handle it holds"
        );
    }
}
