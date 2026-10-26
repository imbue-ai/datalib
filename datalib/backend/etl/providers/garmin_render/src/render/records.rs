//! The account's listings — personal records, badges, gear, workouts,
//! goals — as one line each. Pure; `render.rs` writes them.

use datalib_etl_timeseries_render::text::thousands;
use serde_json::Value;

use super::activities::{hms, humane};
use super::parse::Item;

/// The listings in page order: `garmin_items.kind`, the heading, and
/// the grid's Kind for one of its rows.
pub const KINDS: &[(&str, &str, &str)] = &[
    ("personal_records", "Personal records", "Garmin Record"),
    ("badges", "Badges", "Garmin Badge"),
    ("gear", "Gear", "Garmin Gear"),
    ("workouts", "Workouts", "Garmin Workout"),
    ("goals", "Goals", "Garmin Goal"),
];

enum RecordUnit {
    Time,
    Distance,
    Count,
    Days,
}

/// The record types whose meaning is known, by Garmin's `typeId`. Any
/// other is shown by its number and raw value rather than guessed at.
fn record_type(type_id: i64) -> Option<(&'static str, RecordUnit)> {
    use RecordUnit::*;
    Some(match type_id {
        1 => ("Fastest 1 km", Time),
        2 => ("Fastest mile", Time),
        3 => ("Fastest 5 km", Time),
        4 => ("Fastest 10 km", Time),
        5 => ("Fastest half marathon", Time),
        6 => ("Fastest marathon", Time),
        7 => ("Longest run", Distance),
        8 => ("Longest ride", Distance),
        12 => ("Most steps in a day", Count),
        13 => ("Most steps in a week", Count),
        14 => ("Most steps in a month", Count),
        15 => ("Longest goal streak", Days),
        _ => return None,
    })
}

fn first_str<'v>(v: &'v Value, keys: &[&str]) -> Option<&'v str> {
    keys.iter()
        .find_map(|k| v[*k].as_str())
        .filter(|s| !s.is_empty())
}

/// At most two decimals, and none that are zero.
fn plain_number(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

impl Item {
    /// What the item is called, then what it says: one line of plain text.
    pub fn line(&self, activity_name: impl Fn(&str) -> Option<String>) -> (String, String) {
        let p = &self.payload;
        match self.kind.as_str() {
            "personal_records" => {
                let type_id = p["typeId"].as_i64().unwrap_or_default();
                let value = p["value"].as_f64();
                let (name, shown) = match (record_type(type_id), value) {
                    (Some((name, unit)), Some(v)) => (
                        name.to_string(),
                        match unit {
                            RecordUnit::Time => hms(v),
                            RecordUnit::Distance => format!("{:.2} km", v / 1000.0),
                            RecordUnit::Count => thousands(v.round() as i64),
                            RecordUnit::Days => format!("{v:.0} days"),
                        },
                    ),
                    (Some((name, _)), None) => (name.to_string(), String::new()),
                    (None, v) => (
                        format!("Record type {type_id}"),
                        v.map(|v| format!("value {}", plain_number(v)))
                            .unwrap_or_default(),
                    ),
                };
                let mut said = shown;
                if let Some(t) = p["activityType"].as_str() {
                    said = format!("{said} · {}", humane(t));
                }
                let activity = match &p["activityId"] {
                    Value::Number(n) => activity_name(&n.to_string()),
                    _ => None,
                };
                if let Some(a) = activity {
                    said = format!("{said} · in {a}");
                }
                if let Some(d) = p["prStartTimeLocalFormatted"].as_str() {
                    said = format!("{said} · {}", d.get(..10).unwrap_or(d));
                }
                (name, said.trim_start_matches(" · ").to_string())
            }
            "badges" => {
                let name = first_str(p, &["badgeName"])
                    .unwrap_or(&self.upstream_id)
                    .to_string();
                let mut parts = Vec::new();
                if let Some(d) = p["badgeEarnedDate"].as_str() {
                    parts.push(format!("earned {}", d.get(..10).unwrap_or(d)));
                }
                if let Some(n) = p["badgeEarnedNumber"].as_i64().filter(|n| *n > 1) {
                    parts.push(format!("{n} times"));
                }
                if let Some(pts) = p["badgePoints"].as_i64() {
                    parts.push(format!("{pts} points"));
                }
                (name, parts.join(" · "))
            }
            "gear" => {
                let name = first_str(p, &["displayName", "customMakeModel"])
                    .unwrap_or(&self.upstream_id)
                    .to_string();
                let mut parts = Vec::new();
                if let Some(t) = first_str(p, &["gearTypeName"]) {
                    parts.push(t.to_string());
                }
                if let Some(s) = first_str(p, &["gearStatusName"]) {
                    parts.push(s.to_string());
                }
                if let Some(m) = p["maximumMeters"].as_f64().filter(|m| *m > 0.0) {
                    parts.push(format!("retire at {:.0} km", m / 1000.0));
                }
                (name, parts.join(" · "))
            }
            "workouts" => {
                let name = first_str(p, &["workoutName"])
                    .unwrap_or(&self.upstream_id)
                    .to_string();
                let mut parts = Vec::new();
                if let Some(s) = p["sportType"]["sportTypeKey"].as_str() {
                    parts.push(humane(s));
                }
                if let Some(secs) = p["estimatedDurationInSecs"].as_f64() {
                    parts.push(format!("about {}", hms(secs)));
                }
                (name, parts.join(" · "))
            }
            _ => {
                let name = first_str(p, &["goalName", "name", "displayName"])
                    .unwrap_or(&self.upstream_id)
                    .to_string();
                (name, String::new())
            }
        }
    }

    /// When a personal record was set, in the zone it was set in.
    pub fn created_at(&self) -> Option<String> {
        if self.kind != "personal_records" {
            return None;
        }
        let gmt = self.payload["prStartTimeGmt"].as_i64()?;
        let local = self.payload["prStartTimeLocal"].as_i64()?;
        let offset = i32::try_from((local - gmt) / 1000).ok()?;
        let tz = chrono::FixedOffset::east_opt(offset)?;
        chrono::DateTime::from_timestamp_millis(gmt).map(|t| t.with_timezone(&tz).to_rfc3339())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(kind: &str, payload: Value) -> Item {
        Item {
            id: format!("{kind}#1"),
            kind: kind.into(),
            upstream_id: "1".into(),
            payload,
        }
    }

    #[test]
    fn a_known_record_is_named_and_an_unknown_one_is_not_guessed_at() {
        let pr = item(
            "personal_records",
            json!({"typeId": 3, "value": 1290.0, "activityType": "running", "activityId": 17010413001_i64,
                   "prStartTimeGmt": 12600081000000_i64, "prStartTimeLocal": 12600084600000_i64}),
        );
        let (name, said) = pr.line(|id| (id == "17010413001").then(|| "Holodeck run".to_string()));
        assert_eq!(name, "Fastest 5 km");
        assert_eq!(said, "21:30 · Running · in Holodeck run");
        assert!(
            pr.created_at().unwrap().ends_with("+01:00"),
            "{:?}",
            pr.created_at()
        );

        let odd = item(
            "personal_records",
            json!({"typeId": 99, "value": 223.48699951171875}),
        );
        assert_eq!(
            odd.line(|_| None),
            ("Record type 99".to_string(), "value 223.49".to_string())
        );
        let whole = item("personal_records", json!({"typeId": 99, "value": 1000.0}));
        assert_eq!(whole.line(|_| None).1, "value 1000");
    }

    #[test]
    fn a_badge_a_shoe_and_a_workout_each_read_as_one_line() {
        let badge = item(
            "badges",
            json!({"badgeName": "First Marathon", "badgeEarnedDate": "2368-11-02T09:00:00.0", "badgePoints": 8}),
        );
        assert_eq!(badge.line(|_| None).1, "earned 2368-11-02 · 8 points");
        let shoe = item(
            "gear",
            json!({"displayName": "Starfleet issue trainers", "gearTypeName": "Shoes", "maximumMeters": 800000.0}),
        );
        assert_eq!(shoe.line(|_| None).1, "Shoes · retire at 800 km");
        let w = item(
            "workouts",
            json!({"workoutName": "Tempo intervals", "sportType": {"sportTypeKey": "running"}, "estimatedDurationInSecs": 2700}),
        );
        assert_eq!(
            w.line(|_| None),
            ("Tempo intervals".into(), "Running · about 45:00".into())
        );
    }
}
