//! Who a handle is, as the index knows it: every source's account of the
//! person holding it, one per source, ranked. A source that is about
//! people (an address book, LinkedIn) says more than a chat that only
//! saw a name, so it comes first; among chats, the one where they wrote
//! the most. The contacts app's own answer ranks above all of these, and
//! is joined in by the UI, never here.

use std::collections::BTreeMap;

use datalib_contact_schema::{DatalibContact, Seen};

/// One `source_contacts` row reached through one of its handles.
pub struct HandleRow {
    pub handle: String,
    pub contact: DatalibContact,
}

/// Rows for the same handle, source and person — one per document that
/// mentioned them — become one account; then each handle's accounts are
/// ranked.
pub fn merge_and_rank(rows: Vec<HandleRow>) -> BTreeMap<String, Vec<DatalibContact>> {
    let mut merged: BTreeMap<(String, String, String), DatalibContact> = BTreeMap::new();
    for HandleRow { handle, contact } in rows {
        let key = (handle, contact.source_id.clone(), contact.key.clone());
        match merged.get_mut(&key) {
            None => {
                merged.insert(key, contact);
            }
            Some(have) => merge_into(have, contact),
        }
    }
    let mut out: BTreeMap<String, Vec<DatalibContact>> = BTreeMap::new();
    for ((handle, _, _), contact) in merged {
        out.entry(handle).or_default().push(contact);
    }
    for accounts in out.values_mut() {
        accounts.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.source_id.cmp(&b.source_id))
        });
    }
    out
}

fn merge_into(have: &mut DatalibContact, other: DatalibContact) {
    for name in other.names {
        if !have.names.contains(&name) {
            have.names.push(name);
        }
    }
    have.seen = match (have.seen.take(), other.seen) {
        (Some(a), Some(b)) => Some(Seen {
            items: a.items + b.items,
            last_at: a.last_at.max(b.last_at),
        }),
        (a, b) => a.or(b),
    };
}

/// Lower ranks first: a source about people, then the chat where they
/// wrote the most.
fn rank(c: &DatalibContact) -> (bool, std::cmp::Reverse<u64>) {
    match &c.seen {
        None => (false, std::cmp::Reverse(u64::MAX)),
        Some(s) => (true, std::cmp::Reverse(s.items)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_contact_schema::ContactKind;

    fn account(source: &str, key: &str, name: &str, seen: Option<(u64, &str)>) -> DatalibContact {
        let mut c = DatalibContact::new(source, key, ContactKind::Person);
        c.names = vec![name.to_string()];
        c.seen = seen.map(|(items, at)| Seen {
            items,
            last_at: Some(at.to_string()),
        });
        c
    }

    fn row(handle: &str, c: DatalibContact) -> HandleRow {
        HandleRow {
            handle: handle.to_string(),
            contact: c,
        }
    }

    #[test]
    fn one_account_per_source_summed_over_its_documents() {
        let h = "email:riker@enterprise.org";
        let got = merge_and_rank(vec![
            row(h, account("mail", h, "Will Riker", Some((2, "2369-01-01")))),
            row(h, account("mail", h, "Number One", Some((3, "2369-05-01")))),
        ]);
        let mail = &got[h][0];
        assert_eq!(mail.names, ["Will Riker", "Number One"]);
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
            row(h, account("whatsapp", h, "Deanna", Some((1, "a")))),
            row(h, account("signal", h, "Counselor", Some((9, "b")))),
            row(
                h,
                account("address_book", "Bridge#troi", "Deanna Troi", None),
            ),
        ]);
        let order: Vec<&str> = got[h].iter().map(|c| c.source_id.as_str()).collect();
        assert_eq!(order, ["address_book", "signal", "whatsapp"]);
    }
}
