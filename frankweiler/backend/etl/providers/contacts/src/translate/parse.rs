//! vCard tree / file → in-memory [`ParsedContacts`].
//!
//! `input_path` can be either:
//!
//!   * A directory of `.vcf` files (recursively walked). Each `.vcf`
//!     may contain one or many `BEGIN:VCARD…END:VCARD` blocks.
//!   * A single `.vcf` file. Same multi-block semantics — Google's
//!     "Export contacts" gives you exactly this shape: every contact
//!     concatenated into one file.
//!
//! The addressbook label for a contact is the **file stem** (basename
//! without extension) of the `.vcf` it came from. So
//! `~/Downloads/contacts.vcf` lands every contact in addressbook
//! `"contacts"`, and `<input>/Bridge.vcf` groups everything under
//! `"Bridge"`. Putting contacts that should share an addressbook into
//! a single file is the natural shape.
//!
//! ## Why not pull in a vCard crate?
//!
//! Surveyed the Rust ecosystem (2026-Q2): `vcard4` is the most
//! actively maintained and most spec-faithful for RFC 6350 / vCard
//! 4.0, with looser support for 3.0; `vcard` is older and less
//! maintained; `ical` covers vCalendar primarily and treats vCard as
//! a side concern. None is the de-facto "serde_json of vCards".
//!
//! Our read surface is intentionally small — UID / FN / EMAIL / TEL /
//! ADR / ORG / TITLE / NOTE / PHOTO / REV — so hand-rolling the line
//! folding + param parsing + multi-block splitting is ~50 lines and
//! lets us keep our promoted-column shape (`ParsedContact`) without
//! an adaptation layer. If we ever need to *write* vCards (e.g. push
//! changes back to CardDAV), or hit a real spec edge case (grouped
//! 4.0 properties, quoted-printable PHOTO), swap in `vcard4`:
//!
//!   https://crates.io/crates/vcard4
//!
//! The promoted columns + the `extract::api::vcard_*` helpers are the
//! seam — flip `parse_file` to call `vcard4::parse` and re-derive the
//! same `ParsedContact` fields from its AST.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::extract::api::{vcard_all, vcard_fn, vcard_rev, vcard_uid, VcardProp};

/// One parsed vCard, with everything translate cares about pulled
/// out so render doesn't have to re-walk the text.
#[derive(Debug, Clone)]
pub struct ParsedContact {
    /// Stable identifier from the vCard's `UID:`. Falls back to a
    /// deterministic UUIDv5 derived from `(addressbook, file_path,
    /// block_index)` when a card omits it.
    pub uid: String,
    /// Addressbook label — the file stem of the `.vcf` this contact
    /// came from. Shows up on grid rows as `channel` and groups
    /// contacts together in the UI.
    pub addressbook: String,
    /// Source file on disk — surfaced in tracing so misformatted
    /// vCards point at the right file.
    pub source_path: PathBuf,
    /// `FN` (formatted name). `None` for nameless cards — the
    /// render path falls back to the UID.
    pub display_name: Option<String>,
    /// `REV:` (revision timestamp). Translate uses it for `when_ts`;
    /// falls back to the `--now` stamp when absent.
    pub revision: Option<String>,
    /// Multi-valued properties surfaced in document order.
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

/// Walk `input_path`, returning every vCard found.
///
/// * If `input_path` is a `.vcf` file, parse it directly (may contain
///   many vCards).
/// * If `input_path` is a directory, recursively walk it for `.vcf`
///   files and parse each.
/// * If `input_path` doesn't exist or is neither a file nor a
///   directory, return an empty result. Mirrors how
///   `frankweiler_etl_anthropic::ingest_export_users` handles a
///   missing `users.json` — translate paths shouldn't fail hard on
///   "the user hasn't dropped their export here yet."
///
/// Per-file errors are logged + skipped (one malformed `.vcf` doesn't
/// tank the whole translate run); only IO errors on the directory
/// walk propagate.
pub fn parse(input_path: &Path) -> Result<ParsedContacts> {
    let mut out = ParsedContacts::default();
    if input_path.is_file() {
        parse_into(input_path, &mut out);
    } else if input_path.is_dir() {
        walk_dir(input_path, &mut out)?;
    }
    out.contacts.sort_by(|a, b| {
        a.addressbook
            .cmp(&b.addressbook)
            .then_with(|| a.uid.cmp(&b.uid))
    });
    Ok(out)
}

fn walk_dir(dir: &Path, out: &mut ParsedContacts) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("entry in {}", dir.display()))?;
        paths.push(entry.path());
    }
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk_dir(&path, out)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("vcf") {
            continue;
        }
        parse_into(&path, out);
    }
    Ok(())
}

/// Read one `.vcf` from disk and append every contained vCard to
/// `out`. Log + skip individual block failures.
fn parse_into(path: &Path, out: &mut ParsedContacts) {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                event = "contacts_vcard_read_failed",
                path = %path.display(),
                error = %e,
            );
            return;
        }
    };
    let addressbook = addressbook_label(path);
    for (idx, block) in split_vcards(&raw).into_iter().enumerate() {
        match parse_block(&block, path, &addressbook, idx) {
            Ok(c) => out.contacts.push(c),
            Err(e) => {
                tracing::warn!(
                    event = "contacts_vcard_parse_failed",
                    path = %path.display(),
                    block_index = idx,
                    error = %e,
                );
            }
        }
    }
}

/// Addressbook label = the file stem (basename without extension).
/// Falls back to `"default"` when the path has no stem (e.g.
/// `~/Downloads/.vcf` for whatever reason).
fn addressbook_label(vcard_path: &Path) -> String {
    vcard_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "default".to_string())
}

/// Split a `.vcf` body into individual `BEGIN:VCARD…END:VCARD`
/// blocks. Tolerates CRLF / LF / mixed line endings and case-
/// insensitive markers (RFC 6350 §3.3 says "BEGIN" / "END" are
/// case-insensitive in practice every server emits uppercase, but
/// stay defensive).
///
/// Discards any text outside a block — wrapper-style exports
/// (CardDAV's `<address-data>` wrapping, leading mail-server
/// envelope text) wouldn't survive a round trip through this and
/// shouldn't.
fn split_vcards(body: &str) -> Vec<String> {
    let normalized = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut out: Vec<String> = Vec::new();
    let mut current: Option<String> = None;
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VCARD") {
            current = Some(String::new());
        }
        if let Some(buf) = current.as_mut() {
            buf.push_str(line);
            buf.push('\n');
        }
        if trimmed.eq_ignore_ascii_case("END:VCARD") {
            if let Some(buf) = current.take() {
                out.push(buf);
            }
        }
    }
    out
}

fn parse_block(
    block: &str,
    source_path: &Path,
    addressbook: &str,
    block_index: usize,
) -> Result<ParsedContact> {
    let uid =
        vcard_uid(block).unwrap_or_else(|| derive_uid_from_path(addressbook, source_path, block_index));
    let emails = vcard_all(block, "EMAIL");
    let phones = vcard_all(block, "TEL");
    let addresses = vcard_all(block, "ADR");
    let photo = vcard_all(block, "PHOTO");
    let (photo, photo_url) = pick_photo(photo);
    Ok(ParsedContact {
        uid,
        addressbook: addressbook.to_string(),
        source_path: source_path.to_path_buf(),
        display_name: vcard_fn(block),
        revision: vcard_rev(block),
        emails,
        phones,
        addresses,
        org: extract_single(block, "ORG"),
        title: extract_single(block, "TITLE"),
        note: extract_single(block, "NOTE"),
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

fn derive_uid_from_path(addressbook: &str, path: &Path, block_index: usize) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("anon");
    format!("{addressbook}:{stem}:{block_index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addressbook_label_is_the_file_stem() {
        assert_eq!(addressbook_label(Path::new("/x/Bridge.vcf")), "Bridge");
        assert_eq!(
            addressbook_label(Path::new("/Downloads/contacts.vcf")),
            "contacts"
        );
        // Stem can include the dot when the filename starts with one
        // (POSIX hidden-file convention; Rust's `Path::file_stem`
        // matches that). Not common in real exports, but we tolerate
        // it as the label rather than crashing.
        assert_eq!(addressbook_label(Path::new("/.vcf")), ".vcf");
    }

    #[test]
    fn split_vcards_walks_multiple_blocks_with_crlf_and_lf() {
        let body = "BEGIN:VCARD\r\nUID:a\r\nFN:Alice\r\nEND:VCARD\r\nBEGIN:VCARD\nUID:b\nFN:Bob\nEND:VCARD\n";
        let blocks = split_vcards(body);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("UID:a"));
        assert!(blocks[1].contains("UID:b"));
    }

    #[test]
    fn split_vcards_drops_text_outside_blocks() {
        let body = "junk before\nBEGIN:VCARD\nUID:a\nEND:VCARD\ngarbage between\nBEGIN:VCARD\nUID:b\nEND:VCARD\ntrailer\n";
        let blocks = split_vcards(body);
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].contains("junk"));
        assert!(!blocks[1].contains("garbage"));
        assert!(!blocks[1].contains("trailer"));
    }

    #[test]
    fn parse_single_file_with_many_vcards() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.vcf");
        fs::write(
            &path,
            "BEGIN:VCARD\nVERSION:3.0\nUID:a\nFN:Alice\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:b\nFN:Bob\nEND:VCARD\n",
        )
        .unwrap();
        let parsed = parse(&path).unwrap();
        assert_eq!(parsed.contacts.len(), 2);
        for c in &parsed.contacts {
            // File stem = "contacts" — both end up in that addressbook.
            assert_eq!(c.addressbook, "contacts");
        }
    }

    #[test]
    fn parse_directory_walks_top_level_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Bridge.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\n\
             EMAIL;TYPE=WORK:jlp@enterprise.starfleet\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("Engineering.vcf"),
            "BEGIN:VCARD\nVERSION:3.0\nUID:laforge\nFN:Geordi La Forge\nEND:VCARD\n",
        )
        .unwrap();
        let parsed = parse(dir.path()).unwrap();
        assert_eq!(parsed.contacts.len(), 3);
        // Sort order: (addressbook, uid).
        assert_eq!(parsed.contacts[0].addressbook, "Bridge");
        assert_eq!(parsed.contacts[0].uid, "picard");
        assert_eq!(parsed.contacts[1].uid, "riker");
        assert_eq!(parsed.contacts[2].addressbook, "Engineering");
    }

    #[test]
    fn parse_missing_path_returns_empty_silently() {
        let parsed = parse(Path::new("/this/does/not/exist.vcf")).unwrap();
        assert_eq!(parsed.contacts.len(), 0);
    }

    #[test]
    fn parse_picks_inline_base64_photo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contacts.vcf");
        // Tiny 1×1 PNG — base64 round-trip target.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";
        let vcard = format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nPHOTO;ENCODING=b;TYPE=PNG:{png_b64}\nEND:VCARD\n"
        );
        fs::write(&path, &vcard).unwrap();
        let parsed = parse(&path).unwrap();
        let c = &parsed.contacts[0];
        assert!(c.photo.is_some(), "expected decoded photo");
        let ph = c.photo.as_ref().unwrap();
        assert_eq!(ph.content_type, "image/png");
        assert_eq!(&ph.bytes[1..4], b"PNG");
    }
}
