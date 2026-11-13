//! What a document's rows, edges, source contacts and photos call people
//! and things, settled once for every source as the document is written
//! ([`grid_index::apply_one`](crate::grid_index::apply_one), which both
//! the render store and the grid index write through): a name, a subject
//! or a label loses its bidi controls ([`without_bidi_controls`]), and a
//! source contact loses a name that is someone else's handle
//! ([`NormalizedContact::is_anothers_handle`]). A body (a row's preview,
//! a photo's caption) keeps its controls: right-to-left prose needs them.
//! The page is the renderer's, and its names go through the shared
//! writers (`Title`, `message::chip_link`, `html::escape_name_inline`).

use std::borrow::Cow;

use datalib_contact_schema::NormalizedContact;
use datalib_photo_schema::NormalizedPhoto;
use datalib_schema::edges::EdgeRow;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::search_terms::SearchTerm;

use crate::grid_index::RenderedMarkdown;
use crate::html::{is_bidi_control, without_bidi_controls};

/// `md` as it is stored. Borrowed when nothing needed settling, which is
/// nearly always, so a document's sections are copied only when it
/// changes.
pub(crate) fn settled(md: &RenderedMarkdown) -> Cow<'_, RenderedMarkdown> {
    let mut changed = false;
    let mut rows = md.rows.clone();
    for row in &mut rows {
        changed |= settle_row(row);
    }
    let mut edges = md.edges.clone();
    for edge in &mut edges {
        changed |= settle_edge(edge);
    }
    let mut contacts = md.contacts.clone();
    for contact in &mut contacts {
        changed |= settle_contact(contact);
    }
    let mut photos = md.photos.clone();
    for photo in &mut photos {
        changed |= settle_photo(photo);
    }
    if !changed {
        return Cow::Borrowed(md);
    }
    Cow::Owned(RenderedMarkdown {
        markdown_uuid: md.markdown_uuid.clone(),
        source_id: md.source_id.clone(),
        upstream_cursor: md.upstream_cursor.clone(),
        bucket_key: md.bucket_key.clone(),
        md_path: md.md_path.clone(),
        render_version: md.render_version,
        rows,
        sections: md.sections.clone(),
        section_layout: md.section_layout.clone(),
        content_hash: md.content_hash.clone(),
        edges,
        contacts,
        photos,
        problems: md.problems.clone(),
    })
}

/// A string column not named here is an id, a path, a stamp or a body;
/// `every_grid_row_string_is_settled_or_kept` lists those, and fails on
/// a new column that is in neither list.
fn settle_row(row: &mut GridRow) -> bool {
    let mut changed = false;
    for s in [&mut row.kind, &mut row.source_label] {
        changed |= strip(s);
    }
    let names = [
        &mut row.author,
        &mut row.contact,
        &mut row.email,
        &mut row.phone,
        &mut row.account,
        &mut row.project,
        &mut row.org_name,
        &mut row.channel,
        &mut row.conversation_name,
    ];
    for s in names.into_iter().flatten() {
        changed |= strip(s);
    }
    changed | settle_supplied_terms(row)
}

/// A supplied term is searched for and, for a conversation with no
/// title, drawn as its Subject; a handle holds no control to lose.
fn settle_supplied_terms(row: &mut GridRow) -> bool {
    let Ok(terms) = row.supplied_terms() else {
        return false;
    };
    if !terms.iter().any(|t| t.value.contains(is_bidi_control)) {
        return false;
    }
    row.set_supplied_terms(
        terms
            .into_iter()
            .map(|t| SearchTerm::new(t.kind, without_bidi_controls(&t.value))),
    );
    true
}

fn settle_edge(edge: &mut EdgeRow) -> bool {
    edge.label.as_mut().is_some_and(strip)
}

fn settle_contact(contact: &mut NormalizedContact) -> bool {
    let before = contact.names.clone();
    let mut names: Vec<String> = Vec::with_capacity(contact.names.len());
    for mut name in std::mem::take(&mut contact.names) {
        strip(&mut name);
        if !contact.is_anothers_handle(&name) && !names.contains(&name) {
            names.push(name);
        }
    }
    let mut changed = names != before;
    contact.names = names;
    for s in [&mut contact.org, &mut contact.title].into_iter().flatten() {
        changed |= strip(s);
    }
    for s in contact.groups.iter_mut().chain(&mut contact.members) {
        changed |= strip(s);
    }
    changed
}

/// The caption is prose and keeps its controls; the paths in `original`
/// and `pictures` find the file and stay as the source wrote them.
fn settle_photo(photo: &mut NormalizedPhoto) -> bool {
    let mut changed = strip(&mut photo.file_name);
    let place = &mut photo.place;
    let labels = [
        &mut photo.title,
        &mut photo.headline,
        &mut photo.copyright,
        &mut photo.creator,
        &mut photo.camera,
        &mut photo.lens,
        &mut photo.color_label,
        &mut photo.folder,
        &mut place.location,
        &mut place.city,
        &mut place.state,
        &mut place.country,
    ];
    for s in labels.into_iter().flatten() {
        changed |= strip(s);
    }
    let lists = photo
        .keywords
        .iter_mut()
        .flatten()
        .chain(&mut photo.albums)
        .chain(photo.people.iter_mut().map(|p| &mut p.name));
    for s in lists {
        changed |= strip(s);
    }
    changed
}

fn strip(s: &mut String) -> bool {
    let has = s.contains(is_bidi_control);
    if has {
        *s = without_bidi_controls(s);
    }
    has
}

#[cfg(test)]
mod tests {
    use datalib_contact_schema::{ContactHandle, ContactKind};
    use datalib_photo_schema::{Exposure, MediaKind, Original, Person, Pictures, Place};
    use datalib_schema::grid_rows::{doc_file_rel, GridRowBuilder};
    use datalib_schema::providers::Provider;
    use datalib_schema::search_terms::SearchTermKind;

    use crate::indexed_markdown::IndexedMarkdownStore;

    use super::*;

    const RLO: &str = "\u{202E}";

    /// What the store gives back for `md` once it is written, the way
    /// every caller of the store writes one.
    fn stored(md: RenderedMarkdown) -> RenderedMarkdown {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        let md = RenderedMarkdown {
            md_path: dir.join(doc_file_rel(&md.markdown_uuid)),
            ..md
        };
        let st = IndexedMarkdownStore::open(dir).unwrap();
        st.put_document(dir, &md).unwrap();
        st.commit("one document").unwrap();
        st.close();
        let rd = IndexedMarkdownStore::open_for_reading(dir, None)
            .unwrap()
            .expect("a committed store is readable");
        let pin = rd.pin().unwrap().clone();
        let mut docs = rd.documents_matching(dir, None, &pin).unwrap();
        rd.close();
        docs.pop().expect("one document")
    }

    fn doc(row: GridRow) -> RenderedMarkdown {
        RenderedMarkdown {
            markdown_uuid: "md-1".into(),
            source_id: "src".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path: "md-1.md".into(),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            section_layout: None,
            content_hash: None,
            edges: Vec::new(),
            contacts: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn row() -> GridRowBuilder {
        GridRow::builder()
            .uuid("row-1")
            .provider(Provider::Test)
            .kind("Event")
            .source_label("Test")
            .conversation_uuid("md-1")
            .markdown_uuid(Some("md-1".to_string()))
            .qmd_path(Some(doc_file_rel("md-1")))
            .entire_chat("/chat/md-1")
            .body("Orders \u{202E}exe.live stays as written in a body")
            .is_document(true)
            .item_count(Some(1))
    }

    fn photo() -> NormalizedPhoto {
        NormalizedPhoto {
            uuid: "row-1".into(),
            source_id: "src".into(),
            kind: MediaKind::Photo,
            title: None,
            headline: None,
            caption: None,
            copyright: None,
            creator: None,
            captured_at: None,
            offset_from: None,
            edited_at_utc: None,
            camera: None,
            lens: None,
            exposure: Exposure::default(),
            rating: None,
            pick: None,
            color_label: None,
            keywords: Vec::new(),
            people: Vec::new(),
            albums: Vec::new(),
            place: Place::default(),
            geo: None,
            h3: Vec::new(),
            width: None,
            height: None,
            duration_seconds: None,
            file_name: "ten_forward.jpg".into(),
            folder: None,
            original: Original::default(),
            pictures: Pictures::Lightroom {
                catalog: None,
                image_id_local: 1,
                file_uuid: "F1".into(),
            },
        }
    }

    /// A right-to-left override showed `evil.exe` as `exe.live` in the
    /// grid's Author, Subject and Calendar columns, and in a contact's
    /// name, for any source: none of them went through chat-common.
    #[test]
    fn every_source_s_rows_and_contacts_lose_their_bidi_controls() {
        let r = row()
            .author(format!("Pic{RLO}ard"))
            .contact(format!("Wo{RLO}rf"))
            .channel(format!("Bridge {RLO}werC"))
            .conversation_name(format!("Orders {RLO}exe.live"))
            .org_name("\u{2067}Starfleet\u{2069}".to_string())
            .build()
            .unwrap();
        let mut troi = NormalizedContact::new("src", "troi", ContactKind::Person);
        troi.names = vec![format!("Deanna {RLO}Troi"), "Deanna\u{200F}".into()];
        troi.org = Some(format!("{RLO}Betazed"));
        troi.groups = vec![format!("Bridge{RLO}")];
        let got = stored(RenderedMarkdown {
            contacts: vec![troi],
            ..doc(r)
        });

        let row = &got.rows[0];
        assert_eq!(row.author.as_deref(), Some("Picard"));
        assert_eq!(row.contact.as_deref(), Some("Worf"));
        assert_eq!(row.channel.as_deref(), Some("Bridge werC"));
        assert_eq!(row.conversation_name.as_deref(), Some("Orders exe.live"));
        assert_eq!(row.org_name.as_deref(), Some("Starfleet"));
        let troi = &got.contacts[0];
        assert_eq!(troi.names, ["Deanna Troi", "Deanna\u{200F}"], "marks stay");
        assert_eq!(troi.org.as_deref(), Some("Betazed"));
        assert_eq!(troi.groups, ["Bridge"]);
    }

    /// A forged `From: "picard@enterprise.org" <q@continuum.test>` named
    /// Q's record after Picard's address, and the search then knew Q by
    /// it. A name that is a handle the record holds stays.
    #[test]
    fn a_contact_loses_a_name_that_is_someone_else_s_handle() {
        let mut q = NormalizedContact::new("mail", "email:q@continuum.test", ContactKind::Person);
        q.handles = vec![ContactHandle::email(None, "q@continuum.test")];
        q.names = vec![
            "picard@enterprise.org".into(),
            "picard\u{202E}@enterprise.org".into(),
            "+1 202 555 0101".into(),
            "Q".into(),
            "q@continuum.test".into(),
        ];
        let got = stored(RenderedMarkdown {
            contacts: vec![q],
            ..doc(row().build().unwrap())
        });
        assert_eq!(got.contacts[0].names, ["Q", "q@continuum.test"]);
    }

    /// A storage report and a diff group's documents are written straight
    /// to the store, never through `emit_doc`, and kept their controls.
    #[test]
    fn a_document_written_straight_to_the_store_is_settled_too() {
        let r = row().author(format!("Pic{RLO}ard")).build().unwrap();
        let got = stored(doc(r));
        assert_eq!(got.rows[0].author.as_deref(), Some("Picard"));
    }

    /// A contact's own row showed its address and number in the grid's
    /// Email and Phone columns as the card wrote them, overrides and all.
    #[test]
    fn a_row_s_email_phone_kind_and_source_label_lose_their_controls() {
        let r = row()
            .kind(format!("Ev{RLO}ent"))
            .source_label(format!("Te{RLO}st"))
            .email(Some(format!("worf{RLO}@enterprise.org")))
            .phone(Some(format!("+1 202 555 {RLO}0101")))
            .build()
            .unwrap();
        let got = stored(doc(r));
        let row = &got.rows[0];
        assert_eq!(row.kind, "Event");
        assert_eq!(row.source_label, "Test");
        assert_eq!(row.email.as_deref(), Some("worf@enterprise.org"));
        assert_eq!(row.phone.as_deref(), Some("+1 202 555 0101"));
        assert!(row.preview.contains(RLO), "a body keeps its controls");
    }

    /// A direct message's Subject is drawn from its supplied terms, by the
    /// name shown where the other party has no handle; a label is shown
    /// as it is searched.
    #[test]
    fn a_row_s_supplied_terms_lose_their_controls() {
        let mut r = row().build().unwrap();
        r.set_supplied_terms([
            SearchTerm::new(SearchTermKind::Subject, format!("Gu{RLO}inan")),
            SearchTerm::new(SearchTermKind::Label, format!("Ten {RLO}Forward")),
            SearchTerm::new(SearchTermKind::To, "email:guinan@enterprise.org"),
        ]);
        let got = stored(doc(r));
        let values: Vec<String> = got.rows[0]
            .supplied_terms()
            .unwrap()
            .into_iter()
            .map(|t| t.value)
            .collect();
        assert_eq!(
            values,
            ["Ten Forward", "Guinan", "email:guinan@enterprise.org"]
        );
    }

    #[test]
    fn an_edge_s_label_loses_its_controls() {
        let mut d = doc(row().build().unwrap());
        d.edges = vec![EdgeRow {
            edge_uuid: "e-1".into(),
            src_markdown_uuid: "md-1".into(),
            src_anchor_uuid: None,
            dst_markdown_uuid: "md-2".into(),
            dst_anchor_uuid: None,
            label: Some(format!("url{RLO}-target")),
        }];
        let got = stored(d);
        assert_eq!(got.edges[0].label.as_deref(), Some("url-target"));
    }

    /// A Lightroom title, keyword or place written with an override
    /// showed reordered in the photos applet's sheet and on its tiles.
    #[test]
    fn a_photo_s_labels_lose_their_controls_and_its_caption_keeps_them() {
        let mut p = photo();
        p.title = Some(format!("Ten {RLO}Forward"));
        p.headline = Some(format!("Guinan{RLO}"));
        p.caption = Some(format!("A quiet {RLO}evening"));
        p.copyright = Some(format!("{RLO}Starfleet"));
        p.creator = Some(format!("Da{RLO}ta"));
        p.camera = Some(format!("Tri{RLO}corder"));
        p.lens = Some(format!("{RLO}50mm"));
        p.color_label = Some(format!("Re{RLO}d"));
        p.keywords = vec![vec![format!("Ship{RLO}"), format!("Bar{RLO}")]];
        p.people = vec![Person {
            name: format!("Gui{RLO}nan"),
            face: None,
        }];
        p.albums = vec![format!("Enter{RLO}prise")];
        p.place = Place {
            location: Some(format!("Ten {RLO}Forward")),
            city: Some(format!("{RLO}Deck 10")),
            state: Some(format!("Sector{RLO} 001")),
            country: Some(format!("Feder{RLO}ation")),
        };
        p.file_name = format!("ten_forward{RLO}gpj.exe");
        p.folder = Some(format!("{RLO}2369"));
        let mut d = doc(row().build().unwrap());
        d.photos = vec![p];
        let got = &stored(d).photos[0];
        assert_eq!(got.title.as_deref(), Some("Ten Forward"));
        assert_eq!(got.headline.as_deref(), Some("Guinan"));
        assert_eq!(got.copyright.as_deref(), Some("Starfleet"));
        assert_eq!(got.creator.as_deref(), Some("Data"));
        assert_eq!(got.camera.as_deref(), Some("Tricorder"));
        assert_eq!(got.lens.as_deref(), Some("50mm"));
        assert_eq!(got.color_label.as_deref(), Some("Red"));
        assert_eq!(got.keywords, [["Ship", "Bar"]]);
        assert_eq!(got.people[0].name, "Guinan");
        assert_eq!(got.albums, ["Enterprise"]);
        assert_eq!(got.place.location.as_deref(), Some("Ten Forward"));
        assert_eq!(got.place.city.as_deref(), Some("Deck 10"));
        assert_eq!(got.place.state.as_deref(), Some("Sector 001"));
        assert_eq!(got.place.country.as_deref(), Some("Federation"));
        assert_eq!(got.file_name, "ten_forwardgpj.exe");
        assert_eq!(got.folder.as_deref(), Some("2369"));
        assert_eq!(
            got.caption.as_deref(),
            Some(format!("A quiet {RLO}evening").as_str()),
            "a caption is prose"
        );
    }

    #[test]
    fn a_document_with_nothing_to_settle_is_not_copied() {
        let d = doc(row().author("Picard".to_string()).build().unwrap());
        assert!(matches!(settled(&d), Cow::Borrowed(_)));
    }

    /// Every string column of `GridRow` is either settled or listed here
    /// as one a person does not read as a name: an id, a path, a stamp,
    /// a body. A column added to `GridRow` fails this until it is put in
    /// `settle_row` or here.
    #[test]
    fn every_grid_row_string_is_settled_or_kept() {
        const KEPT: &[&str] = &[
            "uuid",
            "provider",
            "created_at",
            "modified_at",
            "touched_at",
            "author_handle",
            "org_uuid",
            "conversation_uuid",
            "entire_chat",
            "preview",
            "content_hash",
            "qmd_path",
            "source_url",
            "git_sha",
            "upstream_id",
            "upstream_entity_kind",
            "upstream_account",
            "notion_page_uuid",
            "notion_block_uuid",
            "markdown_uuid",
            "diff_status",
            "diff_changed_columns",
            "image",
        ];
        let base = serde_json::to_value(row().build().unwrap()).unwrap();
        let columns = base.as_object().unwrap().keys().cloned();
        let mut strings = Vec::new();
        for column in columns {
            let mut v = base.clone();
            v[&column] = if column == "supplied_terms" {
                let term = SearchTerm::new(SearchTermKind::Subject, format!("a{RLO}b"));
                datalib_schema::search_terms::encode_supplied([term]).into()
            } else {
                format!("a{RLO}b").into()
            };
            let Ok(mut r) = serde_json::from_value::<GridRow>(v) else {
                continue; // not a string column
            };
            strings.push(column.clone());
            let was_settled = settle_row(&mut r);
            let kept = KEPT.contains(&column.as_str());
            assert!(
                was_settled != kept,
                "GridRow.{column}: settle it in `settle_row` if a person reads it \
                 as a name or a label, or add it to KEPT if it is an id, a path, \
                 a stamp or a body"
            );
        }
        for k in KEPT {
            assert!(
                strings.iter().any(|s| s == k),
                "KEPT names {k}, not a column"
            );
        }
    }
}
