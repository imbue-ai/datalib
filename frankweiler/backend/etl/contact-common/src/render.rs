//! `render_all` — one `.md` + one [`GridRow`] per contact, with
//! fingerprint-skip and the `on_doc_complete` callback the orchestrator
//! threads through. Provider-agnostic: everything provider-specific
//! arrives via [`ContactRenderProfile`] + the [`NormalizedContact`]s.
//!
//! Layout under `out_dir` (one directory per contact, keyed by the stable
//! contact UUID):
//!   `<stanza>/rendered_md/<contact_uuid>/index.md`
//!   `<stanza>/rendered_md/<contact_uuid>/index.grid_rows.json`
//!   `<stanza>/rendered_md/<contact_uuid>/blobs/<uuid>.<ext>`
//!
//! Per-contact granularity (rather than one `.md` per group) lets the
//! qmd embedding index treat each person as a searchable document —
//! `qmd query "Picard"` returns one row, not a whole addressbook.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::title::Title;
use frankweiler_index_lib::emit_sidecar;
use frankweiler_schema::grid_rows::GridRow;
use sha2::{Digest, Sha256};

use crate::types::{ContactPhoto, NormalizedContact};

/// Per-provider knobs the renderer parameterizes on, so a single render
/// function serves every contact-style provider. Sibling of
/// chat-common's `RenderProfile`.
#[derive(Debug, Clone)]
pub struct ContactRenderProfile {
    /// On-disk subdir under `rendered_md/<provider>/…`, the markdown's
    /// `provider:` frontmatter key, and the grid-row `provider` column.
    pub provider: &'static str,
    /// The `source_label` column on every grid row (e.g. `"LinkedIn"`,
    /// `"Apple Contacts"`).
    pub source_label: String,
    /// Discriminator for the contact's grid row (e.g. `"Contact"`).
    pub contact_kind: String,
    /// Bumped by the provider when its contact rendering changes
    /// meaningfully; stamped into the sidecar so a re-run invalidates
    /// stale docs.
    pub render_version: u32,
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub contacts_total: usize,
    pub contacts_rendered: usize,
    pub contacts_skipped: usize,
    pub photos_materialized: usize,
}

/// Render every contact. Returns aggregate counts; per-contact work is
/// delegated to [`render_one`]. A contact that fails to render logs a
/// WARN and is skipped — one bad row shouldn't poison the batch.
pub fn render_all(
    profile: &ContactRenderProfile,
    contacts: &[NormalizedContact],
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        contacts_total: contacts.len(),
        ..Default::default()
    };
    progress.set_length(Some(summary.contacts_total as u64));

    for contact in contacts {
        match render_one(
            profile,
            contact,
            out_dir,
            source_name,
            prior_fingerprints,
            on_doc_complete,
        ) {
            Ok(Outcome::Rendered { photo_written }) => {
                summary.contacts_rendered += 1;
                if photo_written {
                    summary.photos_materialized += 1;
                }
            }
            Ok(Outcome::Skipped) => summary.contacts_skipped += 1,
            Err(e) => {
                tracing::warn!(
                    event = "contact_render_failed",
                    provider = profile.provider,
                    contact_uuid = %contact.contact_uuid,
                    group = %contact.group_label,
                    error = %e,
                );
            }
        }
        progress.inc(1);
    }
    Ok(summary)
}

enum Outcome {
    Rendered { photo_written: bool },
    Skipped,
}

fn render_one(
    profile: &ContactRenderProfile,
    contact: &NormalizedContact,
    out_dir: &Path,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Outcome> {
    let m_uuid = &contact.contact_uuid;
    let fingerprint = compute_fingerprint(profile.render_version, contact);

    let (md_path, json_path, page_dir) = output_paths(out_dir, source_name, contact);
    if prior_fingerprints.get(m_uuid).map(String::as_str) == Some(fingerprint.as_str())
        && md_path.exists()
    {
        return Ok(Outcome::Skipped);
    }
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Photo first — written to `blobs/`, referenced from the markdown
    // with a relative path. If the photo write fails, the markdown still
    // renders (skip the embed) so a broken image doesn't poison the row.
    let photo_rel = match &contact.photo {
        Some(p) => write_photo(&page_dir, m_uuid, p).ok(),
        None => None,
    };
    let photo_written = photo_rel.is_some();

    let md = render_markdown(
        profile,
        contact,
        source_name,
        &fingerprint,
        photo_rel.as_deref(),
    );
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let row = build_grid_row(profile, contact, source_name, &md_rel)?;

    // Sidecar `.grid_rows.json` next to the markdown, mirroring what
    // every other provider writes. The orchestrator commits `rows` into
    // the doltlite grid_rows table via `on_doc_complete`.
    let rows = std::slice::from_ref(&row);
    emit_sidecar(
        &json_path,
        m_uuid,
        &fingerprint,
        profile.render_version,
        rows,
        &[],
    )?;

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: contact.when_ts.clone(),
        md_path,
        render_version: profile.render_version,
        rows: vec![row],
        edges: Vec::new(),
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;

    Ok(Outcome::Rendered { photo_written })
}

fn output_paths(
    out_dir: &Path,
    source_name: &str,
    contact: &NormalizedContact,
) -> (PathBuf, PathBuf, PathBuf) {
    // One directory per contact, keyed by the stable contact UUID — never a
    // name/group-label slug, so a rename or regrouping re-renders in place.
    // The contact's `blobs/` (photo) live inside this dir. Display name and
    // group label still live in the frontmatter + grid row.
    let page_dir =
        frankweiler_etl::layout::rendered_md_root(out_dir, source_name).join(&contact.contact_uuid);
    let md_path = page_dir.join("index.md");
    let json_path = page_dir.join("index.grid_rows.json");
    (md_path, json_path, page_dir)
}

fn display_or_id(contact: &NormalizedContact) -> &str {
    contact
        .display_name
        .as_deref()
        .or(contact.external_id.as_deref())
        .unwrap_or(&contact.contact_uuid)
}

fn compute_fingerprint(render_version: u32, contact: &NormalizedContact) -> String {
    let mut h = Sha256::new();
    h.update(render_version.to_be_bytes());
    h.update(b"|");
    h.update(contact.contact_uuid.as_bytes());
    h.update(b"|");
    h.update(contact.group_uuid.as_bytes());
    h.update(b"|");
    h.update(contact.group_label.as_bytes());
    h.update(b"|");
    h.update(contact.display_name.as_deref().unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(contact.external_id.as_deref().unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(contact.when_ts.as_deref().unwrap_or("").as_bytes());
    h.update(b"|");
    h.update(contact.source_url.as_deref().unwrap_or("").as_bytes());
    for f in &contact.fields {
        h.update(b"\n");
        h.update(f.label.as_bytes());
        h.update(b"=");
        h.update(f.value.as_bytes());
    }
    h.update(b"|photo:");
    h.update((contact.photo.as_ref().map(|p| p.bytes.len()).unwrap_or(0) as u64).to_be_bytes());
    h.update(b"|photo_url:");
    h.update(contact.photo_url.as_deref().unwrap_or("").as_bytes());
    format!("{:x}", h.finalize())
}

fn render_markdown(
    profile: &ContactRenderProfile,
    contact: &NormalizedContact,
    source_name: &str,
    fingerprint: &str,
    photo_rel: Option<&str>,
) -> String {
    let m_uuid = &contact.contact_uuid;
    let mut out = String::with_capacity(2048);

    out.push_str("---\n");
    out.push_str(&format!("markdown_uuid: {m_uuid}\n"));
    out.push_str(&format!("source_fingerprint: {fingerprint}\n"));
    out.push_str(&format!("source_name: {source_name}\n"));
    out.push_str(&format!("provider: {}\n", profile.provider));
    out.push_str(&format!("group: {}\n", yaml_safe(&contact.group_label)));
    if let Some(id) = &contact.external_id {
        out.push_str(&format!("external_id: {}\n", yaml_safe(id)));
    }
    if let Some(dn) = &contact.display_name {
        out.push_str(&format!("title: {}\n", yaml_safe(dn)));
    }
    // Omit `when_ts:` entirely when we don't have one; the grid row
    // emits `None` to match.
    if let Some(ts) = &contact.when_ts {
        out.push_str(&format!("when_ts: {}\n", yaml_safe(ts)));
    }
    out.push_str("---\n\n");

    let title = display_or_id(contact).to_string();
    // Shared `Title` helper so contact pages carry the same
    // `data-page-title-uuid` hook the Vue side uses for the
    // copy-page-id button. `source_url` is the contact's canonical web
    // page (a LinkedIn profile, say) when the provider has one.
    out.push_str(
        &Title {
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

    out
}

fn build_grid_row(
    profile: &ContactRenderProfile,
    contact: &NormalizedContact,
    source_name: &str,
    md_rel: &str,
) -> Result<GridRow> {
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
        .when_ts(contact.when_ts.clone())
        .author(Some(title))
        .account(Some(source_name.to_string()))
        .channel(Some(contact.group_label.clone()))
        .conversation_name(Some(contact.group_label.clone()))
        .conversation_uuid(contact.group_uuid.clone())
        .entire_chat(format!("/contact/{}", contact.contact_uuid))
        .text(text)
        .qmd_path(Some(md_rel.to_string()))
        .source_url(contact.source_url.clone())
        .external_id(contact.external_id.clone())
        .markdown_uuid(Some(contact.contact_uuid.clone()))
        .build()
        .map_err(anyhow::Error::from)
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
            contact_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            group_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
            group_label: "LinkedIn Connections".to_string(),
            display_name: Some("Jean-Luc Picard".to_string()),
            external_id: Some("https://www.linkedin.com/in/jlp".to_string()),
            // Offset-bearing per the grid's when_ts contract (the
            // builder now rejects bare dates — see GridRowBuilder).
            when_ts: Some("2024-01-02T00:00:00+00:00".to_string()),
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
            provider: "linkedin",
            source_label: "LinkedIn".to_string(),
            contact_kind: "Contact".to_string(),
            render_version: 1,
        }
    }

    #[test]
    fn fingerprint_is_stable_and_sensitive() {
        let c = mk_contact();
        assert_eq!(compute_fingerprint(1, &c), compute_fingerprint(1, &c));
        assert_ne!(compute_fingerprint(1, &c), compute_fingerprint(2, &c));
        let mut c2 = mk_contact();
        c2.fields[0].value = "Klingon Empire".to_string();
        assert_ne!(compute_fingerprint(1, &c), compute_fingerprint(1, &c2));
    }

    #[test]
    fn markdown_has_title_url_and_field_table() {
        let md = render_markdown(&mk_profile(), &mk_contact(), "linkedin", "fp", None);
        assert!(md.contains("Jean-Luc Picard"));
        assert!(md.contains("https://www.linkedin.com/in/jlp"));
        assert!(md.contains("| Company | Starfleet |"));
        // Pipe inside a value is escaped so it doesn't break the table.
        assert!(md.contains("Captain \\| USS Enterprise"));
        assert!(md.contains("provider: linkedin"));
    }

    #[test]
    fn grid_row_carries_uuid_url_and_searchtext() {
        let row = build_grid_row(&mk_profile(), &mk_contact(), "linkedin", "rendered_md/x.md")
            .expect("valid contact grid row");
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
    }
}
