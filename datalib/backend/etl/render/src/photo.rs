//! A source contact's photo, written beside the rendered page that
//! carries the contact, and the path the index's asset route serves it
//! at (`docs/dev/contacts.md` §"Photos").

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_contact_schema::{is_drawable_photo, NormalizedContact, Photo};

/// Write `bytes` to `<page_dir>/blobs/<stem>.<ext>`; the path relative
/// to the page.
pub fn write_photo(
    page_dir: &Path,
    stem: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<String> {
    let blobs_dir = page_dir.join("blobs");
    fs::create_dir_all(&blobs_dir).with_context(|| format!("mkdir -p {}", blobs_dir.display()))?;
    let filename = format!("{stem}.{}", ext_for(content_type));
    let path = blobs_dir.join(&filename);
    fs::write(&path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(format!("blobs/{filename}"))
}

/// `contact` as the index holds it: its photo, written at `rel` beside
/// the page `doc_uuid`, as the URL the app serves it at, where it is an
/// image a browser draws.
pub fn with_photo_url(
    contact: &NormalizedContact,
    doc_uuid: &str,
    rel: Option<&str>,
) -> NormalizedContact {
    let drawable = matches!(
        &contact.photo,
        Some(Photo::Inline { content_type, .. }) if is_drawable_photo(content_type)
    );
    let mut out = contact.clone();
    out.photo_url = rel
        .filter(|_| drawable)
        .map(|rel| format!("/applet/unified_document_index/asset/{doc_uuid}/{rel}"));
    out
}

/// Every inline photo among `contacts` written beside the page, each
/// named by its bytes so one page holds many people's, and given its URL.
/// A photo that will not write leaves its contact without one.
pub fn place_photos(
    contacts: Vec<NormalizedContact>,
    page_dir: &Path,
    doc_uuid: &str,
) -> Vec<NormalizedContact> {
    contacts
        .into_iter()
        .map(|c| {
            let Some(Photo::Inline {
                content_type,
                bytes,
            }) = &c.photo
            else {
                return c;
            };
            let stem = format!("photo-{}", &datalib_etl::blob_cas::blake3_hex(bytes)[..16]);
            match write_photo(page_dir, &stem, content_type, bytes) {
                Ok(rel) => with_photo_url(&c, doc_uuid, Some(&rel)),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "a contact's photo would not write");
                    c
                }
            }
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_contact_schema::ContactKind;

    fn with_photo(key: &str, content_type: &str, bytes: &[u8]) -> NormalizedContact {
        let mut c = NormalizedContact::new("tng", key, ContactKind::Person);
        c.photo = Some(Photo::Inline {
            content_type: content_type.into(),
            bytes: bytes.to_vec(),
        });
        c
    }

    /// Two people on one page each get their own file, and only an image
    /// a browser draws gets a URL.
    #[test]
    fn each_photo_on_a_page_is_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let placed = place_photos(
            vec![
                with_photo("riker", "image/png", b"number one"),
                with_photo("worf", "image/png", b"klingon"),
                with_photo("data", "image/heic", b"android"),
                NormalizedContact::new("tng", "troi", ContactKind::Person),
            ],
            dir.path(),
            "doc",
        );
        let urls: Vec<Option<&str>> = placed.iter().map(|c| c.photo_url.as_deref()).collect();
        assert!(urls[0]
            .unwrap()
            .starts_with("/applet/unified_document_index/asset/doc/blobs/photo-"));
        assert_ne!(urls[0], urls[1]);
        assert_eq!(urls[2], None, "a heic photo is kept but not drawn");
        assert_eq!(urls[3], None);
        assert_eq!(
            std::fs::read_dir(dir.path().join("blobs")).unwrap().count(),
            3
        );
    }
}
