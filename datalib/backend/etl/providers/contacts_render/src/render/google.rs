//! Google's contacts and labels → [`ParsedContact`]. A contact is a card;
//! a label is a group card whose members are the contacts filed under it,
//! and one of Google's own groups is a category, since it is no card
//! anyone made.

use std::collections::{BTreeMap, HashMap};

use datalib_contact_schema::{ContactHandle, Detail};
use datalib_etl_contacts::ingest::google::db::{LoadedGoogleGroup, LoadedGooglePerson, PHOTOS};
use datalib_etl_contacts::ingest::google::selection::{is_label, memberships, name as group_name};
use datalib_etl_contacts::ingest::google::ADDRESSBOOK_NAME;
use datalib_etl_contacts::ingest::google::{label_page, person_page, primary, str_of, ACCOUNT_ID};
use datalib_etl_render::inputs::Input;
use datalib_handle::Handle;
use serde_json::Value;

use super::ids;
use super::parse::{field_label, name_from_the_rest, nonblank, ParsedContact};

/// Google files every contact under it, so it says nothing of one.
const MY_CONTACTS: &str = "contactGroups/myContacts";

pub fn parse(
    people: &[LoadedGooglePerson],
    groups: &[LoadedGoogleGroup],
    login: Option<&str>,
) -> Vec<ParsedContact> {
    let mut members: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for p in people {
        for group in memberships(&p.person) {
            members
                .entry(group)
                .or_default()
                .push(p.resource_name.clone());
        }
    }
    let own: HashMap<&str, &LoadedGoogleGroup> = groups
        .iter()
        .filter(|g| !is_label(&g.group))
        .map(|g| (g.resource_name.as_str(), g))
        .collect();
    let cards = people.iter().map(|p| person(p, &own, login));
    let labels = groups.iter().filter(|g| is_label(&g.group)).map(|g| {
        let members = members.remove(g.resource_name.as_str());
        label(g, members.unwrap_or_default(), login)
    });
    cards.chain(labels).collect()
}

/// The document a raw row of Google's is, or belongs to: `"<address
/// book>#people/<id>"`, with the photo's source after another `#` for one.
pub fn document_of(source_id: &str, table: &str, id: &str) -> Option<String> {
    if ![PHOTOS, "google_people", "google_contact_groups"].contains(&table) {
        return None;
    }
    let resource_name = id.split('#').nth(1)?;
    Some(ids::contact(source_id, ADDRESSBOOK_NAME, resource_name).uuid)
}

fn person(
    p: &LoadedGooglePerson,
    own_groups: &HashMap<&str, &LoadedGoogleGroup>,
    login: Option<&str>,
) -> ParsedContact {
    let v = &p.person;
    let mut inputs = vec![
        Input::new("google_people", &p.id),
        Input::new("accounts", ACCOUNT_ID),
    ];
    let mut categories = Vec::new();
    for group in memberships(v).filter(|g| *g != MY_CONTACTS) {
        if let Some(group) = own_groups.get(group) {
            inputs.push(Input::new("google_contact_groups", &group.id));
            categories.extend(group_name(&group.group));
        }
    }
    let photo = p.photo.as_ref().and_then(|(id, blake3)| {
        inputs.push(Input::new("google_photos", id));
        blake3.clone()
    });
    let emails = distinct(items(v, "emailAddresses").filter_map(|e| {
        let value = str_of(e, "value")?;
        Some(ContactHandle::email(which(e), value))
    }));
    let phones = distinct(items(v, "phoneNumbers").filter_map(phone));
    let organization = primary(v, "organizations");
    let org: Vec<String> = organization.map(org_parts).unwrap_or_default();
    let mut names = names(v);
    let display_name = if names.is_empty() {
        name_from_the_rest(&emails, &phones, &org)
    } else {
        Some(names.remove(0))
    };
    ParsedContact {
        uid: p.resource_name.clone(),
        addressbook: ADDRESSBOOK_NAME.into(),
        addressbook_id: Some(p.addressbook_id.clone()),
        display_name,
        other_names: names,
        revision: contact_update_time(v),
        created: None,
        is_group: false,
        members: Vec::new(),
        categories,
        emails,
        phones,
        details: details(v, organization),
        org,
        title: organization.and_then(|o| str_of(o, "title")),
        note: primary(v, "biographies").and_then(|b| str_of(b, "value")),
        photo,
        photo_url: None,
        source_url: person_page(&p.resource_name, login),
        inputs,
    }
}

fn label(g: &LoadedGoogleGroup, members: Vec<String>, login: Option<&str>) -> ParsedContact {
    ParsedContact {
        uid: g.resource_name.clone(),
        addressbook: ADDRESSBOOK_NAME.into(),
        addressbook_id: Some(g.addressbook_id.clone()),
        display_name: group_name(&g.group),
        other_names: Vec::new(),
        revision: g
            .group
            .pointer("/metadata/updateTime")
            .and_then(Value::as_str)
            .map(str::to_string),
        created: None,
        is_group: true,
        members,
        categories: Vec::new(),
        emails: Vec::new(),
        phones: Vec::new(),
        details: Vec::new(),
        org: Vec::new(),
        title: None,
        note: None,
        photo: None,
        photo_url: None,
        source_url: label_page(&g.resource_name, login),
        inputs: vec![
            Input::new("google_contact_groups", &g.id),
            Input::new("accounts", ACCOUNT_ID),
        ],
    }
}

/// When the contact itself was last changed: Google stamps each place it
/// knows the person from, and a profile's stamp is not the card's.
fn contact_update_time(person: &Value) -> Option<String> {
    person
        .pointer("/metadata/sources")?
        .as_array()?
        .iter()
        .find(|s| s.get("type").and_then(Value::as_str) == Some("CONTACT"))
        .and_then(|s| str_of(s, "updateTime"))
}

/// Every name the person has, the primary one first.
fn names(person: &Value) -> Vec<String> {
    let display = |n: &Value| str_of(n, "displayName").and_then(nonblank);
    let mut out: Vec<String> = Vec::new();
    let first = primary(person, "names").and_then(display);
    for name in first
        .into_iter()
        .chain(items(person, "names").filter_map(display))
    {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// A number as typed, with the handle Google's own reading of it gives
/// where the number was typed without its country code.
fn phone(p: &Value) -> Option<ContactHandle> {
    let mut phone = ContactHandle::phone(which(p), str_of(p, "value")?);
    if phone.handle.is_none() {
        phone.handle = str_of(p, "canonicalForm").and_then(|c| Handle::tel(&c));
    }
    Some(phone)
}

/// An address given by both the card and the person's profile is one.
fn distinct(handles: impl Iterator<Item = ContactHandle>) -> Vec<ContactHandle> {
    let mut out: Vec<ContactHandle> = Vec::new();
    for h in handles {
        let same =
            |o: &ContactHandle| o.value == h.value || (h.handle.is_some() && o.handle == h.handle);
        if !out.iter().any(|o| o.medium == h.medium && same(o)) {
            out.push(h);
        }
    }
    out
}

/// What the source calls this one: `work`, `home fax`, or the person's
/// own word. Google spells its own in title case beside the code
/// (`homeFax`, "Home Fax"); a word someone typed is the same in both.
fn which(item: &Value) -> Option<String> {
    let shown = str_of(item, "formattedType").or_else(|| str_of(item, "type"))?;
    let code = str_of(item, "type");
    Some(if code.as_deref() == Some(shown.as_str()) {
        shown
    } else {
        shown.to_lowercase()
    })
}

fn org_parts(organization: &Value) -> Vec<String> {
    ["name", "department"]
        .iter()
        .filter_map(|k| str_of(organization, k))
        .collect()
}

/// Where an organization says someone sits. Its `value` is often only the
/// word "desk", so the building and floor are what place it.
fn location(l: &Value) -> Option<String> {
    let floor = str_of(l, "floor").map(|f| format!("floor {f}"));
    let parts = [
        str_of(l, "value"),
        str_of(l, "buildingId").filter(|b| b != "$$NO_BUILDING$$"),
        floor,
        str_of(l, "floorSection"),
        str_of(l, "deskCode"),
    ];
    let parts: Vec<String> = parts.into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Everything the card says that has no field of its own on the page.
fn details(person: &Value, organization: Option<&Value>) -> Vec<Detail> {
    let mut out: Vec<Detail> = Vec::new();
    for n in items(person, "nicknames") {
        out.extend(str_of(n, "value").map(|v| Detail::new("Nickname", v)));
    }
    for a in items(person, "addresses") {
        out.extend(address(a));
    }
    for o in items(person, "organizations").filter(|o| Some(*o) != organization) {
        let parts: Vec<String> = str_of(o, "title").into_iter().chain(org_parts(o)).collect();
        if !parts.is_empty() {
            out.push(Detail::new("Organization", parts.join(" · ")));
        }
    }
    for b in items(person, "birthdays") {
        let said = date(b).or_else(|| str_of(b, "text"));
        out.extend(said.map(|v| Detail::new("Birthday", v)));
    }
    for e in items(person, "events") {
        out.extend(date(e).map(|v| Detail::new(field_label("Event", which(e).as_deref()), v)));
    }
    for im in items(person, "imClients") {
        let on = str_of(im, "formattedProtocol").or_else(|| str_of(im, "protocol"));
        out.extend(
            str_of(im, "username").map(|v| Detail::new(field_label("IM", on.as_deref()), v)),
        );
    }
    for (field, key, label) in [
        ("occupations", "value", "Occupation"),
        ("relations", "person", "Relation"),
        ("urls", "value", "Website"),
        ("sipAddresses", "value", "SIP"),
        ("calendarUrls", "url", "Calendar"),
        ("externalIds", "value", "ID"),
        ("skills", "value", "Skill"),
        ("interests", "value", "Interest"),
    ] {
        for item in items(person, field) {
            out.extend(
                str_of(item, key)
                    .map(|v| Detail::new(field_label(label, which(item).as_deref()), v)),
            );
        }
    }
    for l in items(person, "locations") {
        out.extend(location(l).map(|v| Detail::new("Location", v)));
    }
    for u in items(person, "userDefined") {
        if let (Some(key), Some(value)) = (str_of(u, "key"), str_of(u, "value")) {
            out.push(Detail::new(key, value));
        }
    }
    let mut seen: Vec<Detail> = Vec::new();
    out.retain(|d| {
        let new = !seen.contains(d);
        seen.push(d.clone());
        new
    });
    out
}

fn address(a: &Value) -> Option<Detail> {
    let parts: Vec<String> = [
        "poBox",
        "extendedAddress",
        "streetAddress",
        "city",
        "region",
        "postalCode",
        "country",
    ]
    .iter()
    .filter_map(|k| str_of(a, k))
    .collect();
    let text = str_of(a, "formattedValue").or_else(|| nonblank(parts.join(", ")))?;
    Some(Detail::new(
        field_label("Address", which(a).as_deref()),
        text,
    ))
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Google's date, any part of which may be unknown.
fn date(item: &Value) -> Option<String> {
    let part = |k: &str| {
        item.pointer(&format!("/date/{k}"))
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
    };
    let month_name = |m: u64| MONTHS.get(m as usize - 1).copied();
    Some(match (part("year"), part("month"), part("day")) {
        (Some(y), Some(m), Some(d)) => format!("{y:04}-{m:02}-{d:02}"),
        (Some(y), Some(m), None) => format!("{y:04}-{m:02}"),
        (Some(y), None, _) => format!("{y:04}"),
        (None, Some(m), Some(d)) => format!("{} {d}", month_name(m)?),
        (None, Some(m), None) => month_name(m)?.to_string(),
        (None, None, _) => return None,
    })
}

fn items<'a>(person: &'a Value, field: &str) -> impl Iterator<Item = &'a Value> {
    person
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::progress::Progress;
    use datalib_etl_render::grid_index::RenderedMarkdown;
    use datalib_etl_render::inputs::RawRange;
    use datalib_etl_render::section::join;
    use serde_json::json;

    use super::super::parse::ParsedContacts;
    use super::super::render::render_all;

    const BOOK: &str = "google!people/1701/connections";

    fn person(id: &str, mut v: Value) -> LoadedGooglePerson {
        v["resourceName"] = json!(format!("people/{id}"));
        LoadedGooglePerson {
            id: format!("{BOOK}#people/{id}"),
            addressbook_id: BOOK.into(),
            resource_name: format!("people/{id}"),
            person: v,
            photo: None,
        }
    }

    fn group(id: &str, name: &str, kind: &str) -> LoadedGoogleGroup {
        LoadedGoogleGroup {
            id: format!("{BOOK}#contactGroups/{id}"),
            addressbook_id: BOOK.into(),
            resource_name: format!("contactGroups/{id}"),
            group: json!({"groupType": kind, "name": id, "formattedName": name}),
        }
    }

    fn groups() -> Vec<LoadedGoogleGroup> {
        vec![
            group("myContacts", "My Contacts", "SYSTEM_CONTACT_GROUP"),
            group("starred", "Starred", "SYSTEM_CONTACT_GROUP"),
            group("5ea9", "Bridge crew", "USER_CONTACT_GROUP"),
        ]
    }

    fn filed(ids: &[&str]) -> Value {
        ids.iter()
            .map(|id| {
                json!({"contactGroupMembership":
                    {"contactGroupResourceName": format!("contactGroups/{id}")}})
            })
            .collect()
    }

    fn riker() -> LoadedGooglePerson {
        person(
            "c1",
            json!({
                "metadata": {"sources": [
                    {"type": "PROFILE", "id": "42", "updateTime": "2026-01-01T00:00:00Z"},
                    {"type": "CONTACT", "id": "c1", "updateTime": "2026-09-01T10:00:00.000001Z"}
                ]},
                "names": [
                    {"displayName": "Will Riker", "metadata": {"source": {"type": "PROFILE"}}},
                    {"displayName": "William Riker", "metadata": {"primary": true}}
                ],
                "nicknames": [{"value": "Number One"}],
                "emailAddresses": [
                    {"value": "riker@enterprise.test", "type": "work", "formattedType": "Work"},
                    {"value": "Riker@Enterprise.test"},
                    {"value": "will@alaska.test", "type": "Cabin", "formattedType": "Cabin"}
                ],
                "phoneNumbers": [
                    {"value": "(202) 555-0101", "canonicalForm": "+12025550101",
                     "type": "homeFax", "formattedType": "Home Fax"},
                    {"value": "555-0102"}
                ],
                "organizations": [
                    {"name": "Starfleet", "department": "Operations", "title": "Commander",
                     "metadata": {"primary": true}},
                    {"name": "USS Titan", "title": "Captain"}
                ],
                "addresses": [{"formattedValue": "Deck 8\nUSS Enterprise", "type": "home", "formattedType": "Home"}],
                "birthdays": [{"date": {"month": 8, "day": 19}}],
                "events": [{"date": {"year": 2379, "month": 4, "day": 1}, "type": "anniversary", "formattedType": "Anniversary"}],
                "relations": [{"person": "Deanna Troi", "type": "spouse", "formattedType": "Spouse"}],
                "urls": [{"value": "https://titan.test", "type": "work", "formattedType": "Work"}],
                "locations": [{"value": "desk", "type": "desk", "buildingId": "NCC-1701-D", "floor": "8"},
                    {"value": "Ten Forward", "buildingId": "$$NO_BUILDING$$"}],
                "userDefined": [{"key": "Instrument", "value": "Trombone"}],
                "biographies": [{"value": "Plays jazz.\nBeard since 2365.", "contentType": "TEXT_PLAIN"}],
                "memberships": filed(&["5ea9", "myContacts", "starred"])
            }),
        )
    }

    fn one(p: LoadedGooglePerson) -> ParsedContact {
        parse(&[p], &groups(), Some("picard@enterprise.test")).remove(0)
    }

    #[test]
    fn a_contact_says_everything_google_does() {
        let c = one(riker());
        assert_eq!(c.display_name.as_deref(), Some("William Riker"));
        assert_eq!(c.other_names, ["Will Riker"]);
        assert_eq!(
            c.revision.as_deref(),
            Some("2026-09-01T10:00:00.000001Z"),
            "the card's own stamp, not the profile's"
        );
        let emails: Vec<(&str, Option<&str>)> = c
            .emails
            .iter()
            .map(|e| (e.value.as_str(), e.label.as_deref()))
            .collect();
        assert_eq!(
            emails,
            [
                ("riker@enterprise.test", Some("work")),
                ("will@alaska.test", Some("Cabin")),
            ],
            "one address given twice is one; a word someone typed stays as typed"
        );
        assert_eq!(
            (c.org.join("/"), c.title.as_deref()),
            ("Starfleet/Operations".into(), Some("Commander"))
        );
        assert_eq!(c.note.as_deref(), Some("Plays jazz.\nBeard since 2365."));
        assert_eq!(c.categories, ["Starred"]);
        let details: Vec<(&str, &str)> = c
            .details
            .iter()
            .map(|d| (d.label.as_str(), d.value.as_str()))
            .collect();
        assert_eq!(
            details,
            [
                ("Nickname", "Number One"),
                ("Address (home)", "Deck 8\nUSS Enterprise"),
                ("Organization", "Captain · USS Titan"),
                ("Birthday", "August 19"),
                ("Event (anniversary)", "2379-04-01"),
                ("Relation (spouse)", "Deanna Troi"),
                ("Website (work)", "https://titan.test"),
                ("Location", "desk, NCC-1701-D, floor 8"),
                ("Location", "Ten Forward"),
                ("Instrument", "Trombone"),
            ]
        );
        assert_eq!(
            c.source_url.as_deref(),
            Some("https://contacts.google.com/person/c1?authuser=picard%40enterprise.test")
        );
    }

    /// A number typed as it is dialled at home keeps its spelling and has
    /// the handle of the number Google reads it as; one Google reads no
    /// number in has none.
    #[test]
    fn a_number_google_reads_in_full_has_a_handle() {
        let c = one(riker());
        let phones: Vec<(&str, Option<&str>, Option<&str>)> = c
            .phones
            .iter()
            .map(|p| {
                (
                    p.value.as_str(),
                    p.label.as_deref(),
                    p.handle.as_ref().map(|h| h.as_str()),
                )
            })
            .collect();
        assert_eq!(
            phones,
            [
                ("(202) 555-0101", Some("home fax"), Some("tel:+12025550101")),
                ("555-0102", None, None),
            ]
        );
    }

    #[test]
    fn a_contact_with_no_name_is_called_by_what_it_has() {
        let email = json!([{"value": "q@continuum.test"}]);
        let c = one(person("c9", json!({"emailAddresses": email})));
        assert_eq!(c.display_name.as_deref(), Some("q@continuum.test"));
        let c = one(person(
            "c9",
            json!({"organizations": [{"name": "Starfleet"}]}),
        ));
        assert_eq!(c.display_name.as_deref(), Some("Starfleet"));
        assert_eq!(one(person("c9", json!({}))).display_name, None);
    }

    fn rendered(people: Vec<LoadedGooglePerson>) -> Vec<RenderedMarkdown> {
        let parsed = ParsedContacts {
            contacts: parse(&people, &groups(), Some("picard@enterprise.test")),
            login: Some("picard@enterprise.test".into()),
            ..Default::default()
        };
        let mut out = Vec::new();
        render_all(
            &parsed,
            "google_contacts",
            &Progress::default(),
            RawRange::cold(),
            &mut |md| {
                out.push(md);
                Ok(())
            },
        )
        .unwrap();
        out
    }

    fn named<'a>(docs: &'a [RenderedMarkdown], name: &str) -> &'a RenderedMarkdown {
        docs.iter()
            .find(|d| d.rows[0].contact.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("no page for {name}"))
    }

    /// A label is a page of its own listing who is in it, its members are
    /// filed under it, and Google's own groups are no page: `starred` is
    /// a group the contact is in, `myContacts` is not mentioned.
    #[test]
    fn a_label_is_a_page_and_a_place_its_members_are_in() {
        let data = person(
            "c2",
            json!({"names": [{"displayName": "Data"}], "memberships": filed(&["5ea9"])}),
        );
        let docs = rendered(vec![riker(), data]);
        assert_eq!(docs.len(), 3);
        let label = named(&docs, "Bridge crew");
        assert_eq!(label.rows[0].kind, "Contact group");
        assert_eq!(
            label.rows[0].source_url.as_deref(),
            Some("https://contacts.google.com/label/5ea9?authuser=picard%40enterprise.test")
        );
        let text = join(&label.sections);
        assert!(
            text.contains("- [William Riker](mailto:riker@enterprise.test"),
            "{text}"
        );
        assert!(text.contains("- Data\n"), "{text}");

        let riker = named(&docs, "William Riker");
        assert_eq!(
            riker.rows[0].account.as_deref(),
            Some("picard@enterprise.test")
        );
        let groups = &riker.contacts[0].groups;
        assert_eq!(*groups, ["Bridge crew", "Starred"]);
        let filed: Vec<&str> = riker
            .filings
            .iter()
            .map(|f| f.under_uuid.as_str())
            .collect();
        assert!(filed.contains(&label.markdown_uuid.as_str()), "{filed:?}");
    }

    /// What a page reads decides what a change renders again: a contact
    /// its own row, its label's and its photo's; a label its members'.
    #[test]
    fn a_page_reads_every_row_it_shows() {
        let mut p = riker();
        p.photo = Some((format!("{}#PROFILE/1c1", p.id), None));
        let parsed = parse(&[p], &groups(), None);
        let read = |c: &ParsedContact| -> Vec<String> {
            c.inputs
                .iter()
                .map(|i| format!("{}:{}", i.table, i.id))
                .collect()
        };
        let card = read(&parsed[0]);
        for want in [
            format!("google_people:{BOOK}#people/c1"),
            format!("google_photos:{BOOK}#people/c1#PROFILE/1c1"),
            format!("google_contact_groups:{BOOK}#contactGroups/starred"),
            "accounts:google".to_string(),
        ] {
            assert!(card.contains(&want), "{want} not in {card:?}");
        }
        assert_eq!(parsed[1].members, ["people/c1"]);
        assert!(
            read(&parsed[1]).contains(&format!("google_contact_groups:{BOOK}#contactGroups/5ea9"))
        );
    }

    /// A download's problem about a photo or a contact names only its raw
    /// row; it opens that contact's page.
    #[test]
    fn a_problem_about_a_photo_opens_the_contacts_page() {
        let docs = rendered(vec![riker()]);
        let page = &named(&docs, "William Riker").markdown_uuid;
        for (table, id) in [
            ("google_people", format!("{BOOK}#people/c1")),
            ("google_photos", format!("{BOOK}#people/c1#PROFILE/1c1")),
        ] {
            assert_eq!(
                document_of("google_contacts", table, &id).as_ref(),
                Some(page),
                "{table}"
            );
        }
        assert_eq!(document_of("google_contacts", "contacts", "ab#uid"), None);
    }
}
