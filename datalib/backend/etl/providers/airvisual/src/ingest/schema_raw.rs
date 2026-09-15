//! Raw-store schema for the AirVisual provider: one row per device and
//! one row per sample, a typed column per measurement and nothing else
//! — no payload, no bookkeeping sidecar. A blank cell in the history
//! file is a NULL here. Every column of a line is typed, so the line
//! itself would only repeat them; and a sensor sample has no fetch to
//! retry, so the sidecar would double the row count for nothing (the
//! same call `fsindex` makes).

use datalib_etl::bulk::SQL_CHUNK;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl_macros::RawTable;
use sqlx::{Sqlite, Transaction};

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

/// One row per line of a history file, keyed on the device and the
/// second it was logged. The measurement columns are the device's own,
/// in its own units; `Temperature(F)` is left out because it is
/// `temperature_c` converted. The `outdoor_*` pair is the followed
/// public station's index, not this sensor's.
pub const AIRVISUAL_SAMPLES_DDL: &str = "CREATE TABLE IF NOT EXISTS airvisual_samples (
    device_id      TEXT NOT NULL,
    ts_ms          INTEGER NOT NULL,
    pm25_ugm3      REAL NULL,
    pm10_ugm3      REAL NULL,
    pm1_ugm3       REAL NULL,
    aqi_us         REAL NULL,
    aqi_cn         REAL NULL,
    outdoor_aqi_us REAL NULL,
    outdoor_aqi_cn REAL NULL,
    temperature_c  REAL NULL,
    humidity_pct   REAL NULL,
    co2_ppm        REAL NULL,
    voc_ppb        REAL NULL,
    source_file    TEXT NOT NULL,
    PRIMARY KEY (device_id, ts_ms)
)";

/// The measurement columns, in DDL order — the render's metric table
/// is checked against this list.
pub const SAMPLE_MEASUREMENTS: &[&str] = &[
    "pm25_ugm3",
    "pm10_ugm3",
    "pm1_ugm3",
    "aqi_us",
    "aqi_cn",
    "outdoor_aqi_us",
    "outdoor_aqi_cn",
    "temperature_c",
    "humidity_pct",
    "co2_ppm",
    "voc_ppb",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AirvisualSampleRow {
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

/// Chunked multi-row upsert, the later row winning on a repeated
/// `(device_id, ts_ms)`.
pub async fn upsert_samples(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[AirvisualSampleRow],
) -> anyhow::Result<()> {
    const COLS: usize = 14;
    let set: String = SAMPLE_MEASUREMENTS
        .iter()
        .chain(["source_file"].iter())
        .map(|c| format!("{c} = excluded.{c}"))
        .collect::<Vec<_>>()
        .join(", ");
    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = String::from(
            "INSERT INTO airvisual_samples (device_id, ts_ms, pm25_ugm3, pm10_ugm3, pm1_ugm3, \
             aqi_us, aqi_cn, outdoor_aqi_us, outdoor_aqi_cn, temperature_c, humidity_pct, \
             co2_ppm, voc_ppb, source_file) VALUES ",
        );
        datalib_etl::bulk::push_placeholders(&mut sql, chunk.len(), COLS);
        sql.push_str(" ON CONFLICT(device_id, ts_ms) DO UPDATE SET ");
        sql.push_str(&set);
        // Audited: the column names are the `&'static str` list above, and the
        // VALUES run is `push_placeholders` over `chunk.len()`; every value is
        // bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = q
                .bind(&r.device_id)
                .bind(r.ts_ms)
                .bind(r.pm25_ugm3)
                .bind(r.pm10_ugm3)
                .bind(r.pm1_ugm3)
                .bind(r.aqi_us)
                .bind(r.aqi_cn)
                .bind(r.outdoor_aqi_us)
                .bind(r.outdoor_aqi_cn)
                .bind(r.temperature_c)
                .bind(r.humidity_pct)
                .bind(r.co2_ppm)
                .bind(r.voc_ppb)
                .bind(&r.source_file);
        }
        q.execute(&mut **tx).await?;
    }
    datalib_etl::download_metrics::record_upserts("airvisual_samples", rows.len());
    Ok(())
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

pub fn unplaced_id_recipe(device_id: &str, source_file: &str, line_no: i64) -> String {
    format!("{device_id}#{source_file}#{line_no}")
}

pub fn full_ddl() -> Vec<String> {
    vec![
        AirvisualDeviceRow::ddl(),
        AIRVISUAL_SAMPLES_DDL.to_string(),
        AirvisualUnplacedSampleRow::ddl(),
        datalib_etl::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_ddl_covers_every_table_and_carries_no_sidecar() {
        let blob = full_ddl().join("\n");
        for t in DATA_TABLES {
            assert!(
                blob.contains(&format!("CREATE TABLE IF NOT EXISTS {t} (")),
                "{t}"
            );
        }
        assert!(!blob.contains("_bookkeeping"), "no sidecar for a sample");
        assert!(blob.contains("ingested_files"));
        assert!(blob.contains("PRIMARY KEY (device_id, ts_ms)"));
        for c in SAMPLE_MEASUREMENTS {
            assert!(blob.contains(&format!("{c} ")), "{c} is a sample column");
        }
    }
}
