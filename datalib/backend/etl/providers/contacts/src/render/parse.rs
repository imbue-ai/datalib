//! Raw doltlite store → in-memory [`ParsedContacts`].
//!
//! Render reads from the per-source doltlite raw store written by
//! either CardDAV ([`crate::download::fetch`]) or the local file
//! walker ([`crate::download::vcf_dir::fetch`]). Both writers produce
//! the same row shape, so render has exactly one input contract.
//!
//! Each [`super::super::download::db::LoadedRawContact`] carries the
//! raw vCard text (already unwrapped from the `{"vcard": …}` payload
//! envelope on the SQL side) plus the addressbook label. We split
//! the text into `BEGIN:VCARD…END:VCARD` blocks defensively — a
//! single `contacts.payload` row usually carries one block, but
//! Google-Takeout-shaped sources concatenate many under one `href`
//! and the file-walker leaves that shape intact.
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
//! The promoted columns + the `download::api::vcard_*` helpers are the
//! seam — flip `parse_file` to call `vcard4::parse` and re-derive the
//! same `ParsedContact` fields from its AST.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::download::api::{vcard_all, vcard_fn, vcard_rev, vcard_uid, VcardProp};
use crate::download::db::{LoadedRawContact, RawDb};

/// One parsed vCard, with everything render cares about pulled
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
    /// `REV:` (revision timestamp). Render uses it for `when_ts`;
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

/// Decoded photo bytes plus a content-type guess. Render writes
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

/// Load every contact from the raw doltlite store at `db_path` and
/// parse each vCard. Returns an empty [`ParsedContacts`] when the
/// store is absent or empty — render paths shouldn't fail hard
/// when the upstream download hasn't run yet.
///
/// Sync wrapper around the async loader so callers in the
/// (synchronous) render dispatch can stay synchronous, matching
/// every other provider's `parse(&fixture)` shape.
pub fn parse(db_path: &Path) -> Result<ParsedContacts> {
    if !db_path.exists() {
        return Ok(ParsedContacts::default());
    }
    let path = db_path.to_path_buf();
    let rows = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            db.load_all_for_render_and_index_md().await
        })
    })?;
    Ok(parse_loaded(rows))
}

/// Same as [`parse`] but takes an already-loaded row vector. Useful
/// for tests that want to skip the doltlite round-trip.
pub fn parse_loaded(rows: Vec<LoadedRawContact>) -> ParsedContacts {
    let mut out = ParsedContacts::default();
    for row in rows {
        let source_path = PathBuf::from(&row.href);
        let blocks = split_vcards(&row.vcard);
        // A row that carries no recognizable block is still useful —
        // treat the whole payload as one block (the CardDAV path
        // stores one vCard per row without surrounding delimiters in
        // some servers' responses).
        let iter: Vec<String> = if blocks.is_empty() {
            vec![row.vcard.clone()]
        } else {
            blocks
        };
        for (idx, block) in iter.into_iter().enumerate() {
            match parse_block(&block, &source_path, &row.addressbook_label, idx) {
                Ok(mut c) => {
                    if c.uid.is_empty() {
                        c.uid = if idx == 0 {
                            row.uid.clone()
                        } else {
                            format!("{}:{idx}", row.uid)
                        };
                    }
                    out.contacts.push(c);
                }
                Err(e) => {
                    tracing::warn!(
                        event = "contacts_vcard_parse_failed",
                        href = %row.href,
                        block_index = idx,
                        error = %e,
                    );
                }
            }
        }
    }
    out.contacts.sort_by(|a, b| {
        a.addressbook
            .cmp(&b.addressbook)
            .then_with(|| a.uid.cmp(&b.uid))
    });
    out
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
    let uid = vcard_uid(block)
        .unwrap_or_else(|| derive_uid_from_path(addressbook, source_path, block_index));
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
                return (
                    Some(ContactPhoto {
                        bytes,
                        content_type,
                    }),
                    None,
                );
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
                    return (
                        Some(ContactPhoto {
                            bytes,
                            content_type,
                        }),
                        None,
                    );
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
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("anon");
    format!("{addressbook}:{stem}:{block_index}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(label: &str, uid: &str, vcard: &str) -> LoadedRawContact {
        LoadedRawContact {
            uid: uid.into(),
            href: format!("{label}.vcf"),
            addressbook_label: label.into(),
            vcard: vcard.into(),
        }
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
    fn parse_loaded_groups_by_addressbook() {
        let rows = vec![
            make_row(
                "Bridge",
                "picard",
                "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nEND:VCARD\n",
            ),
            make_row(
                "Bridge",
                "riker",
                "BEGIN:VCARD\nVERSION:3.0\nUID:riker\nFN:William Riker\nEND:VCARD\n",
            ),
            make_row(
                "Engineering",
                "laforge",
                "BEGIN:VCARD\nVERSION:3.0\nUID:laforge\nFN:Geordi La Forge\nEND:VCARD\n",
            ),
        ];
        let parsed = parse_loaded(rows);
        assert_eq!(parsed.contacts.len(), 3);
        assert_eq!(parsed.contacts[0].addressbook, "Bridge");
        assert_eq!(parsed.contacts[0].uid, "picard");
        assert_eq!(parsed.contacts[1].uid, "riker");
        assert_eq!(parsed.contacts[2].addressbook, "Engineering");
    }

    #[test]
    fn parse_loaded_handles_multi_block_payload() {
        let rows = vec![make_row(
            "contacts",
            "ignored",
            "BEGIN:VCARD\nVERSION:3.0\nUID:a\nFN:Alice\nEND:VCARD\n\
             BEGIN:VCARD\nVERSION:3.0\nUID:b\nFN:Bob\nEND:VCARD\n",
        )];
        let parsed = parse_loaded(rows);
        assert_eq!(parsed.contacts.len(), 2);
        for c in &parsed.contacts {
            assert_eq!(c.addressbook, "contacts");
        }
    }

    #[test]
    fn parse_loaded_picks_inline_base64_photo() {
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";
        let vcard = format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:picard\nFN:Jean-Luc Picard\nPHOTO;ENCODING=b;TYPE=PNG:{png_b64}\nEND:VCARD\n"
        );
        let rows = vec![make_row("contacts", "picard", &vcard)];
        let parsed = parse_loaded(rows);
        let c = &parsed.contacts[0];
        assert!(c.photo.is_some(), "expected decoded photo");
        let ph = c.photo.as_ref().unwrap();
        assert_eq!(ph.content_type, "image/png");
        assert_eq!(&ph.bytes[1..4], b"PNG");
    }

    #[test]
    fn parse_missing_db_returns_empty_silently() {
        let parsed = parse(Path::new("/this/does/not/exist.doltlite_db")).unwrap();
        assert_eq!(parsed.contacts.len(), 0);
    }
}
