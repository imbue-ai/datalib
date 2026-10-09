//! `Maps (your places)/Saved Places.json` walker.
//!
//! GeoJSON `FeatureCollection`; one feature per saved/starred place.
//! PK recipe: `uuidv5(NS, "maps_saved:{place}:{date}")`, where the place
//! is the URL's feature id, its `cid`, or — for a pin dropped on an
//! address rather than a place — its `q=` address query.

use datalib_etl::download_problems::SkippedRecord;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::fsscan;
use datalib_problems::{Problem, Reason};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_files::file_checkpoint::{self, SnapshotCounts};
use serde_json::Value;

use super::db::RawDb;
use super::schema_raw::{ns_id, MapsSavedPlaceRow};
use datalib_etl::doltlite_raw::WirePayload;

pub(crate) const FILE_REL: &str = "Maps (your places)/Saved Places.json";
const SCOPE: &str = "google_takeout/maps_saved_places";

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<SnapshotCounts> {
    let mut skipped = None;
    let n = file_checkpoint::ingest_snapshot(db.pool(), SCOPE, scan.file(FILE_REL), |bytes| {
        let skipped = skipped.insert(Vec::new());
        let geo: Value = serde_json::from_slice(bytes).context("parse Saved Places.json")?;
        let features = geo
            .get("features")
            .and_then(|v| v.as_array())
            .ok_or_else(|| super::unknown_layout(FILE_REL, "has no `features` list"))?;
        let mut rows: Vec<MapsSavedPlaceRow> = Vec::with_capacity(features.len());
        for f in features {
            let Some(props) = f.get("properties") else {
                skipped.push(SkippedRecord {
                    entry: f.to_string(),
                    problem: Problem::field("properties", Reason::NoIdentity, ""),
                });
                continue;
            };
            let date = props.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let url = props
                .get("google_maps_url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let key = place_key(url).unwrap_or("");
            if key.is_empty() || date.is_empty() {
                let (field, value) = if date.is_empty() {
                    ("date", date)
                } else {
                    ("google_maps_url", url)
                };
                skipped.push(SkippedRecord {
                    entry: f.to_string(),
                    problem: Problem::field(field, Reason::NoIdentity, value),
                });
                continue;
            }
            let id = ns_id(&format!("maps_saved:{key}:{date}"));
            let payload = serde_json::to_string(f).context("serialize saved-place feature")?;
            rows.push(MapsSavedPlaceRow {
                id_and_payload: WirePayload { id, payload },
                when_ts: Some(date.to_string()),
            });
        }
        super::require_some_read(FILE_REL, features.len(), rows.len())?;
        Ok(rows)
    })
    .await?;
    // `None`: the file was unchanged, and last run's rows still hold.
    if let Some(skipped) = skipped {
        found.skipped("maps_saved_places", skipped);
    }
    progress.set_message(&format!("maps_saved_places: {}", n.written));
    Ok(n)
}

fn place_key(url: &str) -> Option<&str> {
    if let Some(rest) = url.find("!1s").map(|i| &url[i + 3..]) {
        let end = rest.find('!').unwrap_or(rest.len());
        let ftid = &rest[..end];
        if !ftid.is_empty() {
            return Some(ftid);
        }
    }
    if let Some(rest) = url.find("cid=").map(|i| &url[i + 4..]) {
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let cid = &rest[..end];
        if !cid.is_empty() {
            return Some(cid);
        }
    }
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("q="))
        .filter(|q| !q.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftid_wins_over_cid() {
        assert_eq!(
            place_key("https://maps.google.com/?cid=42&data=!1sabc!8m"),
            Some("abc"),
        );
    }

    #[test]
    fn falls_back_to_cid() {
        assert_eq!(
            place_key("https://maps.google.com/?cid=12345"),
            Some("12345"),
        );
    }

    #[test]
    fn a_pin_on_an_address_is_keyed_by_its_query() {
        assert_eq!(
            place_key("http://maps.google.com/?q=Quark%27s+Bar,+Deep+Space+Nine"),
            Some("Quark%27s+Bar,+Deep+Space+Nine"),
        );
        assert_eq!(place_key("http://maps.google.com/?q="), None);
        assert_eq!(place_key("http://maps.google.com/"), None);
    }
}
