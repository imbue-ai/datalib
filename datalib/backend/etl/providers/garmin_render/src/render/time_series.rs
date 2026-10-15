//! Garmin's time series: one per daily field in [`FIELDS`], by calendar
//! day, and one per weigh-in measurement, by instant. Values are stored
//! in one unit per quantity — durations in seconds, distances in metres
//! — so they share an axis with any other source's.

use std::path::Path;

use anyhow::Result;
use datalib_etl_render::inputs::RawRange;
use datalib_id::{composite_key, entity_id_str, IdNamespace};
use datalib_time_series_schema::{day_number, Point, TimeKind, TimeSeries};

use super::daily::{DayValue, Field, Show, FIELDS};
use super::parse::{parse, ParsedGarmin, WeighIn};

const KIND_TIME_SERIES: &str = "time_series";
/// The one thing a Garmin source measures: the account it mirrors.
const ENTITY: &str = "account";

pub fn time_series_id(source_id: &str, metric: &str) -> String {
    entity_id_str(
        IdNamespace::Garmin,
        source_id,
        None,
        KIND_TIME_SERIES,
        &composite_key(&[ENTITY, metric]),
        None,
    )
}

/// The raw store's time series at `pin` (else its head), and the commit
/// they were read at; `None` when nothing is committed yet.
pub fn read(
    raw_path: &Path,
    source_id: &str,
    pin: Option<&str>,
) -> Result<(Option<String>, Vec<TimeSeries>)> {
    let parsed = parse(
        raw_path,
        RawRange {
            cursor: None,
            pin,
            stale: None,
        },
    )?;
    let all = time_series(source_id, &parsed);
    Ok((parsed.head, all))
}

/// Every time series `parsed` holds, in [`FIELDS`] order then the
/// weigh-ins'. One with no points is left out.
pub fn time_series(source_id: &str, parsed: &ParsedGarmin) -> Vec<TimeSeries> {
    let daily = FIELDS.iter().map(|f| daily_series(source_id, parsed, f));
    let weigh_ins = WEIGH_IN_METRICS
        .iter()
        .map(|m| weigh_in_series(source_id, &parsed.weigh_ins, m));
    daily
        .chain(weigh_ins)
        .filter(|s| !s.points.is_empty())
        .collect()
}

/// The stored unit for a field, and the factor from Garmin's value to it.
fn unit_of(f: &Field) -> (&'static str, f64) {
    match (f.show, f.unit) {
        (Show::Duration, _) => ("s", 1.0),
        (Show::Kilometres, _) => ("m", 1.0),
        (_, "ms") => ("s", 0.001),
        (_, "min") => ("s", 60.0),
        (_, unit) => (unit, 1.0),
    }
}

fn daily_series(source_id: &str, parsed: &ParsedGarmin, f: &Field) -> TimeSeries {
    let (unit, factor) = unit_of(f);
    let points = parsed
        .days
        .iter()
        .filter_map(|(date, day)| {
            let t = day_number(date)?;
            Some(match day.values.get(f.key)? {
                DayValue::Num(v) => Point::value(t, v * factor),
                DayValue::Text(s) => Point {
                    t,
                    value: None,
                    text: Some(s.clone()),
                },
            })
        })
        .collect();
    TimeSeries {
        time_series_id: time_series_id(source_id, f.key),
        source_id: source_id.to_string(),
        entity: ENTITY.to_string(),
        metric: f.key.to_string(),
        label: f.label.to_string(),
        unit: unit.to_string(),
        time_kind: TimeKind::Day,
        points,
    }
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
        unit: "kg/m²",
        value: |w| w.bmi,
    },
];

/// Two weigh-ins at one instant keep the later one, since a point is
/// keyed by its time.
fn weigh_in_series(source_id: &str, weigh_ins: &[WeighIn], m: &WeighInMetric) -> TimeSeries {
    let mut points: Vec<Point> = Vec::new();
    for w in weigh_ins {
        let Some(v) = (m.value)(w) else { continue };
        match points.last_mut() {
            Some(last) if last.t == w.timestamp_gmt_ms => last.value = Some(v),
            _ => points.push(Point::value(w.timestamp_gmt_ms, v)),
        }
    }
    TimeSeries {
        time_series_id: time_series_id(source_id, m.key),
        source_id: source_id.to_string(),
        entity: ENTITY.to_string(),
        metric: m.key.to_string(),
        label: m.label.to_string(),
        unit: m.unit.to_string(),
        time_kind: TimeKind::Instant,
        points,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::daily::Day;

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

    fn get<'a>(all: &'a [TimeSeries], metric: &str) -> &'a TimeSeries {
        all.iter().find(|s| s.metric == metric).unwrap()
    }

    #[test]
    fn daily_fields_are_stored_by_day_in_one_unit_per_quantity() {
        let all = time_series("garmin", &parsed());
        let day = day_number("2369-04-14").unwrap();
        for (metric, unit, value) in [
            ("sleep_total", "s", 26_400.0),
            ("distance", "m", 4_200.0),
            ("hrv_night", "s", 0.045),
            ("im_moderate", "s", 1_800.0),
            ("resting_hr", "bpm", 48.0),
        ] {
            let s = get(&all, metric);
            assert_eq!(
                (s.unit.as_str(), s.time_kind),
                (unit, TimeKind::Day),
                "{metric}"
            );
            assert_eq!(s.points, vec![Point::value(day, value)], "{metric}");
        }
        assert_eq!(
            get(&all, "hrv_status").points[0].text.as_deref(),
            Some("BALANCED")
        );
    }

    #[test]
    fn weigh_ins_are_instants_and_a_repeated_instant_keeps_the_later() {
        let all = time_series("garmin", &parsed());
        let w = get(&all, "weight");
        assert_eq!(w.time_kind, TimeKind::Instant);
        assert_eq!(
            w.points,
            vec![Point::value(1_000, 80.0), Point::value(2_000, 79.4)]
        );
        assert_eq!(
            get(&all, "body_fat").points,
            vec![Point::value(2_000, 17.9)]
        );
    }

    #[test]
    fn empty_time_series_are_left_out_and_ids_differ_by_source_and_metric() {
        let all = time_series("garmin", &parsed());
        assert!(all.iter().all(|s| !s.points.is_empty()));
        assert!(!all.iter().any(|s| s.metric == "bmi"));
        assert_ne!(time_series_id("a", "weight"), time_series_id("b", "weight"));
        assert_ne!(time_series_id("a", "weight"), time_series_id("a", "bmi"));
        assert_eq!(time_series_id("a", "weight"), time_series_id("a", "weight"));
    }
}
