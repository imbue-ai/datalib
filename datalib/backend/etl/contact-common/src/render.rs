//! `render_all` — one `.md` + one [`GridRow`] per contact, handed to
//! the `on_doc_complete` callback the orchestrator threads through. Provider-agnostic: everything provider-specific
//! arrives via [`ContactRenderProfile`] + the [`NormalizedContact`]s.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{Bucket, Buckets};
use datalib_etl_render::section::{join, Section};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use crate::types::{ContactPhoto, NormalizedContact};

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
    /// `contact_uuid`.
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
    contacts: &[NormalizedContact],
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

    for contact in contacts {
        summary.documents.push(contact.contact_uuid.clone());
        summary.buckets.push(Bucket {
            key: contact.contact_uuid.clone(),
            inputs: contact.inputs.clone(),
        });
        // Nothing here fails on the card's account — a row that will not
        // validate is recorded as a problem and a photo that will not
        // write is skipped — so what is left is the disk and the sink,
        // and either failing is the run's to report, not one card's.
        let photo_written = render_one(profile, contact, out_dir, source_id, on_doc_complete)
            .with_context(|| format!("render contact {}", contact.contact_uuid))?;
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
    contact: &NormalizedContact,
    out_dir: &Path,
    source_id: &str,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<bool> {
    let m_uuid = &contact.contact_uuid;
    let (md_path, page_dir) = output_paths(out_dir, source_id, contact);
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Photo first — written to `blobs/`, referenced from the markdown
    // with a relative path. If the photo write fails, the markdown still
    // renders (skip the embed) so a broken image doesn't poison the row.
    let photo_rel = match &contact.photo {
        Some(p) => write_photo(&page_dir, m_uuid, p).ok(),
        None => None,
    };
    let photo_written = photo_rel.is_some();

    let sections = render_markdown(profile, contact, source_id, photo_rel.as_deref());
    fs::write(&md_path, join(&sections)).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut problems: Vec<ProblemRow> = Vec::new();
    let row = build_grid_row(profile, contact, source_id, &md_rel, &mut problems);

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
        problems,
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;

    Ok(photo_written)
}

fn output_paths(
    out_dir: &Path,
    source_id: &str,
    contact: &NormalizedContact,
) -> (PathBuf, PathBuf) {
    // One directory per contact, keyed by the stable contact UUID — never a
    // name/group-label slug, so a rename or regrouping re-renders in place.
    // The contact's `blobs/` (photo) live inside this dir. Display name and
    // group label still live in the frontmatter + grid row.
    let page_dir =
        datalib_etl::layout::render_markdown_root(out_dir, source_id).join(&contact.contact_uuid);
    let md_path = page_dir.join("index.md");
    (md_path, page_dir)
}

fn display_or_id(contact: &NormalizedContact) -> &str {
    contact
        .display_name
        .as_deref()
        .or(contact.external_id.as_deref())
        .unwrap_or(&contact.contact_uuid)
}

fn render_markdown(
    profile: &ContactRenderProfile,
    contact: &NormalizedContact,
    source_id: &str,
    photo_rel: Option<&str>,
) -> Vec<Section> {
    let m_uuid = &contact.contact_uuid;
    let mut out = String::with_capacity(512);

    out.push_str("---\n");
    out.push_str(&format!("markdown_uuid: {m_uuid}\n"));
    out.push_str(&format!("source_id: {source_id}\n"));
    out.push_str(&format!("provider: {}\n", profile.provider));
    out.push_str(&format!("group: {}\n", yaml_safe(&contact.group_label)));
    if let Some(id) = &contact.external_id {
        out.push_str(&format!("external_id: {}\n", yaml_safe(id)));
    }
    if let Some(dn) = &contact.display_name {
        out.push_str(&format!("title: {}\n", yaml_safe(dn)));
    }
    // A stamp we don't have is omitted, never written empty; the grid
    // row is `None` to match.
    if let Some(ts) = &contact.created_at {
        out.push_str(&format!("created_at: {}\n", yaml_safe(ts)));
    }
    if let Some(ts) = &contact.modified_at {
        out.push_str(&format!("modified_at: {}\n", yaml_safe(ts)));
    }
    out.push_str("---\n\n");
    let frontmatter = Section::unkeyed(out);

    // The page body — title, photo, field table — is the one section
    // the contact's grid row names.
    let mut out = String::with_capacity(1024);
    let title = display_or_id(contact).to_string();
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
        out.push_str(&format!("![{title}]({rel})\n\n"));
    }

    let mut table_rows: Vec<(String, String)> = contact
        .fields
        .iter()
        .map(|f| (f.label.clone(), f.value.clone()))
        .collect();
    if let Some(url) = &contact.photo_url {
        table_rows.push(("Photo URL".to_string(), format!("<{url}>")));
    }

    if !table_rows.is_empty() {
        out.push_str("| Field | Value |\n");
        out.push_str("| --- | --- |\n");
        for (k, v) in table_rows {
            out.push_str(&format!("| {} | {} |\n", k, escape_table_cell(&v)));
        }
        out.push('\n');
    }

    vec![frontmatter, Section::keyed(m_uuid, out)]
}

/// `None`, with the reason recorded on `problems`, when the row will
/// not validate — see `GridRowBuilder::build_or_record`.
fn build_grid_row(
    profile: &ContactRenderProfile,
    contact: &NormalizedContact,
    source_id: &str,
    md_rel: &str,
    problems: &mut Vec<ProblemRow>,
) -> Option<GridRow> {
    let title = display_or_id(contact).to_string();
    // Body the UI displays / qmd indexes — compact, single string:
    // the name followed by every field value.
    let mut text = title.clone();
    for f in &contact.fields {
        text.push('\n');
        text.push_str(&f.value);
    }

    GridRow::builder()
        .uuid(contact.contact_uuid.clone())
        .provider(profile.provider)
        .kind(profile.contact_kind.clone())
        .source_label(profile.source_label.clone())
        .is_document(true)
        .created_at(contact.created_at.clone())
        .modified_at(contact.modified_at.clone())
        .author(Some(title))
        .account(profile.account.clone())
        .channel(Some(contact.group_label.clone()))
        .conversation_name(Some(contact.group_label.clone()))
        .conversation_uuid(contact.group_uuid.clone())
        .entire_chat(format!("/contact/{}", contact.contact_uuid))
        .text(text)
        .qmd_path(Some(md_rel.to_string()))
        .source_url(contact.source_url.clone())
        .upstream_id(contact.external_id.clone())
        .upstream_entity_kind(Some(profile.contact_entity_kind.to_string()))
        .upstream_scope(contact.upstream_scope.clone())
        .markdown_uuid(Some(contact.contact_uuid.clone()))
        .build_or_record(
            source_id,
            &contact.contact_uuid,
            profile.render_version,
            problems,
        )
}

fn write_photo(page_dir: &Path, contact_uuid: &str, photo: &ContactPhoto) -> Result<String> {
    let blobs_dir = page_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).with_context(|| format!("mkdir -p {}", blobs_dir.display()))?;
    let ext = ext_for(&photo.content_type);
    let filename = format!("{contact_uuid}.{ext}");
    let path = blobs_dir.join(&filename);
    fs::write(&path, &photo.bytes).with_context(|| format!("write {}", path.display()))?;
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

fn escape_table_cell(s: &str) -> String {
    // Pipes break table cells; backslash-escape them. Collapse newlines
    // (which also break cells) into spaces.
    s.replace('|', "\\|").replace('\n', " ")
}

fn yaml_safe(s: &str) -> String {
    if s.chars().any(|c| ":#[]{}&*?,|>'\"%@`\n".contains(c)) {
        let escaped = s.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContactField;

    fn mk_contact() -> NormalizedContact {
        NormalizedContact {
            inputs: Vec::new(),
            contact_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            group_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
            group_label: "LinkedIn Connections".to_string(),
            display_name: Some("Jean-Luc Picard".to_string()),
            external_id: Some("https://www.linkedin.com/in/jlp".to_string()),
            upstream_scope: None,
            // Offset-bearing per the grid's created_at contract (the
            // builder now rejects bare dates — see GridRowBuilder).
            created_at: Some("2024-01-02T00:00:00+00:00".to_string()),
            modified_at: None,
            source_url: Some("https://www.linkedin.com/in/jlp".to_string()),
            fields: vec![
                ContactField::new("Company", "Starfleet"),
                ContactField::new("Position", "Captain | USS Enterprise"),
            ],
            photo: None,
            photo_url: None,
        }
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
            Some(mk_contact().contact_uuid.as_str()),
            "the body is the contact's section"
        );
        let md = join(&sections);
        assert!(md.contains("Jean-Luc Picard"));
        assert!(md.contains("https://www.linkedin.com/in/jlp"));
        assert!(md.contains("| Company | Starfleet |"));
        // Pipe inside a value is escaped so it doesn't break the table.
        assert!(md.contains("Captain \\| USS Enterprise"));
        assert!(md.contains("provider: linkedin"));
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
        assert!(row.text.contains("Jean-Luc Picard"));
        assert!(row.text.contains("Starfleet"));
        assert_eq!(
            row.conversation_name.as_deref(),
            Some("LinkedIn Connections")
        );
        // The profile's account, not the source name: a source name is
        // not a login and polluted every `account:` filter.
        assert_eq!(row.account.as_deref(), Some("jlp@enterprise.test"));
    }

    /// The sink's answer is the run's answer: a document it refuses fails
    /// the render rather than being logged and left out.
    #[test]
    fn a_sink_that_refuses_fails_the_render() {
        let dir = std::env::temp_dir().join(format!("contact-common-sink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut refuse = |_: RenderedMarkdown| -> Result<()> { anyhow::bail!("no room") };
        let err = render_all(
            &mk_profile(),
            &[mk_contact()],
            &dir,
            "linkedin",
            &Progress::default(),
            &mut refuse,
        )
        .expect_err("a refused document fails the render");
        assert!(format!("{err:#}").contains("no room"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
