//! Garmin HTTP fixture synthesizer: one JSON spec (the TNG account) →
//! every playback response the ingest walk asks for between the spec's
//! `since` and `today`. The walk's request shapes are imported from the
//! ingest module rather than copied, so a path change there cannot
//! strand the fixtures.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use datalib_etl::http::{HttpRequest, HttpResponse};
use datalib_etl::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::ingest::api::{base_url, req_get, req_get_bytes};
use crate::ingest::{daily_path, date, ACTIVITY_PAGE, ITEM_KINDS, WEIGHT_CHUNK_DAYS};
use datalib_etl_garmin_config::DEFAULT_REFRESH_DAYS;

/// The spec file. Everything is optional but the account and the two
/// dates; a metric or a day the spec does not mention answers `{}`.
#[derive(Debug, Deserialize)]
pub struct GarminSpec {
    pub since: String,
    pub today: String,
    pub social_profile: Value,
    #[serde(default)]
    pub user_settings: Value,
    #[serde(default)]
    pub devices: Vec<Value>,
    /// `metric → (date → payload)`.
    #[serde(default)]
    pub daily: BTreeMap<String, BTreeMap<String, Value>>,
    /// Flat weigh-ins; grouped into `dailyWeightSummaries` by
    /// `calendarDate` the way the range endpoint answers.
    #[serde(default)]
    pub weigh_ins: Vec<Value>,
    #[serde(default)]
    pub activities: Vec<SpecActivity>,
    /// `kind → items`.
    #[serde(default)]
    pub items: BTreeMap<String, Vec<Value>>,
}

#[derive(Debug, Deserialize)]
pub struct SpecActivity {
    pub listing: Value,
    #[serde(default)]
    pub detail: Value,
    /// Bytes for the FIT file inside the download zip, as text. A real
    /// FIT file is binary; a fixture only needs to round-trip.
    #[serde(default)]
    pub fit: Option<String>,
}

pub struct GarminSynth {
    pub spec_path: PathBuf,
    pub domain: String,
}

impl GarminSynth {
    pub fn new(spec_path: impl Into<PathBuf>) -> Self {
        Self {
            spec_path: spec_path.into(),
            domain: "garmin.com".into(),
        }
    }

    pub fn load(&self) -> Result<GarminSpec> {
        let bytes = fs::read(&self.spec_path)
            .with_context(|| format!("read {}", self.spec_path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", self.spec_path.display()))
    }
}

impl Synthesizer for GarminSynth {
    fn name(&self) -> &'static str {
        "garmin"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        let spec = self.load()?;
        let since = date(&spec.since)?;
        let today = date(&spec.today)?;
        let base = base_url(&self.domain);
        let display_name = spec.social_profile["displayName"]
            .as_str()
            .context("spec social_profile.displayName")?
            .to_string();
        let profile_pk = spec.social_profile["profileId"]
            .as_i64()
            .or_else(|| spec.social_profile["id"].as_i64())
            .map(|n| n.to_string());
        let mut n = 0usize;
        let mut put = |req: HttpRequest, resp: HttpResponse| -> Result<()> {
            write_fixture(out_root, &req, &resp)?;
            n += 1;
            Ok(())
        };
        let get = |path: &str| req_get(&format!("{base}{path}"));

        put(
            get("/userprofile-service/socialProfile"),
            json_response(&spec.social_profile),
        )?;
        put(
            get("/userprofile-service/userprofile/user-settings"),
            json_response(&spec.user_settings),
        )?;
        put(
            get("/device-service/deviceregistration/devices"),
            json_response(&Value::Array(spec.devices.clone())),
        )?;

        for metric in datalib_etl_garmin_config::DAILY_METRICS {
            let days = spec.daily.get(*metric);
            let mut day = since;
            while day <= today {
                let d = day.format("%Y-%m-%d").to_string();
                let body = days
                    .and_then(|m| m.get(&d))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                put(
                    get(&daily_path(metric, &display_name, &d)),
                    json_response(&body),
                )?;
                day += Duration::days(1);
            }
        }

        // The weight and activity walks resume from `since` on a first
        // run and from a week before `today` on the next, so both
        // windows get fixtures.
        let mut starts = vec![since, today - Duration::days(DEFAULT_REFRESH_DAYS)];
        starts.sort();
        starts.dedup();
        for start in starts {
            let mut chunk_start = start;
            while chunk_start <= today {
                let chunk_end = (chunk_start + Duration::days(WEIGHT_CHUNK_DAYS - 1)).min(today);
                let body = weight_range_reply(&spec.weigh_ins, chunk_start, chunk_end);
                put(
                    get(&format!(
                        "/weight-service/weight/range/{}/{}?includeAll=true",
                        chunk_start.format("%Y-%m-%d"),
                        chunk_end.format("%Y-%m-%d")
                    )),
                    json_response(&body),
                )?;
                chunk_start = chunk_end + Duration::days(1);
            }

            let listings: Vec<Value> = spec
                .activities
                .iter()
                .map(|a| a.listing.clone())
                .filter(|l| {
                    l["startTimeGMT"]
                        .as_str()
                        .and_then(|t| date(t.get(..10).unwrap_or("")).ok())
                        .is_none_or(|d| d >= start)
                })
                .collect();
            let mut page = 0usize;
            loop {
                let chunk: Vec<Value> = listings
                    .iter()
                    .skip(page * ACTIVITY_PAGE)
                    .take(ACTIVITY_PAGE)
                    .cloned()
                    .collect();
                let full = chunk.len() == ACTIVITY_PAGE;
                put(
                    get(&format!(
                        "/activitylist-service/activities/search/activities?start={}&limit={ACTIVITY_PAGE}&startDate={}",
                        page * ACTIVITY_PAGE,
                        start.format("%Y-%m-%d")
                    )),
                    json_response(&Value::Array(chunk)),
                )?;
                // A full page makes the walk ask for one more.
                if !full {
                    break;
                }
                page += 1;
            }
        }
        for a in &spec.activities {
            let id = a.listing["activityId"]
                .as_i64()
                .map(|n| n.to_string())
                .or_else(|| a.listing["activityId"].as_str().map(str::to_string))
                .context("spec activity listing.activityId")?;
            put(
                get(&format!("/activity-service/activity/{id}")),
                json_response(&a.detail),
            )?;
            let fit = a
                .fit
                .clone()
                .unwrap_or_else(|| format!(".FIT synthetic {id}"));
            put(
                req_get_bytes(&format!("{base}/download-service/files/activity/{id}")),
                zip_response(&format!("{id}_ACTIVITY.fit"), fit.as_bytes())?,
            )?;
        }

        for kind in ITEM_KINDS {
            let path = match *kind {
                "personal_records" => format!(
                    "/personalrecord-service/personalrecord/prs/{}",
                    urlencoding::encode(&display_name)
                ),
                "gear" => match &profile_pk {
                    Some(pk) => format!("/gear-service/gear/filterGear?userProfilePk={pk}"),
                    None => continue,
                },
                "badges" => "/badge-service/badge/earned".to_string(),
                "workouts" => "/workout-service/workouts?start=0&limit=1000".to_string(),
                "goals" => {
                    "/goal-service/goal/goals?status=active&start=0&limit=1000&sortOrder=asc"
                        .to_string()
                }
                _ => unreachable!(),
            };
            let items = spec.items.get(*kind).cloned().unwrap_or_default();
            put(get(&path), json_response(&Value::Array(items)))?;
        }

        Ok(SynthesizeReport {
            fixtures_written: n,
        })
    }
}

/// The range endpoint's shape: one summary per calendar day holding
/// that day's weigh-ins.
pub fn weight_range_reply(weigh_ins: &[Value], start: NaiveDate, end: NaiveDate) -> Value {
    let mut by_day: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for w in weigh_ins {
        let Some(d) = w["calendarDate"].as_str() else {
            continue;
        };
        let Ok(day) = date(d) else { continue };
        if day >= start && day <= end {
            by_day.entry(d.to_string()).or_default().push(w.clone());
        }
    }
    json!({
        "dailyWeightSummaries": by_day
            .into_iter()
            .map(|(d, list)| json!({"summaryDate": d, "allWeightMetrics": list}))
            .collect::<Vec<_>>(),
    })
}

fn zip_response(name: &str, bytes: &[u8]) -> Result<HttpResponse> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut w = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        w.start_file(name, opts)?;
        w.write_all(bytes)?;
        w.finish()?;
    }
    let mut headers = BTreeMap::new();
    headers.insert("content-type".into(), "application/zip".into());
    Ok(HttpResponse {
        status: 200,
        headers,
        body: buf.into_inner(),
        duration_ms: 0,
    })
}
