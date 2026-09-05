//! `Maps (your places)/Saved Places.json` walker.
//!
//! GeoJSON `FeatureCollection`; one feature per saved/starred place.
//! PK recipe: `uuidv5(NS, "maps_saved:{ftid_or_cid}:{date}")`.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::file_checkpoint::{self};
use datalib_etl::progress::Progress;
use serde_json::Value;
use tracing::warn;

use super::db::RawDb;
use super::schema_raw::{ns_id, MapsSavedPlaceRow};
use datalib_etl::doltlite_raw::WirePayload;

const FILE_REL: &str = "Maps (your places)/Saved Places.json";
const SCOPE: &str = "google_takeout/maps_saved_places";

pub async fn ingest(db: &RawDb, root: &Path, progress: &Progress) -> Result<usize> {
    let path = root.join(FILE_REL);
    let n = file_checkpoint::ingest_changed_file(db.pool(), SCOPE, &path, |bytes| {
        let geo: Value = serde_json::from_slice(bytes).context("parse Saved Places.json")?;
        let Some(features) = geo.get("features").and_then(|v| v.as_array()) else {
            warn!(event = "maps_saved_no_features", path = %path.display());
            return Ok(Vec::new());
        };
        let mut rows: Vec<MapsSavedPlaceRow> = Vec::with_capacity(features.len());
        for f in features {
            let Some(props) = f.get("properties") else {
                continue;
            };
            let date = props.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let url = props
                .get("google_maps_url")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let key = extract_ftid_or_cid(url).unwrap_or("");
            if key.is_empty() || date.is_empty() {
                warn!(event = "maps_saved_missing_key", path = %path.display());
                continue;
            }
            let id = ns_id(&format!("maps_saved:{key}:{date}"));
            let payload = serde_json::to_string(f).context("serialize saved-place feature")?;
            rows.push(MapsSavedPlaceRow {
                id_and_payload: WirePayload { id, payload },
                when_ts: Some(date.to_string()),
            });
        }
        Ok(rows)
    })
    .await?;
    progress.set_message(&format!("maps_saved_places: {n}"));
    Ok(n)
}

/// Saved-place URLs sometimes carry an `!1s<ftid>` segment (same
/// shape as a review URL) and sometimes a `cid=<digits>` query
/// parameter (older entries). Prefer the ftid when present.
fn extract_ftid_or_cid(url: &str) -> Option<&str> {
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
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftid_wins_over_cid() {
        assert_eq!(
            extract_ftid_or_cid("https://maps.google.com/?cid=42&data=!1sabc!8m"),
            Some("abc"),
        );
    }

    #[test]
    fn falls_back_to_cid() {
        assert_eq!(
            extract_ftid_or_cid("https://maps.google.com/?cid=12345"),
            Some("12345"),
        );
    }
}
