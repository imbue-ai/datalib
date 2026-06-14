//! `Maps (your places)/Reviews.json` walker.
//!
//! The file is a GeoJSON `FeatureCollection`. Each `feature` is one
//! review the user wrote. PK recipe:
//! `uuidv5(NS, "maps_review:{ftid}:{date}")`, where `ftid` is the hex
//! id after `!1s` in the feature's `google_maps_url`.

use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::Value;
use tracing::warn;

use super::db::RawDb;
use super::schema_raw::{ns_id, MapsReviewRow};
use frankweiler_etl::doltlite_raw::WirePayload;

const FILE_REL: &str = "Maps (your places)/Reviews.json";
const SCOPE: &str = "google_takeout/maps_reviews";

pub async fn ingest(db: &RawDb, root: &Path, progress: &Progress) -> Result<usize> {
    let path = root.join(FILE_REL);
    if !path.exists() {
        return Ok(0);
    }
    let fp = FileFingerprint::of(&path)?;
    let stamped = file_checkpoint::load(db.pool(), SCOPE).await?;
    if file_checkpoint::should_skip(&stamped, &fp) {
        return Ok(0);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let geo: Value = serde_json::from_slice(&bytes).context("parse Reviews.json")?;
    let Some(features) = geo.get("features").and_then(|v| v.as_array()) else {
        warn!(event = "maps_reviews_no_features", path = %path.display());
        return Ok(0);
    };
    let mut rows: Vec<MapsReviewRow> = Vec::with_capacity(features.len());
    for f in features {
        let Some(props) = f.get("properties") else {
            continue;
        };
        let date = props.get("date").and_then(|v| v.as_str()).unwrap_or("");
        let ftid = props
            .get("google_maps_url")
            .and_then(|v| v.as_str())
            .and_then(extract_ftid)
            .unwrap_or("");
        if ftid.is_empty() || date.is_empty() {
            warn!(event = "maps_review_missing_key", path = %path.display());
            continue;
        }
        let id = ns_id(&format!("maps_review:{ftid}:{date}"));
        let payload = serde_json::to_string(f).context("serialize maps_review feature")?;
        rows.push(MapsReviewRow {
            id_and_payload: WirePayload { id, payload },
            when_ts: Some(date.to_string()),
        });
    }
    let n = rows.len();
    progress.set_message(&format!("maps_reviews: {n}"));

    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await.context("begin maps_reviews tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    file_checkpoint::record_finished(&mut tx, SCOPE, &fp).await?;
    tx.commit().await.context("commit maps_reviews tx")?;
    Ok(n)
}

/// Pull the hex ftid out of a Google Maps URL of the shape
/// `https://www.google.com/maps/.../@.../!1s<HEX>!...`. Returns
/// `None` when the URL doesn't carry an ftid.
fn extract_ftid(url: &str) -> Option<&str> {
    let key = "!1s";
    let after = &url[url.find(key)? + key.len()..];
    let end = after.find('!').unwrap_or(after.len());
    let ftid = &after[..end];
    if ftid.is_empty() {
        None
    } else {
        Some(ftid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ftid_from_url() {
        assert_eq!(
            extract_ftid("https://www.google.com/maps/place/X/@1,2,15z/data=!4m1!1sabc123def!8m2"),
            Some("abc123def"),
        );
        assert_eq!(extract_ftid("https://www.google.com/maps/place/X"), None);
    }
}
