//! Map parsed vCards into [`NormalizedContact`]s and hand them to the
//! shared [`frankweiler_etl_contact_common`] renderer.
//!
//! Everything cross-cutting — per-contact `.md` + `.grid_rows.json`
//! layout, the `| Field | Value |` table, photo materialization,
//! fingerprint-skip, the `on_doc_complete` callback — lives in
//! contact-common (the sibling of chat-common). This provider keeps
//! only what's CardDAV-specific: the vCard → field mapping, the
//! `(account, addressbook, uid)` UUID recipes, and the per-source
//! `source_label` humanization.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use frankweiler_etl::grid_index::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_contact_common::{
    render_all as cc_render_all, ContactField, ContactPhoto, ContactRenderProfile,
    NormalizedContact, RenderSummary,
};

use super::parse::{ParsedContact, ParsedContacts};
use super::{addressbook_uuid, contact_uuid};

/// Bump when the rendered layout changes enough that every existing
/// contact doc needs re-rendering. Bumped to 2 when contacts adopted the
/// shared contact-common layout (uuid-named files, generic frontmatter,
/// richer grid-row search text).
pub const RENDER_VERSION: u32 = 2;

/// Render entry point. Same signature as before the contact-common
/// migration so the sync runner's render match-arm is unchanged.
///
/// Contacts are not event-shaped — vCards without a `REV:` field have no
/// source-side timestamp, and we never fabricate one. Such rows carry
/// `when_ts: None` all the way to the GridRow.
pub fn render_all(
    parsed: &ParsedContacts,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let profile = ContactRenderProfile {
        provider: "contacts",
        source_label: humanize_source_label(source_name),
        contact_kind: "Contact".to_string(),
        render_version: RENDER_VERSION,
    };
    let contacts: Vec<NormalizedContact> = parsed
        .contacts
        .iter()
        .map(|c| normalize(c, source_name))
        .collect();
    cc_render_all(
        &profile,
        &contacts,
        out_dir,
        source_name,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )
}

/// One [`ParsedContact`] → one [`NormalizedContact`]. The UUID recipes
/// (`contact_uuid` / `addressbook_uuid`) are upstream-stable: the same
/// vCard yields the same ids whether it came over CardDAV or off disk.
fn normalize(contact: &ParsedContact, source_name: &str) -> NormalizedContact {
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

    NormalizedContact {
        contact_uuid: contact_uuid(source_name, &contact.addressbook, &contact.uid),
        group_uuid: addressbook_uuid(source_name, &contact.addressbook),
        group_label: contact.addressbook.clone(),
        display_name: contact.display_name.clone(),
        external_id: Some(contact.uid.clone()),
        // vCard `REV` → grid-ready `when_ts`. Fastmail emits *basic* ISO 8601
        // (`20260605T191839Z`), which isn't RFC 3339 and would be rejected at
        // `GridRow::build`; coerce it (already-valid values pass through).
        when_ts: contact
            .revision
            .as_deref()
            .and_then(frankweiler_time::coerce_when_ts),
        // CardDAV carries no per-contact public web URL (see the prior
        // note in this file's history re: Fastmail's internal short ids).
        source_url: None,
        fields,
        photo: contact.photo.as_ref().map(|p| ContactPhoto {
            bytes: p.bytes.clone(),
            content_type: p.content_type.clone(),
        }),
        photo_url: contact.photo_url.clone(),
    }
}

fn field_label(base: &str, type_label: &Option<String>) -> String {
    match type_label {
        Some(s) if !s.is_empty() => format!("{base} ({s})"),
        _ => base.to_string(),
    }
}

/// `source_name` is the YAML config key (`apple_contacts`,
/// `fastmail_contacts`, …). Surface a human label on grid rows by
/// stripping the `_contacts` suffix + casing. Keeps the row's Source
/// column from looking like a slug.
fn humanize_source_label(source_name: &str) -> String {
    let base = source_name
        .strip_suffix("_contacts")
        .or_else(|| source_name.strip_suffix("-contacts"))
        .unwrap_or(source_name);
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
    use crate::download::api::VcardProp;

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
    fn normalize_maps_fields_uuids_and_when_ts() {
        let n = normalize(&sample(), "tng_contacts");
        assert_eq!(
            n.contact_uuid,
            contact_uuid("tng_contacts", "Bridge", "tng-picard")
        );
        assert_eq!(n.group_uuid, addressbook_uuid("tng_contacts", "Bridge"));
        assert_eq!(n.group_label, "Bridge");
        assert_eq!(n.display_name.as_deref(), Some("Jean-Luc Picard"));
        assert_eq!(n.external_id.as_deref(), Some("tng-picard"));
        assert_eq!(n.when_ts.as_deref(), Some("2370-04-15T00:00:00Z"));
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
    // straight into `when_ts` makes `GridRow::build` reject the row and drops
    // the contact's `.grid_rows.json`. Normalize must canonicalize it to an
    // explicit-offset RFC 3339 `when_ts` the grid accepts.
    #[test]
    fn normalize_canonicalizes_basic_iso_rev() {
        let mut c = sample();
        c.revision = Some("20260605T191839Z".to_string());
        let n = normalize(&c, "fastmail_contacts");
        assert_eq!(n.when_ts.as_deref(), Some("2026-06-05T19:18:39+00:00"));
        // The grid's own contract must accept it (this is what was failing).
        frankweiler_time::validate_iso_offset(n.when_ts.as_deref().unwrap())
            .expect("normalized when_ts must satisfy GridRow's RFC 3339 contract");
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
