//! Render one `.md` per contact + one [`GridRow`] per contact.
//!
//! Layout under `out_dir`:
//!   `rendered_md/contacts/<source_name>/<addressbook>/<uid>__<slug>.md`
//!   `rendered_md/contacts/<source_name>/<addressbook>/<uid>__<slug>.grid_rows.json`
//!   `rendered_md/contacts/<source_name>/<addressbook>/blobs/<uid>.<ext>`
//!
//! Picking per-contact granularity (rather than one .md per
//! addressbook) lets the qmd embedding index treat each person as a
//! searchable document — `qmd query "Picard"` returns one row, not a
//! whole addressbook.

use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_schema::grid_rows::GridRow;

use super::parse::{ContactPhoto, ParsedContact, ParsedContacts};
use super::{addressbook_uuid, contact_uuid};

/// Bump when the rendered layout changes enough that every existing
/// contact doc needs re-rendering.
pub const RENDER_VERSION: u32 = 1;

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub contacts_total: usize,
    pub contacts_rendered: usize,
    pub contacts_skipped: usize,
    pub photos_materialized: usize,
}

/// Translate entry point. Matches the shape of beeper's `render_all`
/// so the sync runner's translate match-arm wires up the same way.
/// The `now` stamp is used as a fallback `when_ts` for vCards
/// without `REV:`.
pub fn render_all(
    parsed: &ParsedContacts,
    out_dir: &Path,
    source_name: &str,
    now: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        contacts_total: parsed.contacts.len(),
        ..Default::default()
    };
    progress.set_length(Some(summary.contacts_total as u64));

    for contact in &parsed.contacts {
        match render_one(
            contact,
            out_dir,
            source_name,
            now,
            prior_fingerprints,
            on_doc_complete,
        ) {
            Ok(RenderOutcome::Rendered { photo_written }) => {
                summary.contacts_rendered += 1;
                if photo_written {
                    summary.photos_materialized += 1;
                }
            }
            Ok(RenderOutcome::Skipped) => summary.contacts_skipped += 1,
            Err(e) => {
                tracing::warn!(
                    event = "contacts_render_failed",
                    uid = %contact.uid,
                    addressbook = %contact.addressbook,
                    error = %e,
                );
            }
        }
        progress.inc(1);
    }
    Ok(summary)
}

enum RenderOutcome {
    Rendered { photo_written: bool },
    Skipped,
}

fn render_one(
    contact: &ParsedContact,
    out_dir: &Path,
    source_name: &str,
    now: &str,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderOutcome> {
    let m_uuid = contact_uuid(source_name, &contact.addressbook, &contact.uid);
    let a_uuid = addressbook_uuid(source_name, &contact.addressbook);
    let fingerprint = compute_fingerprint(contact);

    let (md_path, json_path, page_dir) = output_paths(out_dir, source_name, contact);
    if prior_fingerprints.get(&m_uuid).map(String::as_str) == Some(fingerprint.as_str())
        && md_path.exists()
    {
        return Ok(RenderOutcome::Skipped);
    }
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Photo first — written to `blobs/`, referenced from the markdown
    // with a relative path. If the photo write fails, the markdown
    // still renders (skip the embed) so a broken image doesn't
    // poison the whole row.
    let photo_rel = match &contact.photo {
        Some(p) => write_photo(&page_dir, contact, p).ok(),
        None => None,
    };
    let photo_written = photo_rel.is_some();

    let when_ts = contact.revision.clone().unwrap_or_else(|| now.to_string());

    let md = render_markdown(
        contact,
        source_name,
        &m_uuid,
        &fingerprint,
        &when_ts,
        photo_rel.as_deref(),
    );
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let row = build_grid_row(contact, source_name, &m_uuid, &a_uuid, &when_ts, &md_rel);

    // Sidecar `.grid_rows.json` next to the markdown so an
    // ad-hoc inspector can read both at once. The orchestrator
    // already commits `rows` into the doltlite grid_rows table via
    // `on_doc_complete`; this sidecar mirrors what every other
    // provider writes for symmetry.
    let sidecar = serde_json::json!({
        "header": {
            "markdown_uuid": m_uuid,
            "source_fingerprint": fingerprint,
            "render_version": RENDER_VERSION,
        },
        "rows": [&row],
    });
    fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)
        .with_context(|| format!("write {}", json_path.display()))?;

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: contact.revision.clone(),
        md_path,
        render_version: RENDER_VERSION,
        rows: vec![row],
        edges: Vec::new(),
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;

    Ok(RenderOutcome::Rendered { photo_written })
}

fn output_paths(
    out_dir: &Path,
    source_name: &str,
    contact: &ParsedContact,
) -> (PathBuf, PathBuf, PathBuf) {
    let page_dir = out_dir
        .join("rendered_md")
        .join("contacts")
        .join(source_name)
        .join(&contact.addressbook);
    let stem = format!(
        "{}__{}",
        &contact.uid,
        slugify(contact.display_name.as_deref().unwrap_or(&contact.uid))
    );
    let md_path = page_dir.join(format!("{stem}.md"));
    let json_path = page_dir.join(format!("{stem}.grid_rows.json"));
    (md_path, json_path, page_dir)
}

fn compute_fingerprint(contact: &ParsedContact) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    contact.uid.hash(&mut h);
    contact.addressbook.hash(&mut h);
    contact.display_name.hash(&mut h);
    contact.revision.hash(&mut h);
    for e in &contact.emails {
        e.value.hash(&mut h);
        for (k, v) in &e.params {
            k.hash(&mut h);
            v.hash(&mut h);
        }
    }
    for t in &contact.phones {
        t.value.hash(&mut h);
    }
    for a in &contact.addresses {
        a.value.hash(&mut h);
    }
    contact.org.hash(&mut h);
    contact.title.hash(&mut h);
    contact.note.hash(&mut h);
    contact.photo.as_ref().map(|p| p.bytes.len()).hash(&mut h);
    format!("{:016x}", h.finish())
}

fn render_markdown(
    contact: &ParsedContact,
    source_name: &str,
    m_uuid: &str,
    fingerprint: &str,
    when_ts: &str,
    photo_rel: Option<&str>,
) -> String {
    let mut out = String::with_capacity(2048);

    out.push_str("---\n");
    out.push_str(&format!("markdown_uuid: {m_uuid}\n"));
    out.push_str(&format!("source_fingerprint: {fingerprint}\n"));
    out.push_str(&format!("source_name: {source_name}\n"));
    out.push_str("provider: contacts\n");
    out.push_str(&format!(
        "addressbook: {}\n",
        yaml_safe(&contact.addressbook)
    ));
    out.push_str(&format!("uid: {}\n", yaml_safe(&contact.uid)));
    if let Some(dn) = &contact.display_name {
        out.push_str(&format!("title: {}\n", yaml_safe(dn)));
    }
    out.push_str(&format!("when_ts: {when_ts}\n"));
    out.push_str("---\n\n");

    let title = contact
        .display_name
        .clone()
        .unwrap_or_else(|| contact.uid.clone());
    out.push_str(&format!("# {title}\n\n"));

    if let Some(rel) = photo_rel {
        out.push_str(&format!("![{title}]({rel})\n\n"));
    }
    if let Some(org) = &contact.org {
        out.push_str(&format!("**{}**", org.replace(';', " — ")));
        if let Some(t) = &contact.title {
            out.push_str(&format!(" — {t}"));
        }
        out.push_str("\n\n");
    } else if let Some(t) = &contact.title {
        out.push_str(&format!("**{t}**\n\n"));
    }

    if !contact.emails.is_empty() {
        out.push_str("## Emails\n\n");
        for e in &contact.emails {
            out.push_str(&format!("- {}{}\n", label_prefix(&e.type_label()), e.value));
        }
        out.push('\n');
    }
    if !contact.phones.is_empty() {
        out.push_str("## Phones\n\n");
        for p in &contact.phones {
            out.push_str(&format!("- {}{}\n", label_prefix(&p.type_label()), p.value));
        }
        out.push('\n');
    }
    if !contact.addresses.is_empty() {
        out.push_str("## Addresses\n\n");
        for a in &contact.addresses {
            // ADR is `;`-separated: PO box; ext; street; locality; region; postcode; country
            let pretty = a.value.replace(';', ", ");
            out.push_str(&format!("- {}{}\n", label_prefix(&a.type_label()), pretty));
        }
        out.push('\n');
    }
    if let Some(n) = &contact.note {
        out.push_str("## Notes\n\n");
        out.push_str(n);
        out.push_str("\n\n");
    }
    if let Some(url) = &contact.photo_url {
        out.push_str(&format!("Photo: <{url}>\n\n"));
    }

    out
}

fn build_grid_row(
    contact: &ParsedContact,
    source_name: &str,
    m_uuid: &str,
    a_uuid: &str,
    when_ts: &str,
    md_rel: &str,
) -> GridRow {
    let title = contact
        .display_name
        .clone()
        .unwrap_or_else(|| contact.uid.clone());
    // Body the UI displays / qmd indexes. Compact, single-string.
    let mut text = title.clone();
    if let Some(o) = &contact.org {
        text.push('\n');
        text.push_str(&o.replace(';', " — "));
    }
    if let Some(t) = &contact.title {
        text.push('\n');
        text.push_str(t);
    }
    for e in &contact.emails {
        text.push('\n');
        text.push_str(&e.value);
    }
    for p in &contact.phones {
        text.push('\n');
        text.push_str(&p.value);
    }

    GridRow {
        uuid: m_uuid.to_string(),
        provider: "contacts".to_string(),
        kind: "Contact".to_string(),
        source_label: humanize_source_label(source_name),
        when_ts: when_ts.to_string(),
        author: Some(title.clone()),
        account: Some(source_name.to_string()),
        org_uuid: None,
        org_name: None,
        project: None,
        channel: Some(contact.addressbook.clone()),
        conversation_name: Some(contact.addressbook.clone()),
        conversation_uuid: a_uuid.to_string(),
        message_index: None,
        entire_chat: format!("/contact/{m_uuid}"),
        text,
        slack_link: None,
        qmd_path: Some(md_rel.to_string()),
        source_url: None,
        git_sha: None,
        external_id: Some(contact.uid.clone()),
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(m_uuid.to_string()),
    }
}

fn write_photo(page_dir: &Path, contact: &ParsedContact, photo: &ContactPhoto) -> Result<String> {
    let blobs_dir = page_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).with_context(|| format!("mkdir -p {}", blobs_dir.display()))?;
    let ext = ext_for(&photo.content_type);
    let filename = format!("{}.{ext}", &contact.uid);
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
        _ => "bin",
    }
}

fn label_prefix(t: &Option<String>) -> String {
    match t {
        Some(s) if !s.is_empty() => format!("**{s}**: "),
        _ => String::new(),
    }
}

fn yaml_safe(s: &str) -> String {
    // Quote values containing YAML-significant characters so the
    // frontmatter doesn't bite a downstream parser.
    if s.chars().any(|c| ":#[]{}&*?,|>'\"%@`\n".contains(c)) {
        let escaped = s.replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// `source_name` is the YAML config key (`apple_contacts`,
/// `fastmail_contacts`, …). Surface a human label on grid rows by
/// stripping the `_contacts` suffix + casing. Keeps the row's
/// Source column from looking like a slug.
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

    #[test]
    fn slugify_strips_punctuation_and_collapses_runs() {
        assert_eq!(slugify("Jean-Luc Picard"), "jean-luc-picard");
        assert_eq!(slugify("---"), "");
        assert_eq!(slugify("  Captain  "), "captain");
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
