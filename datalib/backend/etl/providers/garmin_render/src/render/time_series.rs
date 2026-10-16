//! Garmin's time series: one per daily field in [`FIELDS`], by calendar
//! day; one per weigh-in measurement, by instant; and one per record
//! field of each activity's FIT file, by instant, about the activity's
//! page. Values keep Garmin's own unit, named in UCUM; a reader converts
//! between units of one quantity.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobCas;
use datalib_etl_render::inputs::RawRange;
use datalib_id::{composite_key, entity_id_str, IdNamespace};
use datalib_time_series_schema::{day_number, Points, TimeKind, TimeSeries};

use super::daily::{DayValue, Field, Show, FIELDS};
use super::fit::{self, Scale, RECORD_FIELDS};
use datalib_etl_garmin::ingest::db_path_for;

use super::parse::{parse, Activity, ParsedGarmin, WeighIn};
use super::render::activity_uuid;

const KIND_TIME_SERIES: &str = "time_series";
/// Part of every id's recipe, so the ids stay what they were when a
/// time series also named the thing it measured.
const ACCOUNT: &str = "account";

pub fn time_series_id(source_id: &str, name: &str) -> String {
    entity_id_str(
        IdNamespace::Garmin,
        source_id,
        None,
        KIND_TIME_SERIES,
        &composite_key(&[ACCOUNT, name]),
        None,
    )
}

/// An activity's time series, one per record field, keyed apart from
/// the account's by the activity.
pub fn activity_time_series_id(source_id: &str, activity_id: &str, name: &str) -> String {
    entity_id_str(
        IdNamespace::Garmin,
        source_id,
        None,
        KIND_TIME_SERIES,
        &composite_key(&["activity", activity_id, name]),
        None,
    )
}

/// What [`read`] found.
pub struct Read {
    /// The commit the raw store was read at; `None` when nothing is
    /// committed yet.
    pub head: Option<String>,
    pub time_series: Vec<TimeSeries>,
    /// An activity whose FIT file is held but could not be read, and
    /// why. Its time series are left out; the rest are not.
    pub unreadable: Vec<(String, String)>,
}

/// The raw store's time series at `pin` (else its head), the FIT files'
/// included.
pub fn read(raw_path: &Path, source_id: &str, pin: Option<&str>) -> Result<Read> {
    let parsed = parse(
        raw_path,
        RawRange {
            cursor: None,
            pin,
            stale: None,
        },
    )?;
    let mut all = time_series(source_id, &parsed);
    let fits = load_fits(raw_path, &parsed.activities)?;
    let mut unreadable = Vec::new();
    for a in &parsed.activities {
        let Some(blake3) = &a.fit_blake3 else {
            continue;
        };
        let Some(bytes) = fits.get(blake3) else {
            unreadable.push((
                a.id.clone(),
                format!("its FIT file {blake3} is not in the blob store"),
            ));
            continue;
        };
        match fit::records(bytes) {
            Ok(records) => all.extend(activity_series(source_id, a, &records)),
            Err(e) => unreadable.push((a.id.clone(), format!("{e:#}"))),
        }
    }
    Ok(Read {
        head: parsed.head,
        time_series: all,
        unreadable,
    })
}

/// The FIT files `activities` name that the blob store holds, by
/// blake3.
fn load_fits(raw_path: &Path, activities: &[Activity]) -> Result<HashMap<String, Vec<u8>>> {
    let db_path = db_path_for(raw_path);
    let wanted: Vec<&str> = activities
        .iter()
        .filter_map(|a| a.fit_blake3.as_deref())
        .collect();
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let cas = BlobCas::open_for_render(&db_path)
                .await
                .with_context(|| format!("open the blob store beside {}", db_path.display()))?
                .with_context(|| {
                    format!(
                        "the raw store names FIT files, but there is no blob store beside {}",
                        db_path.display()
                    )
                })?;
            let mut out = HashMap::new();
            let mut failed = None;
            for b in wanted {
                match cas.get(b).await {
                    Ok(Some(obj)) => {
                        out.insert(b.to_string(), obj.bytes);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            cas.close().await;
            match failed {
                Some(e) => Err(e),
                None => Ok(out),
            }
        })
    })
}

/// One time series per field of [`RECORD_FIELDS`] the records hold,
/// leaving out a field another one supersedes when that one is there.
/// A record repeating the last one's time keeps the later value; one
/// going back in time is left out, since a time series ascends.
fn activity_series(source_id: &str, a: &Activity, records: &[fit::Record]) -> Vec<TimeSeries> {
    let about_uuid = activity_uuid(source_id, &a.id, a.start_gmt_ms());
    let about_name = match a.start_iso() {
        Some(start) => format!("{} · {}", a.name(), &start[..start.len().min(10)]),
        None => a.name(),
    };
    let held = |num: u8| records.iter().any(|r| r.fields.contains_key(&num));
    RECORD_FIELDS
        .iter()
        .filter(|f| held(f.num) && !f.superseded_by.is_some_and(held))
        .map(|f| {
            let mut pairs: Vec<(i64, i64)> = Vec::new();
            for r in records {
                let Some(&raw) = r.fields.get(&f.num) else {
                    continue;
                };
                match pairs.last_mut() {
                    Some(last) if last.0 >= r.t_ms => {
                        if last.0 == r.t_ms {
                            last.1 = raw;
                        }
                    }
                    _ => pairs.push((r.t_ms, raw)),
                }
            }
            let points = match f.scale {
                Scale::Integer => Points::integers(pairs),
                scale => Points::floats(
                    pairs
                        .into_iter()
                        .map(|(t, raw)| (t, fit::decimal_value(raw, scale))),
                ),
            };
            TimeSeries {
                time_series_id: activity_time_series_id(source_id, &a.id, f.name),
                source_id: source_id.to_string(),
                about_uuid: Some(about_uuid.clone()),
                about_name: Some(about_name.clone()),
                name: f.name.to_string(),
                human_name: f.human_name.to_string(),
                unit: f.unit.to_string(),
                time_kind: TimeKind::Instant,
                points,
            }
        })
        .collect()
}

/// Every time series `parsed` holds, in [`FIELDS`] order then the
/// weigh-ins'. One with no points is left out.
pub fn time_series(source_id: &str, parsed: &ParsedGarmin) -> Vec<TimeSeries> {
    let daily = FIELDS
        .iter()
        .filter_map(|f| daily_series(source_id, parsed, f));
    let weigh_ins = WEIGH_IN_METRICS
        .iter()
        .map(|m| weigh_in_series(source_id, &parsed.weigh_ins, m));
    daily
        .chain(weigh_ins)
        .filter(|s| !s.points.is_empty())
        .collect()
}

/// A field's unit in UCUM; `None` for one this table does not name,
/// which a test refuses.
fn unit_of(f: &Field) -> Option<&'static str> {
    Some(match (f.show, f.unit) {
        (Show::Text, _) => "1",
        (Show::Duration, _) => "s",
        (Show::Kilometres, _) => "m",
        (_, "bpm") => "{beats}/min",
        (_, "brpm") => "{breaths}/min",
        (_, "ml/kg/min") => "mL/kg/min",
        (_, "years") => "a",
        (_, "ms") => "ms",
        (_, "min") => "min",
        (_, "kcal") => "kcal",
        (_, "%") => "%",
        (_, "") => match f.key {
            "steps" => "{steps}",
            "floors" => "{floors}",
            _ => "{score}",
        },
        _ => return None,
    })
}

fn daily_series(source_id: &str, parsed: &ParsedGarmin, f: &Field) -> Option<TimeSeries> {
    let unit = unit_of(f)?;
    let days = parsed
        .days
        .iter()
        .filter_map(|(date, day)| Some((day_number(date)?, day.values.get(f.key)?)));
    // A value of the other kind than the field's is left out.
    let points = if f.show == Show::Text {
        Points::texts(days.filter_map(|(t, v)| match v {
            DayValue::Text(s) => Some((t, s.clone())),
            DayValue::Num(_) => None,
        }))
    } else {
        Points::floats(days.filter_map(|(t, v)| match v {
            DayValue::Num(x) => Some((t, *x)),
            DayValue::Text(_) => None,
        }))
    };
    Some(TimeSeries {
        time_series_id: time_series_id(source_id, f.key),
        source_id: source_id.to_string(),
        about_uuid: None,
        about_name: None,
        name: f.key.to_string(),
        human_name: f.label.to_string(),
        unit: unit.to_string(),
        time_kind: TimeKind::Day,
        points,
    })
}

struct WeighInMetric {
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    value: fn(&WeighIn) -> Option<f64>,
}

const WEIGH_IN_METRICS: &[WeighInMetric] = &[
    WeighInMetric {
        key: "weight",
        label: "Weight",
        unit: "kg",
        value: |w| Some(w.weight_kg),
    },
    WeighInMetric {
        key: "body_fat",
        label: "Body fat",
        unit: "%",
        value: |w| w.body_fat_pct,
    },
    WeighInMetric {
        key: "bmi",
        label: "BMI",
        unit: "kg/m2",
        value: |w| w.bmi,
    },
];

/// Two weigh-ins at one instant keep the later one, since a point is
/// keyed by its time.
fn weigh_in_series(source_id: &str, weigh_ins: &[WeighIn], m: &WeighInMetric) -> TimeSeries {
    let mut points: Vec<(i64, f64)> = Vec::new();
    for w in weigh_ins {
        let Some(v) = (m.value)(w) else { continue };
        match points.last_mut() {
            Some(last) if last.0 == w.timestamp_gmt_ms => last.1 = v,
            _ => points.push((w.timestamp_gmt_ms, v)),
        }
    }
    TimeSeries {
        time_series_id: time_series_id(source_id, m.key),
        source_id: source_id.to_string(),
        about_uuid: None,
        about_name: None,
        name: m.key.to_string(),
        human_name: m.label.to_string(),
        unit: m.unit.to_string(),
        time_kind: TimeKind::Instant,
        points: Points::floats(points),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::daily::Day;
    use datalib_time_series_schema::{Unit, Values};

    fn weigh_in(ms: i64, kg: f64, fat: Option<f64>) -> WeighIn {
        WeighIn {
            id: ms.to_string(),
            calendar_date: "2369-04-14".into(),
            timestamp_gmt_ms: ms,
            weight_kg: kg,
            bmi: None,
            body_fat_pct: fat,
            source_type: None,
        }
    }

    fn parsed() -> ParsedGarmin {
        let mut p = ParsedGarmin::default();
        let mut day = Day::default();
        day.values.insert("sleep_total", DayValue::Num(26_400.0));
        day.values.insert("distance", DayValue::Num(4_200.0));
        day.values.insert("hrv_night", DayValue::Num(45.0));
        day.values.insert("im_moderate", DayValue::Num(30.0));
        day.values.insert("resting_hr", DayValue::Num(48.0));
        day.values.insert("steps", DayValue::Text("oops".into()));
        day.values
            .insert("hrv_status", DayValue::Text("BALANCED".into()));
        p.days.insert("2369-04-14".into(), day);
        p.weigh_ins = vec![
            weigh_in(1_000, 80.0, None),
            weigh_in(2_000, 79.5, Some(18.0)),
            weigh_in(2_000, 79.4, Some(17.9)),
        ];
        p
    }

    fn get<'a>(all: &'a [TimeSeries], name: &str) -> &'a TimeSeries {
        all.iter().find(|s| s.name == name).unwrap()
    }

    /// Every field must name a unit the store accepts; one that did not
    /// would drop its time series.
    #[test]
    fn every_field_has_a_ucum_unit() {
        for f in FIELDS {
            let unit = unit_of(f).unwrap_or_else(|| panic!("{} has no unit", f.key));
            assert!(Unit::parse(unit).is_ok(), "{}: {unit}", f.key);
        }
        for m in WEIGH_IN_METRICS {
            assert!(Unit::parse(m.unit).is_ok(), "{}: {}", m.key, m.unit);
        }
    }

    #[test]
    fn daily_fields_are_stored_by_day_in_garmins_units() {
        let all = time_series("garmin", &parsed());
        let day = day_number("2369-04-14").unwrap();
        for (name, unit, value) in [
            ("sleep_total", "s", 26_400.0),
            ("distance", "m", 4_200.0),
            ("hrv_night", "ms", 45.0),
            ("im_moderate", "min", 30.0),
            ("resting_hr", "{beats}/min", 48.0),
        ] {
            let s = get(&all, name);
            assert_eq!(
                (s.unit.as_str(), s.time_kind),
                (unit, TimeKind::Day),
                "{name}"
            );
            assert_eq!(s.points, Points::floats([(day, value)]), "{name}");
        }
        assert_eq!(
            get(&all, "hrv_status").points.values,
            Values::Text(vec!["BALANCED".into()])
        );
        assert!(
            !all.iter().any(|s| s.name == "steps"),
            "a text in a number field is left out, and the time series with it"
        );
    }

    #[test]
    fn weigh_ins_are_instants_and_a_repeated_instant_keeps_the_later() {
        let all = time_series("garmin", &parsed());
        let w = get(&all, "weight");
        assert_eq!(w.time_kind, TimeKind::Instant);
        assert_eq!(w.points, Points::floats([(1_000, 80.0), (2_000, 79.4)]));
        assert_eq!(
            get(&all, "body_fat").points,
            Points::floats([(2_000, 17.9)])
        );
    }

    #[test]
    fn empty_time_series_are_left_out_and_ids_differ_by_source_and_name() {
        let all = time_series("garmin", &parsed());
        assert!(all.iter().all(|s| !s.points.is_empty()));
        assert!(!all.iter().any(|s| s.name == "bmi"));
        assert_ne!(time_series_id("a", "weight"), time_series_id("b", "weight"));
        assert_ne!(time_series_id("a", "weight"), time_series_id("a", "bmi"));
        assert_eq!(time_series_id("a", "weight"), time_series_id("a", "weight"));
    }

    fn activity() -> Activity {
        Activity {
            id: "17".into(),
            listing: serde_json::json!({
                "activityName": "Morning run",
                "startTimeGMT": "2369-04-14 06:30:00",
                "startTimeLocal": "2369-04-14 08:30:00",
            }),
            detail: serde_json::Value::Null,
            fit_blake3: Some("b3".into()),
        }
    }

    /// A field another one supersedes is left out when that one is
    /// there, every time series is about the activity's page, and a
    /// scaled field keeps its decimals.
    #[test]
    fn an_activitys_records_become_its_time_series() {
        let buf = crate::render::fit::tests::fit_file(&[
            (1_000_000_030, 140, 3_210),
            (1_000_000_031, 141, 3_300),
        ]);
        let records = fit::records(&buf).unwrap();
        let a = activity();
        let all = activity_series("garmin", &a, &records);
        let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["heart_rate", "speed"]);
        let page = activity_uuid("garmin", "17", a.start_gmt_ms());
        assert!(all
            .iter()
            .all(|s| s.about_uuid.as_deref() == Some(page.as_str())));
        assert_eq!(
            all[0].about_name.as_deref(),
            Some("Morning run · 2369-04-14")
        );
        assert_eq!(all[0].points.values, Values::Integer(vec![140, 141]));
        assert_eq!(all[1].points.values, Values::Float(vec![3.21, 3.3]));
        assert_ne!(
            all[0].time_series_id,
            activity_time_series_id("garmin", "18", "heart_rate")
        );

        let mut with_enhanced = records.clone();
        with_enhanced[0].fields.insert(73, 3_210);
        let names: Vec<String> = activity_series("garmin", &a, &with_enhanced)
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["heart_rate", "enhanced_speed"]);
    }

    #[test]
    fn every_record_field_has_a_ucum_unit() {
        for f in RECORD_FIELDS {
            assert!(Unit::parse(f.unit).is_ok(), "{}: {}", f.name, f.unit);
        }
    }
}
