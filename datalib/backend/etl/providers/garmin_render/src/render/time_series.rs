//! Garmin's time series: one per daily field in [`FIELDS`], by calendar
//! day, and one per weigh-in measurement, by instant. Values keep
//! Garmin's own unit, named in UCUM; a reader converts between units of
//! one quantity.

use std::path::Path;

use anyhow::Result;
use datalib_etl_render::inputs::RawRange;
use datalib_id::{composite_key, entity_id_str, IdNamespace};
use datalib_time_series_schema::{day_number, Points, TimeKind, TimeSeries};

use super::daily::{DayValue, Field, Show, FIELDS};
use super::parse::{parse, ParsedGarmin, WeighIn};

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
}
