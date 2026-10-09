//! `render_all` — one `.md` + one [`GridRow`] per contact, handed to
//! the `on_doc_complete` callback the orchestrator threads through.
//! Provider-agnostic: everything provider-specific arrives via
//! [`ContactRenderProfile`] + the [`ContactDoc`]s.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::escape_md_inline;
use datalib_etl_render::inputs::{Bucket, Buckets};
use datalib_etl_render::section::{join, Section};
use datalib_etl_render::title::Title;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use datalib_contact_schema::{is_drawable_photo, ContactHandle, Medium, NormalizedContact, Photo};

use crate::types::ContactDoc;

/// Per-provider knobs the renderer parameterizes on, so a single render
/// function serves every contact-style provider. Sibling of
/// chat-common's `RenderProfile`.
#[derive(Debug, Clone)]
pub struct ContactRenderProfile {
    /// On-disk subdir under `render_markdown/<provider>/…`, the markdown's
    /// `provider:` frontmatter key, and the grid-row `provider` column.
    pub provider: Provider,
    /// The `source_label` column on every grid row (e.g. `"LinkedIn"`,
    /// `"Apple Contacts"`).
    pub source_label: String,
    /// Discriminator for the contact's grid row (e.g. `"Contact"`).
    pub contact_kind: String,
    /// `grid_rows.upstream_entity_kind` for every row — the
    /// `entity_kind` component of the `datalib_id` recipe that minted
    /// `doc_uuid`.
    pub contact_entity_kind: &'static str,
    /// Whose mirror this is — the `account` column on every row. A
    /// LinkedIn export names its owner; a `.vcf` file names nobody.
    pub account: Option<String>,
    /// Bumped by the provider when its contact rendering changes
    /// meaningfully; stamped into the store so a re-run invalidates
    /// stale docs.
    pub render_version: u32,
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub contacts_total: usize,
    pub contacts_rendered: usize,
    pub photos_materialized: usize,
    /// Every document this call considered, rendered and skipped alike —
    /// and the ones whose render failed, which are documents we could not
    /// rewrite rather than contacts the address book lost. See
    /// `datalib_etl_chat_common::render::RenderSummary::documents`.
    pub documents: Vec<String>,
    /// Every contact rendered, with what it read — what the processor
    /// declares through `RenderCtx::declare_bucket`.
    pub buckets: Buckets,
}

pub fn render_all(
    profile: &ContactRenderProfile,
    contacts: &[ContactDoc],
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        contacts_total: contacts.len(),
        ..Default::default()
    };
    progress.set_length(Some(summary.contacts_total as u64));

    for doc in contacts {
        summary.documents.push(doc.doc_uuid.clone());
        summary.buckets.push(Bucket {
            key: doc.doc_uuid.clone(),
            inputs: doc.inputs.clone(),
        });
        // Nothing here fails on the card's account — a row that will not
        // validate is recorded as a problem and a photo that will not
        // write is skipped — so what is left is the disk and the sink,
        // and either failing is the run's to report, not one card's.
        let photo_written = render_one(profile, doc, out_dir, source_id, on_doc_complete)
            .with_context(|| format!("render contact {}", doc.doc_uuid))?;
        summary.contacts_rendered += 1;
        if photo_written {
            summary.photos_materialized += 1;
        }
        progress.inc(1);
    }
    Ok(summary)
}

fn render_one(
    profile: &ContactRenderProfile,
    doc: &ContactDoc,
    out_dir: &Path,
    source_id: &str,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<bool> {
    let m_uuid = &doc.doc_uuid;
    let contact = &doc.contact;
    let (md_path, page_dir) = output_paths(out_dir, source_id, doc);
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Photo first — written to `blobs/`, referenced from the markdown
    // with a relative path. If the photo write fails, the markdown still
    // renders (skip the embed) so a broken image doesn't poison the row.
    let photo_rel = match &contact.photo {
        Some(Photo::Inline {
            content_type,
            bytes,
        }) => write_photo(&page_dir, m_uuid, content_type, bytes).ok(),
        _ => None,
    };
    let photo_written = photo_rel.is_some();

    let sections = render_markdown(profile, doc, source_id, photo_rel.as_deref());
    fs::write(&md_path, join(&sections)).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut problems: Vec<ProblemRow> = Vec::new();
    let row = build_grid_row(profile, doc, source_id, &md_rel, &mut problems);

    // `row` reaches the index through `on_doc_complete` below; the
    // renderer writes no projection of its own any more.
    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_id: source_id.to_string(),
        upstream_cursor: contact
            .modified_at
            .clone()
            .or_else(|| contact.created_at.clone()),
        bucket_key: Some(m_uuid.clone()),
        md_path,
        render_version: profile.render_version,
        rows: row.into_iter().collect(),
        sections,
        search_terms: Vec::new(),
        edges: Vec::new(),
        // The page is about this person, so it carries them: the index
        // can then say who any of their handles is, and where their
        // photo is served from.
        contacts: vec![with_photo_url(contact, m_uuid, photo_rel.as_deref())],
        problems,
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;

    Ok(photo_written)
}

/// The contact as the index will hold it: with the photo this render
/// wrote beside the page as the URL the app serves it at, where it is an
/// image a browser draws. The index's asset route takes
/// `<markdown_uuid>/<path relative to the page>`.
fn with_photo_url(
    contact: &NormalizedContact,
    doc_uuid: &str,
    photo_rel: Option<&str>,
) -> NormalizedContact {
    let drawable = matches!(
        &contact.photo,
        Some(Photo::Inline { content_type, .. }) if is_drawable_photo(content_type)
    );
    let mut out = contact.clone();
    out.photo_url = photo_rel
        .filter(|_| drawable)
        .map(|rel| format!("/applet/unified_index/asset/{doc_uuid}/{rel}"));
    out
}

fn output_paths(out_dir: &Path, source_id: &str, doc: &ContactDoc) -> (PathBuf, PathBuf) {
    // One directory per contact, keyed by the stable contact UUID — never a
    // name/group-label slug, so a rename or regrouping re-renders in place.
    // The contact's `blobs/` (photo) live inside this dir. Display name and
    // group label still live in the frontmatter + grid row.
    let page_dir =
        datalib_etl::layout::render_markdown_root(out_dir, source_id).join(&doc.doc_uuid);
    let md_path = page_dir.join("index.md");
    (md_path, page_dir)
}

fn display_or_id(doc: &ContactDoc) -> &str {
    doc.contact
        .name()
        .or(Some(doc.contact.key.as_str()).filter(|k| !k.is_empty()))
        .unwrap_or(&doc.doc_uuid)
}

/// The page's field table and the grid row's text, in one order for
/// every source: where the person is filed, who they are, how to reach
/// them, then whatever else the source says.
pub fn table_rows(contact: &NormalizedContact) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = Vec::new();
    rows.extend(
        contact
            .groups
            .iter()
            .map(|g| ("Group".to_string(), g.clone())),
    );
    rows.extend(
        contact
            .members
            .iter()
            .map(|m| ("Member".to_string(), m.clone())),
    );
    if let Some(org) = &contact.org {
        rows.push(("Org".to_string(), org.clone()));
    }
    if let Some(title) = &contact.title {
        rows.push(("Title".to_string(), title.clone()));
    }
    rows.extend(
        contact
            .handles
            .iter()
            .map(|h| (handle_label(h), h.value.clone())),
    );
    rows.extend(
        contact
            .details
            .iter()
            .map(|d| (d.label.clone(), d.value.clone())),
    );
    if let Some(note) = &contact.note {
        rows.push(("Note".to_string(), note.clone()));
    }
    rows
}

fn handle_label(h: &ContactHandle) -> String {
    let base = match h.medium {
        Medium::Email => "Email",
        Medium::Phone => "Phone",
        Medium::Other => h.handle.as_ref().map_or("Handle", |h| h.kind().as_str()),
    };
    match h.label.as_deref() {
        Some(l) if !l.is_empty() => format!("{base} ({l})"),
        _ => base.to_string(),
    }
}

fn render_markdown(
    profile: &ContactRenderProfile,
    doc: &ContactDoc,
    source_id: &str,
    photo_rel: Option<&str>,
) -> Vec<Section> {
    let m_uuid = &doc.doc_uuid;
    let contact = &doc.contact;
    let mut out = String::with_capacity(512);

    out.push_str("---\n");
    out.push_str(&format!("markdown_uuid: {m_uuid}\n"));
    out.push_str(&format!("source_id: {source_id}\n"));
    out.push_str(&format!("provider: {}\n", profile.provider));
    out.push_str(&format!("group: {}\n", yaml_scalar(&doc.group_label)));
    if !contact.key.is_empty() {
        out.push_str(&format!("external_id: {}\n", yaml_scalar(&contact.key)));
    }
    if let Some(dn) = contact.name() {
        out.push_str(&format!("title: {}\n", yaml_scalar(dn)));
    }
    // A stamp we don't have is omitted, never written empty; the grid
    // row is `None` to match.
    if let Some(ts) = &contact.created_at {
        out.push_str(&format!("created_at: {}\n", yaml_scalar(ts)));
    }
    if let Some(ts) = &contact.modified_at {
        out.push_str(&format!("modified_at: {}\n", yaml_scalar(ts)));
    }
    out.push_str("---\n\n");
    let frontmatter = Section::unkeyed(out);

    // The page body — title, photo, field table — is the one section
    // the contact's grid row names.
    let mut out = String::with_capacity(1024);
    let title = display_or_id(doc).to_string();
    // Shared `Title` helper so contact pages carry the same
    // `data-page-title-uuid` hook the Vue side uses for the
    // copy-page-id button. `source_url` is the contact's canonical web
    // page (a LinkedIn profile, say) when the provider has one.
    out.push_str(
        &Title {
            suffix: None,
            text: &title,
            markdown_uuid: Some(m_uuid),
            source_url: contact.source_url.as_deref(),
        }
        .render(),
    );

    if let Some(rel) = photo_rel {
        out.push_str(&format!("![{}]({rel})\n\n", escape_md_inline(&title)));
    }

    let mut table_rows: Vec<(String, String)> = table_rows(contact)
        .into_iter()
        .map(|(label, value)| (escape_md_inline(&label), table_cell(&value)))
        .collect();
    if let Some(Photo::Url(url)) = &contact.photo {
        table_rows.push(("Photo URL".to_string(), autolink_cell(url)));
    }

    if !table_rows.is_empty() {
        out.push_str("| Field | Value |\n");
        out.push_str("| --- | --- |\n");
        for (k, v) in table_rows {
            out.push_str(&format!("| {k} | {v} |\n"));
        }
        out.push('\n');
    }

    vec![frontmatter, Section::keyed(m_uuid, out)]
}

/// `None`, with the reason recorded on `problems`, when the row will
/// not validate — see `GridRowBuilder::build_or_record`.
fn build_grid_row(
    profile: &ContactRenderProfile,
    doc: &ContactDoc,
    source_id: &str,
    md_rel: &str,
    problems: &mut Vec<ProblemRow>,
) -> Option<GridRow> {
    let contact = &doc.contact;
    let title = display_or_id(doc).to_string();
    // Body the UI displays / qmd indexes — compact, single string:
    // the name followed by every field value.
    let mut text = title.clone();
    for (_, value) in table_rows(contact) {
        text.push('\n');
        text.push_str(&value);
    }

    GridRow::builder()
        .uuid(doc.doc_uuid.clone())
        .provider(profile.provider)
        .kind(profile.contact_kind.clone())
        .source_label(profile.source_label.clone())
        .is_document(true)
        .item_count(Some(1))
        .created_at(contact.created_at.clone())
        .modified_at(contact.modified_at.clone())
        .author(Some(title))
        .account(profile.account.clone())
        .channel(Some(doc.group_label.clone()))
        .conversation_name(Some(doc.group_label.clone()))
        .conversation_uuid(doc.group_uuid.clone())
        .entire_chat(format!("/contact/{}", doc.doc_uuid))
        .body(text)
        .qmd_path(Some(md_rel.to_string()))
        .source_url(contact.source_url.clone())
        .upstream_id(Some(contact.key.clone()).filter(|k| !k.is_empty()))
        .upstream_entity_kind(Some(profile.contact_entity_kind.to_string()))
        .upstream_account(doc.upstream_account.clone())
        .markdown_uuid(Some(doc.doc_uuid.clone()))
        .build_or_record(source_id, &doc.doc_uuid, profile.render_version, problems)
}

fn write_photo(
    page_dir: &Path,
    doc_uuid: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<String> {
    let blobs_dir = page_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).with_context(|| format!("mkdir -p {}", blobs_dir.display()))?;
    let ext = ext_for(content_type);
    let filename = format!("{doc_uuid}.{ext}");
    let path = blobs_dir.join(&filename);
    fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(format!("blobs/{filename}"))
}

fn ext_for(content_type: &str) -> &'static str {
    match content_type.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        // LinkedIn serves its default "ghost" avatar as an SVG og:image
        // for connections with no public photo; keep the extension so it
        // renders inline rather than as an opaque `.bin`.
        "image/svg+xml" | "image/svg" => "svg",
        _ => "bin",
    }
}

/// A plain value as one table cell: each line escaped, and a line break,
/// which would end the row, drawn as `<br>`.
fn table_cell(value: &str) -> String {
    value
        .lines()
        .map(escape_md_inline)
        .collect::<Vec<_>>()
        .join("<br>")
}

/// A URL as a link in a table cell, with anything that would end the
/// link or the cell percent-encoded.
fn autolink_cell(url: &str) -> String {
    let mut out = String::from("<");
    for c in url.chars() {
        match c {
            '<' | '>' | '|' => out.push_str(&format!("%{:02X}", c as u32)),
            c if c.is_whitespace() => out.push_str(&format!("%{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('>');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_contact_schema::{ContactKind, Detail};

    fn mk_contact() -> ContactDoc {
        let mut contact = NormalizedContact::new(
            "linkedin",
            "https://www.linkedin.com/in/jlp",
            ContactKind::Person,
        );
        contact.names = vec!["Jean-Luc Picard".to_string()];
        // Offset-bearing per the grid's created_at contract (the
        // builder now rejects bare dates — see GridRowBuilder).
        contact.created_at = Some("2024-01-02T00:00:00+00:00".to_string());
        contact.source_url = Some("https://www.linkedin.com/in/jlp".to_string());
        contact.org = Some("Starfleet".to_string());
        contact.title = Some("Captain | USS Enterprise".to_string());
        ContactDoc {
            contact,
            doc_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            group_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
            group_label: "LinkedIn Connections".to_string(),
            upstream_account: None,
            inputs: Vec::new(),
        }
    }

    /// Every source's page lists the same things in the same order, and a
    /// number with no country code is still on it.
    #[test]
    fn the_table_is_one_order_for_every_source() {
        let mut c = NormalizedContact::new("s", "k", ContactKind::Person);
        c.note = Some("two\nlines".into());
        c.details = vec![Detail::new("Address (home)", "1 Main St")];
        c.handles = vec![
            ContactHandle::email(Some("work".into()), "riker@enterprise.org"),
            ContactHandle::phone(Some("cell".into()), "(555) 010-1234"),
        ];
        c.title = Some("Commander".into());
        c.org = Some("Starfleet".into());
        c.members = vec!["Troi".into()];
        c.groups = vec!["Bridge".into()];
        let labels: Vec<String> = table_rows(&c).into_iter().map(|(l, _)| l).collect();
        assert_eq!(
            labels,
            [
                "Group",
                "Member",
                "Org",
                "Title",
                "Email (work)",
                "Phone (cell)",
                "Address (home)",
                "Note"
            ]
        );
        assert_eq!(table_rows(&c)[5].1, "(555) 010-1234");
        assert_eq!(table_rows(&c)[7].1, "two\nlines");
    }

    fn mk_profile() -> ContactRenderProfile {
        ContactRenderProfile {
            provider: Provider::Linkedin,
            source_label: "LinkedIn".to_string(),
            contact_kind: "Contact".to_string(),
            contact_entity_kind: "contact",
            account: Some("jlp@enterprise.test".to_string()),
            render_version: 1,
        }
    }

    #[test]
    fn markdown_has_title_url_and_field_table() {
        let sections = render_markdown(&mk_profile(), &mk_contact(), "linkedin", None);
        assert_eq!(sections[0].uuid, None, "frontmatter belongs to no row");
        assert_eq!(
            sections[1].uuid.as_deref(),
            Some(mk_contact().doc_uuid.as_str()),
            "the body is the contact's section"
        );
        let md = join(&sections);
        assert!(md.contains("Jean-Luc Picard"));
        assert!(md.contains("https://www.linkedin.com/in/jlp"));
        assert!(md.contains("| Org | Starfleet |"));
        // Pipe inside a value is escaped so it doesn't break the table.
        assert!(md.contains("Captain \\| USS Enterprise"));
        assert!(md.contains("provider: linkedin"));
    }

    /// A name, a field's label and its value are what the address book
    /// says: none of it opens a tag or breaks out of the table, and a
    /// multi-line note keeps its lines.
    #[test]
    fn a_contact_in_markup_renders_escaped() {
        const MARKUP: &str = "<script>x</script> & co";
        let mut contact = mk_contact();
        contact.contact.names = vec![format!("{MARKUP}]")];
        contact.contact.org = None;
        contact.contact.title = None;
        contact.contact.details = vec![Detail::new(MARKUP, MARKUP)];
        contact.contact.note = Some("# not a heading\n| not a cell".to_string());
        contact.contact.photo = Some(Photo::Url("https://e.invalid/a b|c>.png".to_string()));
        let md = join(&render_markdown(
            &mk_profile(),
            &contact,
            "linkedin",
            Some("blobs/p.png"),
        ));
        let (_, body) = md
            .split_once("---\n\n")
            .expect("front matter, then the body");
        assert!(!body.contains("<script>"), "{md}");
        assert!(
            md.contains("![&lt;script&gt;x&lt;/script&gt; &amp; co\\]](blobs/p.png)"),
            "{md}"
        );
        assert!(
            md.contains("| &lt;script&gt;x&lt;/script&gt; &amp; co | &lt;script&gt;x&lt;/script&gt; &amp; co |"),
            "{md}"
        );
        assert!(
            md.contains("| Note | \\# not a heading<br>\\| not a cell |"),
            "{md}"
        );
        assert!(
            md.contains("| Photo URL | <https://e.invalid/a%20b%7Cc%3E.png> |"),
            "{md}"
        );
    }

    #[test]
    fn grid_row_carries_uuid_url_and_searchtext() {
        let mut problems = Vec::new();
        let row = build_grid_row(
            &mk_profile(),
            &mk_contact(),
            "linkedin",
            "render_markdown/x.md",
            &mut problems,
        )
        .expect("valid contact grid row");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(row.uuid, "11111111-1111-1111-1111-111111111111");
        assert_eq!(row.kind, "Contact");
        assert_eq!(
            row.source_url.as_deref(),
            Some("https://www.linkedin.com/in/jlp")
        );
        assert!(row.preview.contains("Jean-Luc Picard"));
        assert!(row.preview.contains("Starfleet"));
        assert_eq!(
            row.conversation_name.as_deref(),
            Some("LinkedIn Connections")
        );
        // The profile's account, not the source name: a source name is
        // not a login and polluted every `account:` filter.
        assert_eq!(row.account.as_deref(), Some("jlp@enterprise.test"));
    }

    /// A photo the source gave is written beside the page and reaches
    /// the index as the URL the app serves it at; a contact without one
    /// carries no URL, so the chip draws an initial.
    #[test]
    fn a_photo_written_beside_the_page_is_the_contacts_photo_url() {
        let dir = tempfile::tempdir().unwrap();
        let mut with_photo = mk_contact();
        with_photo.contact.photo = Some(Photo::Inline {
            content_type: "image/png".to_string(),
            bytes: b"\x89PNG not really".to_vec(),
        });
        let mut without = mk_contact();
        without.doc_uuid = "33333333-3333-3333-3333-333333333333".to_string();
        // Written beside the page as the source gave it, but no browser
        // draws it: no URL, so the chip draws an initial.
        let mut opaque = mk_contact();
        opaque.doc_uuid = "44444444-4444-4444-4444-444444444444".to_string();
        opaque.contact.photo = Some(Photo::Inline {
            content_type: "application/octet-stream".to_string(),
            bytes: b"who knows".to_vec(),
        });
        let mut got: Vec<RenderedMarkdown> = Vec::new();
        let mut sink = |r: RenderedMarkdown| -> Result<()> {
            got.push(r);
            Ok(())
        };
        render_all(
            &mk_profile(),
            &[with_photo, without, opaque],
            dir.path(),
            "linkedin",
            &Progress::default(),
            &mut sink,
        )
        .unwrap();
        let urls: Vec<Option<String>> = got
            .iter()
            .map(|r| r.contacts[0].photo_url.clone())
            .collect();
        assert_eq!(
            urls,
            [
                Some(
                    "/applet/unified_index/asset/11111111-1111-1111-1111-111111111111\
                     /blobs/11111111-1111-1111-1111-111111111111.png"
                        .to_string()
                ),
                None,
                None,
            ]
        );
        assert!(
            got[2]
                .md_path
                .parent()
                .unwrap()
                .join("blobs/44444444-4444-4444-4444-444444444444.bin")
                .is_file(),
            "the undrawable photo is still kept beside its page"
        );
        let written = got[0]
            .md_path
            .parent()
            .unwrap()
            .join("blobs/11111111-1111-1111-1111-111111111111.png");
        assert!(written.is_file(), "the URL names a file beside the page");
    }

    /// The sink's answer is the run's answer: a document it refuses fails
    /// the render rather than being logged and left out.
    #[test]
    fn a_sink_that_refuses_fails_the_render() {
        let dir = tempfile::tempdir().unwrap();
        let mut refuse = |_: RenderedMarkdown| -> Result<()> { anyhow::bail!("no room") };
        let err = render_all(
            &mk_profile(),
            &[mk_contact()],
            dir.path(),
            "linkedin",
            &Progress::default(),
            &mut refuse,
        )
        .expect_err("a refused document fails the render");
        assert!(format!("{err:#}").contains("no room"), "{err:#}");
    }
}
