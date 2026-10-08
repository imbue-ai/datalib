//! Map parsed vCards into `NormalizedContact`s and hand them to the shared
//! [`datalib_etl_contact_common`] renderer.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use datalib_contact_schema::{ContactHandle, ContactKind, Detail, NormalizedContact, Photo};
use datalib_etl::progress::Progress;
use datalib_etl_contact_common::{render_all as cc_render_all, ContactDoc, ContactRenderProfile};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{keys_reading, Bucket, Buckets, RawRange};

use datalib_etl_contacts::ingest::schema_raw::member_uid;

use super::ids;
use super::parse::{ParsedContact, ParsedContacts};
use datalib_schema::providers::Provider;

/// Bump when the rendered layout changes enough that every existing
/// contact doc needs re-rendering. Bumped to 2 when contacts adopted the
/// shared contact-common layout (uuid-named files, generic frontmatter,
/// richer grid-row search text); to 3 when `account` stopped carrying
/// the source name; to 4 when ids moved onto `datalib_id` under
/// the configured source and every row gained its backpointer — every uuid
/// moved; to 5 when labels came from `X-ABLabel` and every `TYPE`,
/// `CREATED` became `created_at`, and groups listed their members; to 7
/// when a card listed the groups it is in, and to 8 when its `CATEGORIES`
/// joined them; to 10 when a `+1` number without ten digits after the 1
/// stopped having a handle; to 12 when a card's photo reached the index
/// as the URL the app serves it at.
/// 13: a photo no browser draws has no URL.
pub const RENDER_VERSION: u32 = 13;

/// Every card by `(addressbook, UID)`, for a group to name its members.
type Cards<'a> = HashMap<(&'a str, &'a str), &'a ParsedContact>;

/// The groups naming each `(addressbook, UID)` as a member.
type Groups<'a> = HashMap<(&'a str, &'a str), Vec<&'a ParsedContact>>;

fn groups_by_member(contacts: &[ParsedContact]) -> Groups<'_> {
    let mut out: Groups = HashMap::new();
    for group in contacts.iter().filter(|c| c.is_group) {
        for uid in group.members.iter().filter_map(|m| member_uid(m)) {
            out.entry((group.addressbook.as_str(), uid))
                .or_default()
                .push(group);
        }
    }
    for groups in out.values_mut() {
        groups.sort_by(|a, b| {
            (a.display_name.as_deref(), &a.uid).cmp(&(b.display_name.as_deref(), &b.uid))
        });
        groups.dedup_by(|a, b| std::ptr::eq(*a, *b));
    }
    out
}

/// Every bucket a pass looked at — named first with nothing, then the
/// rendered ones with what they read — for the processor to declare.
pub fn render_all(
    parsed: &ParsedContacts,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    range: RawRange<'_>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Buckets> {
    let profile = |contact_kind: &str| ContactRenderProfile {
        provider: Provider::Contacts,
        source_label: humanize_source_label(source_id),
        contact_kind: contact_kind.to_string(),
        contact_entity_kind: ids::KIND_CONTACT,
        // A `.vcf` file has no login behind it.
        account: None,
        render_version: RENDER_VERSION,
    };
    let cards: Cards = parsed
        .contacts
        .iter()
        .map(|c| ((c.addressbook.as_str(), c.uid.as_str()), c))
        .collect();
    let groups = groups_by_member(&parsed.contacts);
    let mut contacts: Vec<(bool, ContactDoc)> = parsed
        .contacts
        .iter()
        .map(|c| (c.is_group, normalize(c, source_id, &cards, &groups)))
        .collect();

    // What to render: the contacts the driver found stale, plus every
    // card of a row the diff named — one `contacts` row can hold several
    // cards, which is why the row alone could never name a document and
    // the mapping goes through the parse.
    let forward = parsed.changed.as_ref().map(|changed| {
        keys_reading(
            changed,
            contacts
                .iter()
                .map(|(_, c)| (c.doc_uuid.as_str(), c.inputs.as_slice())),
        )
    });
    let render = range.narrow(forward.as_ref());
    let mut buckets: Buckets = render
        .iter()
        .flatten()
        .map(|key| Bucket {
            key: key.clone(),
            inputs: Vec::new(),
        })
        .collect();
    if let Some(render) = &render {
        contacts.retain(|(_, c)| render.contains(&c.doc_uuid));
    }
    let (groups, people): (Vec<_>, Vec<_>) = contacts.into_iter().partition(|(g, _)| *g);
    for (kind, cards) in [("Contact", people), ("Contact group", groups)] {
        let cards: Vec<ContactDoc> = cards.into_iter().map(|(_, c)| c).collect();
        let summary = cc_render_all(
            &profile(kind),
            &cards,
            out_dir,
            source_id,
            progress,
            on_doc_complete,
        )?;
        buckets.extend(summary.buckets);
    }
    Ok(buckets)
}

fn normalize(
    contact: &ParsedContact,
    source_id: &str,
    cards: &Cards,
    groups: &Groups,
) -> ContactDoc {
    let id = ids::contact(source_id, &contact.addressbook, &contact.uid);
    let kind = if contact.is_group {
        ContactKind::Group
    } else {
        ContactKind::Person
    };
    let mut person = NormalizedContact::new(source_id, id.natural_key.clone(), kind);
    let mut inputs = contact.inputs.clone();
    // The group's name is on this page, and its card is where the
    // membership lives: a member added or dropped re-renders this card.
    let mut group_names: Vec<String> = Vec::new();
    for group in groups
        .get(&(contact.addressbook.as_str(), contact.uid.as_str()))
        .into_iter()
        .flatten()
    {
        inputs.extend(group.inputs.iter().cloned());
        group_names.push(
            group
                .display_name
                .clone()
                .unwrap_or_else(|| group.uid.clone()),
        );
    }
    group_names.extend(contact.categories.iter().filter_map(|c| category_name(c)));
    let mut seen = std::collections::HashSet::new();
    group_names.retain(|name| seen.insert(name.clone()));
    person.groups = group_names;
    for member in &contact.members {
        let uid = member_uid(member).unwrap_or(member);
        let card = cards.get(&(contact.addressbook.as_str(), uid));
        // The member's name is part of this page, so its card is an input.
        inputs.extend(card.iter().flat_map(|c| c.inputs.iter().cloned()));
        let name = card
            .and_then(|c| c.display_name.clone())
            .unwrap_or_else(|| member.clone());
        person.members.push(name);
    }
    person.names = contact.display_name.iter().cloned().collect();
    person.org = (!contact.org.is_empty()).then(|| contact.org.join(" — "));
    person.title = contact.title.clone();
    person.handles.extend(
        contact
            .emails
            .iter()
            .map(|e| ContactHandle::email(e.label(), e.value.clone())),
    );
    person.handles.extend(
        contact
            .phones
            .iter()
            .map(|p| ContactHandle::phone(p.label(), p.value.clone())),
    );
    for a in &contact.addresses {
        // ADR is `;`-separated: PO box; ext; street; locality; region; postcode; country
        person.details.push(Detail::new(
            field_label("Address", &a.label()),
            a.text_list(';').join(", "),
        ));
    }
    person.note = contact.note.clone();
    person.created_at = contact
        .created
        .as_deref()
        .and_then(datalib_time::coerce_record_stamp);
    // vCard `REV` is the revision stamp. Fastmail emits *basic* ISO
    // 8601 (`20260605T191839Z`), which isn't RFC 3339 and would be
    // rejected at `GridRow::build`; coerce it (already-valid values
    // pass through).
    person.modified_at = contact
        .revision
        .as_deref()
        .and_then(datalib_time::coerce_record_stamp);
    // CardDAV carries no per-contact public web URL (see the prior
    // note in this file's history re: Fastmail's internal short ids).
    person.photo = match (&contact.photo, &contact.photo_url) {
        (Some(p), _) => Some(Photo::Inline {
            content_type: p.content_type.clone(),
            bytes: p.bytes.clone(),
        }),
        (None, Some(url)) => Some(Photo::Url(url.clone())),
        (None, None) => None,
    };
    ContactDoc {
        contact: person,
        doc_uuid: id.uuid,
        group_uuid: ids::addressbook(source_id, &contact.addressbook).uuid,
        group_label: contact.addressbook.clone(),
        upstream_account: None,
        inputs,
    }
}

/// How a `CATEGORIES` name reads on the card. Google files every contact
/// in the address book under `myContacts`, which says nothing on one
/// card; `starred` is its star. Any other name is the person's own.
fn category_name(category: &str) -> Option<String> {
    match category {
        "myContacts" => None,
        "starred" => Some("Starred".to_string()),
        other => Some(other.to_string()),
    }
}

fn field_label(base: &str, label: &Option<String>) -> String {
    match label {
        Some(s) if !s.is_empty() => format!("{base} ({s})"),
        _ => base.to_string(),
    }
}

/// `source_id` is the group id (`apple_contacts`, `fastmail_contacts`,
/// …). Surface a human label on grid rows by stripping the `_contacts`
/// suffix + casing. Keeps the row's Source column from looking like a
/// slug.
fn humanize_source_label(source_id: &str) -> String {
    let base = source_id
        .strip_suffix("_contacts")
        .or_else(|| source_id.strip_suffix("-contacts"))
        .unwrap_or(source_id);
    let mut out = String::new();
    let mut capitalize = true;
    for c in base.chars() {
        if c == '_' || c == '-' {
            out.push(' ');
            capitalize = true;
        } else if capitalize {
            out.extend(c.to_uppercase());
            capitalize = false;
        } else {
            out.push(c);
        }
    }
    if out.is_empty() {
        return "Contacts".to_string();
    }
    format!("{out} Contacts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_contact_common::table_rows;
    use datalib_etl_contacts::ingest::api::VcardProp;
    use datalib_etl_render::inputs::Input;

    fn prop(value: &str, ty: Option<&str>) -> VcardProp {
        VcardProp {
            value: value.to_string(),
            params: ty
                .map(|t| vec![("TYPE".to_string(), t.to_string())])
                .unwrap_or_default(),
            ab_label: None,
        }
    }

    fn sample() -> ParsedContact {
        ParsedContact {
            inputs: Vec::new(),
            uid: "tng-picard".to_string(),
            addressbook: "Bridge".to_string(),
            source_path: std::path::PathBuf::from("Bridge.vcf"),
            display_name: Some("Jean-Luc Picard".to_string()),
            revision: Some("2370-04-15T00:00:00Z".to_string()),
            created: None,
            is_group: false,
            members: Vec::new(),
            categories: Vec::new(),
            emails: vec![prop("jlp@enterprise", Some("WORK"))],
            phones: vec![prop("+1-555", Some("WORK"))],
            addresses: vec![prop(";;Ready Room;Deck 1;;;", Some("WORK"))],
            org: vec!["Starfleet".to_string(), "USS Enterprise".to_string()],
            title: Some("Captain".to_string()),
            note: Some("Make it so.".to_string()),
            photo: None,
            photo_url: None,
        }
    }

    #[test]
    fn normalize_maps_fields_uuids_and_stamps() {
        let n = normalize(&sample(), "tng_contacts", &Cards::new(), &Groups::new());
        assert_eq!(
            n.doc_uuid,
            ids::contact("tng_contacts", "Bridge", "tng-picard").uuid
        );
        assert_eq!(
            n.group_uuid,
            ids::addressbook("tng_contacts", "Bridge").uuid
        );
        assert_eq!(n.group_label, "Bridge");
        assert_eq!(n.contact.name(), Some("Jean-Luc Picard"));
        assert_eq!(n.contact.key, "Bridge#tng-picard");
        assert_eq!(n.upstream_account, None);
        assert_eq!(n.contact.created_at, None, "this card has no CREATED");
        assert_eq!(
            n.contact.modified_at.as_deref(),
            Some("2370-04-15T00:00:00Z")
        );
        // Org `;` becomes ` — `; address `;` becomes `, `; typed labels.
        let rows = table_rows(&n.contact);
        let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Org",
                "Title",
                "Email (work)",
                "Phone (work)",
                "Address (work)",
                "Note"
            ]
        );
        assert_eq!(rows[0].1, "Starfleet — USS Enterprise");
    }

    /// A card's text goes to contact-common as the card has it, markup
    /// and line breaks included: contact-common escapes it where it
    /// becomes the page, which a `<br>` added here would defeat.
    #[test]
    fn a_note_in_markup_is_handed_on_as_typed() {
        let mut c = sample();
        c.note = Some("<script>x</script> & co\nsecond line".to_string());
        let n = normalize(&c, "tng_contacts", &Cards::new(), &Groups::new());
        assert_eq!(
            n.contact.note.as_deref(),
            Some("<script>x</script> & co\nsecond line")
        );
    }

    // Fastmail exports the vCard `REV` in *basic* ISO 8601 (no separators,
    // e.g. `20260605T191839Z`). That string is not RFC 3339, so flowing it
    // straight into `modified_at` makes `GridRow::build` reject the row and drops
    // the contact's `.grid_rows.json`. Normalize must canonicalize it to an
    // explicit-offset RFC 3339 stamp the grid accepts.
    #[test]
    fn normalize_canonicalizes_basic_iso_rev() {
        let mut c = sample();
        c.revision = Some("20260605T191839Z".to_string());
        let n = normalize(&c, "fastmail_contacts", &Cards::new(), &Groups::new());
        assert_eq!(
            n.contact.modified_at.as_deref(),
            Some("2026-06-05T19:18:39+00:00")
        );
        // The grid's own contract must accept it (this is what was failing).
        datalib_time::validate_iso_offset(n.contact.modified_at.as_deref().unwrap())
            .expect("normalized modified_at must satisfy GridRow's RFC 3339 contract");
    }

    /// A group card lists its members by name, and a renamed member
    /// re-renders the group because the member's row is its input.
    #[test]
    fn a_group_names_its_members_and_reads_their_rows() {
        let picard = ParsedContact {
            inputs: vec![Input::new("contacts", "row-picard")],
            ..sample()
        };
        let group = ParsedContact {
            uid: "tng-senior-staff".to_string(),
            display_name: Some("Senior Staff".to_string()),
            created: Some("23700101T000000Z".to_string()),
            is_group: true,
            members: vec![
                "urn:uuid:tng-picard".to_string(),
                "urn:uuid:tng-q".to_string(),
            ],
            emails: Vec::new(),
            phones: Vec::new(),
            addresses: Vec::new(),
            org: Vec::new(),
            title: None,
            note: None,
            inputs: vec![Input::new("contacts", "row-group")],
            ..sample()
        };
        let cards: Cards = [(("Bridge", "tng-picard"), &picard)].into_iter().collect();
        let n = normalize(&group, "tng_contacts", &cards, &Groups::new());
        let rows = table_rows(&n.contact);
        let members: Vec<(&str, &str)> =
            rows.iter().map(|(l, v)| (l.as_str(), v.as_str())).collect();
        assert_eq!(
            members,
            vec![("Member", "Jean-Luc Picard"), ("Member", "urn:uuid:tng-q")]
        );
        assert_eq!(
            n.contact.created_at.as_deref(),
            Some("2370-01-01T00:00:00+00:00")
        );
        assert_eq!(n.contact.kind, ContactKind::Group);
        let rows: Vec<&str> = n.inputs.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(rows, vec!["row-group", "row-picard"]);
    }

    /// A card lists the groups it is in, first, and reads each group's
    /// row: dropping it from a group re-renders it.
    #[test]
    fn a_member_lists_its_groups_and_reads_their_rows() {
        let picard = ParsedContact {
            inputs: vec![Input::new("contacts", "row-picard")],
            ..sample()
        };
        let group = |uid: &str, name: &str| ParsedContact {
            uid: uid.to_string(),
            display_name: Some(name.to_string()),
            is_group: true,
            members: vec!["urn:uuid:tng-picard".to_string()],
            inputs: vec![Input::new("contacts", format!("row-{uid}"))],
            ..sample()
        };
        let contacts = vec![
            picard,
            group("g2", "Senior Staff"),
            group("g1", "Away Team"),
        ];
        let groups = groups_by_member(&contacts);
        let n = normalize(&contacts[0], "tng_contacts", &Cards::new(), &groups);
        let rows = table_rows(&n.contact);
        let listed: Vec<(&str, &str)> = rows
            .iter()
            .take(2)
            .map(|(l, v)| (l.as_str(), v.as_str()))
            .collect();
        assert_eq!(
            listed,
            vec![("Group", "Away Team"), ("Group", "Senior Staff")]
        );
        let rows: Vec<&str> = n.inputs.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(rows, vec!["row-picard", "row-g1", "row-g2"]);
    }

    /// Google's labels arrive as `CATEGORIES`; they list beside the
    /// groups, `starred` reads as a star, `myContacts` (every contact has
    /// it) not at all, and a name both a group card and a category give
    /// lists once.
    #[test]
    fn categories_list_beside_the_groups() {
        let mut c = sample();
        c.categories = vec![
            "myContacts".to_string(),
            "starred".to_string(),
            "Bridge crew".to_string(),
            "Away Team".to_string(),
        ];
        let away = ParsedContact {
            uid: "g1".to_string(),
            display_name: Some("Away Team".to_string()),
            is_group: true,
            members: vec!["urn:uuid:tng-picard".to_string()],
            ..sample()
        };
        let contacts = vec![c, away];
        let groups = groups_by_member(&contacts);
        let n = normalize(&contacts[0], "tng_contacts", &Cards::new(), &groups);
        let listed: Vec<&str> = n.contact.groups.iter().map(String::as_str).collect();
        assert_eq!(listed, vec!["Away Team", "Starred", "Bridge crew"]);
    }

    #[test]
    fn humanize_source_label_strips_contacts_suffix() {
        assert_eq!(humanize_source_label("apple_contacts"), "Apple Contacts");
        assert_eq!(
            humanize_source_label("fastmail-contacts"),
            "Fastmail Contacts"
        );
        assert_eq!(humanize_source_label("home"), "Home Contacts");
    }
}
