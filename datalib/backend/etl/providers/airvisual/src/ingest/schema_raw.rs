//! Raw-store schema for the AirVisual provider: one row per device and
//! one row per sample, a typed column per measurement. A blank cell in
//! the history file is a NULL here; the whole line is kept as the
//! payload beside them.

use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl_macros::RawTable;

pub const DATA_TABLES: &[&str] = &["airvisual_devices", "airvisual_samples"];

/// The `file_checkpoint` scope holding which history files this source
/// has finished with.
pub const CURSOR_SCOPE: &str = "airvisual/export";

/// One row per device. `id` is the configured (or self-reported) device
/// name; the other columns come from the folder's
/// `latest_config_measurements.json` when it is there. `last_ts_ms` is
/// the newest sample stored, rewritten after each run.
#[derive(Debug, Clone, Default, RawTable)]
#[raw_table(table = "airvisual_devices")]
pub struct AirvisualDeviceRow {
    pub id: String,
    pub serial_number: Option<String>,
    pub model: Option<String>,
    pub timezone: Option<String>,
    pub node_name: Option<String>,
    pub last_ts_ms: Option<i64>,
}

/// One row per line of a history file. The measurement columns are the
/// device's own, in its own units; `Temperature(F)` is left to the
/// payload because it is `temperature_c` converted. The `outdoor_*`
/// pair is the followed public station's index, not this sensor's.
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "airvisual_samples",
    index = "airvisual_samples_by_device_ts:device_name,ts_ms"
)]
pub struct AirvisualSampleRow {
    pub id_and_payload: WirePayload,
    pub device_name: String,
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

/// Same shape as `yolink_readings`' id, minus the metric, so a
/// time-series consumer keys every device's samples the same way.
pub fn sample_id_recipe(device_name: &str, ts_ms: i64) -> String {
    format!("{device_name}#{ts_ms}")
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![AirvisualDeviceRow::ddl()];
    out.extend(AirvisualSampleRow::all_ddl());
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
