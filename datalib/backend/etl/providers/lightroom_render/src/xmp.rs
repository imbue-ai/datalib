//! The few fields of Lightroom's per-image XMP packet that no other column
//! of the catalog holds: the capture time's offset, the title and the
//! headline. `Adobe_AdditionalMetadata.xmp` is a 4-byte big-endian length
//! and then the packet, zlib-compressed.

use std::io::Read;

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpFields {
    pub date_time_original: Option<String>,
    pub create_date: Option<String>,
    pub title: Option<String>,
    pub headline: Option<String>,
}

/// The packet's text, `None` when the blob is not one Lightroom wrote.
pub fn decompress(blob: &[u8]) -> Option<String> {
    let body = blob.get(4..)?;
    let mut text = String::new();
    flate2::read::ZlibDecoder::new(body)
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// XMP writes a simple property either as an attribute of its
/// `rdf:Description` or as an element of its own; both are read.
pub fn fields(packet: &str) -> XmpFields {
    XmpFields {
        date_time_original: property(packet, "exif", "DateTimeOriginal"),
        create_date: property(packet, "xmp", "CreateDate"),
        title: alt_default(packet, "dc", "title"),
        headline: property(packet, "photoshop", "Headline"),
    }
}

fn property(packet: &str, prefix: &str, name: &str) -> Option<String> {
    let attr = Regex::new(&format!(r#"\b{prefix}:{name}="([^"]*)""#)).expect("a fixed pattern");
    let elem = Regex::new(&format!(r"<{prefix}:{name}>([^<]*)</{prefix}:{name}>"))
        .expect("a fixed pattern");
    attr.captures(packet)
        .or_else(|| elem.captures(packet))
        .and_then(|c| unescaped(&c[1]))
}

/// A language-alternative property's default: `dc:title`'s
/// `<rdf:li xml:lang="x-default">`, else its first entry.
fn alt_default(packet: &str, prefix: &str, name: &str) -> Option<String> {
    static LI: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?s)<rdf:li([^>]*)>(.*?)</rdf:li>"#).expect("a fixed pattern")
    });
    let block = Regex::new(&format!(r"(?s)<{prefix}:{name}>(.*?)</{prefix}:{name}>"))
        .expect("a fixed pattern");
    let inner = block.captures(packet)?.get(1)?.as_str();
    let entries: Vec<(&str, &str)> = LI
        .captures_iter(inner)
        .map(|c| {
            (
                c.get(1).map_or("", |m| m.as_str()),
                c.get(2).map_or("", |m| m.as_str()),
            )
        })
        .collect();
    let (_, text) = entries
        .iter()
        .find(|(attrs, _)| attrs.contains("x-default"))
        .or(entries.first())?;
    unescaped(text)
}

fn unescaped(raw: &str) -> Option<String> {
    let text = quick_xml::escape::unescape(raw)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| raw.to_string());
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn blob(packet: &str) -> Vec<u8> {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(packet.as_bytes()).unwrap();
        let body = z.finish().unwrap();
        let mut out = (packet.len() as u32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    const PACKET: &str = r#"<x:xmpmeta><rdf:RDF><rdf:Description
   xmp:CreateDate="2364-03-02T08:00:00+01:00"
   exif:DateTimeOriginal="2364-03-02T07:59:12-08:00"
   photoshop:Headline="Ten Forward">
   <dc:title><rdf:Alt>
     <rdf:li xml:lang="de">Zehn Vorne</rdf:li>
     <rdf:li xml:lang="x-default">Riker &amp; Troi</rdf:li>
   </rdf:Alt></dc:title>
  </rdf:Description></rdf:RDF></x:xmpmeta>"#;

    #[test]
    fn reads_what_only_the_packet_holds() {
        let text = decompress(&blob(PACKET)).expect("a Lightroom blob");
        assert_eq!(
            fields(&text),
            XmpFields {
                date_time_original: Some("2364-03-02T07:59:12-08:00".into()),
                create_date: Some("2364-03-02T08:00:00+01:00".into()),
                title: Some("Riker & Troi".into()),
                headline: Some("Ten Forward".into()),
            }
        );
    }

    /// The same property written as an element rather than an attribute.
    #[test]
    fn reads_an_element_as_well_as_an_attribute() {
        let packet = "<exif:DateTimeOriginal>2364-03-02T07:59:12</exif:DateTimeOriginal>";
        assert_eq!(
            fields(packet).date_time_original.as_deref(),
            Some("2364-03-02T07:59:12")
        );
    }

    #[test]
    fn a_blob_that_is_not_zlib_is_no_packet() {
        assert_eq!(decompress(b"\0\0\0\x05hello"), None);
        assert_eq!(decompress(b"ab"), None);
    }
}
