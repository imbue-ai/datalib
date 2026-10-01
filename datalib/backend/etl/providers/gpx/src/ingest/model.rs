//! A GPX document split into the rows the store keeps, and written back
//! from them. Pure: no I/O, no database. `INGEST.md` §"Tables" says what
//! each row holds and why.

use anyhow::{bail, Result};

use super::tree::{self, Doc, Element, Layout, Node};

/// A `wpt`, `rtept` or `trkpt`: GPX's `wptType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointRow {
    /// [`point_key`] of everything below.
    pub id: String,
    pub lat: Option<String>,
    pub lon: Option<String>,
    pub ele: Option<String>,
    pub time: Option<String>,
    /// `time` as unix milliseconds, when it parses. Derived, so not part
    /// of the key's hash.
    pub time_ms: Option<i64>,
    /// The attributes as written, when they are anything but
    /// `lat="…" lon="…"` in that order.
    pub attrs_xml: Option<String>,
    /// Every child but the leading `ele` and `time`, compact.
    pub rest_xml: Option<String>,
    pub self_closing: bool,
}

/// A `trk`, `trkseg` or `rte`: everything about it but its members.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContainerRow {
    pub attrs_xml: Option<String>,
    /// Children before the first member, compact.
    pub head_xml: Option<String>,
    /// Children after the first member that are not members, compact.
    /// Empty in a file that follows the schema.
    pub tail_xml: Option<String>,
    pub self_closing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub row: ContainerRow,
    pub points: Vec<PointRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub row: ContainerRow,
    pub points: Vec<PointRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub row: ContainerRow,
    pub segments: Vec<Segment>,
}

/// One file's rows, in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpxFile {
    pub version: Option<String>,
    pub creator: Option<String>,
    pub prolog: String,
    pub epilog: String,
    pub layout: Layout,
    /// The root's children before the first `wpt`, `rte` or `trk`:
    /// `metadata`, in a file that follows the schema.
    pub head_xml: Option<String>,
    /// The root's other children after that: its `extensions`.
    pub tail_xml: Option<String>,
    pub waypoints: Vec<PointRow>,
    pub routes: Vec<Route>,
    pub tracks: Vec<Track>,
}

impl GpxFile {
    /// Every point id the file names, of all three kinds.
    pub fn point_ids(&self) -> std::collections::HashSet<String> {
        let tracks = self
            .tracks
            .iter()
            .flat_map(|t| &t.segments)
            .flat_map(|s| &s.points);
        let routes = self.routes.iter().flat_map(|r| &r.points);
        self.waypoints
            .iter()
            .chain(routes)
            .chain(tracks)
            .map(|p| p.id.clone())
            .collect()
    }
}

/// How a list of points is ordered in the store. The ordinal is part of
/// a member row's key, so it is chosen to survive the edits people make:
/// deleting a stray point from a recorded track moves nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum PointOrder {
    /// Every point has a time and the times strictly increase, so a
    /// point's ordinal is its time in unix milliseconds.
    Time,
    /// The ordinal is the point's position in the list.
    Position,
}

impl PointOrder {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// The order a list takes, and each point's ordinal under it.
    pub fn of(points: &[PointRow]) -> (Self, Vec<i64>) {
        let times: Option<Vec<i64>> = points.iter().map(|p| p.time_ms).collect();
        match times {
            Some(t) if !t.is_empty() && t.windows(2).all(|w| w[0] < w[1]) => (Self::Time, t),
            _ => (Self::Position, (0..points.len() as i64).collect()),
        }
    }
}

/// How faithfully the store holds a file, measured at ingest by writing
/// it back from its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum Fidelity {
    /// Byte for byte.
    Exact,
    /// The same elements, attributes and text; only what XML says does
    /// not matter differs: whitespace between elements, attribute order
    /// and quoting, comments.
    Equivalent,
    /// Something was moved or lost. Reported as a problem.
    Lossy,
}

impl Fidelity {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    pub fn measure(original: &str, rebuilt: &str) -> Result<Self> {
        if original == rebuilt {
            return Ok(Self::Exact);
        }
        let a = tree::parse(original)?;
        let b = tree::parse(rebuilt)?;
        Ok(if tree::canonical(&a.root) == tree::canonical(&b.root) {
            Self::Equivalent
        } else {
            Self::Lossy
        })
    }
}

pub fn split(doc: &Doc) -> Result<GpxFile> {
    if doc.root.name != "gpx" {
        bail!("the root element is <{}>, not <gpx>", doc.root.name);
    }
    let parts = Parts::of(&doc.root.children, &["wpt", "rte", "trk"]);
    let mut file = GpxFile {
        version: doc.root.attr("version").map(unescape),
        creator: doc.root.attr("creator").map(unescape),
        prolog: doc.prolog.clone(),
        epilog: doc.epilog.clone(),
        layout: doc.layout.clone(),
        head_xml: compact(&parts.head),
        tail_xml: compact(&parts.tail),
        waypoints: Vec::new(),
        routes: Vec::new(),
        tracks: Vec::new(),
    };
    for m in parts.members {
        match m.name.as_str() {
            "wpt" => file.waypoints.push(point_row(m)),
            "rte" => {
                let p = Parts::of(&m.children, &["rtept"]);
                file.routes.push(Route {
                    row: p.container_row(m),
                    points: p.members.into_iter().map(point_row).collect(),
                });
            }
            _ => {
                let p = Parts::of(&m.children, &["trkseg"]);
                let segments = p
                    .members
                    .iter()
                    .map(|s| {
                        let sp = Parts::of(&s.children, &["trkpt"]);
                        Segment {
                            row: sp.container_row(s),
                            points: sp.members.into_iter().map(point_row).collect(),
                        }
                    })
                    .collect();
                file.tracks.push(Track {
                    row: p.container_row(m),
                    segments,
                });
            }
        }
    }
    Ok(file)
}

/// The file the rows describe. A file whose members were interleaved
/// with other children, or whose `wpt`s, `rte`s and `trk`s were not in
/// that order, comes back in schema order; [`Fidelity`] says so.
pub fn rebuild(file: &GpxFile) -> Result<String> {
    let mut out = file.prolog.clone();
    if !file.prolog.ends_with("/>") {
        let mut children = fragment(&file.head_xml)?;
        for w in &file.waypoints {
            children.push(Node::Element(point_element("wpt", w)?));
        }
        for r in &file.routes {
            let pts = r
                .points
                .iter()
                .map(|p| point_element("rtept", p))
                .collect::<Result<_>>()?;
            children.push(Node::Element(container_element("rte", &r.row, pts)?));
        }
        for t in &file.tracks {
            let mut segs = Vec::with_capacity(t.segments.len());
            for s in &t.segments {
                let pts = s
                    .points
                    .iter()
                    .map(|p| point_element("trkpt", p))
                    .collect::<Result<_>>()?;
                segs.push(container_element("trkseg", &s.row, pts)?);
            }
            children.push(Node::Element(container_element("trk", &t.row, segs)?));
        }
        children.extend(fragment(&file.tail_xml)?);
        let root = Element {
            children,
            ..Element::default()
        };
        tree::write_body(&mut out, &root, "gpx", &file.layout);
    }
    out.push_str(&file.epilog);
    Ok(out)
}

/// A content key that sorts by time: the point's time in the leading 48
/// bits, then a hash of everything the row holds. Two files holding the
/// same point hold the same row.
pub fn point_key(p: &PointRow) -> String {
    let mut h = blake3::Hasher::new();
    for part in [&p.lat, &p.lon, &p.ele, &p.time, &p.attrs_xml, &p.rest_xml] {
        match part {
            Some(s) => {
                h.update(b"1");
                h.update(s.as_bytes());
            }
            None => {
                h.update(b"0");
            }
        }
        h.update(b"\x1f");
    }
    h.update(if p.self_closing { b"1" } else { b"0" });
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&h.finalize().as_bytes()[..16]);
    datalib_id::stamped_hash(bytes, p.time_ms)
        .hyphenated()
        .to_string()
}

/// A container's children: the non-members before the first member, the
/// members, and every other child after it.
struct Parts<'a> {
    head: Vec<Node>,
    members: Vec<&'a Element>,
    tail: Vec<Node>,
}

impl<'a> Parts<'a> {
    fn of(children: &'a [Node], member_names: &[&str]) -> Self {
        let mut p = Parts {
            head: Vec::new(),
            members: Vec::new(),
            tail: Vec::new(),
        };
        for c in children {
            match c {
                Node::Element(e) if member_names.contains(&e.name.as_str()) => p.members.push(e),
                other if p.members.is_empty() => p.head.push(other.clone()),
                other => p.tail.push(other.clone()),
            }
        }
        // Rebuilt in the order the names are given, as the schema says.
        p.members.sort_by_key(|e| {
            member_names
                .iter()
                .position(|n| *n == e.name)
                .unwrap_or(usize::MAX)
        });
        p
    }

    fn container_row(&self, el: &Element) -> ContainerRow {
        ContainerRow {
            attrs_xml: attrs_xml(&el.attrs),
            head_xml: compact(&self.head),
            tail_xml: compact(&self.tail),
            self_closing: el.self_closing,
        }
    }
}

fn point_row(el: &Element) -> PointRow {
    let standard_attrs = el.attrs.len() == 2 && el.attrs[0].0 == "lat" && el.attrs[1].0 == "lon";
    let mut kids: &[Node] = &el.children;
    let mut ele = None;
    let mut time = None;
    if el.is_element_only() {
        if let Some(t) = liftable(kids, "ele") {
            ele = Some(t);
            kids = &kids[1..];
        }
        if let Some(t) = liftable(kids, "time") {
            time = Some(t);
            kids = &kids[1..];
        }
    }
    let time_ms = time.as_deref().and_then(time_ms);
    let mut row = PointRow {
        id: String::new(),
        lat: el.attr("lat").map(str::to_string),
        lon: el.attr("lon").map(str::to_string),
        ele,
        time,
        time_ms,
        attrs_xml: if standard_attrs {
            None
        } else {
            Some(attrs_string(&el.attrs))
        },
        rest_xml: compact(kids),
        self_closing: el.self_closing,
    };
    row.id = point_key(&row);
    row
}

/// GPX times are UTC by definition, and some writers leave off the `Z`.
fn time_ms(t: &str) -> Option<i64> {
    datalib_time::record_stamp_ms(t).or_else(|| datalib_time::record_stamp_ms(&format!("{t}Z")))
}

/// The text of `kids[0]` when it is `<name>text</name>` and nothing
/// more: no attributes, no CDATA, no references.
fn liftable(kids: &[Node], name: &str) -> Option<String> {
    let Some(Node::Element(e)) = kids.first() else {
        return None;
    };
    if e.name != name || !e.attrs.is_empty() || e.self_closing {
        return None;
    }
    match e.children.as_slice() {
        [Node::Text(t)] if !t.contains('&') => Some(t.clone()),
        _ => None,
    }
}

fn point_element(name: &str, p: &PointRow) -> Result<Element> {
    let attrs = match &p.attrs_xml {
        Some(a) => tree::parse_attrs(a)?,
        None => vec![
            ("lat".to_string(), p.lat.clone().unwrap_or_default()),
            ("lon".to_string(), p.lon.clone().unwrap_or_default()),
        ],
    };
    let mut children = Vec::new();
    for (tag, value) in [("ele", &p.ele), ("time", &p.time)] {
        if let Some(v) = value {
            children.push(Node::Element(Element {
                name: tag.to_string(),
                attrs: Vec::new(),
                children: vec![Node::Text(v.clone())],
                self_closing: false,
            }));
        }
    }
    children.extend(fragment(&p.rest_xml)?);
    Ok(Element {
        name: name.to_string(),
        attrs,
        children,
        self_closing: p.self_closing,
    })
}

fn container_element(name: &str, row: &ContainerRow, members: Vec<Element>) -> Result<Element> {
    let mut children = fragment(&row.head_xml)?;
    children.extend(members.into_iter().map(Node::Element));
    children.extend(fragment(&row.tail_xml)?);
    Ok(Element {
        name: name.to_string(),
        attrs: match &row.attrs_xml {
            Some(a) => tree::parse_attrs(a)?,
            None => Vec::new(),
        },
        children,
        self_closing: row.self_closing,
    })
}

fn compact(nodes: &[Node]) -> Option<String> {
    (!nodes.is_empty()).then(|| tree::write_compact(nodes))
}

fn fragment(xml: &Option<String>) -> Result<Vec<Node>> {
    match xml {
        Some(x) => tree::parse_fragment(x),
        None => Ok(Vec::new()),
    }
}

fn attrs_string(attrs: &[(String, String)]) -> String {
    let mut s = String::new();
    tree::write_attrs(&mut s, attrs);
    s
}

fn attrs_xml(attrs: &[(String, String)]) -> Option<String> {
    (!attrs.is_empty()).then(|| attrs_string(attrs))
}

fn unescape(raw: &str) -> String {
    quick_xml::escape::unescape(raw)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDENTED: &str = r#"<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>
<gpx xmlns="http://www.topografix.com/GPX/1/1" xmlns:tng="urn:tng" version="1.1" creator="Tricorder">
  <metadata>
    <name>Away mission</name>
  </metadata>
  <wpt lat="46.1" lon="7.2">
    <ele>1200</ele>
    <time>2364-04-13T08:00:00Z</time>
    <name>Beam-down site</name>
  </wpt>
  <trk>
    <name>Survey</name>
    <trkseg>
      <trkpt lat="46.1" lon="7.2">
        <ele>1200.5</ele>
        <time>2364-04-13T08:00:01Z</time>
        <extensions>
          <tng:meta s="0.4" />
        </extensions>
      </trkpt>
      <trkpt lat="46.11" lon="7.21">
        <ele>1201</ele>
        <time>2364-04-13T08:00:05Z</time>
      </trkpt>
    </trkseg>
  </trk>
</gpx>
"#;

    fn ingest(src: &str) -> GpxFile {
        split(&tree::parse(src).unwrap()).unwrap()
    }

    #[test]
    fn an_indented_file_comes_back_byte_for_byte() {
        let f = ingest(INDENTED);
        assert_eq!(f.waypoints.len(), 1);
        assert_eq!(f.tracks[0].segments[0].points.len(), 2);
        assert_eq!(f.creator.as_deref(), Some("Tricorder"));
        let out = rebuild(&f).unwrap();
        assert_eq!(out, INDENTED);
        assert_eq!(Fidelity::measure(INDENTED, &out).unwrap(), Fidelity::Exact);
    }

    #[test]
    fn time_and_position_are_lifted_and_the_rest_kept_whole() {
        let f = ingest(INDENTED);
        let p = &f.tracks[0].segments[0].points[0];
        assert_eq!(p.lat.as_deref(), Some("46.1"));
        assert_eq!(p.ele.as_deref(), Some("1200.5"));
        assert_eq!(p.time.as_deref(), Some("2364-04-13T08:00:01Z"));
        assert_eq!(
            p.rest_xml.as_deref(),
            Some(r#"<extensions><tng:meta s="0.4"/></extensions>"#)
        );
        assert_eq!(p.attrs_xml, None);
        assert_eq!(
            f.head_xml.as_deref(),
            Some("<metadata><name>Away mission</name></metadata>")
        );
    }

    #[test]
    fn the_same_point_at_another_indent_is_the_same_row() {
        let compact = INDENTED
            .lines()
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("");
        let a = ingest(INDENTED);
        let b = ingest(&compact);
        assert_eq!(
            a.tracks[0].segments[0].points, b.tracks[0].segments[0].points,
            "whitespace between elements is the file's layout, not the point's"
        );
    }

    #[test]
    fn the_key_sorts_by_time_and_moves_with_content() {
        let f = ingest(INDENTED);
        let pts = &f.tracks[0].segments[0].points;
        assert!(pts[0].id < pts[1].id);
        assert_eq!(datalib_id::stamp_of(&pts[0].id), pts[0].time_ms);
        let edited = ingest(&INDENTED.replace("<ele>1201</ele>", "<ele>1202</ele>"));
        let e = &edited.tracks[0].segments[0].points;
        assert_eq!(e[0].id, pts[0].id);
        assert_ne!(e[1].id, pts[1].id);
        assert_eq!(
            e[1].id[..13],
            pts[1].id[..13],
            "same time, so the same prefix"
        );
    }

    #[test]
    fn strictly_increasing_times_order_by_time_else_by_position() {
        let f = ingest(INDENTED);
        let (order, ords) = PointOrder::of(&f.tracks[0].segments[0].points);
        assert_eq!(order, PointOrder::Time);
        assert_eq!(ords[0], f.tracks[0].segments[0].points[0].time_ms.unwrap());
        let same = ingest(&INDENTED.replace("08:00:05Z", "08:00:01Z"));
        assert_eq!(
            PointOrder::of(&same.tracks[0].segments[0].points),
            (PointOrder::Position, vec![0, 1])
        );
    }

    #[test]
    fn odd_attributes_and_out_of_order_children_survive() {
        let src = "<gpx>\n<wpt lon=\"7\" lat=\"46\" sym=\"x\">\n<name>n</name>\n<time>2364-04-13T08:00:00Z</time>\n</wpt>\n</gpx>";
        let f = ingest(src);
        let w = &f.waypoints[0];
        assert_eq!(w.lat.as_deref(), Some("46"));
        assert_eq!(w.time, None, "not leading, so not lifted");
        assert!(w.attrs_xml.is_some());
        assert_eq!(rebuild(&f).unwrap(), src);
    }

    #[test]
    fn interleaved_members_are_reported_lossy() {
        let src = "<gpx>\n<trk>\n<trkseg/>\n</trk>\n<wpt lat=\"1\" lon=\"2\"/>\n</gpx>";
        let f = ingest(src);
        let out = rebuild(&f).unwrap();
        assert_eq!(Fidelity::measure(src, &out).unwrap(), Fidelity::Lossy);
    }

    #[test]
    fn requoted_attributes_are_equivalent() {
        let src = "<gpx>\n<wpt lat='1' lon='2'/>\n</gpx>";
        let f = ingest(src);
        let out = rebuild(&f).unwrap();
        assert_eq!(Fidelity::measure(src, &out).unwrap(), Fidelity::Equivalent);
    }

    #[test]
    fn a_kml_file_is_refused() {
        assert!(split(&tree::parse("<kml><Document/></kml>").unwrap()).is_err());
    }

    #[test]
    fn strum_spellings_are_the_stored_ones() {
        for o in [PointOrder::Time, PointOrder::Position] {
            assert_eq!(PointOrder::parse(o.as_str()), Some(o));
        }
        for f in [Fidelity::Exact, Fidelity::Equivalent, Fidelity::Lossy] {
            assert_eq!(Fidelity::parse(f.as_str()), Some(f));
        }
    }
}
