//! Raw-store schema for the AirVisual provider: one row per device and
//! one row per sample, a typed column per measurement. A blank cell in
//! the history file is a NULL here; the whole line is kept as the
//! payload beside them.

use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl_macros::RawTable;

pub const DATA_TABLES: &[&str] = &[
    "airvisual_devices",
    "airvisual_samples",
    "airvisual_unplaced_samples",
];

/// Prefix of the `file_checkpoint` scopes holding which history files
/// this source has finished with — one scope per device, since two Pros
/// name their files identically.
pub const CURSOR_SCOPE_PREFIX: &str = "airvisual/export/";

pub fn cursor_scope(device_id: &str) -> String {
    format!("{CURSOR_SCOPE_PREFIX}{device_id}")
}

/// One row per device. `id` is the serial number: the device's own
/// identity, which keys every sample. `name` is what a person calls it
/// — the config's `name`, else the device's own `node_name` — and may
/// change without re-keying anything. The rest comes from the folder's
/// `latest_config_measurements.json` when it is there; `last_ts_ms` is
/// the newest sample stored, rewritten after each run.
#[derive(Debug, Clone, Default, RawTable)]
#[raw_table(table = "airvisual_devices")]
pub struct AirvisualDeviceRow {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub mac_address: Option<String>,
    pub app_version: Option<String>,
    pub system_version: Option<String>,
    pub timezone: Option<String>,
    pub last_ts_ms: Option<i64>,
}

/// One row per line of a history file. The measurement columns are the
/// device's own, in its own units; `Temperature(F)` is left to the
/// payload because it is `temperature_c` converted. The `outdoor_*`
/// pair is the followed public station's index, not this sensor's.
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "airvisual_samples",
    index = "airvisual_samples_by_device_ts:device_id,ts_ms"
)]
pub struct AirvisualSampleRow {
    pub id_and_payload: WirePayload,
    /// The device's serial — `airvisual_devices.id`.
    pub device_id: String,
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
    /// The history file this row was last read from, relative to the
    /// export path — which of a `corrupt_` / `restored_` pair won.
    pub source_file: String,
}

/// A line logged before the device's clock was set. Its timestamp is
/// seconds since that boot, not since 1970, and every pre-clock boot
/// counts from zero again — so it is keyed on its place in the file,
/// which is stable because the device only appends. Kept whole so a
/// re-ingest is idempotent and nothing the device wrote is thrown away.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "airvisual_unplaced_samples")]
pub struct AirvisualUnplacedSampleRow {
    pub id_and_payload: WirePayload,
    pub device_id: String,
    pub source_file: String,
    /// 1-based line number in `source_file`, the header being line 1.
    pub line_no: i64,
    /// The `Timestamp` cell as logged: seconds since the boot.
    pub device_ts_s: i64,
}

/// Same shape as `yolink_readings`' id, minus the metric, so a
/// time-series consumer keys every device's samples the same way.
pub fn sample_id_recipe(device_id: &str, ts_ms: i64) -> String {
    format!("{device_id}#{ts_ms}")
}

pub fn unplaced_id_recipe(device_id: &str, source_file: &str, line_no: i64) -> String {
    format!("{device_id}#{source_file}#{line_no}")
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![AirvisualDeviceRow::ddl()];
    out.extend(AirvisualSampleRow::all_ddl());
    out.push(AirvisualUnplacedSampleRow::ddl());
    out.push(datalib_etl::file_checkpoint::INGESTED_FILES_DDL.to_string());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_ddl_covers_every_table() {
        let blob = full_ddl().join("\n");
        for t in DATA_TABLES {
            assert!(
                blob.contains(&format!("CREATE TABLE IF NOT EXISTS {t} (")),
                "{t}"
            );
            assert!(
                blob.contains(&format!("{t}_bookkeeping")),
                "{t} bookkeeping"
            );
        }
        assert!(blob.contains("airvisual_samples_by_device_ts"));
        assert!(blob.contains("ingested_files"));
        let pm25 = blob
            .lines()
            .find(|l| l.trim_start().starts_with("pm25_ugm3"))
            .expect("pm25_ugm3 column");
        assert!(pm25.trim_end_matches(',').ends_with("REAL NULL"), "{pm25}");
    }
}
