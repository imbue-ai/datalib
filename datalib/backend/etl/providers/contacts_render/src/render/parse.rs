//! Raw doltlite store → in-memory [`ParsedContacts`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use datalib_contact_schema::{ContactHandle, Detail, Photo};

use datalib_etl_contacts::ingest::api::{
    split_vcards, vcard_all, vcard_categories, vcard_created, vcard_is_group, vcard_members,
    vcard_n_family_given, vcard_rev, vcard_uid, VcardProp,
};
use datalib_etl_contacts::ingest::db::{LoadedRawContact, RawDb};
use datalib_etl_contacts::ingest::photo::card_photo;
use datalib_etl_render::inputs::{changed_rows, Input, RawRange};
use datalib_etl_render::photo;

use super::google;

/// One card, whichever way it was stored, with everything render cares
/// about pulled out of the raw row.
#[derive(Debug, Clone)]
pub struct ParsedContact {
    /// The vCard's `UID:`, or Google's resource name (`people/c1701`). A
    /// card without a `UID:` (every card of a Google export) takes the id
    /// its download gave its row, from the name or the file position;
    /// that id is the card's alone, where the row's href is shared by a
    /// whole file.
    pub uid: String,
    /// Addressbook label — the file stem of the `.vcf` this contact
    /// came from. Shows up on grid rows as `channel` and groups
    /// contacts together in the UI.
    pub addressbook: String,
    /// The `addressbooks` row the card is in: upstream's id for the
    /// address book, which a rename keeps. `None` for a row that names
    /// none.
    pub addressbook_id: Option<String>,
    /// `FN` (formatted name). `None` for nameless cards — the
    /// render path falls back to the UID.
    pub display_name: Option<String>,
    /// The other names the card gives.
    pub other_names: Vec<String>,
    /// When the card was last changed (`REV:`), as written. Render uses
    /// it for `modified_at`; null when absent.
    pub revision: Option<String>,
    /// `CREATED:`, which Fastmail writes; render's `created_at`.
    pub created: Option<String>,
    /// A contact group, whose `members` name the cards in it.
    pub is_group: bool,
    pub members: Vec<String>,
    /// The groups it is in that have no card of their own: a vCard's
    /// `CATEGORIES`, as written, which is how Google's export files a
    /// contact under its labels.
    pub categories: Vec<String>,
    /// Multi-valued properties surfaced in document order.
    pub emails: Vec<ContactHandle>,
    pub phones: Vec<ContactHandle>,
    /// Its addresses and anything else it says.
    pub details: Vec<Detail>,
    /// The company, then its units.
    pub org: Vec<String>,
    pub title: Option<String>,
    pub note: Option<String>,
    /// The blake3 of the photo in the blob store, where the download put
    /// it. `None` for a vCard's URL-only photo, which is `photo_url`.
    pub photo: Option<String>,
    pub photo_url: Option<String>,
    /// The contact's page at the service, where it has one.
    pub source_url: Option<String>,
    /// The raw rows this card came from: its own row and the
    /// `addressbooks` row its label is read off.
    pub inputs: Vec<Input>,
}

#[derive(Debug, Default)]
pub struct ParsedContacts {
    pub contacts: Vec<ParsedContact>,
    /// The signed-in account, where the source has one.
    pub login: Option<String>,
    /// The rows the diff names as changed since the cursor, per table.
    /// `None` when there is no cursor: everything renders. A changed row
    /// maps to its cards through `ParsedContact::inputs`, which is how a
    /// row holding several cards names each of them.
    pub changed: Option<std::collections::HashMap<String, std::collections::HashSet<String>>>,
    /// The commit this parse read.
    pub head: Option<String>,
    /// Each card photo the blob store holds, by hash. A card's photo
    /// missing here is one the store lacks.
    pub held_photos: HashMap<String, Photo>,
}

/// The raw tables a card's `inputs` name rows of.
const INPUT_TABLES: &[&str] = &[
    "accounts",
    "addressbooks",
    "contacts",
    "google_people",
    "google_contact_groups",
    "google_photos",
];

/// Load every contact from the raw doltlite store at `db_path` and parse
/// it. `None` when the store is absent or cannot be read — a render
/// path must not fail hard because the download has not run yet, and an
/// unreadable store is not one that lost every contact it had.
pub fn parse(db_path: &Path, range: RawRange<'_>) -> Result<Option<ParsedContacts>> {
    if !db_path.exists() {
        return Ok(None);
    }
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let Some(db) = RawDb::open_reader(&path, range.pin).await? else {
                return Ok(None);
            };
            let loaded = async {
                let rows = db.load_all_for_render_and_index_md().await?;
                let people = db.load_google_people().await?;
                let groups = db.load_google_groups().await?;
                let login = db.google_login().await?;
                let pin = db.pin().expect("open_reader returns a pinned handle");
                let changed = changed_rows(db.pool(), range, pin, INPUT_TABLES).await?;
                let mut parsed = parse_loaded(rows);
                parsed
                    .contacts
                    .extend(google::parse(&people, &groups, login.as_deref()));
                parsed.login = login;
                parsed.changed = changed;
                parsed.head = Some(pin.commit().to_string());
                if let Some(cas) = db.cas() {
                    let hashes: Vec<&str> = parsed
                        .contacts
                        .iter()
                        .filter_map(|c| c.photo.as_deref())
                        .collect();
                    parsed.held_photos = cas
                        .describe_many(&hashes)
                        .await?
                        .into_iter()
                        .map(|(hash, held)| (hash, photo::stored(cas.step(), &held)))
                        .collect();
                }
                Ok::<_, anyhow::Error>(parsed)
            }
            .await;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            db.close().await;
            loaded.map(Some)
        })
    })
}

pub fn parse_loaded(rows: Vec<LoadedRawContact>) -> ParsedContacts {
    let mut out = ParsedContacts::default();
    for row in rows {
        let source_path = PathBuf::from(&row.href);
        let blocks = split_vcards(&row.vcard);
        // A row that carries no recognizable block is still useful —
        // treat the whole payload as one block (the CardDAV path
        // stores one vCard per row without surrounding delimiters in
        // some servers' responses).
        let iter: Vec<String> = if blocks.is_empty() {
            vec![row.vcard.clone()]
        } else {
            blocks
        };
        for (idx, block) in iter.into_iter().enumerate() {
            let mut c = parse_block(&block, &row.addressbook_label);
            if c.uid.is_empty() {
                c.uid = match idx {
                    _ if row.uid.is_empty() => {
                        derive_uid_from_path(&row.addressbook_label, &source_path, idx)
                    }
                    0 => row.uid.clone(),
                    _ => format!("{}:{idx}", row.uid),
                };
            }
            c.inputs.push(Input::new("contacts", &row.id));
            if let Some(book) = &row.addressbook_id {
                c.inputs.push(Input::new("addressbooks", book));
            }
            c.addressbook_id = row.addressbook_id.clone();
            out.contacts.push(c);
        }
    }
    out.contacts.sort_by(|a, b| {
        a.addressbook
            .cmp(&b.addressbook)
            .then_with(|| a.uid.cmp(&b.uid))
    });
    out
}

/// `FN`, else the `N` line's given and family names, else the first
/// email, else the first phone, else the organization. A card with none
/// of these keeps `None` and is titled by its id downstream — but an
/// address-book export routinely holds cards that are just an email
/// address, and `contacts:#93:0` says nothing where `weishi@x.test` does.
fn display_name(block: &str, emails: &[ContactHandle], phones: &[ContactHandle]) -> Option<String> {
    let own = single_text(block, "FN").and_then(nonblank).or_else(|| {
        vcard_n_family_given(block).and_then(|(family, given)| nonblank([given, family].join(" ")))
    });
    own.or_else(|| name_from_the_rest(emails, phones, &org(block)))
}

/// What a card with no name is called: its first email, else its first
/// phone, else its organization.
pub(super) fn name_from_the_rest(
    emails: &[ContactHandle],
    phones: &[ContactHandle],
    org: &[String],
) -> Option<String> {
    emails
        .first()
        .and_then(|e| nonblank(e.value.clone()))
        .or_else(|| phones.first().and_then(|p| nonblank(p.value.clone())))
        .or_else(|| nonblank(org.join(" — ")))
}

pub(super) fn nonblank(s: String) -> Option<String> {
    (!s.trim().is_empty()).then(|| s.trim().to_string())
}

fn parse_block(block: &str, addressbook: &str) -> ParsedContact {
    let uid = vcard_uid(block).unwrap_or_default();
    let emails: Vec<ContactHandle> = vcard_all(block, "EMAIL")
        .iter()
        .map(|e| ContactHandle::email(e.label(), e.value.clone()))
        .collect();
    let phones: Vec<ContactHandle> = vcard_all(block, "TEL")
        .iter()
        .map(|p| ContactHandle::phone(p.label(), phone_number(&p.value)))
        .collect();
    let details = vcard_all(block, "ADR").iter().map(address).collect();
    let (photo, photo_url) = card_photo(block);
    let photo = photo.map(|p| datalib_etl::blob_cas::blake3_hex(&p.bytes));
    ParsedContact {
        addressbook_id: None,
        inputs: Vec::new(),
        uid,
        addressbook: addressbook.to_string(),
        display_name: display_name(block, &emails, &phones),
        other_names: Vec::new(),
        revision: vcard_rev(block),
        created: vcard_created(block),
        is_group: vcard_is_group(block),
        members: vcard_members(block),
        categories: vcard_categories(block),
        emails,
        phones,
        details,
        org: org(block),
        title: single_text(block, "TITLE"),
        note: single_text(block, "NOTE"),
        photo,
        photo_url,
        source_url: None,
    }
}

/// A vCard 4 `TEL;VALUE=uri:tel:+1-…` carries the number as a URI; the
/// card means the number.
fn phone_number(value: &str) -> String {
    let v = value.trim();
    match v.get(..4) {
        Some(scheme) if scheme.eq_ignore_ascii_case("tel:") => v[4..].to_string(),
        _ => v.to_string(),
    }
}

/// `ADR` is `;`-separated: PO box; ext; street; locality; region;
/// postcode; country. Most cards leave most of them empty.
fn address(a: &VcardProp) -> Detail {
    let parts: Vec<String> = a
        .text_list(';')
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    Detail::new(
        field_label("Address", a.label().as_deref()),
        parts.join(", "),
    )
}

/// A detail's label and the source's word for which one: `Address (home)`.
pub(super) fn field_label(base: &str, which: Option<&str>) -> String {
    match which {
        Some(w) if !w.is_empty() => format!("{base} ({w})"),
        _ => base.to_string(),
    }
}

fn single_text(vcard: &str, name: &str) -> Option<String> {
    vcard_all(vcard, name).first().map(VcardProp::text)
}

fn org(vcard: &str) -> Vec<String> {
    vcard_all(vcard, "ORG")
        .first()
        .map(|p| p.text_list(';'))
        .unwrap_or_default()
}

fn derive_uid_from_path(addressbook: &str, path: &Path, block_index: usize) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("anon");
    format!("{addressbook}:{stem}:{block_index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(label: &str, uid: &str, vcard: &str) -> LoadedRawContact {
        LoadedRawContact {
            id: format!("{label}:{uid}"),
            addressbook_id: Some(label.into()),
            uid: uid.into(),
            href: format!("{label}.vcf"),
            addressbook_label: label.into(),
            vcard: vcard.into(),
        }
    }

    /// A vcf file's cards after the first get an href that is a bare
    /// fragment (`#10`), so a card with no `UID` is keyed
    /// `contacts:#10:0` — the `#` is the only thing saying *which*
    /// card, since every card in the file has the same file stem.
    ///
    /// That key then goes to `composite_key`. Guards against it refusing
    /// a part containing `#`, which took the whole render step of a live
    /// contacts source down with it. Dropping or flattening the `#`
    /// instead would be worse than the panic — every nameless card in
    /// the file would collapse onto one id and all but one would
    /// vanish from the grid.
    #[test]
    fn cards_without_a_uid_keep_the_id_their_row_was_given() {
        // As the `.vcf` download writes a Google export: one row per card,
        // `Borg.vcf#<n>` hrefs, and an id it chose from the name or, for a
        // nameless card, the file position.
        let row = |href: &str, uid: &str, vcard: &str| LoadedRawContact {
            id: format!("Borg#{uid}"),
            addressbook_id: Some("Borg".into()),
            uid: uid.into(),
            href: href.into(),
            addressbook_label: "Borg".into(),
            vcard: vcard.into(),
        };
        let parsed = parse_loaded(vec![
            row(
                "Borg.vcf",
                "name:hugh",
                "BEGIN:VCARD\r\nFN:Hugh\r\nEND:VCARD\r\n",
            ),
            row(
                "Borg.vcf#1",
                "name:seven",
                "BEGIN:VCARD\r\nFN:Seven of Nine\r\nEND:VCARD\r\n",
            ),
            row(
                "Borg.vcf#2",
                "Borg:Borg:2",
                "BEGIN:VCARD\r\nitem1.EMAIL;TYPE=INTERNET:drone@borg.test\r\nEND:VCARD\r\n",
            ),
        ]);
        let uids: Vec<&str> = parsed.contacts.iter().map(|c| c.uid.as_str()).collect();
        assert_eq!(uids, vec!["Borg:Borg:2", "name:hugh", "name:seven"]);
    }

    #[test]
    fn nameless_cards_keyed_by_an_href_fragment_stay_distinct() {
        let row = |href: &str| LoadedRawContact {
            id: format!("contacts:{href}"),
            addressbook_id: Some("contacts".into()),
            uid: String::new(),
            href: href.into(),
            addressbook_label: "contacts".into(),
            vcard: "BEGIN:VCARD\nEMAIL:someone@x.test\nEND:VCARD".into(),
        };
        let parsed = parse_loaded(vec![row("#10"), row("#11")]);
        let uids: Vec<&str> = parsed.contacts.iter().map(|c| c.uid.as_str()).collect();
        assert_eq!(uids, vec!["contacts:#10:0", "contacts:#11:0"]);

        let ids: Vec<String> = parsed
            .contacts
            .iter()
            .map(|c| crate::render::ids::contact("tng_contacts", &c.addressbook, &c.uid).uuid)
            .collect();
        assert_ne!(ids[0], ids[1], "two nameless cards collapsed onto one id");
    }

    /// A card with no `FN` is titled by its name, email, number or
    /// organization rather than by its synthesized uid (`contacts:#93:0`),
    /// which says nothing on a real address book's email-only cards.
    #[test]
    fn nameless_card_is_titled_by_what_it_does_have() {
        let name = |block: &str| parse_block(block, "book").display_name;
        assert_eq!(
            name("BEGIN:VCARD\nN:Picard;Jean-Luc;;;\nEMAIL:jlp@x.test\nEND:VCARD").as_deref(),
            Some("Jean-Luc Picard")
        );
        assert_eq!(
            name("BEGIN:VCARD\nitem1.EMAIL;TYPE=INTERNET:weishi@x.test\nEND:VCARD").as_deref(),
            Some("weishi@x.test")
        );
        assert_eq!(
            name("BEGIN:VCARD\nTEL:+1-555\nEND:VCARD").as_deref(),
            Some("+1-555")
        );
        assert_eq!(
            name("BEGIN:VCARD\nORG:Starfleet;Ops\nEND:VCARD").as_deref(),
            Some("Starfleet — Ops")
        );
        assert_eq!(name("BEGIN:VCARD\nEND:VCARD"), None);
        // `FN` still wins when present, and a blank one does not.
        assert_eq!(
            name("BEGIN:VCARD\nFN:Captain\nEMAIL:jlp@x.test\nEND:VCARD").as_deref(),
            Some("Captain")
        );
        assert_eq!(
            name("BEGIN:VCARD\nFN: \nEMAIL:jlp@x.test\nEND:VCARD").as_deref(),
            Some("jlp@x.test")
        );
    }

    /// RFC 6350 escapes are undone in every text a person reads, and a
    /// structured value splits only where its `;` is not escaped — read
    /// raw, `ORG:Starfleet\; Command` was two units and the note kept its
    /// backslashes.
    #[test]
    fn text_values_are_unescaped_and_structured_ones_split_on_bare_semicolons() {
        let card = "BEGIN:VCARD\n\
            FN:Picard\\, Jean-Luc\n\
            ORG:Starfleet\\; Command;USS Enterprise\\, NCC-1701-D\n\
            TITLE:Captain\\; Diplomat\n\
            NOTE:Make it so.\\nTea\\, Earl Grey\\, hot.\n\
            ADR;TYPE=WORK:;;Ready Room\\, Deck 1;;;;\n\
            END:VCARD";
        let c = parse_block(card, "book");
        assert_eq!(c.display_name.as_deref(), Some("Picard, Jean-Luc"));
        assert_eq!(c.org, ["Starfleet; Command", "USS Enterprise, NCC-1701-D"]);
        assert_eq!(c.title.as_deref(), Some("Captain; Diplomat"));
        assert_eq!(c.note.as_deref(), Some("Make it so.\nTea, Earl Grey, hot."));
        assert_eq!(
            c.details,
            [Detail::new("Address (work)", "Ready Room, Deck 1")],
            "empty parts are left out"
        );
    }

    /// A vCard 4 number written as a URI (`TEL;VALUE=uri:tel:+1-…`) is
    /// the number: it reads and links as one.
    #[test]
    fn a_tel_uri_is_its_number() {
        let c = parse_block(
            "BEGIN:VCARD\nFN:Worf\nTEL;TYPE=cell;VALUE=uri:tel:+1-202-555-0101\nEND:VCARD",
            "book",
        );
        assert_eq!(c.phones[0].value, "+1-202-555-0101");
        assert_eq!(
            c.phones[0].handle.as_ref().map(|h| h.as_str()),
            Some("tel:+12025550101")
        );
    }

    #[test]
    fn split_vcards_walks_multiple_blocks_with_crlf_and_lf() {
        let body = "BEGIN:VCARD\r\nUID:a\r\nFN:Alice\r\nEND:VCARD\r\nBEGIN:VCARD\nUID:b\nFN:Bob\nEND:VCARD\n";
        let blocks = split_vcards(body);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("UID:a"));
        assert!(blocks[1].contains("UID:b"));
    }

    #[test]
    fn split_vcards_drops_text_outside_blocks() {
        let body = "junk before\nBEGIN:VCARD\nUID:a\nEND:VCARD\ngarbage between\nBEGIN:VCARD\nUID:b\nEND:VCARD\ntrailer\n";
        let blocks = split_vcards(body);
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].contains("junk"));
        assert!(!blocks[1].contains("garbage"));
        assert!(!blocks[1].contains("trailer"));
    }

    #[test]
    fn parse_loaded_groups_by_addressbook() {
        let rows = vec![
            make_row(
                "Bridge",
                "picard",
                "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n",
            ),
            make_row(
                "Bridge",
                "riker",
                "BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
            ),
            make_row(
                "Engineering",
                "laforge",
                "BEGIN:VCARD\nVERSION:3.0\nUID:laforge\nFN:Geordi La Forge\nEND:VCARD\n",
            ),
        ];
        let parsed = parse_loaded(rows);
        assert_eq!(parsed.contacts.len(), 3);
        assert_eq!(parsed.contacts[0].addressbook, "Bridge");
        assert_eq!(parsed.contacts[0].uid, "picard");
        assert_eq!(parsed.contacts[1].uid, "riker");
        assert_eq!(parsed.contacts[2].addressbook, "Engineering");
    }

    #[test]
    fn parse_loaded_handles_multi_block_payload() {
        let rows = vec![make_row(
            "contacts",
            "ignored",
            "BEGIN:VCARD\nVERSION:3.0\nUID:a\nFN:Alice\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:b\nFN:Bob\nEND:VCARD\n",
        )];
        let parsed = parse_loaded(rows);
        assert_eq!(parsed.contacts.len(), 2);
        for c in &parsed.contacts {
            assert_eq!(c.addressbook, "contacts");
        }
    }

    #[test]
    fn parse_loaded_picks_inline_base64_photo() {
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";
        let vcard = format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nPHOTO;ENCODING=b;TYPE=PNG:{png_b64}\nEND:VCARD\n"
        );
        let rows = vec![make_row("contacts", "picard", &vcard)];
        let parsed = parse_loaded(rows);
        let c = &parsed.contacts[0];
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(png_b64)
            .unwrap();
        assert_eq!(
            c.photo.as_deref(),
            Some(datalib_etl::blob_cas::blake3_hex(&bytes).as_str()),
            "the photo is named by its decoded bytes"
        );
    }

    #[test]
    /// An absent store is `None`, not an empty parse. The caller sweeps
    /// every document this pass did not name, so "no store yet" and "the
    /// source lost every contact" must not look the same to it.
    fn parse_missing_db_reads_as_absent_not_empty() {
        let parsed = parse(
            Path::new("/this/does/not/exist.doltlite_db"),
            RawRange::cold(),
        )
        .unwrap();
        assert!(parsed.is_none());
    }
}
