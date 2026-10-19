//! `Saved/<list>.csv`: one file per Maps list, a row per place on it.
//!
//! The header is Google's (`Title,Note,URL,Tags,Comment`). A list with a
//! description has it on the lines before the header. Every row is kept
//! whole, keyed by the header it sits under, so render can say which
//! columns it does not read.

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::fsscan;
use serde_json::{json, Map, Value};

use super::db::RawDb;
use super::per_file::{self, Counts, Feed, FromFile, Occurrences};
use super::schema_raw::{ns_id, MapsSavedListRow};

pub const FEED: Feed = Feed {
    name: "maps_saved_lists",
    dir: "Saved",
    reads: |rel| rel.ends_with(".csv") && rel.matches('/').count() == 1,
};

impl FromFile for MapsSavedListRow {
    fn file(&self) -> &str {
        &self.file
    }
}

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<Counts> {
    let n = per_file::ingest(db, scan, &FEED, found, |f, bytes| parse(&f.rel, bytes)).await?;
    progress.set_message(&format!("maps_saved_lists: {}", n.written));
    Ok(n)
}

/// The list's name is its file's: Google names the file after the list.
pub fn list_name(rel: &str) -> &str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    name.strip_suffix(".csv").unwrap_or(name)
}

pub fn parse(rel: &str, bytes: &[u8]) -> Result<Vec<MapsSavedListRow>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes));
    let records: Vec<csv::StringRecord> = reader
        .records()
        .collect::<std::result::Result<_, _>>()
        .context("parse the list's CSV")?;
    let is_header =
        |r: &csv::StringRecord| r.iter().any(|c| c == "Title") && r.iter().any(|c| c == "URL");
    let Some(at) = records.iter().position(is_header) else {
        return Err(super::unknown_layout(
            &super::shape::shape_rel(rel),
            "has no `Title` and `URL` header",
        ));
    };
    let description: Vec<&str> = records[..at]
        .iter()
        .flat_map(|r| r.iter())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    let header: Vec<&str> = records[at].iter().collect();
    let list = list_name(rel);
    let mut seen = Occurrences::default();
    let mut rows = Vec::new();
    for (position, record) in records[at + 1..].iter().enumerate() {
        if record.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        let mut cells = Map::new();
        for (i, value) in record.iter().enumerate() {
            let key = header
                .get(i)
                .filter(|h| !h.is_empty())
                .map_or_else(|| format!("column {}", i + 1), |h| h.to_string());
            cells.insert(key, Value::String(value.to_string()));
        }
        let cell = |k: &str| cells.get(k).and_then(Value::as_str).unwrap_or("");
        let natural = format!("{}\u{1f}{}", cell("Title"), cell("URL"));
        let n = seen.next(&natural);
        let id = ns_id(&format!("maps_saved_list:{rel}:{natural}:{n}"));
        let payload = json!({
            "list": list,
            "description": (!description.is_empty()).then(|| description.join("\n")),
            "position": position,
            "row": Value::Object(cells),
        });
        rows.push(MapsSavedListRow {
            id_and_payload: WirePayload {
                id,
                payload: payload.to_string(),
            },
            file: rel.to_string(),
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payloads(rows: &[MapsSavedListRow]) -> Vec<Value> {
        rows.iter()
            .map(|r| serde_json::from_str(&r.id_and_payload.payload).unwrap())
            .collect()
    }

    #[test]
    fn a_row_is_kept_whole_under_its_header() {
        let csv = "Title,Note,URL,Tags,Comment\n\
                   Ten Forward,\"Guinan's bar,\nask for the green drink\",https://www.google.com/maps/place/x,,\n\
                   ,,,,\n";
        let rows = parse("Saved/Starbase leave.csv", csv.as_bytes()).unwrap();
        let p = payloads(&rows);
        assert_eq!(p.len(), 1, "the blank row is not a place");
        assert_eq!(p[0]["list"], "Starbase leave");
        assert_eq!(
            p[0]["row"]["Note"],
            "Guinan's bar,\nask for the green drink"
        );
        assert_eq!(p[0]["row"]["Tags"], "");
        assert!(p[0]["description"].is_null());
    }

    /// A list with a description has it on the lines before its header;
    /// read as the header, its rows lost their columns.
    #[test]
    fn a_description_comes_before_the_header() {
        let csv = "Where the crew goes on shore leave\n\nTitle,Note,URL,Tags,Comment\nRisa,,https://www.google.com/maps/place/r,,\n";
        let rows = parse("Saved/Shore leave.csv", csv.as_bytes()).unwrap();
        let p = payloads(&rows);
        assert_eq!(p[0]["description"], "Where the crew goes on shore leave");
        assert_eq!(p[0]["row"]["Title"], "Risa");
    }

    /// The same place twice on a list keeps two rows.
    #[test]
    fn the_same_entry_twice_keeps_two_ids() {
        let csv = "Title,Note,URL\nRisa,,https://x\nRisa,,https://x\n";
        let rows = parse("Saved/L.csv", csv.as_bytes()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].id_and_payload.id, rows[1].id_and_payload.id);
    }

    #[test]
    fn a_file_with_no_header_is_not_a_list() {
        assert!(parse("Saved/L.csv", b"just,some,cells\n").is_err());
    }
}
