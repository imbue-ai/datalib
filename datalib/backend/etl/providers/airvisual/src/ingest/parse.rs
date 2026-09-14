//! The AirVisual Pro's history files, `YYYYMM_AirVisual_values.txt`:
//! semicolon-separated, one header line, every line ending in a `;`.
//! Two header variants exist (see `INGEST.md`), and the epoch
//! `Timestamp` column is the only reliable time — `Date`/`Time` are
//! local and change format between files.

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use tracing::warn;

/// A sample recorded before the clock was set reads as January 1970;
/// nothing before this is a reading anyone can place in time.
const CLOCK_SET_AFTER_S: i64 = 946_684_800; // 2000-01-01T00:00:00Z

/// One line of a history file, typed. A `None` is a cell the device
/// left blank (sensor off) or marked `-1`.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Sample {
    pub ts_ms: i64,
    pub pm25_ugm3: Option<f64>,
    pub pm10_ugm3: Option<f64>,
    pub pm1_ugm3: Option<f64>,
    pub aqi_us: Option<f64>,
    pub aqi_cn: Option<f64>,
    pub outdoor_aqi_us: Option<f64>,
    pub outdoor_aqi_cn: Option<f64>,
    pub temperature_c: Option<f64>,
    pub humidity_pct: Option<f64>,
    pub co2_ppm: Option<f64>,
    pub voc_ppb: Option<f64>,
    /// `{header: value}` for the whole line.
    pub payload: String,
}

/// Header → the column it fills. A header not listed here stays in the
/// payload only; the parser warns once per file so a firmware that adds
/// a column is noticed rather than silently kept as JSON.
/// `Temperature(F)` is deliberately absent: it is `Temperature(C)`
/// converted, and the payload keeps it.
type Setter = fn(&mut Sample, f64);

const COLUMNS: &[(&str, Setter)] = &[
    ("PM2_5(ug/m3)", |s, v| s.pm25_ugm3 = Some(v)),
    ("PM10(ug/m3)", |s, v| s.pm10_ugm3 = Some(v)),
    ("PM1(ug/m3)", |s, v| s.pm1_ugm3 = Some(v)),
    ("PM01(ug/m3)", |s, v| s.pm1_ugm3 = Some(v)),
    ("AQI(US)", |s, v| s.aqi_us = Some(v)),
    ("AQI(CN)", |s, v| s.aqi_cn = Some(v)),
    ("Outdoor AQI(US)", |s, v| s.outdoor_aqi_us = Some(v)),
    ("Outdoor AQI(CN)", |s, v| s.outdoor_aqi_cn = Some(v)),
    ("Temperature(C)", |s, v| s.temperature_c = Some(v)),
    ("Humidity(%RH)", |s, v| s.humidity_pct = Some(v)),
    ("CO2(ppm)", |s, v| s.co2_ppm = Some(v)),
    ("VOC(ppb)", |s, v| s.voc_ppb = Some(v)),
];

const IGNORED_HEADERS: &[&str] = &["Date", "Time", "Timestamp", "Temperature(F)"];

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct ParseStats {
    pub lines: usize,
    pub samples: usize,
    /// Cells holding the device's "no reading" markers: empty, or `-1`.
    pub sentinels: usize,
    /// Lines stamped before the clock was set (the 1970 file).
    pub clock_unset: usize,
    /// Lines with the wrong field count, a non-numeric timestamp, or a
    /// cell that would not parse.
    pub bad_lines: usize,
}

pub struct Parsed {
    pub samples: Vec<Sample>,
    pub stats: ParseStats,
}

pub fn parse(body: &str, file_label: &str) -> Result<Parsed> {
    // The month being written ends in a block of NULs the device has
    // reserved but not yet filled.
    let body = body.trim_end_matches('\0');
    let mut lines = body.lines();
    let header_line = lines.next().ok_or_else(|| anyhow!("empty file"))?;
    let headers = split(header_line);
    let time_idx = headers
        .iter()
        .position(|h| *h == "Timestamp")
        .ok_or_else(|| anyhow!("no Timestamp column in header {header_line:?}"))?;

    let mut columns: Vec<(usize, Setter)> = Vec::new();
    for (i, h) in headers.iter().enumerate() {
        if let Some((_, set)) = COLUMNS.iter().find(|(name, _)| name == h) {
            columns.push((i, *set));
        } else if !IGNORED_HEADERS.contains(h) {
            warn!(
                event = "airvisual_unknown_column",
                file = file_label,
                column = *h,
                "column kept in the payload but not typed; add it to COLUMNS to store it",
            );
        }
    }
    if columns.is_empty() {
        bail!("no known measurement columns in header {header_line:?}");
    }

    let mut out = Vec::new();
    let mut stats = ParseStats::default();
    for (n, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        stats.lines += 1;
        let fields = split(line);
        let ts_s = fields.get(time_idx).and_then(|s| s.parse::<i64>().ok());
        let (Some(ts_s), true) = (ts_s, fields.len() == headers.len()) else {
            stats.bad_lines += 1;
            warn!(
                event = "airvisual_bad_line",
                file = file_label,
                line = n + 2,
                text = line,
            );
            continue;
        };
        if ts_s < CLOCK_SET_AFTER_S {
            stats.clock_unset += 1;
            continue;
        }
        let mut sample = Sample {
            ts_ms: ts_s * 1000,
            ..Default::default()
        };
        let mut bad = false;
        for (idx, set) in &columns {
            let raw = fields[*idx];
            if raw.is_empty() || raw == "-1" || raw == "-1.0" {
                stats.sentinels += 1;
                continue;
            }
            match raw.parse::<f64>() {
                Ok(v) => set(&mut sample, v),
                Err(_) => {
                    bad = true;
                    warn!(
                        event = "airvisual_bad_value",
                        file = file_label,
                        line = n + 2,
                        column = headers[*idx],
                        value = raw,
                    );
                }
            }
        }
        if bad {
            stats.bad_lines += 1;
        }
        let mut m = serde_json::Map::with_capacity(headers.len());
        for (h, v) in headers.iter().zip(&fields) {
            m.insert(h.to_string(), serde_json::Value::String(v.to_string()));
        }
        sample.payload = serde_json::Value::Object(m).to_string();
        out.push(sample);
    }
    stats.samples = out.len();
    Ok(Parsed {
        samples: out,
        stats,
    })
}

/// Split on `;`, dropping the one trailing separator every line carries.
/// Only one: a run of `;;` at the end is empty cells, not decoration.
fn split(line: &str) -> Vec<&str> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let line = line.strip_suffix(';').unwrap_or(line);
    line.split(';').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "Date;Time;Timestamp;PM2_5(ug/m3);AQI(US);AQI(CN);PM10(ug/m3);PM1(ug/m3);Outdoor AQI(US);Outdoor AQI(CN);Temperature(C);Temperature(F);Humidity(%RH);CO2(ppm);\n\
        2026/09/01;00:00:36;1788220836;1.0;6;1;1.0;1.0;0;0;23.5;74.3;48;425;\n\
        2026/09/01;00:15:36;1788221736;;;;;;56;17;;;;;\n\
        2026/09/01;00:30:36;1788222636;-1.0;-1;-1;;;56;17;;;;;\n";

    const RESTORED: &str =
        "Date;Time;Timestamp;PM2_5(ug/m3);PM10(ug/m3);PM1(ug/m3);Temperature(C);CO2(ppm);\n\
        9/1/2025;00:00:00;1756684800;14.0;17.0;17.0;21.6;423;\n";

    #[test]
    fn full_header() {
        let p = parse(FULL, "t").unwrap();
        insta::assert_yaml_snapshot!(p.samples);
        insta::assert_yaml_snapshot!("full_header_stats", p.stats);
    }

    #[test]
    fn restored_short_header() {
        let p = parse(RESTORED, "t").unwrap();
        assert_eq!(p.samples.len(), 1);
        assert_eq!(p.samples[0].ts_ms, 1_756_684_800_000);
        assert_eq!(
            p.samples[0].humidity_pct, None,
            "the short header has no humidity"
        );
        insta::assert_yaml_snapshot!(p.samples);
    }

    #[test]
    fn a_sensor_off_line_keeps_only_the_outdoor_columns() {
        let p = parse(FULL, "t").unwrap();
        let s = &p.samples[1];
        assert_eq!(s.ts_ms, 1_788_221_736_000);
        assert_eq!(
            (s.pm25_ugm3, s.co2_ppm, s.temperature_c),
            (None, None, None)
        );
        assert_eq!(
            (s.outdoor_aqi_us, s.outdoor_aqi_cn),
            (Some(56.0), Some(17.0))
        );
    }

    #[test]
    fn minus_one_is_a_sentinel_not_a_value() {
        let p = parse(FULL, "t").unwrap();
        let s = &p.samples[2];
        assert_eq!((s.pm25_ugm3, s.aqi_us, s.aqi_cn), (None, None, None));
        assert_eq!(p.stats.sentinels, 8 + 8);
    }

    #[test]
    fn pre_clock_lines_are_dropped_and_counted() {
        let body = "Date;Time;Timestamp;PM2_5(ug/m3);CO2(ppm);\n\
            1970/01/01;00:04:14;254;0.0;683;\n\
            2026/09/01;00:00:36;1788220836;1.0;425;\n";
        let p = parse(body, "t").unwrap();
        assert_eq!(p.stats.clock_unset, 1);
        assert_eq!(p.samples.len(), 1);
    }

    #[test]
    fn a_truncated_last_line_costs_one_line_not_the_file() {
        let body = "Date;Time;Timestamp;PM2_5(ug/m3);CO2(ppm);\n\
            2026/09/01;00:00:36;1788220836;1.0;425;\n\
            2026/09/01;00:15:36;17882";
        let p = parse(body, "t").unwrap();
        assert_eq!(p.stats.bad_lines, 1);
        assert_eq!(p.samples.len(), 1);
    }

    #[test]
    fn the_live_files_reserved_nul_tail_is_not_a_line() {
        let body = "Date;Time;Timestamp;PM2_5(ug/m3);CO2(ppm);\n\
            2026/09/01;00:00:36;1788220836;1.0;425;\n\0\0\0\0\0\0\0\0";
        let p = parse(body, "t").unwrap();
        assert_eq!(
            (p.stats.lines, p.stats.bad_lines, p.samples.len()),
            (1, 0, 1)
        );
    }

    #[test]
    fn the_payload_keeps_every_column() {
        let p = parse(FULL, "t").unwrap();
        let payload: serde_json::Value = serde_json::from_str(&p.samples[0].payload).unwrap();
        assert_eq!(payload["Temperature(F)"], "74.3");
        assert_eq!(payload["Date"], "2026/09/01");
        assert_eq!(payload.as_object().unwrap().len(), 14);
    }

    #[test]
    fn a_file_without_a_timestamp_column_is_an_error() {
        assert!(parse("Date;Time;PM2_5(ug/m3);\n", "t").is_err());
        assert!(parse("", "t").is_err());
    }
}
