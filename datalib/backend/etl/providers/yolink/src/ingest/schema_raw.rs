//! Raw-store schema for the YoLink provider.

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::WirePayloadRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &["yolink_devices", "yolink_readings"];

/// `yolink_devices` — one row per configured device, carrying its
/// per-device config snapshot plus the high-water cursor used to
/// resume incremental fetching.
pub const YOLINK_DEVICES_DDL: &str = "CREATE TABLE IF NOT EXISTS yolink_devices (
    id TEXT PRIMARY KEY,
    family_device_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    start_ms INTEGER NOT NULL,
    last_ts_ms INTEGER NULL
)";

/// Row matching [`YOLINK_DEVICES_DDL`]. Hand-rolled `BulkUpsertable`
/// (no payload column — every field is a typed column). `last_ts_ms`
/// is bumped separately via the `UPDATE yolink_devices SET
/// last_ts_ms = …` cursor advance after each successful window, so
/// it's NOT in the promoted-column list (bulk-upsert won't clobber
/// the cursor).
#[derive(Debug, Clone, Default)]
pub struct YolinkDeviceRow {
    pub id: String,
    pub family_device_id: String,
    pub kind: String,
    pub start_ms: i64,
}

impl BulkUpsertable for YolinkDeviceRow {
    const TABLE: &'static str = "yolink_devices";
    const TYPED_COLUMNS: &'static [&'static str] = &["family_device_id", "kind", "start_ms"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(&self.family_device_id)
            .bind(&self.kind)
            .bind(self.start_ms)
    }
}

/// `yolink_readings` — one row per sensor sample.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "yolink_readings")]
pub struct YolinkReadingRow {
    pub id_and_payload: WirePayload,
    pub device_name: String,
    pub ts_ms: i64,
    pub metric: String,
    pub value: f64,
}

impl YolinkReadingRow {
    /// Mint a row with its synthesized PK plus the per-CSV-row
    /// payload. `payload_json` is the JSON-encoded
    /// `{header: value}` map of the source CSV row — see the DDL
    /// docstring for the rationale.
    pub fn new(
        device_name: &str,
        ts_ms: i64,
        metric: &str,
        value: f64,
        payload_json: String,
    ) -> Self {
        Self {
            id_and_payload: WirePayload {
                id: reading_id_recipe(device_name, ts_ms, metric),
                payload: payload_json,
            },
            device_name: device_name.to_string(),
            ts_ms,
            metric: metric.to_string(),
            value,
        }
    }
}

/// Index on `yolink_readings(device_name, ts_ms)` — supports the
/// "max ts for this device" cursor lookup and the "readings for
/// device X over a time range" queries downstream consumers run.
pub const YOLINK_READINGS_BY_DEVICE_TS_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS yolink_readings_by_device_ts
        ON yolink_readings(device_name, ts_ms)";

/// Recipe for the synthesized [`YOLINK_READINGS_DDL`] primary key.
pub fn reading_id_recipe(device_name: &str, ts_ms: i64, metric: &str) -> String {
    format!("{device_name}#{ts_ms}#{metric}")
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        YOLINK_DEVICES_DDL.to_string(),
        YolinkReadingRow::ddl(),
        YOLINK_READINGS_BY_DEVICE_TS_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
