//! Document identity and metadata pulled straight out of the PDF file.
//!
//! Everything here is **best-effort and non-fatal**. A PDF that lopdf
//! cannot parse still converts fine — pdf-inspector has its own
//! parser — so a failure in this module downgrades to `None` columns
//! rather than failing the document. That asymmetry is deliberate:
//! conversion is the job, lineage is a bonus.
//!
//! See [`super::schema_raw`] §"Ship of Theseus" for why these are hints
//! rather than keys.

use lopdf::{Document, Object};

/// Best-effort identity + metadata for one PDF.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocIdentity {
    /// Trailer `/ID[0]`, lowercase hex. Spec'd as permanent for the
    /// document's lifetime — in practice many producers regenerate it.
    pub pdf_id_permanent: Option<String>,
    /// `xmpMM:DocumentID` — stable across edits and saves.
    pub xmp_document_id: Option<String>,
    /// `xmpMM:InstanceID` — a fresh GUID on every save.
    pub xmp_instance_id: Option<String>,
    /// `xmpMM:OriginalDocumentID` — the ancestor this was derived from.
    pub xmp_original_document_id: Option<String>,
    pub title: Option<String>,
    /// Info-dict `/Author`, falling back to XMP `dc:creator`. Frequently
    /// absent — most producers never set it — so callers must treat
    /// `None` as the normal case, not a failure.
    pub author: Option<String>,
    /// ISO-8601 with the source offset preserved (per AGENTS.md).
    pub created_at: Option<String>,
    pub modified_at: Option<String>,
    /// The trailer declares `/Encrypt`. Strings and streams are
    /// ciphertext, so metadata is unreliable and conversion may yield
    /// nothing.
    pub encrypted: bool,
}

/// Parse the whole document once and pull out every identity field we
/// know how to find. Never returns `Err`; an unparseable file yields
/// `DocIdentity::default()`.
pub fn extract(bytes: &[u8]) -> DocIdentity {
    let Ok(doc) = Document::load_mem(bytes) else {
        return DocIdentity::default();
    };
    let mut out = DocIdentity {
        encrypted: doc.trailer.get(b"Encrypt").is_ok(),
        ..Default::default()
    };

    // ── Trailer /ID ──────────────────────────────────────────────────
    // `/ID` is a two-element array of byte strings: [permanent, per-save].
    if let Ok(Object::Array(ids)) = doc.trailer.get(b"ID") {
        if let Some(Ok(first)) = ids.first().map(|o| o.as_str()) {
            if !first.is_empty() {
                out.pdf_id_permanent = Some(to_hex(first));
            }
        }
    }

    // ── Info dictionary ──────────────────────────────────────────────
    if let Ok(info_ref) = doc.trailer.get(b"Info") {
        let info = match info_ref {
            Object::Reference(id) => doc.get_object(*id).ok().and_then(|o| o.as_dict().ok()),
            Object::Dictionary(d) => Some(d),
            _ => None,
        };
        if let Some(info) = info {
            out.title = info.get(b"Title").ok().and_then(text_of);
            out.author = info.get(b"Author").ok().and_then(text_of);
            out.created_at = info
                .get(b"CreationDate")
                .ok()
                .and_then(text_of)
                .and_then(|s| parse_pdf_date(&s));
            out.modified_at = info
                .get(b"ModDate")
                .ok()
                .and_then(text_of)
                .and_then(|s| parse_pdf_date(&s));
        }
    }

    // ── XMP metadata stream ──────────────────────────────────────────
    if let Some(xmp) = xmp_bytes(&doc) {
        let xmp = String::from_utf8_lossy(&xmp);
        out.xmp_document_id = xmp_field(&xmp, "xmpMM:DocumentID");
        out.xmp_instance_id = xmp_field(&xmp, "xmpMM:InstanceID");
        out.xmp_original_document_id = xmp_field(&xmp, "xmpMM:OriginalDocumentID");
        // Only as a fallback: the Info dict is the more commonly
        // populated of the two, and when both exist they agree.
        if out.author.is_none() {
            out.author = xmp_field(&xmp, "dc:creator")
                .as_deref()
                .and_then(first_rdf_item);
        }
    }

    out
}

/// The catalog's `/Metadata` stream, decompressed. `None` when absent
/// or undecodable.
fn xmp_bytes(doc: &Document) -> Option<Vec<u8>> {
    let catalog = doc.catalog().ok()?;
    let meta = catalog.get(b"Metadata").ok()?;
    let obj = match meta {
        Object::Reference(id) => doc.get_object(*id).ok()?,
        other => other,
    };
    let stream = obj.as_stream().ok()?;
    // Most XMP packets are stored uncompressed, but FlateDecode is
    // legal and common enough to matter.
    stream
        .decompressed_content()
        .ok()
        .or_else(|| Some(stream.content.clone()))
}

/// Pull one XMP property by qualified name (`xmpMM:DocumentID`,
/// `dc:creator`). XMP is RDF/XML and can encode a property either as an
/// attribute (`xmpMM:DocumentID="uuid:…"`) or as an element
/// (`<xmpMM:DocumentID>uuid:…</xmpMM:DocumentID>`); real files use both,
/// so we try each rather than pulling in an XML parser for four fields.
fn xmp_field(xmp: &str, qname: &str) -> Option<String> {
    let elem_open = format!("<{qname}>");
    if let Some(start) = xmp.find(&elem_open) {
        let rest = &xmp[start + elem_open.len()..];
        if let Some(end) = rest.find(&format!("</{qname}>")) {
            let v = rest[..end].trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    let attr = format!("{qname}=");
    let start = xmp.find(&attr)? + attr.len();
    let rest = &xmp[start..];
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let rest = &rest[1..];
    let end = rest.find(quote)?;
    let v = rest[..end].trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// `dc:creator` is an ordered array, not a scalar: XMP wraps it as
/// `<rdf:Seq><rdf:li>Name</rdf:li>…</rdf:Seq>`. Take the first entry —
/// `grid_rows.author` is one column, and the first listed creator is
/// the primary one by RDF convention. A bare (non-array) value passes
/// through unchanged, since some writers emit that instead.
fn first_rdf_item(inner: &str) -> Option<String> {
    let inner = inner.trim();
    if let Some(start) = inner.find("<rdf:li") {
        // Skip any attributes on the <rdf:li ...> tag itself.
        let after_tag = inner[start..].find('>')? + start + 1;
        let rest = &inner[after_tag..];
        let end = rest.find("</rdf:li>")?;
        let v = rest[..end].trim();
        return (!v.is_empty()).then(|| v.to_string());
    }
    // No array wrapper, and nothing that looks like leftover markup.
    (!inner.is_empty() && !inner.contains('<')).then(|| inner.to_string())
}

/// Decode a PDF text-string object to a Rust `String`, handling the
/// UTF-16BE BOM form the spec allows.
fn text_of(o: &Object) -> Option<String> {
    let raw = o.as_str().ok()?;
    if raw.len() >= 2 && raw[0] == 0xFE && raw[1] == 0xFF {
        let units: Vec<u16> = raw[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16_lossy(&units);
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    let s = String::from_utf8_lossy(raw);
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn to_hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// Convert a PDF date string to ISO-8601 **preserving the source
/// offset**, per AGENTS.md §"Timestamp convention".
///
/// The PDF form is `D:YYYYMMDDHHmmSSOHH'mm'` where `O` is `+`, `-`, or
/// `Z`, and every component after the year is optional. `D:20240115`
/// alone is legal. An offsetless timestamp is rendered without one
/// rather than being invented as UTC — we genuinely do not know the
/// zone, and guessing would fabricate information the file did not
/// carry.
pub fn parse_pdf_date(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s.strip_prefix("D:").unwrap_or(s);
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return None;
    }
    let at =
        |i: usize, n: usize| -> Option<&str> { (digits.len() >= i + n).then(|| &digits[i..i + n]) };
    let year = at(0, 4)?;
    let month = at(4, 2).unwrap_or("01");
    let day = at(6, 2).unwrap_or("01");
    let hour = at(8, 2).unwrap_or("00");
    let min = at(10, 2).unwrap_or("00");
    let sec = at(12, 2).unwrap_or("00");

    let mut out = format!("{year}-{month}-{day}T{hour}:{min}:{sec}");

    // Offset: whatever follows the digit run.
    let tail = &s[digits.len()..];
    let mut tc = tail.chars();
    match tc.next() {
        Some('Z') | Some('z') => out.push_str("+00:00"),
        Some(sign @ ('+' | '-')) => {
            let rest: String = tail[1..].chars().filter(|c| c.is_ascii_digit()).collect();
            if rest.len() >= 2 {
                let oh = &rest[0..2];
                let om = if rest.len() >= 4 { &rest[2..4] } else { "00" };
                out.push(sign);
                out.push_str(oh);
                out.push(':');
                out.push_str(om);
            }
        }
        // No offset in the source: leave it off rather than assume UTC.
        _ => {}
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_pdf_date_keeps_its_offset() {
        assert_eq!(
            parse_pdf_date("D:20240115103000-08'00'").as_deref(),
            Some("2024-01-15T10:30:00-08:00")
        );
    }

    #[test]
    fn z_becomes_an_explicit_zero_offset() {
        assert_eq!(
            parse_pdf_date("D:20240115103000Z").as_deref(),
            Some("2024-01-15T10:30:00+00:00")
        );
    }

    #[test]
    fn offsetless_date_does_not_invent_utc() {
        // We don't know the zone; claiming +00:00 would fabricate it.
        assert_eq!(
            parse_pdf_date("D:20240115103000").as_deref(),
            Some("2024-01-15T10:30:00")
        );
    }

    #[test]
    fn truncated_date_fills_defaults() {
        assert_eq!(
            parse_pdf_date("D:2024").as_deref(),
            Some("2024-01-01T00:00:00")
        );
    }

    #[test]
    fn positive_offset_and_missing_minutes() {
        assert_eq!(
            parse_pdf_date("D:20240115103000+05").as_deref(),
            Some("2024-01-15T10:30:00+05:00")
        );
    }

    #[test]
    fn garbage_date_is_none() {
        assert_eq!(parse_pdf_date("not-a-date"), None);
        assert_eq!(parse_pdf_date(""), None);
    }

    #[test]
    fn xmp_element_form_is_found() {
        let x = r#"<rdf:Description><xmpMM:DocumentID>uuid:abc-123</xmpMM:DocumentID></rdf:Description>"#;
        assert_eq!(
            xmp_field(x, "xmpMM:DocumentID").as_deref(),
            Some("uuid:abc-123")
        );
    }

    #[test]
    fn xmp_attribute_form_is_found() {
        let x = r#"<rdf:Description xmpMM:InstanceID="uuid:def-456" xmpMM:DocumentID="uuid:abc"/>"#;
        assert_eq!(
            xmp_field(x, "xmpMM:InstanceID").as_deref(),
            Some("uuid:def-456")
        );
        assert_eq!(
            xmp_field(x, "xmpMM:DocumentID").as_deref(),
            Some("uuid:abc")
        );
    }

    #[test]
    fn xmp_missing_field_is_none() {
        assert_eq!(xmp_field("<rdf:Description/>", "xmpMM:DocumentID"), None);
    }

    #[test]
    fn dc_creator_seq_yields_the_first_entry() {
        let x = "<dc:creator><rdf:Seq><rdf:li>Jean-Luc Picard</rdf:li>\
                 <rdf:li>William Riker</rdf:li></rdf:Seq></dc:creator>";
        let raw = xmp_field(x, "dc:creator").unwrap();
        assert_eq!(first_rdf_item(&raw).as_deref(), Some("Jean-Luc Picard"));
    }

    #[test]
    fn dc_creator_scalar_passes_through() {
        assert_eq!(
            first_rdf_item("Geordi La Forge").as_deref(),
            Some("Geordi La Forge")
        );
    }

    #[test]
    fn rdf_li_with_attributes_is_handled() {
        assert_eq!(
            first_rdf_item(r#"<rdf:Seq><rdf:li xml:lang="x-default">Data</rdf:li></rdf:Seq>"#)
                .as_deref(),
            Some("Data")
        );
    }

    #[test]
    fn leftover_markup_is_not_mistaken_for_a_name() {
        // An empty Seq must yield None rather than a chunk of RDF.
        assert_eq!(first_rdf_item("<rdf:Seq></rdf:Seq>"), None);
    }

    #[test]
    fn unparseable_bytes_yield_empty_identity_not_an_error() {
        assert_eq!(extract(b"this is not a pdf"), DocIdentity::default());
    }

    #[test]
    fn utf16be_title_is_decoded() {
        let mut raw = vec![0xFE, 0xFF];
        for u in "Hi".encode_utf16() {
            raw.extend_from_slice(&u.to_be_bytes());
        }
        let o = Object::String(raw, lopdf::StringFormat::Hexadecimal);
        assert_eq!(text_of(&o).as_deref(), Some("Hi"));
    }
}
