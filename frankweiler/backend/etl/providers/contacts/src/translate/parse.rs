//! vCard tree → in-memory [`ParsedContacts`].
//!
//! Layout the parser expects:
//!
//!   `<input_path>/<addressbook>/<some_name>.vcf`
//!
//! `<addressbook>` is treated as the addressbook label — it shows
//! up on grid rows as `channel` and groups contacts together in
//! the UI. The leaf filename doesn't matter; the vCard's `UID:`
//! property carries upstream identity.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::extract::api::{vcard_all, vcard_fn, vcard_rev, vcard_uid, VcardProp};

/// One parsed vCard, with everything translate cares about pulled
/// out so render doesn't have to re-walk the text.
#[derive(Debug, Clone)]
pub struct ParsedContact {
    /// Stable identifier from the vCard's `UID:`. Falls back to a
    /// deterministic UUIDv5 derived from `(addressbook, file_path)`
    /// when a card omits it.
    pub uid: String,
    /// Addressbook label (the parent directory under `input_path`).
    pub addressbook: String,
    /// Source file on disk — surfaced in tracing so misformatted
    /// vCards point at the right card.
    pub source_path: PathBuf,
    /// `FN` (formatted name). `None` for nameless cards — the
    /// render path falls back to the UID.
    pub display_name: Option<String>,
    /// `REV:` (revision timestamp). Translate uses it for `when_ts`;
    /// falls back to the `--now` stamp when absent.
    pub revision: Option<String>,
    /// Multi-valued properties surfaced in display order.
    pub emails: Vec<VcardProp>,
    pub phones: Vec<VcardProp>,
    pub addresses: Vec<VcardProp>,
    /// `ORG:` parts (joined with `;` upstream). The first segment
    /// is the company; subsequent ones are units / departments.
    pub org: Option<String>,
    pub title: Option<String>,
    pub note: Option<String>,
    /// Inline `PHOTO` payload. We decode the base64 once at parse
    /// time so render doesn't have to think about encoding rules.
    /// `None` for URL-only photos (those go in `photo_url`).
    pub photo: Option<ContactPhoto>,
    pub photo_url: Option<String>,
}

/// Decoded photo bytes plus a content-type guess. Translate writes
/// these into a sibling `blobs/` directory at render time.
#[derive(Debug, Clone)]
pub struct ContactPhoto {
    pub bytes: Vec<u8>,
    /// `image/jpeg`, `image/png`, …. Derived from the `TYPE=` param;
    /// defaults to `application/octet-stream` when absent.
    pub content_type: String,
}

#[derive(Debug, Default)]
pub struct ParsedContacts {
    pub contacts: Vec<ParsedContact>,
}

/// Walk `input_path` recursively, picking up every `.vcf` file.
/// Errors on individual cards are logged and skipped — one
/// malformed vCard shouldn't tank the whole translate run.
pub fn parse(input_path: &Path) -> Result<ParsedContacts> {
    let mut out = ParsedContacts::default();
    walk_dir(input_path, input_path, &mut out)?;
    out.contacts.sort_by(|a, b| {
        a.addressbook
            .cmp(&b.addressbook)
            .then_with(|| a.uid.cmp(&b.uid))
    });
    Ok(out)
}

fn walk_dir(root: &Path, dir: &Path, out: &mut ParsedContacts) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let entries = fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("entry in {}", dir.display()))?;
        paths.push(entry.path());
    }
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk_dir(root, &path, out)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("vcf") {
            continue;
        }
        let addressbook = addressbook_label(root, &path);
        match parse_file(&path, &addressbook) {
            Ok(c) => out.contacts.push(c),
            Err(e) => {
                tracing::warn!(
                    event = "contacts_vcard_parse_failed",
                    path = %path.display(),
                    error = %e,
                );
            }
        }
    }
    Ok(())
}

/// Label for the addressbook a contact belongs to: the first path
/// component under `root`. Falls back to `"default"` when the vCard
/// sits directly under root.
fn addressbook_label(root: &Path, vcard_path: &Path) -> String {
    let rel = vcard_path.strip_prefix(root).unwrap_or(vcard_path);
    let mut comps = rel.components();
    let first = comps.next();
    match (first, comps.next()) {
        // root/<addressbook>/<file.vcf> — there's still another
        // component after the first one, so the first IS the
        // addressbook.
        (Some(c), Some(_)) => c.as_os_str().to_string_lossy().into_owned(),
        // root/file.vcf — no addressbook subdir.
        _ => "default".to_string(),
    }
}

fn parse_file(path: &Path, addressbook: &str) -> Result<ParsedContact> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let uid = vcard_uid(&raw).unwrap_or_else(|| derive_uid_from_path(addressbook, path));
    let emails = vcard_all(&raw, "EMAIL");
    let phones = vcard_all(&raw, "TEL");
    let addresses = vcard_all(&raw, "ADR");
    let photo = vcard_all(&raw, "PHOTO");
    let (photo, photo_url) = pick_photo(photo);
    Ok(ParsedContact {
        uid,
        addressbook: addressbook.to_string(),
        source_path: path.to_path_buf(),
        display_name: vcard_fn(&raw),
        revision: vcard_rev(&raw),
        emails,
        phones,
        addresses,
        org: extract_single(&raw, "ORG"),
        title: extract_single(&raw, "TITLE"),
        note: extract_single(&raw, "NOTE"),
        photo,
        photo_url,
    })
}

fn extract_single(vcard: &str, name: &str) -> Option<String> {
    vcard_all(vcard, name).into_iter().next().map(|p| p.value)
}

/// First base64-encoded photo wins; fall back to the first URL-only
/// photo if no inline binary is present.
fn pick_photo(props: Vec<VcardProp>) -> (Option<ContactPhoto>, Option<String>) {
    let mut url: Option<String> = None;
    for p in props {
        // base64-encoded inline body. RFC 6350 §6.7.1 ENCODING=b
        // (vCard 3.0) or vCard 4.0's bare `data:` URL.
        let is_b64 = p
            .param("ENCODING")
            .map(|v| v.eq_ignore_ascii_case("b") || v.eq_ignore_ascii_case("base64"))
            .unwrap_or(false);
        if is_b64 {
            // Strip whitespace base64 may have picked up from line
            // folding.
            let cleaned: String = p.value.chars().filter(|c| !c.is_whitespace()).collect();
            use base64::Engine;
            let engine = base64::engine::general_purpose::STANDARD;
            if let Ok(bytes) = engine.decode(cleaned.as_bytes()) {
                let content_type = p
                    .param("TYPE")
                    .map(|t| format!("image/{}", t.to_ascii_lowercase()))
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                return (Some(ContactPhoto { bytes, content_type }), None);
            }
        }
        // vCard 4.0 inline `data:` URL.
        if p.value.starts_with("data:") {
            if let Some((meta, b64)) = p.value.trim_start_matches("data:").split_once(",") {
                let content_type = meta
                    .split(';')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("application/octet-stream")
                    .to_string();
                use base64::Engine;
                let engine = base64::engine::general_purpose::STANDARD;
                let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                if let Ok(bytes) = engine.decode(cleaned.as_bytes()) {
                    return (Some(ContactPhoto { bytes, content_type }), None);
                }
            }
        }
        // URL-only PHOTO.
        if p.value.starts_with("http://") || p.value.starts_with("https://") {
            url = Some(p.value.clone());
        }
    }
    (None, url)
}

fn derive_uid_from_path(addressbook: &str, path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("anon");
    format!("{addressbook}:{stem}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressbook_label_uses_first_dir_under_root() {
        let root = Path::new("/x");
        assert_eq!(
            addressbook_label(root, Path::new("/x/Personal/picard.vcf")),
            "Personal"
        );
        assert_eq!(
            addressbook_label(root, Path::new("/x/picard.vcf")),
            "default"
        );
    }

    #[test]
    fn parse_walks_directory_and_returns_sorted_contacts() {
        let dir = tempfile::tempdir().unwrap();
        let ab1 = dir.path().join("Bridge");
        let ab2 = dir.path().join("Engineering");
        fs::create_dir_all(&ab1).unwrap();
        fs::create_dir_all(&ab2).unwrap();
        fs::write(
            ab1.join("picard.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEMAIL;TYPE=WORK:jlp@enterprise.starfleet\nEND:VCARD\n",
        )
        .unwrap();
        fs::write(
            ab2.join("laforge.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nUID:laforge\nFN:Geordi La Forge\nEND:VCARD\n",
        )
        .unwrap();

        let parsed = parse(dir.path()).unwrap();
        assert_eq!(parsed.contacts.len(), 2);
        // Sorted by (addressbook, uid).
        assert_eq!(parsed.contacts[0].addressbook, "Bridge");
        assert_eq!(parsed.contacts[0].uid, "picard");
        assert_eq!(parsed.contacts[0].emails.len(), 1);
        assert_eq!(parsed.contacts[0].emails[0].value, "jlp@enterprise.starfleet");
        assert_eq!(parsed.contacts[1].addressbook, "Engineering");
    }

    #[test]
    fn parse_picks_inline_base64_photo() {
        let dir = tempfile::tempdir().unwrap();
        let ab = dir.path().join("Bridge");
        fs::create_dir_all(&ab).unwrap();
        // Tiny 1x1 PNG — base64 round-trip target.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";
        let vcard = format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nPHOTO;ENCODING=b;TYPE=PNG:{png_b64}\nEND:VCARD\n"
        );
        fs::write(ab.join("picard.vcf"), &vcard).unwrap();
        let parsed = parse(dir.path()).unwrap();
        let c = &parsed.contacts[0];
        assert!(c.photo.is_some(), "expected decoded photo");
        let ph = c.photo.as_ref().unwrap();
        assert_eq!(ph.content_type, "image/png");
        // Decoded PNG starts with the magic 8 bytes (?PNG?).
        assert!(ph.bytes.len() > 8);
        assert_eq!(&ph.bytes[1..4], b"PNG");
    }
}
