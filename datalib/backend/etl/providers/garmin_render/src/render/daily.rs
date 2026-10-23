//! The per-day metrics as a person reads them: which value comes from
//! which endpoint's payload, the pages they are grouped into, and how
//! each is written. Reading them is `parse.rs`; this file is pure.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use datalib_etl_render::html::escape_md_inline;
use datalib_etl_render::message::stamp_text_html;
use datalib_etl_timeseries_render::text::thousands;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Sleep,
    Heart,
    Stress,
    Movement,
    Breathing,
    Fitness,
}

impl Family {
    pub const ALL: [Family; 6] = [
        Family::Sleep,
        Family::Heart,
        Family::Stress,
        Family::Movement,
        Family::Breathing,
        Family::Fitness,
    ];

    /// The page's file stem, its plot's, and its id's natural key.
    pub fn slug(self) -> &'static str {
        match self {
            Family::Sleep => "sleep",
            Family::Heart => "heart",
            Family::Stress => "stress",
            Family::Movement => "movement",
            Family::Breathing => "breathing",
            Family::Fitness => "fitness",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Family::Sleep => "Sleep",
            Family::Heart => "Heart rate and HRV",
            Family::Stress => "Stress and Body Battery",
            Family::Movement => "Steps and activity",
            Family::Breathing => "Respiration and SpO₂",
            Family::Fitness => "Training and fitness",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// A number with this many decimals.
    Number(usize),
    /// A whole number with thousands separators.
    Count,
    /// Seconds, shown as hours and minutes and plotted in hours.
    Duration,
    /// Metres, shown and plotted in kilometres.
    Kilometres,
    /// One of Garmin's upper-case qualifiers (`BALANCED`), shown in lower case.
    Text,
}

#[derive(Debug, Clone, Copy)]
pub enum Extract {
    /// A JSON path into the day's payload.
    Path(&'static str),
    /// A field of the day's training-readiness entry: see [`pick_readiness`].
    Readiness(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub key: &'static str,
    pub family: Family,
    pub metric: &'static str,
    pub extract: Extract,
    pub label: &'static str,
    pub unit: &'static str,
    pub show: Show,
}

const fn field(
    key: &'static str,
    family: Family,
    metric: &'static str,
    path: &'static str,
    label: &'static str,
    unit: &'static str,
    show: Show,
) -> Field {
    Field {
        key,
        family,
        metric,
        extract: Extract::Path(path),
        label,
        unit,
        show,
    }
}

use Family::*;
use Show::*;

/// Every per-day value the pages show, in page order. The daily summary
/// already carries heart rate, stress, Body Battery and movement for
/// the day, so those are read from it rather than from their own
/// endpoints' minute-by-minute payloads.
#[rustfmt::skip]
pub const FIELDS: &[Field] = &[
    field("sleep_score", Sleep, "sleep", "$.dailySleepDTO.sleepScores.overall.value", "Sleep score", "", Number(0)),
    field("sleep_total", Sleep, "sleep", "$.dailySleepDTO.sleepTimeSeconds", "Asleep", "", Duration),
    field("sleep_deep", Sleep, "sleep", "$.dailySleepDTO.deepSleepSeconds", "Deep", "", Duration),
    field("sleep_light", Sleep, "sleep", "$.dailySleepDTO.lightSleepSeconds", "Light", "", Duration),
    field("sleep_rem", Sleep, "sleep", "$.dailySleepDTO.remSleepSeconds", "REM", "", Duration),
    field("sleep_awake", Sleep, "sleep", "$.dailySleepDTO.awakeSleepSeconds", "Awake", "", Duration),
    field("resting_hr", Heart, "daily_summary", "$.restingHeartRate", "Resting HR", "bpm", Number(0)),
    field("min_hr", Heart, "daily_summary", "$.minHeartRate", "Min HR", "bpm", Number(0)),
    field("max_hr", Heart, "daily_summary", "$.maxHeartRate", "Max HR", "bpm", Number(0)),
    field("hrv_night", Heart, "hrv", "$.hrvSummary.lastNightAvg", "HRV last night", "ms", Number(0)),
    field("hrv_week", Heart, "hrv", "$.hrvSummary.weeklyAvg", "HRV 7-day avg", "ms", Number(0)),
    field("hrv_status", Heart, "hrv", "$.hrvSummary.status", "HRV status", "", Text),
    field("stress_avg", Stress, "daily_summary", "$.averageStressLevel", "Avg stress", "", Number(0)),
    field("stress_max", Stress, "daily_summary", "$.maxStressLevel", "Max stress", "", Number(0)),
    field("bb_high", Stress, "daily_summary", "$.bodyBatteryHighestValue", "Body Battery high", "", Number(0)),
    field("bb_low", Stress, "daily_summary", "$.bodyBatteryLowestValue", "Body Battery low", "", Number(0)),
    field("bb_charged", Stress, "daily_summary", "$.bodyBatteryChargedValue", "Body Battery charged", "", Number(0)),
    field("bb_drained", Stress, "daily_summary", "$.bodyBatteryDrainedValue", "Body Battery drained", "", Number(0)),
    field("steps", Movement, "daily_summary", "$.totalSteps", "Steps", "", Count),
    field("distance", Movement, "daily_summary", "$.totalDistanceMeters", "Distance", "km", Kilometres),
    field("floors", Movement, "daily_summary", "$.floorsAscended", "Floors", "", Number(0)),
    field("active_kcal", Movement, "daily_summary", "$.activeKilocalories", "Active calories", "kcal", Count),
    field("total_kcal", Movement, "daily_summary", "$.totalKilocalories", "Total calories", "kcal", Count),
    field("im_moderate", Movement, "daily_summary", "$.moderateIntensityMinutes", "Moderate minutes", "min", Number(0)),
    field("im_vigorous", Movement, "daily_summary", "$.vigorousIntensityMinutes", "Vigorous minutes", "min", Number(0)),
    field("resp_waking", Breathing, "daily_summary", "$.avgWakingRespirationValue", "Waking respiration", "brpm", Number(1)),
    field("resp_sleep", Breathing, "respiration", "$.avgSleepRespirationValue", "Sleep respiration", "brpm", Number(1)),
    field("spo2_avg", Breathing, "spo2", "$.averageSpO2", "Avg SpO₂", "%", Number(0)),
    field("spo2_low", Breathing, "spo2", "$.lowestSpO2", "Lowest SpO₂", "%", Number(0)),
    Field {
        key: "readiness",
        family: Fitness,
        metric: "training_readiness",
        extract: Extract::Readiness("score"),
        label: "Training readiness",
        unit: "",
        show: Number(0),
    },
    Field {
        key: "readiness_level",
        family: Fitness,
        metric: "training_readiness",
        extract: Extract::Readiness("level"),
        label: "Readiness level",
        unit: "",
        show: Text,
    },
    field("vo2max", Fitness, "max_metrics", "$[0].generic.vo2MaxPreciseValue", "VO₂ max", "ml/kg/min", Number(1)),
    field("fitness_age", Fitness, "fitness_age", "$.fitnessAge", "Fitness age", "years", Number(1)),
];

/// A day can hold several training-readiness entries — the one Garmin
/// computes on waking, then updates after exercise and through the
/// day, in no fixed order. The waking one is the day's score as the
/// Connect app shows it; failing that, the first.
pub fn pick_readiness<'v>(payload: &'v Value, key: &str) -> Option<&'v Value> {
    let entries = payload.as_array()?;
    let entry = entries
        .iter()
        .find(|e| e["inputContext"] == "AFTER_WAKEUP_RESET")
        .or_else(|| entries.first())?;
    entry.get(key).filter(|v| !v.is_null())
}

#[derive(Debug, Clone, PartialEq)]
pub enum DayValue {
    Num(f64),
    Text(String),
}

#[derive(Debug, Default, Clone)]
pub struct Day {
    pub values: BTreeMap<&'static str, DayValue>,
    /// Seconds east of UTC the day was lived in, from the daily
    /// summary's paired GMT and local stamps; `None` without one.
    pub offset_secs: Option<i32>,
}

/// Every day with at least one value, by `YYYY-MM-DD`.
pub type Days = BTreeMap<String, Day>;

fn fields_of(family: Family) -> impl Iterator<Item = &'static Field> {
    FIELDS.iter().filter(move |f| f.family == family)
}

pub fn has_any(days: &Days, family: Family) -> bool {
    days.values()
        .any(|d| fields_of(family).any(|f| d.values.contains_key(f.key)))
}

/// The value as a table cell: no unit, which the column header carries.
pub fn cell(f: &Field, v: &DayValue) -> String {
    match (f.show, v) {
        (_, DayValue::Text(s)) => escape_md_inline(&s.to_lowercase().replace('_', " ")),
        (Number(decimals), DayValue::Num(x)) => format!("{x:.decimals$}"),
        (Count, DayValue::Num(x)) => thousands(x.round() as i64),
        (Duration, DayValue::Num(x)) => {
            let mins = (x / 60.0).round() as i64;
            format!("{}:{:02}", mins / 60, mins % 60)
        }
        (Kilometres, DayValue::Num(x)) => format!("{:.2}", x / 1000.0),
        (Text, DayValue::Num(x)) => x.to_string(),
    }
}

/// The value with its label and unit, as running text.
pub fn phrase(f: &Field, v: &DayValue) -> String {
    let value = match (f.show, v) {
        (Duration, DayValue::Num(x)) => {
            let mins = (x / 60.0).round() as i64;
            match mins / 60 {
                0 => format!("{mins} min"),
                h => format!("{h} h {:02} min", mins % 60),
            }
        }
        _ => cell(f, v),
    };
    if f.unit.is_empty() {
        format!("{} {value}", f.label)
    } else {
        format!("{} {value} {}", f.label, f.unit)
    }
}

fn column_header(f: &Field) -> String {
    match f.show {
        Duration => format!("{} (h:mm)", f.label),
        _ if f.unit.is_empty() => f.label.to_string(),
        _ => format!("{} ({})", f.label, f.unit),
    }
}

/// `label value · label value …` for one family's values on one day.
pub fn family_line(family: Family, day: &Day) -> Option<String> {
    let parts: Vec<String> = fields_of(family)
        .filter_map(|f| day.values.get(f.key).map(|v| phrase(f, v)))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The values a day's row leads with, so the grid's preview, which is
/// cut short, shows them.
const HEADLINE: &[&str] = &[
    "sleep_total",
    "sleep_score",
    "resting_hr",
    "hrv_night",
    "steps",
    "stress_avg",
    "bb_high",
    "readiness",
];

pub fn headline(day: &Day) -> Option<String> {
    let parts: Vec<String> = HEADLINE
        .iter()
        .filter_map(|k| {
            let f = FIELDS.iter().find(|f| f.key == *k)?;
            day.values.get(k).map(|v| phrase(f, v))
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// How many of the newest days a page's table shows. The plot has all
/// of them.
pub const TABLE_DAYS: usize = 90;

pub fn family_table(out: &mut String, family: Family, days: &Days) {
    let fields: Vec<&Field> = fields_of(family)
        .filter(|f| days.values().any(|d| d.values.contains_key(f.key)))
        .collect();
    out.push_str("| Day |");
    for f in &fields {
        let _ = write!(out, " {} |", column_header(f));
    }
    out.push_str("\n| --- |");
    for f in &fields {
        out.push_str(if f.show == Text { " --- |" } else { " ---: |" });
    }
    out.push('\n');
    let rows = days
        .iter()
        .rev()
        .filter(|(_, d)| fields.iter().any(|f| d.values.contains_key(f.key)))
        .take(TABLE_DAYS);
    for (date, d) in rows {
        let _ = write!(out, "| {} |", stamp_text_html(date));
        for f in &fields {
            let c = d.values.get(f.key).map(|v| cell(f, v)).unwrap_or_default();
            let _ = write!(out, " {c} |");
        }
        out.push('\n');
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn day(values: &[(&'static str, DayValue)]) -> Day {
        Day {
            values: values.iter().cloned().collect(),
            offset_secs: None,
        }
    }

    #[test]
    fn every_field_key_is_unique_and_every_metric_is_one_the_ingest_walks() {
        let mut keys: Vec<_> = FIELDS.iter().map(|f| f.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), FIELDS.len());
        for f in FIELDS {
            assert!(
                datalib_etl_garmin_config::DAILY_METRICS.contains(&f.metric),
                "{} reads {}",
                f.key,
                f.metric
            );
        }
    }

    #[test]
    fn the_waking_readiness_entry_wins_wherever_it_sits() {
        let p = json!([
            {"inputContext": "UPDATE_REALTIME_VARIABLES", "score": 40},
            {"inputContext": "AFTER_WAKEUP_RESET", "score": 71, "level": "MODERATE"},
        ]);
        assert_eq!(pick_readiness(&p, "score"), Some(&json!(71)));
        let only = json!([{"inputContext": "AFTER_POST_EXERCISE_RESET", "score": 33}]);
        assert_eq!(pick_readiness(&only, "score"), Some(&json!(33)));
        assert_eq!(pick_readiness(&json!(null), "score"), None);
    }

    #[test]
    fn values_read_as_a_person_would_say_them() {
        let by = |k| FIELDS.iter().find(|f| f.key == k).unwrap();
        assert_eq!(
            phrase(by("sleep_total"), &DayValue::Num(25_920.0)),
            "Asleep 7 h 12 min"
        );
        assert_eq!(
            phrase(by("sleep_awake"), &DayValue::Num(600.0)),
            "Awake 10 min"
        );
        assert_eq!(phrase(by("steps"), &DayValue::Num(10234.0)), "Steps 10,234");
        assert_eq!(
            phrase(by("distance"), &DayValue::Num(8046.7)),
            "Distance 8.05 km"
        );
        assert_eq!(
            phrase(by("resting_hr"), &DayValue::Num(52.0)),
            "Resting HR 52 bpm"
        );
        assert_eq!(
            phrase(
                by("hrv_status"),
                &DayValue::Text("<b>UN_BALANCED</b>".into())
            ),
            "HRV status &lt;b&gt;un balanced&lt;/b&gt;"
        );
    }

    #[test]
    fn a_family_page_charts_one_quantity_each_and_tables_only_its_columns() {
        let mut days = Days::new();
        days.insert(
            "2369-04-13".into(),
            day(&[
                ("sleep_score", DayValue::Num(84.0)),
                ("sleep_total", DayValue::Num(26_400.0)),
                ("steps", DayValue::Num(9120.0)),
            ]),
        );
        use crate::render::time_series::{family_charts, time_series_id};
        assert_eq!(
            family_charts("g", Family::Sleep, &days),
            [
                vec![time_series_id("g", "sleep_score")],
                vec![time_series_id("g", "sleep_total")],
            ],
            "a score and a duration, each its own chart"
        );
        assert!(family_charts("g", Family::Breathing, &days).is_empty());

        let mut out = String::new();
        family_table(&mut out, Family::Sleep, &days);
        assert!(
            out.starts_with("| Day | Sleep score | Asleep (h:mm) |\n"),
            "{out}"
        );
        assert!(
            out.contains("| <time datetime=\"2369-04-13\">Sun Apr 13th, 2369</time> | 84 | 7:20 |"),
            "{out}"
        );
        assert!(!out.contains("Steps"), "{out}");
    }
}
