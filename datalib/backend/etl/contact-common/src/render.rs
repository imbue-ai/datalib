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
use datalib_etl_render::html::{escape_attr, escape_md_inline, escape_text};
use datalib_etl_render::inputs::{Bucket, Buckets};
use datalib_etl_render::message::chip_link;
use datalib_etl_render::photo::{photo_address, with_photo_url};
use datalib_etl_render::section::{join, Section};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::{Outcome, ProblemRow, Scope, Stage};
use datalib_schema::providers::Provider;
use datalib_schema::search_terms::{SearchTerm, SearchTermKind};

use datalib_contact_schema::{ContactHandle, Medium, NormalizedContact, Photo};
use datalib_handle::Handle;

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
    render_into(
        &mut summary,
        profile,
        contacts,
        out_dir,
        source_id,
        progress,
        on_doc_complete,
    )?;
    Ok(summary)
}

/// [`render_all`]'s loop, for a provider that renders its contacts a
/// batch at a time into one summary; it sets the progress length itself.
pub fn render_into(
    summary: &mut RenderSummary,
    profile: &ContactRenderProfile,
    contacts: &[ContactDoc],
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    for doc in contacts {
        summary.documents.push(doc.doc_uuid.clone());
        summary.buckets.push(Bucket {
            key: doc.doc_uuid.clone(),
            inputs: doc.inputs.clone(),
        });
        // Nothing here fails on the card's account — a row that will not
        // validate is recorded as a problem — so what is left is the disk
        // and the sink, and either failing is the run's to report, not
        // one card's.
        render_one(profile, doc, out_dir, source_id, on_doc_complete)
            .with_context(|| format!("render contact {}", doc.doc_uuid))?;
        summary.contacts_rendered += 1;
        progress.inc(1);
    }
    Ok(())
}

fn render_one(
    profile: &ContactRenderProfile,
    doc: &ContactDoc,
    out_dir: &Path,
    source_id: &str,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let m_uuid = &doc.doc_uuid;
    let contact = &doc.contact;
    let (md_path, page_dir) = output_paths(out_dir, source_id, doc);
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;
    remove_old_blobs(&page_dir);

    let photo = contact.photo.as_ref().and_then(photo_address);
    let sections = render_markdown(profile, doc, source_id, photo.as_deref());
    fs::write(&md_path, join(&sections)).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut problems: Vec<ProblemRow> = doc
        .problems
        .iter()
        .map(|p| {
            ProblemRow::new(
                source_id,
                Stage::Parse,
                Scope::Markdown(m_uuid),
                None,
                Outcome::Nulled,
                p.clone(),
                Some(profile.render_version),
            )
        })
        .collect();
    let mut row = build_grid_row(profile, doc, source_id, &md_rel, &mut problems);
    if let Some(row) = &mut row {
        row.set_supplied_terms(about_terms(contact));
    }

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
        edges: Vec::new(),
        // The page is about this person, so it carries them: the index
        // can then say who any of their handles is, and where their
        // photo is served from.
        contacts: vec![with_photo_url(contact)],
        problems,
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;

    Ok(())
}

/// The `blobs/` an older build wrote a photo into beside the page; the
/// page now links the photo where its blob store holds it.
fn remove_old_blobs(page_dir: &Path) {
    let blobs_dir = page_dir.join("blobs");
    if let Err(e) = fs::remove_dir_all(&blobs_dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(dir = %blobs_dir.display(), error = %e, "contact_common: removing an old blobs/ failed");
        }
    }
}

fn output_paths(out_dir: &Path, source_id: &str, doc: &ContactDoc) -> (PathBuf, PathBuf) {
    // One directory per contact, keyed by the stable contact UUID — never a
    // name/group-label slug, so a rename or regrouping re-renders in place.
    // Display name and group label still live in the frontmatter + grid
    // row.
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

/// One line of a contact's field list: what it is (`Email`), the
/// source's own word for which one (`work`), the value as written, and
/// either the handle it is, drawn as a chip, or a plain link (a photo's
/// URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    pub label: String,
    pub qualifier: Option<String>,
    pub value: String,
    pub handle: Option<Handle>,
    pub href: Option<String>,
}

/// The page's fields, in one order for every source: how to reach the
/// person, then whatever else the source says, then the note. Name,
/// role and groups are the page's head, not fields, and so is a handle
/// that is neither an address nor a number when the heading's chip is
/// drawn by it: its spelling (`name/Will Riker`) tells a reader nothing.
pub fn field_rows(contact: &NormalizedContact) -> Vec<FieldRow> {
    let head = head_handle(contact);
    let mut rows: Vec<FieldRow> = contact
        .handles
        .iter()
        .filter(|h| !is_the_chips_spelling(h, head.as_ref()))
        .map(|h| FieldRow {
            label: match h.medium {
                Medium::Email => "Email",
                Medium::Phone => "Phone",
                Medium::Other => h.handle.as_ref().map_or("Handle", |h| h.kind().as_str()),
            }
            .to_string(),
            qualifier: h.label.clone().filter(|l| !l.trim().is_empty()),
            value: h.value.clone(),
            handle: h.handle.clone(),
            href: None,
        })
        .collect();
    rows.extend(contact.details.iter().map(|d| {
        let (label, qualifier) = split_qualifier(&d.label);
        FieldRow {
            label,
            qualifier,
            value: d.value.clone(),
            handle: None,
            href: None,
        }
    }));
    if let Some(Photo::Url(url)) = &contact.photo {
        rows.push(FieldRow {
            label: "Photo".to_string(),
            qualifier: None,
            value: url.clone(),
            handle: None,
            href: Some(url.clone()),
        });
    }
    if let Some(note) = &contact.note {
        rows.push(FieldRow {
            label: "Note".to_string(),
            qualifier: None,
            value: note.clone(),
            handle: None,
            href: None,
        });
    }
    rows
}

/// A handle that is neither an address nor a number, when the page's
/// chip is drawn by it: its spelling (`name/Will Riker`) tells a reader
/// nothing the chip does not.
fn is_the_chips_spelling(h: &ContactHandle, head: Option<&Handle>) -> bool {
    h.medium == Medium::Other && h.handle.is_some() && h.handle.as_ref() == head
}

/// A detail's label as providers write it, `Address (home)`, read as
/// the label and the source's word for which one.
fn split_qualifier(label: &str) -> (String, Option<String>) {
    match label.strip_suffix(')').and_then(|l| l.split_once(" (")) {
        Some((base, q)) if !base.trim().is_empty() && !q.trim().is_empty() => {
            (base.to_string(), Some(q.to_string()))
        }
        _ => (label.to_string(), None),
    }
}

/// Who the card is about, as `with:` finds it: each handle it holds and
/// each name it gives.
fn about_terms(contact: &NormalizedContact) -> impl Iterator<Item = SearchTerm> + '_ {
    let handles = contact
        .handles
        .iter()
        .filter_map(|h| h.handle.as_ref())
        .map(|h| h.as_str().to_string());
    let names = contact
        .names
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    handles
        .chain(names)
        .map(|value| SearchTerm::new(SearchTermKind::About, value))
}

/// A contact's first email address or phone number, as the source wrote it.
pub fn first_value(contact: &NormalizedContact, medium: Medium) -> Option<&str> {
    contact
        .handles
        .iter()
        .find(|h| h.medium == medium)
        .map(|h| h.value.as_str())
}

/// The handle a contact's chip is drawn by, from its first email address
/// and its first number: the address, else the number, where either makes
/// one. The grid's Contact cell (`columns.rs::contact_identity` in the
/// applets) reads the `email` and `phone` columns by the same rule.
pub fn chip_handle(email: Option<&str>, phone: Option<&str>) -> Option<Handle> {
    email
        .and_then(Handle::email)
        .or_else(|| phone.and_then(Handle::tel))
}

/// The grid row's Contents: what the card says besides the name and the
/// first address and number, which have columns of their own. The note
/// leads, since it is the one thing nobody but the person wrote; the
/// groups the card is filed in come last.
fn summary(contact: &NormalizedContact) -> String {
    let email = first_value(contact, Medium::Email);
    let phone = first_value(contact, Medium::Phone);
    let head = head_handle(contact);
    let mut lines: Vec<String> = contact.note.iter().cloned().collect();
    lines.extend(contact.org.iter().cloned());
    lines.extend(contact.title.iter().cloned());
    lines.extend(contact.members.iter().cloned());
    lines.extend(
        contact
            .handles
            .iter()
            .filter(|h| match h.medium {
                Medium::Email => Some(h.value.as_str()) != email,
                Medium::Phone => Some(h.value.as_str()) != phone,
                Medium::Other => !is_the_chips_spelling(h, head.as_ref()),
            })
            .map(|h| h.value.clone()),
    );
    lines.extend(contact.details.iter().map(|d| d.value.clone()));
    lines.extend(contact.groups.iter().cloned());
    lines.join("\n")
}

fn render_markdown(
    profile: &ContactRenderProfile,
    doc: &ContactDoc,
    source_id: &str,
    photo: Option<&str>,
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

    // The page body — the head, the fields, a group's members — is the
    // one section the contact's grid row names.
    let mut out = String::with_capacity(1024);
    out.push_str(&head(doc, photo));
    out.push_str(&fields(contact));
    out.push_str(&members(doc));
    vec![frontmatter, Section::keyed(m_uuid, out)]
}

/// The top of the page: the photo, the person as a chip (the right-click
/// that links them to a contact), their role and the groups they are
/// filed in. The heading is a markdown `#` so the chip link inside it is
/// marked; the copy-page-id button finds it through the wrapper's
/// `data-page-title-uuid`.
fn head(doc: &ContactDoc, photo: Option<&str>) -> String {
    let contact = &doc.contact;
    let name = display_or_id(doc);
    let mut out = format!(
        "<div class=\"contact-head\" data-page-title-uuid=\"{}\">\n\n",
        escape_attr(&doc.doc_uuid)
    );
    if let Some(src) = photo {
        out.push_str(&format!(
            "<div class=\"contact-photo\">\n\n![{}]({src})\n\n</div>\n\n",
            escape_md_inline(name)
        ));
    }
    out.push_str("<div class=\"contact-who\">\n\n");
    let heading = match head_handle(contact) {
        Some(handle) => chip_link(name, &handle),
        None => escape_md_inline(name),
    };
    out.push_str(&format!("# {heading}"));
    if let Some(url) = &contact.source_url {
        out.push_str(&format!(
            " <a class=\"source-link\" href=\"{}\" target=\"_blank\" rel=\"noopener noreferrer\">↗</a>",
            escape_attr(url)
        ));
    }
    out.push_str("\n\n");
    let role: Vec<&str> = [contact.title.as_deref(), contact.org.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !role.is_empty() {
        out.push_str(&format!(
            "<p class=\"contact-role\">{}</p>\n\n",
            lines_html(&role.join(" · "))
        ));
    }
    if !contact.groups.is_empty() {
        let pills: Vec<String> = contact
            .groups
            .iter()
            .map(|g| format!("<span class=\"contact-group\">{}</span>", escape_text(g)))
            .collect();
        out.push_str(&format!(
            "<p class=\"contact-groups\">{}</p>\n\n",
            pills.join(" ")
        ));
    }
    out.push_str("</div>\n</div>\n\n");
    out
}

/// The handle the page's heading is drawn as, by the grid's Contact
/// cell's rule (`columns.rs::contact_identity`): the first address, else
/// the first number, else a handle of any kind, since a Facebook friend
/// or a LinkedIn connection may have no other.
fn head_handle(contact: &NormalizedContact) -> Option<Handle> {
    chip_handle(
        first_value(contact, Medium::Email),
        first_value(contact, Medium::Phone),
    )
    .or_else(|| contact.handles.iter().find_map(|h| h.handle.clone()))
}

/// The field list. A plain value is text between tags, with no line break
/// that could end the HTML block whatever it holds. A handle is a chip
/// link, which only markdown makes, so its `<dd>` closes the block around
/// one paragraph holding the link; the `chip-identifiers` class has the
/// viewer draw it as the address rather than the person, and a plain
/// click on it is the chip's, never the mail app's.
fn fields(contact: &NormalizedContact) -> String {
    let rows = field_rows(contact);
    if rows.is_empty() {
        return String::new();
    }
    let mut out = String::from("<dl class=\"contact-fields\">\n");
    for row in rows {
        let qualifier = row
            .qualifier
            .as_deref()
            .map(|q| {
                format!(
                    " <span class=\"contact-qualifier\">{}</span>",
                    escape_text(q)
                )
            })
            .unwrap_or_default();
        let dt = format!("<dt>{}{qualifier}</dt>", escape_text(&row.label));
        let line = match (&row.handle, &row.href) {
            (Some(handle), _) => format!(
                "{dt}<dd class=\"chip-identifiers\">\n\n{}\n\n</dd>\n",
                chip_link(&row.value, handle)
            ),
            (None, Some(href)) => format!(
                "{dt}<dd><a href=\"{}\">{}</a></dd>\n",
                escape_attr(href),
                lines_html(&row.value)
            ),
            (None, None) => format!("{dt}<dd>{}</dd>\n", lines_html(&row.value)),
        };
        out.push_str(&line);
    }
    out.push_str("</dl>\n\n");
    out
}

/// A group's members, each a chip where the member has a handle.
fn members(doc: &ContactDoc) -> String {
    if doc.contact.members.is_empty() {
        return String::new();
    }
    let mut out = String::from("<div class=\"contact-members\">\n\n## Members\n\n");
    for (i, name) in doc.contact.members.iter().enumerate() {
        let line = match doc.member_handles.get(i).and_then(Option::as_ref) {
            Some(handle) => chip_link(name, handle),
            None => escape_md_inline(name),
        };
        out.push_str(&format!("- {line}\n"));
    }
    out.push_str("\n</div>\n\n");
    out
}

/// Plain text between tags, its line breaks kept as `<br>`.
fn lines_html(value: &str) -> String {
    value
        .lines()
        .map(escape_text)
        .collect::<Vec<_>>()
        .join("<br>")
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
    // The contact is the document, so it is its own conversation; the
    // address book it is filed in is the channel.
    GridRow::builder()
        .uuid(doc.doc_uuid.clone())
        .provider(profile.provider)
        .kind(profile.contact_kind.clone())
        .source_label(profile.source_label.clone())
        .is_document(true)
        .item_count(Some(1))
        .created_at(contact.created_at.clone())
        .modified_at(contact.modified_at.clone())
        .contact(Some(title.clone()))
        .email(first_value(contact, Medium::Email).map(str::to_string))
        .phone(first_value(contact, Medium::Phone).map(str::to_string))
        .account(profile.account.clone())
        .channel(Some(doc.group_label.clone()))
        .conversation_name(Some(title))
        .conversation_uuid(doc.doc_uuid.clone())
        .entire_chat(format!("/contact/{}", doc.doc_uuid))
        .body(summary(contact))
        .qmd_path(Some(md_rel.to_string()))
        .source_url(contact.source_url.clone())
        .upstream_id(Some(contact.key.clone()).filter(|k| !k.is_empty()))
        .upstream_entity_kind(Some(profile.contact_entity_kind.to_string()))
        .upstream_account(doc.upstream_account.clone())
        .markdown_uuid(Some(doc.doc_uuid.clone()))
        .build_or_record(source_id, &doc.doc_uuid, profile.render_version, problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_contact_schema::{ContactHandle, ContactKind, Detail};

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
            group_label: "LinkedIn Connections".to_string(),
            member_handles: Vec::new(),
            upstream_account: None,
            inputs: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Every source's page lists the same things in the same order, and a
    /// number with no country code is still on it.
    #[test]
    fn the_fields_are_one_order_for_every_source() {
        let mut c = NormalizedContact::new("s", "k", ContactKind::Person);
        c.note = Some("two\nlines".into());
        c.details = vec![Detail::new("Address (home)", "1 Main St")];
        c.handles = vec![
            ContactHandle::email(Some("work".into()), "riker@enterprise.org"),
            ContactHandle::phone(Some("cell".into()), "(555) 010-1234"),
        ];
        c.title = Some("Commander".into());
        c.org = Some("Starfleet".into());
        c.groups = vec!["Bridge".into()];
        let rows = field_rows(&c);
        let labels: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|r| (r.label.as_str(), r.qualifier.as_deref()))
            .collect();
        assert_eq!(
            labels,
            [
                ("Email", Some("work")),
                ("Phone", Some("cell")),
                ("Address", Some("home")),
                ("Note", None)
            ]
        );
        assert_eq!(
            rows[0].handle.as_ref().map(Handle::as_str),
            Some("email:riker@enterprise.org")
        );
        assert_eq!(rows[1].value, "(555) 010-1234");
        assert_eq!(
            rows[1].handle, None,
            "a number with no country code is no handle"
        );
        assert_eq!(rows[3].value, "two\nlines");
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
    fn markdown_has_a_head_and_a_field_list() {
        let sections = render_markdown(&mk_profile(), &mk_contact(), "linkedin", None);
        assert_eq!(sections[0].uuid, None, "frontmatter belongs to no row");
        assert_eq!(
            sections[1].uuid.as_deref(),
            Some(mk_contact().doc_uuid.as_str()),
            "the body is the contact's section"
        );
        let md = join(&sections);
        assert!(
            md.contains(
                "<div class=\"contact-head\" data-page-title-uuid=\"11111111-1111-1111-1111-111111111111\">"
            ),
            "{md}"
        );
        assert!(md.contains("# Jean-Luc Picard <a class=\"source-link\" href=\"https://www.linkedin.com/in/jlp\""), "{md}");
        assert!(
            md.contains("<p class=\"contact-role\">Captain | USS Enterprise · Starfleet</p>"),
            "{md}"
        );
        assert!(md.contains("provider: linkedin"));
    }

    /// The person leads the page as their chip, by the handle the grid's
    /// Contact cell draws: the first address, else the first number, else
    /// the first handle of any kind.
    #[test]
    fn the_head_is_the_persons_chip() {
        let mut doc = mk_contact();
        doc.contact.handles = vec![
            ContactHandle::phone(Some("cell".into()), "+1 202 555 0101"),
            ContactHandle::email(Some("work".into()), "jlp@enterprise.test"),
        ];
        let md = join(&render_markdown(&mk_profile(), &doc, "linkedin", None));
        assert!(
            md.contains(
                "# [Jean-Luc Picard](mailto:jlp@enterprise.test \"Jean-Luc Picard <jlp@enterprise.test>\")"
            ),
            "{md}"
        );
        assert!(
            md.contains(
                "<dt>Email <span class=\"contact-qualifier\">work</span></dt>\
                 <dd class=\"chip-identifiers\">\n\n\
                 [jlp@enterprise.test](mailto:jlp@enterprise.test \"jlp@enterprise.test\")\n\n</dd>"
            ),
            "{md}"
        );
        doc.contact.handles = vec![ContactHandle::of(
            Handle::facebook_name("Jean-Luc Picard").unwrap(),
        )];
        let md = join(&render_markdown(&mk_profile(), &doc, "linkedin", None));
        assert!(
            md.contains("# [Jean-Luc Picard](datalib:handle/facebook/name/Jean-Luc%20Picard "),
            "{md}"
        );
        assert!(
            !md.contains("<dt>facebook"),
            "the chip is the handle; its spelling is no field: {md}"
        );
    }

    /// A name, a field's label and its value are what the address book
    /// says: none of it opens a tag or ends the field list, and a
    /// multi-line note keeps its lines.
    #[test]
    fn a_contact_in_markup_renders_escaped() {
        const MARKUP: &str = "<script>x</script> & co";
        let mut contact = mk_contact();
        contact.contact.names = vec![format!("{MARKUP}]")];
        contact.contact.org = Some(MARKUP.to_string());
        contact.contact.title = None;
        contact.contact.groups = vec![MARKUP.to_string()];
        contact.contact.details = vec![Detail::new(MARKUP, MARKUP)];
        contact.contact.note = Some("# not a heading\n\n| not a cell".to_string());
        contact.contact.photo = Some(Photo::Url("https://e.invalid/a\"b.png".to_string()));
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
            md.contains("# &lt;script&gt;x&lt;/script&gt; &amp; co\\]"),
            "{md}"
        );
        assert!(
            md.contains(
                "<span class=\"contact-group\">&lt;script&gt;x&lt;/script&gt; &amp; co</span>"
            ),
            "{md}"
        );
        assert!(
            md.contains("<dt>&lt;script&gt;x&lt;/script&gt; &amp; co</dt><dd>&lt;script&gt;x&lt;/script&gt; &amp; co</dd>"),
            "{md}"
        );
        assert!(
            md.contains("<dt>Note</dt><dd># not a heading<br><br>| not a cell</dd>"),
            "{md}"
        );
        assert!(
            md.contains("<dd><a href=\"https://e.invalid/a&quot;b.png\">https://e.invalid/a\"b.png</a></dd>"),
            "{md}"
        );
        let fields = body
            .split_once("<dl class=\"contact-fields\">")
            .and_then(|(_, rest)| rest.split_once("</dl>"))
            .map(|(dl, _)| dl)
            .expect("a field list");
        for line in fields
            .lines()
            .filter(|l| l.starts_with("<dt>") && !l.contains("chip-identifiers"))
        {
            assert!(
                line.ends_with("</dd>"),
                "a plain value keeps to its line, so nothing ends the HTML block: {line}"
            );
        }
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
        assert!(
            !row.preview.contains("Jean-Luc Picard"),
            "the name is the Contact cell"
        );
        assert!(row.preview.contains("Starfleet"));
        assert_eq!(row.conversation_name.as_deref(), Some("Jean-Luc Picard"));
        assert_eq!(
            row.conversation_uuid, row.uuid,
            "the contact is the document"
        );
        assert_eq!(row.channel.as_deref(), Some("LinkedIn Connections"));
        assert_eq!(row.contact.as_deref(), Some("Jean-Luc Picard"));
        assert_eq!(row.author, None, "nobody wrote a contact");
        // The profile's account, not the source name: a source name is
        // not a login and polluted every `account:` filter.
        assert_eq!(row.account.as_deref(), Some("jlp@enterprise.test"));
    }

    fn stored(content_type: &str) -> Photo {
        Photo::Stored {
            step: "linkedin/ingest".to_string(),
            blake3: "ab".repeat(32),
            content_type: content_type.to_string(),
        }
    }

    /// A stored photo is linked at its blob-store address, on the page
    /// and as the contact's URL in the index, and nothing is written
    /// beside the page: a `blobs/` an older build left there goes. A
    /// photo no browser draws is still on its page but gives the chip no
    /// URL, and a contact with no photo carries none, so the chip draws
    /// an initial.
    #[test]
    fn a_stored_photo_is_linked_where_its_blob_store_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let address = format!("/data/linkedin/ingest/blob/{}", "ab".repeat(32));
        let mut with_photo = mk_contact();
        with_photo.contact.photo = Some(stored("image/png"));
        let mut without = mk_contact();
        without.doc_uuid = "33333333-3333-3333-3333-333333333333".to_string();
        let mut opaque = mk_contact();
        opaque.doc_uuid = "44444444-4444-4444-4444-444444444444".to_string();
        opaque.contact.photo = Some(stored("application/octet-stream"));
        let old_blobs = dir
            .path()
            .join("linkedin/render_markdown")
            .join(&with_photo.doc_uuid)
            .join("blobs");
        std::fs::create_dir_all(&old_blobs).unwrap();
        std::fs::write(old_blobs.join("photo.png"), b"an older build's copy").unwrap();
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
        assert_eq!(urls, [Some(address.clone()), None, None]);
        let page = |r: &RenderedMarkdown| std::fs::read_to_string(&r.md_path).unwrap();
        assert!(page(&got[0]).contains(&format!("]({address})")));
        assert!(page(&got[2]).contains(&format!("]({address})")));
        assert!(!page(&got[1]).contains("contact-photo"));
        assert!(!old_blobs.exists(), "the old blobs/ is gone");
        for r in &got {
            assert!(!r.md_path.parent().unwrap().join("blobs").exists());
        }
    }

    /// A problem the provider found with the card, such as a photo its
    /// blob store lacks, reaches the index on the card's document.
    #[test]
    fn the_cards_own_problems_are_on_its_document() {
        let dir = tempfile::tempdir().unwrap();
        let mut doc = mk_contact();
        doc.problems = vec![datalib_schema::problems::Problem::field(
            "photo",
            datalib_schema::problems::Reason::BlobMissing,
            &"ab".repeat(32),
        )];
        let mut got: Vec<RenderedMarkdown> = Vec::new();
        render_all(
            &mk_profile(),
            &[doc],
            dir.path(),
            "linkedin",
            &Progress::default(),
            &mut |r| {
                got.push(r);
                Ok(())
            },
        )
        .unwrap();
        let reasons: Vec<_> = got[0].problems.iter().map(|p| p.reason).collect();
        assert_eq!(reasons, [datalib_schema::problems::Reason::BlobMissing]);
    }

    /// A contact's row carries its first address and number in columns of
    /// their own, and its Contents leads with the note and leaves out
    /// what those columns and the Contact chip already show.
    #[test]
    fn a_contacts_row_has_its_address_and_number_and_a_note_first() {
        let mut doc = mk_contact();
        doc.contact.handles = vec![
            ContactHandle::email(Some("work".into()), "riker@enterprise.org"),
            ContactHandle::email(Some("home".into()), "will@risa.test"),
            ContactHandle::phone(Some("cell".into()), "(555) 010-1234"),
        ];
        doc.contact.note = Some("Plays the trombone".into());
        doc.contact.groups = vec!["Bridge".into()];
        let row = build_grid_row(&mk_profile(), &doc, "linkedin", "x.md", &mut Vec::new())
            .expect("valid contact grid row");
        assert_eq!(row.email.as_deref(), Some("riker@enterprise.org"));
        assert_eq!(row.phone.as_deref(), Some("(555) 010-1234"));
        assert_eq!(
            row.preview,
            "Plays the trombone Starfleet Captain | USS Enterprise will@risa.test Bridge"
        );
    }

    /// A contact known by a handle that is neither an address nor a
    /// number — a Facebook friend, by name — is its chip; the handle's
    /// spelling (`name/Will Riker`) is no part of its Contents.
    #[test]
    fn a_chip_handles_spelling_is_not_in_the_contents() {
        let mut doc = mk_contact();
        doc.contact.names = vec!["Will Riker".into()];
        doc.contact.org = None;
        doc.contact.title = None;
        doc.contact.handles = vec![ContactHandle::of(
            Handle::facebook_name("Will Riker").unwrap(),
        )];
        doc.contact.details = vec![Detail::new("Friends since", "2364-01-05")];
        let row = build_grid_row(&mk_profile(), &doc, "facebook", "x.md", &mut Vec::new())
            .expect("valid contact grid row");
        assert_eq!(row.preview, "2364-01-05");
    }

    /// The chip is the first address, else the first number that makes a
    /// handle; a number with no country code makes none.
    #[test]
    fn a_contacts_chip_is_its_address_else_its_number() {
        let h = |e, p| chip_handle(e, p).map(|h| h.as_str().to_string());
        assert_eq!(
            h(Some("Riker@Enterprise.org"), Some("+1 202 555 0101")).as_deref(),
            Some("email:riker@enterprise.org")
        );
        assert_eq!(
            h(None, Some("+1 202 555 0101")).as_deref(),
            Some("tel:+12025550101")
        );
        assert_eq!(h(None, Some("(202) 555-0101")), None);
    }

    /// `with:` finds a card by each handle it holds and each name it
    /// gives; a number with no country code makes no handle, so no term.
    #[test]
    fn a_card_is_about_its_handles_and_names() {
        let mut c = mk_contact().contact;
        c.names = vec!["Will Riker".into(), "Number One".into()];
        c.handles = vec![
            ContactHandle::email(None, "riker@enterprise.org"),
            ContactHandle::email(None, "Riker@Enterprise.org"),
            ContactHandle::phone(None, "(555) 010-1234"),
        ];
        let column = datalib_schema::search_terms::encode_supplied(about_terms(&c));
        let terms: Vec<(SearchTermKind, String)> =
            datalib_schema::search_terms::decode_supplied(&column.unwrap())
                .unwrap()
                .into_iter()
                .map(|t| (t.kind, t.value))
                .collect();
        let about = |v: &str| (SearchTermKind::About, v.to_string());
        assert_eq!(
            terms,
            [
                about("Number One"),
                about("Will Riker"),
                about("email:riker@enterprise.org")
            ]
        );
    }

    /// A group's page lists its members, each a chip where the member has
    /// a handle and a plain name where not.
    #[test]
    fn a_groups_members_are_chips() {
        let mut doc = mk_contact();
        doc.contact.members = vec!["Will Riker".into(), "Q".into()];
        doc.member_handles = vec![Handle::email("riker@enterprise.org"), None];
        let md = join(&render_markdown(&mk_profile(), &doc, "linkedin", None));
        assert!(
            md.contains(
                "## Members\n\n\
                 - [Will Riker](mailto:riker@enterprise.org \"Will Riker <riker@enterprise.org>\")\n\
                 - Q\n"
            ),
            "{md}"
        );
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
