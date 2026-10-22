//! A card's `PHOTO`. Where the card carries the image itself, the
//! download keeps the decoded bytes in the source's blob store, and the
//! card's page links them there (`docs/dev/contacts.md` §"Photos").

use base64::Engine;

use super::api::{split_vcards, vcard_all, VcardProp};

/// A photo the card carries inline, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardPhoto {
    pub bytes: Vec<u8>,
    /// `image/jpeg`, `image/png`, …, from the `TYPE=` parameter or the
    /// `data:` URL; `application/octet-stream` when it names none. The
    /// blob store corrects it from the bytes.
    pub content_type: String,
}

/// The card's photo: the first inline image, else the first URL-only
/// one.
pub fn card_photo(vcard: &str) -> (Option<CardPhoto>, Option<String>) {
    let mut url: Option<String> = None;
    for p in vcard_all(vcard, "PHOTO") {
        if let Some(photo) = inline(&p) {
            return (Some(photo), None);
        }
        if url.is_none() && (p.value.starts_with("http://") || p.value.starts_with("https://")) {
            url = Some(p.value.clone());
        }
    }
    (None, url)
}

/// The inline photo of every card in a row's vCard text, which for a row
/// a `.vcf` file made may hold several cards.
pub fn inline_photos(vcard_text: &str) -> Vec<CardPhoto> {
    let mut cards = split_vcards(vcard_text);
    if cards.is_empty() {
        cards.push(vcard_text.to_string());
    }
    cards.iter().filter_map(|card| card_photo(card).0).collect()
}

/// vCard 3.0's `ENCODING=b` body, or vCard 4.0's `data:` URL.
fn inline(p: &VcardProp) -> Option<CardPhoto> {
    let is_b64 = p
        .param("ENCODING")
        .is_some_and(|v| v.eq_ignore_ascii_case("b") || v.eq_ignore_ascii_case("base64"));
    if is_b64 {
        let content_type = p
            .param("TYPE")
            .map(|t| format!("image/{}", t.to_ascii_lowercase()))
            .unwrap_or_else(|| "application/octet-stream".to_string());
        return decode(&p.value).map(|bytes| CardPhoto {
            bytes,
            content_type,
        });
    }
    let (meta, b64) = p.value.strip_prefix("data:")?.split_once(',')?;
    let content_type = meta
        .split(';')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    decode(b64).map(|bytes| CardPhoto {
        bytes,
        content_type,
    })
}

/// Base64 with the whitespace line folding left in it removed.
fn decode(b64: &str) -> Option<Vec<u8>> {
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABAQMAAAAl21bKAAAAA1BMVEX/AAAZ4gk3AAAAAXRSTlMAQObYZgAAAApJREFUCNdjYAAAAAIAAeIhvDMAAAAASUVORK5CYII=";

    fn card(uid: &str, photo: &str) -> String {
        format!("BEGIN:VCARD\nVERSION:3.0\nUID:{uid}\nFN:{uid}\n{photo}\nEND:VCARD\n")
    }

    #[test]
    fn an_inline_photo_is_decoded_with_its_type() {
        let (photo, url) = card_photo(&card(
            "picard",
            &format!("PHOTO;ENCODING=b;TYPE=PNG:{PNG_B64}"),
        ));
        let photo = photo.expect("decoded");
        assert_eq!(photo.content_type, "image/png");
        assert_eq!(&photo.bytes[1..4], b"PNG");
        assert_eq!(url, None);

        let (photo, _) = card_photo(&card(
            "riker",
            &format!("PHOTO:data:image/png;base64,{PNG_B64}"),
        ));
        assert_eq!(photo.expect("decoded").content_type, "image/png");
    }

    #[test]
    fn a_url_only_photo_is_its_url() {
        let (photo, url) = card_photo(&card("worf", "PHOTO;VALUE=uri:https://e.invalid/worf.jpg"));
        assert_eq!(photo, None);
        assert_eq!(url.as_deref(), Some("https://e.invalid/worf.jpg"));
    }

    /// A `.vcf` file's row holds every card in it; each one's photo is
    /// found, and a card without one adds nothing.
    #[test]
    fn every_card_in_a_row_gives_its_photo() {
        let text = [
            card("picard", &format!("PHOTO;ENCODING=b;TYPE=PNG:{PNG_B64}")),
            card("data", "NOTE:no photo"),
            card("riker", &format!("PHOTO:data:image/png;base64,{PNG_B64}")),
        ]
        .concat();
        assert_eq!(inline_photos(&text).len(), 2);
    }
}
