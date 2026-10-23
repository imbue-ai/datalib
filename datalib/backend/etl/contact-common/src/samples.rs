//! A small corpus of contacts that between them hit every layout
//! `render::render_markdown` can produce, plus [`write_samples`] to put
//! the rendered `.md` on disk.
//!
//! It exists so a contact's page can be *looked at*: `bazel run
//! //datalib/ui:render_preview` turns these files into
//! `datalib/ui/tests/goldens/contact_preview.html` through the same
//! markdown-it and the same stylesheet the app uses.

use std::path::Path;

use anyhow::Result;
use datalib_contact_schema::{ContactHandle, ContactKind, Detail, NormalizedContact, Photo};
use datalib_etl::progress::Progress;
use datalib_handle::Handle;
use datalib_schema::providers::Provider;

use crate::render::{render_all, ContactRenderProfile};
use crate::types::ContactDoc;

pub fn sample_profile() -> ContactRenderProfile {
    ContactRenderProfile {
        provider: Provider::Contacts,
        source_label: "Contacts".to_string(),
        contact_kind: "Contact".to_string(),
        contact_entity_kind: "contact",
        account: None,
        render_version: 1,
    }
}

/// A stand-in photo, as the samples' blob store would hold it. Nothing
/// serves it, so the preview draws a grey disc in its place.
fn avatar(initials: &str) -> Photo {
    Photo::Stored {
        step: "samples/ingest".to_string(),
        blake3: datalib_etl::blob_cas::blake3_hex(initials.as_bytes()),
        content_type: "image/jpeg".to_string(),
    }
}

fn doc(n: u8, contact: NormalizedContact, group_label: &str) -> ContactDoc {
    ContactDoc {
        contact,
        doc_uuid: format!("00000000-0000-4000-8000-0000000000{n:02}"),
        group_label: group_label.to_string(),
        member_handles: Vec::new(),
        upstream_account: None,
        inputs: Vec::new(),
        problems: Vec::new(),
    }
}

/// Everything a card can say: a photo, a role, two groups, two addresses,
/// a number with its country code and one without, the other details and
/// a note of more than one line.
fn everything() -> ContactDoc {
    let mut c = NormalizedContact::new("samples", "picard", ContactKind::Person);
    c.names = vec!["Jean-Luc Picard".into()];
    c.title = Some("Captain".into());
    c.org = Some("Starfleet — USS Enterprise NCC-1701-D".into());
    c.groups = vec!["Senior Staff".into(), "Away Team".into()];
    c.handles = vec![
        ContactHandle::email(Some("work".into()), "picard@enterprise.starfleet"),
        ContactHandle::email(Some("home".into()), "jl@labarre.earth"),
        ContactHandle::phone(Some("cell".into()), "+1 202 555 0101"),
        ContactHandle::phone(Some("home".into()), "(555) 010-1701"),
    ];
    c.details = vec![
        Detail::new("Address (home)", "Château Picard, La Barre, France"),
        Detail::new("Birthday", "2305-07-13"),
    ];
    c.note = Some("Make it so.\nPrefers Earl Grey, hot.".into());
    c.photo = Some(avatar("JP"));
    doc(1, c, "Bridge")
}

/// The least a card can be and still be a chip: a name and one address.
fn minimal() -> ContactDoc {
    let mut c = NormalizedContact::new("samples", "riker", ContactKind::Person);
    c.names = vec!["William T. Riker".into()];
    c.handles = vec![ContactHandle::email(None, "riker@enterprise.starfleet")];
    doc(2, c, "Bridge")
}

/// No address and no full number, so no handle: the heading is the name
/// alone, with nothing to link.
fn no_handle() -> ContactDoc {
    let mut c = NormalizedContact::new("samples", "q", ContactKind::Person);
    c.names = vec!["Q".into()];
    c.handles = vec![ContactHandle::phone(None, "555-0199")];
    c.note = Some("Omnipotent. Do not answer.".into());
    doc(3, c, "Bridge")
}

/// A group: its members listed as chips, one of them with no handle.
fn group() -> ContactDoc {
    let mut c = NormalizedContact::new("samples", "senior-staff", ContactKind::Group);
    c.names = vec!["Senior Staff".into()];
    c.members = vec![
        "Jean-Luc Picard".into(),
        "William T. Riker".into(),
        "Data".into(),
    ];
    let mut d = doc(4, c, "Bridge");
    d.member_handles = vec![
        Handle::email("picard@enterprise.starfleet"),
        Handle::email("riker@enterprise.starfleet"),
        None,
    ];
    d
}

/// A connection the way LinkedIn gives one: a profile page, a photo only
/// a fetch would reach, and a role.
fn linkedin_style() -> ContactDoc {
    let mut c = NormalizedContact::new(
        "samples",
        "https://www.linkedin.com/in/geordi",
        ContactKind::Person,
    );
    c.names = vec!["Geordi La Forge".into()];
    c.title = Some("Chief Engineer".into());
    c.org = Some("Starfleet".into());
    c.handles = vec![ContactHandle::email(None, "laforge@enterprise.starfleet")];
    c.source_url = Some("https://www.linkedin.com/in/geordi".into());
    c.photo = Some(Photo::Url(
        "https://media.example.invalid/geordi.jpg".into(),
    ));
    doc(5, c, "LinkedIn Connections")
}

/// Every plain field holding markup, which has to read as typed.
fn hostile() -> ContactDoc {
    const MARKUP: &str = "<b>Lore</b> & [brother](javascript:x)";
    let mut c = NormalizedContact::new("samples", "lore", ContactKind::Person);
    c.names = vec![MARKUP.into()];
    c.title = Some(MARKUP.into());
    c.groups = vec![MARKUP.into()];
    c.handles = vec![ContactHandle::email(
        Some(MARKUP.into()),
        "lore@omicron.theta",
    )];
    c.details = vec![Detail::new(MARKUP, MARKUP)];
    c.note = Some("# not a heading\n\n| not a cell |".into());
    doc(6, c, "Bridge")
}

pub fn sample_contacts() -> Vec<ContactDoc> {
    vec![
        everything(),
        minimal(),
        no_handle(),
        group(),
        linkedin_style(),
        hostile(),
    ]
}

/// Render every sample into `out_dir` as a source named `samples`, and
/// return the `.md` paths in corpus order.
pub fn write_samples(out_dir: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    render_all(
        &sample_profile(),
        &sample_contacts(),
        out_dir,
        "samples",
        &Progress::default(),
        &mut |doc| {
            paths.push(doc.md_path.display().to_string());
            Ok(())
        },
    )?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus renders, and between its pages shows both headings: a
    /// person as a chip, and a name with no handle to link.
    #[test]
    fn contact_samples_cover_every_layout() {
        let dir = tempfile::tempdir().unwrap();
        let pages: Vec<String> = write_samples(dir.path())
            .unwrap()
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        assert_eq!(pages.len(), sample_contacts().len());
        let has = |needle: &str| pages.iter().any(|p| p.contains(needle));
        assert!(has(
            "# [Jean-Luc Picard](mailto:picard@enterprise.starfleet"
        ));
        assert!(has("# Q\n"));
        assert!(has("<div class=\"contact-photo\">"));
        assert!(has("## Members"));
        assert!(has("<a class=\"source-link\""));
        assert!(has("<dt>Photo</dt>"));
    }
}
