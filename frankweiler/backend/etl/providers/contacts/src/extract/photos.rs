//! Lift inline vCard `PHOTO` bytes into the per-source CAS, mapped by a
//! `contact_photos` edge row.
//!
//! The vCard text (with its base64 `PHOTO`) stays the canonical wire
//! data in `contacts.payload`; this is an *additional* content-addressed
//! copy so the contact→photo-blob mapping matches the LinkedIn provider's
//! [`contact_photos`](super::schema_raw::CONTACT_PHOTOS_TABLE) shape.
//! (The two providers deliberately don't share code — raw data is owned
//! per-provider — only the table shape.)
//!
//! Runs once per contact: we only lift a `contacts.id` that has no
//! `contact_photos` row yet, so re-extraction is a cheap no-op.

use anyhow::{Context, Result};
use base64::Engine;
use frankweiler_etl::blob_cas::{cas_path_for, BlobCas};
use sqlx::Row;

use super::api::vcard_all;
use super::db::RawDb;

/// Decode + store every not-yet-lifted contact's inline photo. Returns
/// the number of photos newly written to CAS. `entity_db_path` is the
/// contacts entity db; its CAS sibling is derived via [`cas_path_for`].
pub async fn lift_photos_to_cas(db: &RawDb, entity_db_path: &std::path::Path) -> Result<usize> {
    let pool = db.pool();
    // Contacts lacking a contact_photos row. The vCard is unwrapped from
    // the `{"vcard": …}` envelope on the SQL side, same as translate.
    let rows = sqlx::query(
        "SELECT c.id AS id, json_extract(c.payload, '$.vcard') AS vcard
           FROM contacts c
          WHERE c.id NOT IN (SELECT owner_id FROM contact_photos)",
    )
    .fetch_all(pool)
    .await
    .context("select contacts needing photo lift")?;
    if rows.is_empty() {
        return Ok(0);
    }

    let cas = BlobCas::open(&cas_path_for(entity_db_path))
        .await
        .context("open contacts CAS")?;

    let mut stored = 0usize;
    for r in rows {
        let id: String = r.try_get("id").unwrap_or_default();
        let vcard: Option<String> = r.try_get("vcard").ok();
        let Some(vcard) = vcard else { continue };
        if id.is_empty() {
            continue;
        }
        let Some((bytes, content_type)) = decode_inline_photo(&vcard) else {
            // No inline photo on this card — don't record a row, so a
            // later card edit that adds one still gets picked up.
            continue;
        };
        let blake3 = cas
            .put(&bytes, Some(&content_type))
            .await
            .context("cas put contact photo")?;
        sqlx::query(
            "INSERT OR REPLACE INTO contact_photos (id, owner_id, source_url, blake3) \
             VALUES (?, ?, 'vcard:inline', ?)",
        )
        .bind(format!("{id}#vcard:inline"))
        .bind(&id)
        .bind(&blake3)
        .execute(pool)
        .await
        .context("insert contact_photos row")?;
        stored += 1;
    }
    Ok(stored)
}

/// First inline (base64 / `data:`) `PHOTO` in a vCard → `(bytes,
/// content_type)`. Mirrors the decode in `translate::parse` (kept
/// separate: this is the raw side). URL-only photos return `None` here —
/// they carry no bytes to store.
fn decode_inline_photo(vcard: &str) -> Option<(Vec<u8>, String)> {
    let engine = base64::engine::general_purpose::STANDARD;
    for p in vcard_all(vcard, "PHOTO") {
        // vCard 3.0 `ENCODING=b` / `ENCODING=base64`.
        let is_b64 = p
            .param("ENCODING")
            .map(|v| v.eq_ignore_ascii_case("b") || v.eq_ignore_ascii_case("base64"))
            .unwrap_or(false);
        if is_b64 {
            let cleaned: String = p.value.chars().filter(|c| !c.is_whitespace()).collect();
            if let Ok(bytes) = engine.decode(cleaned.as_bytes()) {
                let content_type = p
                    .param("TYPE")
                    .map(|t| format!("image/{}", t.to_ascii_lowercase()))
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                return Some((bytes, content_type));
            }
        }
        // vCard 4.0 inline `data:` URL.
        if let Some(rest) = p.value.strip_prefix("data:") {
            if let Some((meta, b64)) = rest.split_once(',') {
                let content_type = meta
                    .split(';')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
                if let Ok(bytes) = engine.decode(cleaned.as_bytes()) {
                    return Some((bytes, content_type));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_inline_base64_photo() {
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";
        let vcard = format!(
            "BEGIN:VCARD\nVERSION:3.0\nUID:x\nPHOTO;ENCODING=b;TYPE=PNG:{png_b64}\nEND:VCARD\n"
        );
        let (bytes, ct) = decode_inline_photo(&vcard).expect("decoded");
        assert_eq!(ct, "image/png");
        assert_eq!(&bytes[1..4], b"PNG");
        // URL-only photo → no inline bytes.
        let url_card = "BEGIN:VCARD\nPHOTO;VALUE=URI:https://x/p.jpg\nEND:VCARD\n";
        assert!(decode_inline_photo(url_card).is_none());
    }
}
