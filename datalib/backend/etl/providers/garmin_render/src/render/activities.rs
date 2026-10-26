//! One activity as a person reads it: what it was, when, and the
//! numbers Garmin summarised it with. Pure; `render.rs` writes it.

use std::fmt::Write as _;

use datalib_etl_render::html::escape_md_inline;
use datalib_etl_render::message::stamp_text_html;
use datalib_etl_timeseries_render::text::thousands;
use serde_json::Value;

use super::parse::Activity;
use super::stamps::{gmt_ms, local_iso, offset_secs};

impl Activity {
    /// The listing's value, else the detail's summary's: the listing is
    /// what every activity has, the detail what an older one may only
    /// have.
    fn num(&self, listing_key: &str, summary_key: &str) -> Option<f64> {
        self.listing[listing_key]
            .as_f64()
            .or_else(|| self.detail["summaryDTO"][summary_key].as_f64())
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.listing[key]
            .as_str()
            .or_else(|| self.detail[key].as_str())
            .filter(|s| !s.is_empty())
    }

    pub fn name(&self) -> String {
        self.text("activityName")
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} {}", self.type_label(), self.id))
    }

    pub fn type_key(&self) -> Option<&str> {
        self.listing["activityType"]["typeKey"]
            .as_str()
            .or_else(|| self.detail["activityTypeDTO"]["typeKey"].as_str())
    }

    pub fn type_label(&self) -> String {
        humane(self.type_key().unwrap_or("activity"))
    }

    pub fn start_gmt_ms(&self) -> Option<i64> {
        gmt_ms(self.text("startTimeGMT")?)
    }

    /// The start as the wall clock where it happened, with that
    /// place's offset.
    pub fn start_iso(&self) -> Option<String> {
        let gmt = self.text("startTimeGMT")?;
        let local = self.text("startTimeLocal")?;
        local_iso(local, offset_secs(gmt, local)?)
    }

    pub fn device_id(&self) -> Option<String> {
        match &self.listing["deviceId"] {
            Value::Number(n) => Some(n.to_string()),
            Value::String(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// `(label, value)` pairs, in the order a summary reads them, as
    /// plain text.
    pub fn facts(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        let mut push = |label: &'static str, v: Option<String>| {
            if let Some(v) = v {
                out.push((label, v));
            }
        };
        push("Type", Some(self.type_label()));
        push("Start", self.text("startTimeLocal").map(str::to_string));
        push("Location", self.text("locationName").map(str::to_string));
        let distance = self.num("distance", "distance").filter(|d| *d > 0.0);
        push(
            "Distance",
            distance.map(|m| format!("{:.2} km", m / 1000.0)),
        );
        let duration = self.num("duration", "duration");
        push("Duration", duration.map(hms));
        push(
            "Moving time",
            self.num("movingDuration", "movingDuration").map(hms),
        );
        push("Pace", pace(self.type_key(), distance, duration));
        push(
            "Avg HR",
            self.num("averageHR", "averageHR")
                .map(|v| format!("{v:.0} bpm")),
        );
        push(
            "Max HR",
            self.num("maxHR", "maxHR").map(|v| format!("{v:.0} bpm")),
        );
        push(
            "Calories",
            self.num("calories", "calories")
                .map(|v| format!("{} kcal", thousands(v.round() as i64))),
        );
        push(
            "Elevation gain",
            self.num("elevationGain", "elevationGain")
                .map(|v| format!("{v:.0} m")),
        );
        push(
            "Steps",
            self.num("steps", "steps")
                .map(|v| thousands(v.round() as i64)),
        );
        push(
            "Aerobic training effect",
            self.num("aerobicTrainingEffect", "trainingEffect")
                .map(|v| format!("{v:.1}")),
        );
        push(
            "Anaerobic training effect",
            self.num("anaerobicTrainingEffect", "anaerobicTrainingEffect")
                .map(|v| format!("{v:.1}")),
        );
        push(
            "Training load",
            self.num("activityTrainingLoad", "activityTrainingLoad")
                .map(|v| format!("{v:.0}")),
        );
        push(
            "Body Battery",
            self.num("differenceBodyBattery", "differenceBodyBattery")
                .map(|v| format!("{v:+.0}")),
        );
        out
    }
}

/// One of Garmin's `snake_case` keys as words: `lap_swimming` →
/// `Lap swimming`.
pub fn humane(key: &str) -> String {
    let key = key.replace('_', " ");
    let mut chars = key.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => key,
    }
}

/// Seconds as `h:mm:ss`, or `m:ss` under an hour.
pub fn hms(secs: f64) -> String {
    let s = secs.round() as i64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Minutes per km on foot, per 100 m in the water, km/h otherwise.
fn pace(type_key: Option<&str>, distance_m: Option<f64>, secs: Option<f64>) -> Option<String> {
    let (d, t) = (distance_m?, secs?);
    if d <= 0.0 || t <= 0.0 {
        return None;
    }
    let key = type_key.unwrap_or("");
    if key.contains("swim") {
        Some(format!("{} /100 m", hms(t / d * 100.0)))
    } else if ["running", "walking", "hiking"]
        .iter()
        .any(|k| key.contains(k))
    {
        Some(format!("{} /km", hms(t / d * 1000.0)))
    } else {
        Some(format!("{:.1} km/h", d / 1000.0 / (t / 3600.0)))
    }
}

/// The body of the activity's page, below its title.
pub fn body(out: &mut String, a: &Activity, device: Option<&str>) {
    out.push_str("| | |\n| --- | --- |\n");
    for (label, value) in a.facts() {
        let value = match (label, a.start_iso()) {
            ("Start", Some(start)) => stamp_text_html(&start),
            _ => escape_md_inline(&value),
        };
        let _ = writeln!(out, "| {label} | {value} |");
    }
    if let Some(d) = device {
        let _ = writeln!(out, "| Device | {} |", escape_md_inline(d));
    }
    let _ = writeln!(
        out,
        "| Original FIT file | {} |",
        if a.fit_blake3.is_some() {
            "kept"
        } else {
            "not downloaded"
        }
    );
    let _ = writeln!(out, "| Garmin activity id | {} |", escape_md_inline(&a.id));
    out.push('\n');
}

/// The grid row's text: the name, then the facts on one line.
pub fn row_text(a: &Activity) -> String {
    let facts: Vec<String> = a
        .facts()
        .into_iter()
        .filter(|(label, _)| *label != "Type" && *label != "Start")
        .map(|(label, v)| format!("{label} {v}"))
        .collect();
    format!("{}\n{} · {}", a.name(), a.type_label(), facts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn activity(listing: Value) -> Activity {
        Activity {
            id: "17010413001".into(),
            listing,
            detail: json!({"summaryDTO": {"trainingEffect": 3.4}}),
            fit_blake3: Some("b3".into()),
        }
    }

    #[test]
    fn a_run_reads_as_distance_time_and_pace_in_its_own_time_zone() {
        let a = activity(json!({
            "activityName": "Holodeck run: Dixon Hill",
            "activityType": {"typeKey": "running"},
            "startTimeGMT": "2369-04-13 06:30:00",
            "startTimeLocal": "2369-04-13 07:30:00",
            "distance": 8046.7,
            "duration": 2412.0,
            "averageHR": 148.0,
        }));
        assert_eq!(a.start_iso().as_deref(), Some("2369-04-13T07:30:00+01:00"));
        let facts = a.facts();
        let get = |l| facts.iter().find(|(k, _)| *k == l).map(|(_, v)| v.as_str());
        assert_eq!(get("Distance"), Some("8.05 km"));
        assert_eq!(get("Duration"), Some("40:12"));
        assert_eq!(get("Pace"), Some("5:00 /km"));
        assert_eq!(
            get("Aerobic training effect"),
            Some("3.4"),
            "from the detail"
        );
        assert!(row_text(&a).starts_with("Holodeck run: Dixon Hill\nRunning · Distance 8.05 km"));
    }

    #[test]
    fn a_swim_is_paced_per_100_m_and_a_ride_in_km_per_hour() {
        assert_eq!(
            pace(Some("lap_swimming"), Some(1000.0), Some(1200.0)).as_deref(),
            Some("2:00 /100 m")
        );
        assert_eq!(
            pace(Some("cycling"), Some(24140.0), Some(3600.0)).as_deref(),
            Some("24.1 km/h")
        );
        assert_eq!(
            pace(Some("strength_training"), Some(0.0), Some(1200.0)),
            None
        );
    }

    #[test]
    fn an_unnamed_activity_is_called_by_its_type() {
        let a = activity(json!({"activityType": {"typeKey": "lap_swimming"}}));
        assert_eq!(a.name(), "Lap swimming 17010413001");
    }

    #[test]
    fn text_from_garmin_is_escaped_in_the_table() {
        let a = activity(json!({"locationName": "<b>Ten</b> Forward | bar"}));
        let mut out = String::new();
        body(&mut out, &a, Some("<i>x</i>"));
        assert!(!out.contains("<b>") && !out.contains("<i>"), "{out}");
        assert!(out.contains("\\|"), "{out}");
    }
}
