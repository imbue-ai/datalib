//! A photo's XMP as the catalog stores it, and setting its two capture
//! dates. The text is edited in place, so everything else Lightroom wrote
//! stays byte for byte; the result is then parsed with its namespaces and
//! refused unless the two dates read back as set and nothing else moved.
//! Lightroom reads XMP that is not namespace-well-formed as empty, without
//! a word (lightroom/tests/capture_time/RESULTS.md §"Round 5").

use std::collections::BTreeMap;
use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prop {
    pub ns: &'static str,
    pub prefix: &'static str,
    pub local: &'static str,
}

pub const DATE_TIME_ORIGINAL: Prop = Prop {
    ns: "http://ns.adobe.com/exif/1.0/",
    prefix: "exif",
    local: "DateTimeOriginal",
};

pub const DATE_CREATED: Prop = Prop {
    ns: "http://ns.adobe.com/photoshop/1.0/",
    prefix: "photoshop",
    local: "DateCreated",
};

/// The catalog keeps XMP as its length (4 bytes, big-endian), then zlib.
pub fn unpack(blob: &[u8]) -> Result<String, String> {
    let (len, body) = blob
        .split_at_checked(4)
        .ok_or("XMP blob shorter than its length prefix")?;
    let len = u32::from_be_bytes(len.try_into().expect("four bytes")) as usize;
    let mut text = Vec::with_capacity(len);
    ZlibDecoder::new(body)
        .read_to_end(&mut text)
        .map_err(|e| format!("XMP blob does not inflate: {e}"))?;
    if text.len() != len {
        return Err(format!(
            "XMP blob says {len} bytes and inflates to {}",
            text.len()
        ));
    }
    String::from_utf8(text).map_err(|_| "XMP is not UTF-8".into())
}

pub fn pack(text: &str) -> Vec<u8> {
    let mut blob = (text.len() as u32).to_be_bytes().to_vec();
    let mut z = ZlibEncoder::new(&mut blob, Compression::default());
    z.write_all(text.as_bytes()).expect("writing to a Vec");
    z.finish().expect("writing to a Vec");
    blob
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Attribute,
    Element,
}

type Key = (String, String);

/// What one parse of an XMP finds: its `rdf:Description`s, every simple
/// property on them (attribute, or element holding only text) by
/// namespace and local name, raw as written, and the prefix each
/// namespace is declared with.
#[derive(Debug, Default)]
struct Model {
    descriptions: usize,
    description_tag: String,
    props: BTreeMap<Key, (Form, String)>,
    prefixes: BTreeMap<String, String>,
}

fn key(prop: Prop) -> Key {
    (prop.ns.to_string(), prop.local.to_string())
}

fn read(xmp: &str) -> Result<Model, String> {
    let mut reader = NsReader::from_str(xmp);
    let mut model = Model::default();
    let mut depth = 0usize;
    let mut description_depth = None;
    // A child of the description: its key, its text, and whether it held
    // anything but text.
    let mut child: Option<(Key, String, bool)> = None;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("XMP is not well-formed: {e}"))?;
        match &event {
            Event::Start(e) | Event::Empty(e) => {
                let (name, attributes) = resolve(&reader, e, &mut model.prefixes)?;
                let is_description = name == (RDF.to_string(), "Description".to_string());
                if is_description {
                    model.descriptions += 1;
                    model.description_tag = String::from_utf8_lossy(e.name().0.as_bytes()).into();
                    for (k, v) in attributes {
                        model.props.insert(k, (Form::Attribute, v));
                    }
                } else if let Some(d) = description_depth {
                    if depth == d && matches!(event, Event::Start(_)) {
                        child = Some((name, String::new(), false));
                    } else if let Some(c) = child.as_mut() {
                        c.2 = true;
                    }
                }
                if let Event::Start(_) = event {
                    depth += 1;
                    if is_description {
                        description_depth = Some(depth);
                    }
                }
            }
            Event::Text(t) => {
                if let Some(c) = child.as_mut() {
                    c.1.push_str(&t.clone().into_inner());
                }
            }
            Event::CData(_) | Event::GeneralRef(_) => {
                if let Some(c) = child.as_mut() {
                    c.2 = true;
                }
            }
            Event::End(_) => {
                if Some(depth) == description_depth {
                    description_depth = None;
                }
                if description_depth.is_some_and(|d| depth == d + 1) {
                    if let Some((k, text, nested)) = child.take() {
                        if !nested {
                            model.props.insert(k, (Form::Element, text));
                        }
                    }
                }
                depth -= 1;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if depth != 0 {
        return Err("XMP is not well-formed: an element is never closed".into());
    }
    Ok(model)
}

type Resolved = (Key, Vec<(Key, String)>);

/// The element's name and its non-`xmlns` attributes by namespace, and
/// any prefix it declares. A prefix nothing declares is an error.
fn resolve(
    reader: &NsReader<&[u8]>,
    e: &BytesStart,
    prefixes: &mut BTreeMap<String, String>,
) -> Result<Resolved, String> {
    let resolver = reader.resolver();
    let (ns, local) = resolver.resolve_element(e.name());
    let name = (bound(ns, e.name().0)?, local.into_inner().to_string());
    let mut attributes = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|e| format!("XMP is not well-formed: {e}"))?;
        if let Some(binding) = attr.key.as_namespace_binding() {
            if let quick_xml::name::PrefixDeclaration::Named(p) = binding {
                prefixes
                    .entry(attr.value.to_string())
                    .or_insert_with(|| p.to_string());
            }
            continue;
        }
        let (ns, local) = resolver.resolve_attribute(attr.key);
        let k = (bound(ns, attr.key.0)?, local.into_inner().to_string());
        attributes.push((k, attr.value.to_string()));
    }
    Ok((name, attributes))
}

fn bound(ns: ResolveResult, qname: &str) -> Result<String, String> {
    match ns {
        ResolveResult::Bound(n) => Ok(n.0.to_string()),
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(p) => Err(format!(
            "XMP uses the prefix `{p}` (in `{qname}`) without declaring it"
        )),
    }
}

pub fn get(xmp: &str, prop: Prop) -> Result<Option<String>, String> {
    Ok(read(xmp)?.props.remove(&key(prop)).map(|(_, v)| v))
}

/// `xmp` with each of `props` set to `value`.
pub fn set(xmp: &str, props: &[Prop], value: &str) -> Result<String, String> {
    let before = read(xmp)?;
    if before.descriptions != 1 {
        return Err(format!(
            "XMP has {} rdf:Description elements, not one",
            before.descriptions
        ));
    }
    let mut out = xmp.to_string();
    for &prop in props {
        out = set_one(&out, &before, prop, value)?;
    }
    let after = read(&out)?;
    for &prop in props {
        if after.props.get(&key(prop)).map(|(_, v)| v.as_str()) != Some(value) {
            return Err(format!("{}:{} did not read back", prop.prefix, prop.local));
        }
    }
    let others = |m: &Model| {
        m.props
            .iter()
            .filter(|(k, _)| !props.iter().any(|&p| key(p) == **k))
            .map(|(k, (_, v))| (k.clone(), v.clone()))
            .collect::<Vec<_>>()
    };
    if others(&before) != others(&after) {
        return Err("setting the dates changed another property".into());
    }
    Ok(out)
}

fn set_one(xmp: &str, model: &Model, prop: Prop, value: &str) -> Result<String, String> {
    let prefix = model.prefixes.get(prop.ns).map(String::as_str);
    match model.props.get(&key(prop)) {
        Some((Form::Attribute, _)) => {
            let prefix = prefix.ok_or("an attribute in an undeclared namespace")?;
            replace_attribute(xmp, &format!("{prefix}:{}", prop.local), value)
        }
        Some((Form::Element, _)) => {
            let prefix = prefix.ok_or("an element in an undeclared namespace")?;
            replace_element(xmp, &format!("{prefix}:{}", prop.local), value)
        }
        None => {
            let mut insert = String::new();
            let prefix = match prefix {
                Some(p) => p,
                None => {
                    if model.prefixes.values().any(|p| p == prop.prefix) {
                        return Err(format!(
                            "XMP binds the prefix `{}` to another namespace",
                            prop.prefix
                        ));
                    }
                    insert.push_str(&format!("\n    xmlns:{}=\"{}\"", prop.prefix, prop.ns));
                    prop.prefix
                }
            };
            insert.push_str(&format!("\n   {prefix}:{}=\"{value}\"", prop.local));
            let open = format!("<{}", model.description_tag);
            let at = find_tag(xmp, &open).ok_or("no rdf:Description start tag")? + open.len();
            let mut out = xmp.to_string();
            out.insert_str(at, &insert);
            Ok(out)
        }
    }
}

/// The start of `<name` followed by whitespace, `>` or `/`.
fn find_tag(xmp: &str, open: &str) -> Option<usize> {
    xmp.match_indices(open).map(|(i, _)| i).find(|&i| {
        xmp[i + open.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_whitespace() || c == '>' || c == '/')
    })
}

fn replace_attribute(xmp: &str, qname: &str, value: &str) -> Result<String, String> {
    let pattern = format!("{qname}=");
    for (i, _) in xmp.match_indices(&pattern) {
        let after_space = xmp[..i]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let rest = &xmp[i + pattern.len()..];
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        if !after_space {
            continue;
        }
        let start = i + pattern.len() + 1;
        let end = start
            + xmp[start..]
                .find(quote)
                .ok_or("an attribute value is never closed")?;
        return Ok(format!("{}{value}{}", &xmp[..start], &xmp[end..]));
    }
    Err(format!("no `{qname}` attribute to replace"))
}

fn replace_element(xmp: &str, qname: &str, value: &str) -> Result<String, String> {
    let open = format!("<{qname}>");
    let close = format!("</{qname}>");
    let start = xmp.find(&open).ok_or(format!("no `{open}` to replace"))? + open.len();
    let end = start + xmp[start..].find(&close).ok_or(format!("no `{close}`"))?;
    Ok(format!("{}{value}{}", &xmp[..start], &xmp[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: [Prop; 2] = [DATE_TIME_ORIGINAL, DATE_CREATED];
    const T: &str = "2024-07-01T14:00:00+02:00";

    fn wrap(description: &str) -> String {
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n <rdf:RDF xmlns:rdf=\"{RDF}\">\n  {description}\n </rdf:RDF>\n</x:xmpmeta>\n"
        )
    }

    /// The shape Lightroom writes for a JPEG with EXIF dates.
    fn with_dates() -> String {
        wrap(concat!(
            "<rdf:Description rdf:about=\"\"\n",
            "    xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n",
            "    xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n",
            "    xmlns:photoshop=\"http://ns.adobe.com/photoshop/1.0/\"\n",
            "   exif:DateTimeOriginal=\"2024-07-01T12:00:00\"\n",
            "   xmp:ModifyDate=\"2024-07-01T12:00:00\"\n",
            "   photoshop:DateCreated=\"2024-07-01T12:00:00\">\n",
            "   <dc:title xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><rdf:Alt><rdf:li xml:lang=\"x-default\">t</rdf:li></rdf:Alt></dc:title>\n",
            "  </rdf:Description>"
        ))
    }

    /// F10's shape: a dateless JPEG, `exif` declared, `photoshop` not.
    fn dateless() -> String {
        wrap(concat!(
            "<rdf:Description rdf:about=\"\"\n",
            "    xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n",
            "   exif:PixelXDimension=\"320\"/>"
        ))
    }

    #[test]
    fn replaces_attributes_and_leaves_the_rest_byte_for_byte() {
        let before = with_dates();
        let after = set(&before, &BOTH, T).unwrap();
        assert_eq!(get(&after, DATE_TIME_ORIGINAL).unwrap().as_deref(), Some(T));
        assert_eq!(get(&after, DATE_CREATED).unwrap().as_deref(), Some(T));
        assert_eq!(
            after,
            before
                .replace(
                    "exif:DateTimeOriginal=\"2024-07-01T12:00:00\"",
                    &format!("exif:DateTimeOriginal=\"{T}\"")
                )
                .replace(
                    "photoshop:DateCreated=\"2024-07-01T12:00:00\"",
                    &format!("photoshop:DateCreated=\"{T}\"")
                )
        );
    }

    /// Round 5's probe added `photoshop:` attributes without declaring the
    /// namespace, and Lightroom then read none of F10's XMP.
    #[test]
    fn declares_a_namespace_the_xmp_lacks() {
        let after = set(&dateless(), &BOTH, T).unwrap();
        assert!(after.contains("xmlns:photoshop=\"http://ns.adobe.com/photoshop/1.0/\""));
        assert_eq!(after.matches("xmlns:exif=").count(), 1);
        assert_eq!(get(&after, DATE_CREATED).unwrap().as_deref(), Some(T));
        assert_eq!(get(&after, DATE_TIME_ORIGINAL).unwrap().as_deref(), Some(T));
    }

    #[test]
    fn a_date_written_as_an_element_is_replaced_there() {
        let before = wrap(concat!(
            "<rdf:Description rdf:about=\"\" xmlns:exif=\"http://ns.adobe.com/exif/1.0/\" xmlns:photoshop=\"http://ns.adobe.com/photoshop/1.0/\">\n",
            "   <exif:DateTimeOriginal>2024-07-01T12:00:00</exif:DateTimeOriginal>\n",
            "  </rdf:Description>"
        ));
        let after = set(&before, &BOTH, T).unwrap();
        assert!(after.contains(&format!(
            "<exif:DateTimeOriginal>{T}</exif:DateTimeOriginal>"
        )));
        assert_eq!(get(&after, DATE_CREATED).unwrap().as_deref(), Some(T));
    }

    #[test]
    fn a_namespace_under_another_prefix_is_used_as_it_is() {
        let before = wrap(
            "<rdf:Description rdf:about=\"\" xmlns:e=\"http://ns.adobe.com/exif/1.0/\" e:DateTimeOriginal=\"x\"/>",
        );
        let after = set(&before, &[DATE_TIME_ORIGINAL], T).unwrap();
        assert!(after.contains(&format!("e:DateTimeOriginal=\"{T}\"")));
        assert!(!after.contains("xmlns:exif"));
    }

    #[test]
    fn refuses_xmp_that_uses_an_undeclared_prefix() {
        let broken = wrap("<rdf:Description rdf:about=\"\" photoshop:DateCreated=\"x\"/>");
        assert!(set(&broken, &BOTH, T)
            .unwrap_err()
            .contains("without declaring"));
    }

    #[test]
    fn refuses_more_than_one_description() {
        let two = wrap("<rdf:Description rdf:about=\"\"/>\n  <rdf:Description rdf:about=\"\"/>");
        assert!(set(&two, &BOTH, T).unwrap_err().contains("not one"));
    }

    #[test]
    fn refuses_a_prefix_bound_elsewhere() {
        let before = wrap("<rdf:Description rdf:about=\"\" xmlns:exif=\"urn:other\"/>");
        assert!(set(&before, &[DATE_TIME_ORIGINAL], T)
            .unwrap_err()
            .contains("another namespace"));
    }

    #[test]
    fn the_blob_round_trips() {
        let text = with_dates();
        let blob = pack(&text);
        assert_eq!(&blob[..4], &(text.len() as u32).to_be_bytes());
        assert_eq!(unpack(&blob).unwrap(), text);
        assert!(unpack(&blob[..3]).is_err());
        let mut wrong = blob.clone();
        wrong[3] ^= 1;
        assert!(unpack(&wrong).unwrap_err().contains("inflates to"));
    }
}
