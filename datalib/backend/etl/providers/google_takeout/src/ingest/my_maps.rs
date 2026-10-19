//! `My Maps/<map>.kmz`: one zipped KML per map, a row per placemark (a
//! pin, a line, a shape) with its layer and its map.
//!
//! Each element is kept as JSON — attributes as `@name`, text as
//! `#text`, a child repeated as a list — so render can say which it does
//! not read. Styles (icons, colours, line widths) are how the map looks,
//! and are left out.

use std::io::Read as _;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::fsscan;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::{json, Map, Value};

use super::db::RawDb;
use super::per_file::{self, Counts, Feed, FromFile, Occurrences};
use super::schema_raw::{ns_id, MyMapsPlacemarkRow};

pub const FEED: Feed = Feed {
    name: "my_maps",
    dir: "My Maps",
    reads: |rel| rel.to_ascii_lowercase().ends_with(".kmz"),
};

impl FromFile for MyMapsPlacemarkRow {
    fn file(&self) -> &str {
        &self.file
    }
}

/// How a map looks rather than what it says.
const STYLE_TAGS: &[&str] = &["Style", "StyleMap"];

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<Counts> {
    let n = per_file::ingest(db, scan, &FEED, found, |f, bytes| parse(&f.rel, bytes)).await?;
    progress.set_message(&format!("my_maps: {}", n.written));
    Ok(n)
}

pub fn parse(rel: &str, kmz: &[u8]) -> Result<Vec<MyMapsPlacemarkRow>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(kmz)).context("open the .kmz")?;
    let kml_name = (0..zip.len())
        .filter_map(|i| {
            zip.name_for_index(i)
                .and_then(|n| n.ok())
                .map(|n| n.into_owned())
        })
        .find(|n| n.to_ascii_lowercase().ends_with(".kml"))
        .ok_or_else(|| super::unknown_layout(&super::shape::shape_rel(rel), "holds no .kml"))?;
    let mut kml = String::new();
    zip.by_name(&kml_name)
        .context("read the map's .kml")?
        .read_to_string(&mut kml)
        .context("read the map's .kml as text")?;
    let root = xml_to_json(&kml).context("parse the map's KML")?;
    let document = root
        .get("kml")
        .and_then(|k| k.get("Document"))
        .ok_or_else(|| super::unknown_layout(&super::shape::shape_rel(rel), "has no Document"))?;
    let map = without(document, &["Folder", "Placemark"]);
    let mut rows = Vec::new();
    let mut seen = Occurrences::default();
    let mut position = 0usize;
    collect(
        rel,
        document,
        &map,
        None,
        &mut seen,
        &mut position,
        &mut rows,
    );
    Ok(rows)
}

/// The placemarks of `node` and of the folders under it, each with the
/// folder holding it.
fn collect(
    rel: &str,
    node: &Value,
    map: &Value,
    folder: Option<&Value>,
    seen: &mut Occurrences,
    position: &mut usize,
    rows: &mut Vec<MyMapsPlacemarkRow>,
) {
    for placemark in children(node, "Placemark") {
        let name = |v: Option<&Value>| {
            v.and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        let geometry = ["Point", "LineString", "Polygon", "MultiGeometry"]
            .iter()
            .find_map(|g| placemark.get(*g))
            .map(Value::to_string)
            .unwrap_or_default();
        let natural = format!(
            "{}\u{1f}{}\u{1f}{}",
            name(folder),
            name(Some(placemark)),
            blake3::hash(geometry.as_bytes()).to_hex()
        );
        let n = seen.next(&natural);
        let payload = json!({
            "map": map,
            "folder": folder.map(|f| without(f, &["Placemark", "Folder"])),
            "placemark": placemark,
            "position": *position,
        });
        *position += 1;
        rows.push(MyMapsPlacemarkRow {
            id_and_payload: WirePayload {
                id: ns_id(&format!("my_maps:{rel}:{natural}:{n}")),
                payload: payload.to_string(),
            },
            file: rel.to_string(),
        });
    }
    for sub in children(node, "Folder") {
        collect(rel, sub, map, Some(sub), seen, position, rows);
    }
}

/// A child that is one element is a value, two or more a list.
fn children<'a>(node: &'a Value, tag: &str) -> Vec<&'a Value> {
    match node.get(tag) {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(v) => vec![v],
        None => Vec::new(),
    }
}

fn without(node: &Value, tags: &[&str]) -> Value {
    match node.as_object() {
        Some(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| !tags.contains(&k.as_str()) && !STYLE_TAGS.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        None => node.clone(),
    }
}

struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<Node>,
}

impl Node {
    fn into_json(self) -> Value {
        let text = self.text.trim().to_string();
        if self.attrs.is_empty() && self.children.is_empty() {
            return Value::String(text);
        }
        let mut m = Map::new();
        for (k, v) in self.attrs {
            m.insert(format!("@{k}"), Value::String(v));
        }
        if !text.is_empty() {
            m.insert("#text".to_string(), Value::String(text));
        }
        for c in self.children {
            let tag = c.tag.clone();
            let v = c.into_json();
            match m.remove(&tag) {
                None => {
                    m.insert(tag, v);
                }
                Some(Value::Array(mut a)) => {
                    a.push(v);
                    m.insert(tag, Value::Array(a));
                }
                Some(prev) => {
                    m.insert(tag, Value::Array(vec![prev, v]));
                }
            }
        }
        Value::Object(m)
    }
}

/// XML as JSON: `{"kml": {"Document": {...}}}`. Namespace prefixes are
/// kept (`gx:media_links`), the default namespace dropped.
pub fn xml_to_json(xml: &str) -> Result<Value> {
    let mut r = Reader::from_str(xml);
    r.config_mut().check_end_names = false;
    let mut stack: Vec<Node> = vec![Node {
        tag: String::new(),
        attrs: Vec::new(),
        text: String::new(),
        children: Vec::new(),
    }];
    let mut buf = Vec::new();
    let open = |e: &quick_xml::events::BytesStart| Node {
        tag: e.name().as_ref().to_string(),
        attrs: e
            .attributes()
            .flatten()
            .filter(|a| !a.key.as_ref().starts_with("xmlns"))
            .map(|a| {
                (
                    a.key.as_ref().to_string(),
                    datalib_etl::xml::attr_value(&a)
                        .map(|c| c.into_owned())
                        .unwrap_or_default(),
                )
            })
            .collect(),
        text: String::new(),
        children: Vec::new(),
    };
    loop {
        match r.read_event_into(&mut buf).context("read the XML")? {
            Event::Start(e) => stack.push(open(&e)),
            Event::Empty(e) => {
                let node = open(&e);
                if let Some(top) = stack.last_mut() {
                    top.children.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() > 1 {
                    let node = stack.pop().expect("more than the root");
                    if let Some(top) = stack.last_mut() {
                        top.children.push(node);
                    }
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t);
                }
            }
            Event::GeneralRef(g) => {
                if let Some(top) = stack.last_mut() {
                    top.text
                        .push_str(&datalib_etl::xml::reference_text(&g, true));
                }
            }
            Event::CData(c) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    while stack.len() > 1 {
        let node = stack.pop().expect("more than the root");
        if let Some(top) = stack.last_mut() {
            top.children.push(node);
        }
    }
    let root = stack.pop().expect("the root");
    let mut m = Map::new();
    for c in root.children {
        m.insert(c.tag.clone(), c.into_json());
    }
    Ok(Value::Object(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    pub fn kmz(kml: &str) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut out);
        z.start_file("doc.kml", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.write_all(kml.as_bytes()).unwrap();
        z.finish().unwrap();
        out.into_inner()
    }

    const KML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2"><Document><name>Away missions</name>
<description/><Style id="icon-1"><IconStyle><scale>1</scale></IconStyle></Style>
<Folder><name>Shore leave</name>
<Placemark><name>Risa</name><description><![CDATA[Horga'hn optional.<br>Bring sunscreen.]]></description>
<styleUrl>#icon-1</styleUrl><Point><coordinates>-122.4,37.8,0</coordinates></Point></Placemark>
<Placemark><name>Risa</name><styleUrl>#icon-1</styleUrl><Point><coordinates>-122.4,37.8,0</coordinates></Point></Placemark>
</Folder>
<Folder><name>Patrol</name><Placemark><name>Neutral Zone</name><LineString><tessellate>1</tessellate><coordinates>1,2,0 3,4,0</coordinates></LineString>
<ExtendedData><Data name="gx_media_links"><value>https://memory-alpha.example/nz.jpg</value></Data></ExtendedData></Placemark></Folder>
</Document></kml>"#;

    #[test]
    fn every_placemark_with_its_layer_and_map() {
        let rows = parse("My Maps/Away missions.kmz", &kmz(KML)).unwrap();
        let p: Vec<Value> = rows
            .iter()
            .map(|r| serde_json::from_str(&r.id_and_payload.payload).unwrap())
            .collect();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0]["map"]["name"], "Away missions");
        assert!(p[0]["map"].get("Style").is_none(), "styles are left out");
        assert_eq!(p[0]["folder"]["name"], "Shore leave");
        assert_eq!(
            p[0]["placemark"]["description"],
            "Horga'hn optional.<br>Bring sunscreen."
        );
        assert_eq!(
            p[2]["placemark"]["ExtendedData"]["Data"]["@name"],
            "gx_media_links"
        );
        assert_ne!(
            rows[0].id_and_payload.id, rows[1].id_and_payload.id,
            "the same pin twice keeps two rows"
        );
    }

    #[test]
    fn a_kmz_with_no_kml_is_not_a_map() {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut z = zip::ZipWriter::new(&mut out);
        z.start_file("images/icon.png", zip::write::SimpleFileOptions::default())
            .unwrap();
        z.finish().unwrap();
        assert!(parse("My Maps/x.kmz", &out.into_inner()).is_err());
    }
}
