//! Who a handle is, as the index knows it: every source contact for the
//! person holding it, one per source and key, ranked. A source that is about
//! people (an address book, LinkedIn) says more than a chat that only
//! saw a name, so it comes first; among chats, the one where they wrote
//! the most. The contacts app's own answer ranks above all of these, and
//! is joined in by the UI, never here.
//!
//! A chat's record of a person is one row per document, and its name is
//! decided across all of them by how much was written under each, so one
//! message, forged or not, cannot rename the person everywhere.

use std::collections::BTreeMap;

use datalib_contact_schema::{NormalizedContact, Seen};
use datalib_handle::Handle;

/// One `source_contacts` row reached through one of its handles.
pub struct HandleRow {
    pub handle: String,
    pub contact: NormalizedContact,
}

struct Merged {
    contact: NormalizedContact,
    /// How many items were written under each name.
    weight: BTreeMap<String, u64>,
}

/// Rows for the same handle, source and person — one per document that
/// mentioned them — become one source contact; then each handle's are
/// ranked.
pub fn merge_and_rank(rows: Vec<HandleRow>) -> BTreeMap<String, Vec<NormalizedContact>> {
    let mut merged: BTreeMap<(String, String, String), Merged> = BTreeMap::new();
    for HandleRow {
        handle,
        mut contact,
    } in rows
    {
        contact.names = own_names(&handle, &contact);
        let key = (handle, contact.source_id.clone(), contact.key.clone());
        let entry = merged.entry(key).or_insert_with(|| Merged {
            contact: NormalizedContact {
                names: Vec::new(),
                seen: None,
                ..contact.clone()
            },
            weight: BTreeMap::new(),
        });
        add(entry, contact);
    }
    let mut out: BTreeMap<String, Vec<NormalizedContact>> = BTreeMap::new();
    for (
        (handle, _, _),
        Merged {
            mut contact,
            weight,
        },
    ) in merged
    {
        let mut names: Vec<(String, u64)> = weight.into_iter().collect();
        // Most written under first; a tie by the name itself, so the
        // answer does not hang on the order the rows were read in.
        names.sort_by(|(a, wa), (b, wb)| wb.cmp(wa).then_with(|| a.cmp(b)));
        contact.names = names.into_iter().map(|(n, _)| n).collect();
        out.entry(handle).or_default().push(contact);
    }
    for contacts in out.values_mut() {
        contacts.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.source_id.cmp(&b.source_id))
        });
    }
    out
}

/// One document's account of the person. Its names come most used first
/// (chat-common's `baseline_contacts`), so its first is credited with
/// everything the person wrote there, and at least one, for a document
/// they are only named in; the rest are kept, credited nothing.
fn add(into: &mut Merged, other: NormalizedContact) {
    let items = other.seen.as_ref().map_or(0, |s| s.items);
    for (i, name) in other.names.into_iter().enumerate() {
        let credit = if i == 0 { items.max(1) } else { 0 };
        *into.weight.entry(name).or_insert(0) += credit;
    }
    into.contact.seen = match (into.contact.seen.take(), other.seen) {
        (Some(a), Some(b)) => Some(Seen {
            items: a.items + b.items,
            last_at: a.last_at.max(b.last_at),
        }),
        (a, b) => a.or(b),
    };
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

/// Lower ranks first: a source about people, then the chat where they
/// wrote the most.
fn rank(c: &NormalizedContact) -> (bool, std::cmp::Reverse<u64>) {
    match &c.seen {
        None => (false, std::cmp::Reverse(u64::MAX)),
        Some(s) => (true, std::cmp::Reverse(s.items)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_contact_schema::{ContactHandle, ContactKind};

    fn source_contact(
        source: &str,
        key: &str,
        name: &str,
        seen: Option<(u64, &str)>,
    ) -> NormalizedContact {
        let mut c = NormalizedContact::new(source, key, ContactKind::Person);
        c.names = vec![name.to_string()];
        c.seen = seen.map(|(items, at)| Seen {
            items,
            last_at: Some(at.to_string()),
        });
        c
    }

    fn row(handle: &str, c: NormalizedContact) -> HandleRow {
        HandleRow {
            handle: handle.to_string(),
            contact: c,
        }
    }

    #[test]
    fn one_account_per_source_summed_over_its_documents() {
        let h = "email:riker@enterprise.org";
        let got = merge_and_rank(vec![
            row(
                h,
                source_contact("mail", h, "Will Riker", Some((2, "2369-01-01"))),
            ),
            row(
                h,
                source_contact("mail", h, "Number One", Some((3, "2369-05-01"))),
            ),
        ]);
        let mail = &got[h][0];
        assert_eq!(
            mail.names,
            ["Number One", "Will Riker"],
            "most written first"
        );
        assert_eq!(mail.seen.as_ref().unwrap().items, 5);
        assert_eq!(
            mail.seen.as_ref().unwrap().last_at.as_deref(),
            Some("2369-05-01")
        );
    }

    #[test]
    fn a_source_about_people_outranks_chats_and_chats_rank_by_volume() {
        let h = "tel:+15550101010";
        let got = merge_and_rank(vec![
            row(h, source_contact("whatsapp", h, "Deanna", Some((1, "a")))),
            row(h, source_contact("signal", h, "Counselor", Some((9, "b")))),
            row(
                h,
                source_contact("address_book", "Bridge#troi", "Deanna Troi", None),
            ),
        ]);
        let order: Vec<&str> = got[h].iter().map(|c| c.source_id.as_str()).collect();
        assert_eq!(order, ["address_book", "signal", "whatsapp"]);
    }

    /// One forged message in a mailbox (a spam folder's) must not rename
    /// the sender on all of their mail: ten documents under the real name
    /// outweigh one under another, whichever document is read first.
    #[test]
    fn one_message_cannot_rename_a_sender() {
        let h = "email:riker@enterprise.org";
        let genuine = |_document| {
            row(
                h,
                source_contact("mail", h, "William Riker", Some((1, "2369-01-01"))),
            )
        };
        let forged = || {
            row(
                h,
                source_contact(
                    "mail",
                    h,
                    "PAY INVOICE NOW - Riker",
                    Some((1, "2369-09-09")),
                ),
            )
        };
        for forged_at in [0, 5, 10] {
            let mut rows: Vec<HandleRow> = (0..10).map(genuine).collect();
            rows.insert(forged_at, forged());
            let got = merge_and_rank(rows);
            assert_eq!(
                got[h][0].names,
                ["William Riker", "PAY INVOICE NOW - Riker"],
                "forged row read at {forged_at}"
            );
            assert_eq!(got[h][0].seen.as_ref().unwrap().items, 11);
        }
    }

    /// A tie is settled by the name, not by which row came back first.
    #[test]
    fn a_tie_does_not_hang_on_row_order() {
        let h = "email:riker@enterprise.org";
        let a = || row(h, source_contact("mail", h, "Will Riker", Some((2, "a"))));
        let b = || row(h, source_contact("mail", h, "Number One", Some((2, "b"))));
        let one = merge_and_rank(vec![a(), b()]);
        let other = merge_and_rank(vec![b(), a()]);
        assert_eq!(one[h][0].names, other[h][0].names);
        assert_eq!(one[h][0].names, ["Number One", "Will Riker"]);
    }

    /// A display name that is someone else's address is not a name: a
    /// stranger shown as `"picard@enterprise.org" <q@continuum.test>`
    /// must not be labelled as the account owner. The handle's own
    /// address, or another the record holds, may stand as a name.
    #[test]
    fn a_name_that_is_another_handle_is_dropped() {
        let q = "email:q@continuum.test";
        let mut stranger = source_contact("mail", q, "picard@enterprise.org", Some((25, "a")));
        stranger.names.push("mailto:picard@enterprise.org".into());
        stranger.names.push("+1 202 555 0101".into());
        stranger.names.push("q@continuum.test".into());
        let got = merge_and_rank(vec![row(q, stranger)]);
        assert_eq!(got[q][0].names, ["q@continuum.test"]);

        let troi = "email:troi@enterprise.org";
        let mut book = source_contact("book", "Bridge#troi", "deanna@betazed.test", None);
        book.handles = vec![
            ContactHandle::email(None, "troi@enterprise.org"),
            ContactHandle::email(None, "deanna@betazed.test"),
        ];
        let got = merge_and_rank(vec![row(troi, book)]);
        assert_eq!(
            got[troi][0].names,
            ["deanna@betazed.test"],
            "a handle it holds"
        );
    }
}
