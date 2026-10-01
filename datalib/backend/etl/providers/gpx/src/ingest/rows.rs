//! A [`GpxFile`] as table rows and back, and the smallest set of writes
//! that turns one file's stored rows into another's. Pure.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{anyhow, Context, Result};

use super::model::{ContainerRow, Fidelity, GpxFile, PointOrder, PointRow, Route, Segment, Track};
use super::schema_raw::{
    Table, FILE_WPTS, RTEPTS, RTES, RTE_RTEPTS, TRKPTS, TRKS, TRKSEGS, TRKSEG_TRKPTS, WPTS,
};
use super::tree::Layout;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
}

pub type Row = Vec<Value>;

fn text(s: &Option<String>) -> Value {
    s.as_ref().map_or(Value::Null, |s| Value::Text(s.clone()))
}

fn int(b: bool) -> Value {
    Value::Int(b as i64)
}

/// One file's rows: its own, those keyed under its `file_key`, and the
/// shared points it refers to.
#[derive(Debug, Clone)]
pub struct FileRows {
    pub file: Row,
    pub per_file: Vec<(Table, Vec<Row>)>,
    pub points: Vec<(Table, Vec<Row>)>,
}

pub struct FileFacts<'a> {
    pub path: &'a str,
    /// What every per-file row is keyed under; see [`super::rename`].
    pub file_key: &'a str,
    pub blake3: &'a str,
    pub size: i64,
    pub fidelity: Fidelity,
}

pub fn to_rows(facts: &FileFacts<'_>, f: &GpxFile) -> Result<FileRows> {
    let k = || Value::Text(facts.file_key.to_string());
    let (wpt_order, wpt_ords) = PointOrder::of(&f.waypoints);
    let file = vec![
        Value::Text(facts.path.to_string()),
        k(),
        Value::Text(facts.blake3.to_string()),
        Value::Int(facts.size),
        text(&f.version),
        text(&f.creator),
        Value::Text(f.prolog.clone()),
        Value::Text(f.epilog.clone()),
        Value::Text(serde_json::to_string(&f.layout).context("layout as JSON")?),
        text(&f.head_xml),
        text(&f.tail_xml),
        Value::Text(wpt_order.as_str().to_string()),
        Value::Text(facts.fidelity.as_str().to_string()),
    ];

    let mut points: BTreeMap<&'static str, (Table, BTreeMap<String, Row>)> = [WPTS, RTEPTS, TRKPTS]
        .into_iter()
        .map(|t| (t.name, (t, BTreeMap::new())))
        .collect();
    let mut add_point = |t: Table, p: &PointRow| {
        let rows = &mut points.get_mut(t.name).expect("a point table").1;
        rows.entry(p.id.clone()).or_insert_with(|| point_row(p));
    };

    let mut file_wpts = Vec::new();
    for (p, ord) in f.waypoints.iter().zip(wpt_ords) {
        add_point(WPTS, p);
        file_wpts.push(vec![k(), Value::Int(ord), Value::Text(p.id.clone())]);
    }
    let mut rtes = Vec::new();
    let mut rte_rtepts = Vec::new();
    for (i, r) in f.routes.iter().enumerate() {
        let (order, ords) = PointOrder::of(&r.points);
        rtes.push(container(k(), &[i as i64], &r.row, Some(order)));
        for (p, ord) in r.points.iter().zip(ords) {
            add_point(RTEPTS, p);
            rte_rtepts.push(vec![
                k(),
                Value::Int(i as i64),
                Value::Int(ord),
                Value::Text(p.id.clone()),
            ]);
        }
    }
    let mut trks = Vec::new();
    let mut trksegs = Vec::new();
    let mut trkseg_trkpts = Vec::new();
    for (t, trk) in f.tracks.iter().enumerate() {
        trks.push(container(k(), &[t as i64], &trk.row, None));
        for (s, seg) in trk.segments.iter().enumerate() {
            let (order, ords) = PointOrder::of(&seg.points);
            trksegs.push(container(k(), &[t as i64, s as i64], &seg.row, Some(order)));
            for (p, ord) in seg.points.iter().zip(ords) {
                add_point(TRKPTS, p);
                trkseg_trkpts.push(vec![
                    k(),
                    Value::Int(t as i64),
                    Value::Int(s as i64),
                    Value::Int(ord),
                    Value::Text(p.id.clone()),
                ]);
            }
        }
    }
    Ok(FileRows {
        file,
        per_file: vec![
            (FILE_WPTS, file_wpts),
            (RTES, rtes),
            (RTE_RTEPTS, rte_rtepts),
            (TRKS, trks),
            (TRKSEGS, trksegs),
            (TRKSEG_TRKPTS, trkseg_trkpts),
        ],
        points: points
            .into_values()
            .map(|(t, rows)| (t, rows.into_values().collect()))
            .collect(),
    })
}

fn point_row(p: &PointRow) -> Row {
    vec![
        Value::Text(p.id.clone()),
        text(&p.lat),
        text(&p.lon),
        text(&p.ele),
        text(&p.time),
        p.time_ms.map_or(Value::Null, Value::Int),
        text(&p.attrs_xml),
        text(&p.rest_xml),
        int(p.self_closing),
    ]
}

fn container(file_key: Value, idx: &[i64], c: &ContainerRow, order: Option<PointOrder>) -> Row {
    let mut row = vec![file_key];
    row.extend(idx.iter().map(|i| Value::Int(*i)));
    row.extend([
        text(&c.attrs_xml),
        text(&c.head_xml),
        text(&c.tail_xml),
        int(c.self_closing),
    ]);
    if let Some(o) = order {
        row.push(Value::Text(o.as_str().to_string()));
    }
    row
}

#[derive(Debug, Default, PartialEq)]
pub struct TableDiff {
    pub deletes: Vec<Row>,
    pub upserts: Vec<Row>,
}

/// Rows only in `old` go, rows that are new or changed are written, and
/// a row that is the same in both is not touched.
pub fn diff(table: &Table, old: &[Row], new: &[Row]) -> TableDiff {
    let old_by_key: HashMap<&[Value], &Row> = old.iter().map(|r| (&r[..table.key], r)).collect();
    let new_keys: HashSet<&[Value]> = new.iter().map(|r| &r[..table.key]).collect();
    TableDiff {
        deletes: old
            .iter()
            .filter(|r| !new_keys.contains(&r[..table.key]))
            .map(|r| r[..table.key].to_vec())
            .collect(),
        upserts: new
            .iter()
            .filter(|r| old_by_key.get(&r[..table.key]) != Some(r))
            .cloned()
            .collect(),
    }
}

/// The point ids a file's member rows stopped naming: each may now be
/// named by no file at all.
pub fn dropped_ids(old: &[Row], new: &[Row]) -> Vec<String> {
    let last = |r: &Row| match r.last() {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    };
    let kept: HashSet<String> = new.iter().filter_map(last).collect();
    let mut gone: Vec<String> = old
        .iter()
        .filter_map(last)
        .filter(|id| !kept.contains(id))
        .collect();
    gone.sort();
    gone.dedup();
    gone
}

/// A file read back out of the store. `per_file` holds each per-file
/// table's rows for this file, in key order; `points` every point they
/// name, by id.
pub fn from_rows(
    file: &Row,
    per_file: &HashMap<&str, Vec<Row>>,
    points: &HashMap<String, Row>,
) -> Result<GpxFile> {
    let rows = |t: &Table| per_file.get(t.name).map(Vec::as_slice).unwrap_or(&[]);
    let point = |id: &Value| -> Result<PointRow> {
        let id = as_text(id)?;
        let r = points
            .get(&id)
            .ok_or_else(|| anyhow!("member names point {id}, which the store does not hold"))?;
        Ok(PointRow {
            id,
            lat: opt_text(&r[1])?,
            lon: opt_text(&r[2])?,
            ele: opt_text(&r[3])?,
            time: opt_text(&r[4])?,
            time_ms: opt_int(&r[5])?,
            attrs_xml: opt_text(&r[6])?,
            rest_xml: opt_text(&r[7])?,
            self_closing: as_int(&r[8])? != 0,
        })
    };
    let container = |r: &Row, at: usize| -> Result<ContainerRow> {
        Ok(ContainerRow {
            attrs_xml: opt_text(&r[at])?,
            head_xml: opt_text(&r[at + 1])?,
            tail_xml: opt_text(&r[at + 2])?,
            self_closing: as_int(&r[at + 3])? != 0,
        })
    };

    let waypoints = rows(&FILE_WPTS)
        .iter()
        .map(|r| point(&r[2]))
        .collect::<Result<_>>()?;

    let mut routes: Vec<Route> = rows(&RTES)
        .iter()
        .map(|r| {
            Ok(Route {
                row: container(r, 2)?,
                points: Vec::new(),
            })
        })
        .collect::<Result<_>>()?;
    for r in rows(&RTE_RTEPTS) {
        let i = as_int(&r[1])? as usize;
        routes
            .get_mut(i)
            .ok_or_else(|| anyhow!("a route point names route {i}, which is not stored"))?
            .points
            .push(point(&r[3])?);
    }

    let mut tracks: Vec<Track> = rows(&TRKS)
        .iter()
        .map(|r| {
            Ok(Track {
                row: container(r, 2)?,
                segments: Vec::new(),
            })
        })
        .collect::<Result<_>>()?;
    for r in rows(&TRKSEGS) {
        let t = as_int(&r[1])? as usize;
        tracks
            .get_mut(t)
            .ok_or_else(|| anyhow!("a segment names track {t}, which is not stored"))?
            .segments
            .push(Segment {
                row: container(r, 3)?,
                points: Vec::new(),
            });
    }
    for r in rows(&TRKSEG_TRKPTS) {
        let (t, s) = (as_int(&r[1])? as usize, as_int(&r[2])? as usize);
        tracks
            .get_mut(t)
            .and_then(|trk| trk.segments.get_mut(s))
            .ok_or_else(|| anyhow!("a track point names segment {t}/{s}, which is not stored"))?
            .points
            .push(point(&r[4])?);
    }

    let layout: Layout =
        serde_json::from_str(&as_text(&file[8])?).context("stored layout is not JSON")?;
    Ok(GpxFile {
        version: opt_text(&file[4])?,
        creator: opt_text(&file[5])?,
        prolog: as_text(&file[6])?,
        epilog: as_text(&file[7])?,
        layout,
        head_xml: opt_text(&file[9])?,
        tail_xml: opt_text(&file[10])?,
        waypoints,
        routes,
        tracks,
    })
}

fn as_text(v: &Value) -> Result<String> {
    match v {
        Value::Text(s) => Ok(s.clone()),
        other => Err(anyhow!("expected text, found {other:?}")),
    }
}

fn opt_text(v: &Value) -> Result<Option<String>> {
    match v {
        Value::Null => Ok(None),
        other => as_text(other).map(Some),
    }
}

fn as_int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        other => Err(anyhow!("expected an integer, found {other:?}")),
    }
}

fn opt_int(v: &Value) -> Result<Option<i64>> {
    match v {
        Value::Null => Ok(None),
        other => as_int(other).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::{model, tree};

    const SRC: &str = "<gpx version=\"1.1\">\n<wpt lat=\"1\" lon=\"2\"><name>Bridge</name></wpt>\n\
        <rte>\n<rtept lat=\"1\" lon=\"2\"/>\n<rtept lat=\"3\" lon=\"4\"/>\n</rte>\n\
        <trk>\n<name>t</name>\n<trkseg>\n\
        <trkpt lat=\"1\" lon=\"2\"><time>2364-01-01T00:00:00Z</time></trkpt>\n\
        <trkpt lat=\"1\" lon=\"2\"><time>2364-01-01T00:00:01Z</time></trkpt>\n\
        </trkseg>\n<trkseg>\n</trkseg>\n</trk>\n</gpx>\n";

    fn rows_of(src: &str) -> (GpxFile, FileRows) {
        let f = model::split(&tree::parse(src).unwrap()).unwrap();
        let facts = FileFacts {
            path: "logs/away.gpx",
            file_key: "0123456789abcdef",
            blake3: "00",
            size: src.len() as i64,
            fidelity: Fidelity::Exact,
        };
        let rows = to_rows(&facts, &f).unwrap();
        (f, rows)
    }

    #[test]
    fn rows_read_back_into_the_same_file() {
        let (f, rows) = rows_of(SRC);
        let per_file: HashMap<&str, Vec<Row>> = rows
            .per_file
            .iter()
            .map(|(t, r)| (t.name, r.clone()))
            .collect();
        let points: HashMap<String, Row> = rows
            .points
            .iter()
            .flat_map(|(_, r)| r.iter().cloned())
            .map(|r| (as_text(&r[0]).unwrap(), r))
            .collect();
        let back = from_rows(&rows.file, &per_file, &points).unwrap();
        assert_eq!(back, f);
        assert_eq!(model::rebuild(&back).unwrap(), SRC);
    }

    #[test]
    fn a_point_a_file_holds_twice_is_one_row() {
        let src = SRC.replace(
            "<rtept lat=\"3\" lon=\"4\"/>",
            "<rtept lat=\"1\" lon=\"2\"/>",
        );
        let (_, rows) = rows_of(&src);
        let rtepts = &rows
            .points
            .iter()
            .find(|(t, _)| t.name == "gpx_rtepts")
            .unwrap()
            .1;
        assert_eq!(rtepts.len(), 1);
        let members = &rows
            .per_file
            .iter()
            .find(|(t, _)| t.name == "gpx_rte_rtepts")
            .unwrap()
            .1;
        assert_eq!(members.len(), 2);
    }

    /// The edit the design exists for: one point's elevation changes,
    /// and one member row and one point row are all that move.
    #[test]
    fn an_edit_to_one_point_rewrites_one_member_row() {
        let (_, before) = rows_of(SRC);
        let edited = SRC.replacen(
            "<time>2364-01-01T00:00:01Z</time>",
            "<ele>5</ele><time>2364-01-01T00:00:01Z</time>",
            1,
        );
        let (_, after) = rows_of(&edited);
        for ((t, old), (_, new)) in before.per_file.iter().zip(&after.per_file) {
            let d = diff(t, old, new);
            let expected = usize::from(t.name == "gpx_trkseg_trkpts");
            assert_eq!(d.upserts.len(), expected, "{}", t.name);
            assert!(d.deletes.is_empty(), "{}", t.name);
            if t.name == "gpx_trkseg_trkpts" {
                assert_eq!(dropped_ids(old, new).len(), 1);
            }
        }
    }

    #[test]
    fn deleting_a_point_from_a_timed_track_moves_nothing_else() {
        let (_, before) = rows_of(SRC);
        let trimmed = SRC.replacen(
            "<trkpt lat=\"1\" lon=\"2\"><time>2364-01-01T00:00:00Z</time></trkpt>\n",
            "",
            1,
        );
        let (_, after) = rows_of(&trimmed);
        let (t, old) = &before.per_file[5];
        let d = diff(t, old, &after.per_file[5].1);
        assert_eq!((d.deletes.len(), d.upserts.len()), (1, 0));
    }
}
