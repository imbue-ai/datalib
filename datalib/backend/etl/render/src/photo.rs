//! A source contact's photo as a page links it: at its address in the
//! blob store that holds it, which `datalib-http` serves
//! (`docs/dev/contacts.md` §"Photos").

use datalib_contact_schema::{is_drawable_photo, NormalizedContact, Photo};
use datalib_etl::blob_cas::Described;

/// The photo `held` describes, in the blob store of `step`.
pub fn stored(step: &str, held: &Described) -> Photo {
    Photo::Stored {
        step: step.to_string(),
        blake3: held.blake3.clone(),
        content_type: held
            .content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string()),
    }
}

/// Where a page links the photo, whatever its type: its blob-store
/// address. `None` for a URL nothing fetched.
pub fn photo_address(photo: &Photo) -> Option<String> {
    match photo {
        Photo::Stored { step, blake3, .. } => Some(datalib_blob_store::address(step, blake3)),
        Photo::Url(_) => None,
    }
}

/// `contact` as the index holds it: with the URL a chip draws its photo
/// from, where it is an image a browser draws.
pub fn with_photo_url(contact: &NormalizedContact) -> NormalizedContact {
    let mut out = contact.clone();
    out.photo_url = match &contact.photo {
        Some(photo @ Photo::Stored { content_type, .. }) if is_drawable_photo(content_type) => {
            photo_address(photo)
        }
        _ => None,
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_contact_schema::ContactKind;

    fn with_photo(photo: Option<Photo>) -> NormalizedContact {
        let mut c = NormalizedContact::new("tng", "riker", ContactKind::Person);
        c.photo = photo;
        c
    }

    fn held(content_type: Option<&str>) -> Described {
        Described {
            blake3: "ab".repeat(32),
            byte_len: 3,
            content_type: content_type.map(str::to_string),
        }
    }

    /// A stored image a browser draws is drawn from its blob-store
    /// address; a stored photo of another type is linked from its page
    /// but gives the chip no URL, and so does a URL nothing fetched.
    #[test]
    fn only_a_stored_drawable_photo_has_a_url() {
        let address = format!("/data/tng/ingest/blob/{}", "ab".repeat(32));
        let png = stored("tng/ingest", &held(Some("image/png")));
        assert_eq!(photo_address(&png).as_deref(), Some(address.as_str()));
        assert_eq!(
            with_photo_url(&with_photo(Some(png))).photo_url.as_deref(),
            Some(address.as_str())
        );

        let heic = stored("tng/ingest", &held(Some("image/heic")));
        assert_eq!(photo_address(&heic).as_deref(), Some(address.as_str()));
        assert_eq!(with_photo_url(&with_photo(Some(heic))).photo_url, None);

        let untyped = stored("tng/ingest", &held(None));
        assert_eq!(with_photo_url(&with_photo(Some(untyped))).photo_url, None);

        let url = Photo::Url("https://e.invalid/riker.png".into());
        assert_eq!(photo_address(&url), None);
        assert_eq!(with_photo_url(&with_photo(Some(url))).photo_url, None);
        assert_eq!(with_photo_url(&with_photo(None)).photo_url, None);
    }
}
