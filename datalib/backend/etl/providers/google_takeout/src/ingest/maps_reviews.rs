//! `Maps (your places)/Reviews.json` walker.

use datalib_etl::download_problems::SkippedRecord;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::fsscan;
use datalib_problems::{Problem, Reason};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_files::file_checkpoint::{self, SnapshotCounts};
use serde_json::Value;

use super::db::RawDb;
use super::schema_raw::{ns_id, MapsReviewRow};
use datalib_etl::doltlite_raw::WirePayload;

pub(crate) const FILE_REL: &str = "Maps (your places)/Reviews.json";
const SCOPE: &str = "google_takeout/maps_reviews";

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<SnapshotCounts> {
    let mut skipped = None;
    let n = file_checkpoint::ingest_snapshot(db.pool(), SCOPE, scan.file(FILE_REL), |bytes| {
        let skipped = skipped.insert(Vec::new());
        let geo: Value = serde_json::from_slice(bytes).context("parse Reviews.json")?;
        let features = geo
            .get("features")
            .and_then(|v| v.as_array())
            .ok_or_else(|| super::unknown_layout(FILE_REL, "has no `features` list"))?;
        let mut rows: Vec<MapsReviewRow> = Vec::with_capacity(features.len());
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
            let ftid = extract_ftid(url).unwrap_or("");
            if ftid.is_empty() || date.is_empty() {
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
            let id = ns_id(&format!("maps_review:{ftid}:{date}"));
            let payload = serde_json::to_string(f).context("serialize maps_review feature")?;
            rows.push(MapsReviewRow {
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
        found.skipped("maps_reviews", skipped);
    }
    progress.set_message(&format!("maps_reviews: {}", n.written));
    Ok(n)
}

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
