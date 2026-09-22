//! Map parsed vCards into [`NormalizedContact`]s and hand them to the
//! shared [`datalib_etl_contact_common`] renderer.

use std::path::Path;

use anyhow::Result;

use datalib_etl::progress::Progress;
use datalib_etl_contact_common::{
    render_all as cc_render_all, ContactField, ContactPhoto, ContactRenderProfile,
    NormalizedContact,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Bucket, Buckets, RawRange};

use super::ids;
use super::parse::{ParsedContact, ParsedContacts};
use datalib_schema::providers::Provider;

/// Bump when the rendered layout changes enough that every existing
/// contact doc needs re-rendering. Bumped to 2 when contacts adopted the
/// shared contact-common layout (uuid-named files, generic frontmatter,
/// richer grid-row search text); to 3 when `account` stopped carrying
/// the source name; to 4 when ids moved onto `datalib_id` under
/// `SourceInstance` and every row gained its backpointer — every uuid
/// moved.
pub const RENDER_VERSION: u32 = 4;

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
    let profile = ContactRenderProfile {
        provider: Provider::Contacts,
        source_label: humanize_source_label(source_id),
        contact_kind: "Contact".to_string(),
        contact_entity_kind: ids::KIND_CONTACT,
        // A `.vcf` file has no login behind it.
        account: None,
        render_version: RENDER_VERSION,
    };
    let mut contacts: Vec<NormalizedContact> = parsed
        .contacts
        .iter()
        .map(|c| normalize(c, source_id))
        .collect();

    // What to render: the contacts the driver found stale, plus every
    // card of a row the diff named — one `contacts` row can hold several
    // cards, which is why the row alone could never name a document and
    // the mapping goes through the parse.
    let forward = parsed.changed.as_ref().map(|changed| {
        contacts
            .iter()
            .filter(|c| {
                c.inputs
                    .iter()
                    .any(|i| changed.get(&i.table).is_some_and(|ids| ids.contains(&i.id)))
            })
            .map(|c| c.contact_uuid.clone())
            .collect::<std::collections::HashSet<String>>()
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
        contacts.retain(|c| render.contains(&c.contact_uuid));
    }
    let summary = cc_render_all(
        &profile,
        &contacts,
        out_dir,
        source_id,
        progress,
        on_doc_complete,
    )?;
    buckets.extend(summary.buckets);
    Ok(buckets)
}

fn normalize(contact: &ParsedContact, source_id: &str) -> NormalizedContact {
    let mut fields: Vec<ContactField> = Vec::new();
    if let Some(org) = &contact.org {
        fields.push(ContactField::new("Org", org.replace(';', " — ")));
    }
    if let Some(t) = &contact.title {
        fields.push(ContactField::new("Title", t.clone()));
    }
    for e in &contact.emails {
        fields.push(ContactField::new(
            field_label("Email", &e.type_label()),
            e.value.clone(),
        ));
    }
    for p in &contact.phones {
        fields.push(ContactField::new(
            field_label("Phone", &p.type_label()),
            p.value.clone(),
        ));
    }
    for a in &contact.addresses {
        // ADR is `;`-separated: PO box; ext; street; locality; region; postcode; country
        fields.push(ContactField::new(
            field_label("Address", &a.type_label()),
            a.value.replace(';', ", "),
        ));
    }
    if let Some(n) = &contact.note {
        fields.push(ContactField::new("Note", n.replace('\n', " <br> ")));
    }

    let id = ids::contact(source_id, &contact.addressbook, &contact.uid);
    NormalizedContact {
        contact_uuid: id.uuid,
        group_uuid: ids::addressbook(source_id, &contact.addressbook).uuid,
        group_label: contact.addressbook.clone(),
        display_name: contact.display_name.clone(),
        external_id: Some(id.natural_key),
        upstream_scope: Some(source_id.to_string()),
        // A card carries no creation stamp.
        created_at: None,
        // vCard `REV` is the revision stamp. Fastmail emits *basic* ISO
        // 8601 (`20260605T191839Z`), which isn't RFC 3339 and would be
        // rejected at `GridRow::build`; coerce it (already-valid values
        // pass through).
        modified_at: contact
            .revision
            .as_deref()
            .and_then(datalib_time::coerce_record_stamp),
        // CardDAV carries no per-contact public web URL (see the prior
        // note in this file's history re: Fastmail's internal short ids).
        source_url: None,
        fields,
        photo: contact.photo.as_ref().map(|p| ContactPhoto {
            bytes: p.bytes.clone(),
            content_type: p.content_type.clone(),
        }),
        photo_url: contact.photo_url.clone(),
        inputs: contact.inputs.clone(),
    }
}

fn field_label(base: &str, type_label: &Option<String>) -> String {
    match type_label {
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
    use datalib_etl_contacts::ingest::api::VcardProp;

    fn prop(value: &str, ty: Option<&str>) -> VcardProp {
        VcardProp {
            value: value.to_string(),
            params: ty
                .map(|t| vec![("TYPE".to_string(), t.to_string())])
                .unwrap_or_default(),
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
            emails: vec![prop("jlp@enterprise", Some("WORK"))],
            phones: vec![prop("+1-555", Some("WORK"))],
            addresses: vec![prop(";;Ready Room;Deck 1;;;", Some("WORK"))],
            org: Some("Starfleet;USS Enterprise".to_string()),
            title: Some("Captain".to_string()),
            note: Some("Make it so.".to_string()),
            photo: None,
            photo_url: None,
        }
    }

    #[test]
    fn normalize_maps_fields_uuids_and_stamps() {
        let n = normalize(&sample(), "tng_contacts");
        assert_eq!(
            n.contact_uuid,
            ids::contact("tng_contacts", "Bridge", "tng-picard").uuid
        );
        assert_eq!(
            n.group_uuid,
            ids::addressbook("tng_contacts", "Bridge").uuid
        );
        assert_eq!(n.group_label, "Bridge");
        assert_eq!(n.display_name.as_deref(), Some("Jean-Luc Picard"));
        assert_eq!(n.external_id.as_deref(), Some("Bridge#tng-picard"));
        assert_eq!(n.upstream_scope.as_deref(), Some("tng_contacts"));
        assert_eq!(n.created_at, None, "a card has no creation stamp");
        assert_eq!(n.modified_at.as_deref(), Some("2370-04-15T00:00:00Z"));
        // Org `;` becomes ` — `; address `;` becomes `, `; typed labels.
        let labels: Vec<&str> = n.fields.iter().map(|f| f.label.as_str()).collect();
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
        assert_eq!(n.fields[0].value, "Starfleet — USS Enterprise");
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
        let n = normalize(&c, "fastmail_contacts");
        assert_eq!(n.modified_at.as_deref(), Some("2026-06-05T19:18:39+00:00"));
        // The grid's own contract must accept it (this is what was failing).
        datalib_time::validate_iso_offset(n.modified_at.as_deref().unwrap())
            .expect("normalized modified_at must satisfy GridRow's RFC 3339 contract");
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
