//! Raw-store schema for the YoLink provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/dev/data_architecture_ingestion.md`](/docs/dev/data_architecture_ingestion.md)
//! and [`docs/dev/archived/data_architecture_plan.md`](/docs/dev/archived/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! YoLink-specific notes:
//!
//! - **Time-windowed sampling, not UPSERT-of-everything.** Unlike the
//!   JSON-shape providers (anthropic, chatgpt, notion, …), YoLink's
//!   raw rows are individual sensor readings keyed by `(device, ts,
//!   metric)`. Extract walks forward-marching `(start_ms, end_ms)`
//!   windows per device, curls a signed-URL CSV per window, and
//!   UPSERTs each parsed sample. The cursor lives in
//!   `yolink_devices.last_ts_ms` rather than in a per-listing
//!   skip-check.
//!
//! - **Signed-URL auth.** Per-window URLs are signed with
//!   `md5(family_device_id || start_ms || end_ms || device_udid)` —
//!   YoLink's public API does not expose historical CSVs; the scheme
//!   was reverse-engineered from the Safehous/YoLink Android client.
//!   The signing happens in [`crate::extract`] (it touches secrets);
//!   only the table shape lives here.
//!
//! - **No upstream-supplied PK for readings.** Each row is a
//!   `(device, ts, metric)` triple, so we synthesize the PK; see
//!   [`reading_id_recipe`]. This is YoLink's analogue of the UUIDv5
//!   recipes other providers will eventually keep under `uuid.rs`
//!   (plan §P0.4).
//!
//! - **Event-shaped.** Each row in `yolink_readings` is a sample
//!   with its own upstream timestamp in `ts_ms` — the closest thing
//!   this provider has to `GridRow.when_ts`. `yolink_devices` is
//!   config-shaped, not event-shaped.

use frankweiler_etl::bulk::BulkUpsertable;
use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::WirePayloadRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::RawDb::reset` to wipe per-row state without
/// touching bookkeeping. Also drives [`full_ddl`] when it asks the
/// shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["yolink_devices", "yolink_readings"];

/// `yolink_devices` — one row per configured device, carrying its
/// per-device config snapshot plus the high-water cursor used to
/// resume incremental fetching.
///
/// Columns:
/// - `id` — the user-chosen `name:` from the YAML sync config (e.g.
///   `"basement-th"`). Primary key. Stable across runs because the
///   config is the source of truth, not anything upstream.
/// - `family_device_id` — YoLink-side family/device id; one half of
///   the signed-URL secret pair (see module docs). Mirrored here so
///   `dolt diff` surfaces config changes that would silently shift
///   which upstream device a `name:` resolves to.
/// - `kind` — device kind tag (`"temperature_humidity"`,
///   `"watermeter"`, …). Drives per-kind CSV column expectations on
///   the parser side.
/// - `start_ms` — earliest Unix-ms the fetcher will ever walk back
///   to for this device, derived from the YAML `start:` date.
/// - `last_ts_ms` — high-water mark: `MAX(yolink_readings.ts_ms)`
///   for this device after the most recent successful fetch. Drives
///   the per-run resume cursor (see [`crate::extract::fetch_device`]).
///   `NULL` before the first successful window lands a reading.
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
        q: Query<'q, Sqlite, SqliteArguments<'q>>,
    ) -> Query<'q, Sqlite, SqliteArguments<'q>> {
        q.bind(&self.id)
            .bind(&self.family_device_id)
            .bind(&self.kind)
            .bind(self.start_ms)
    }
}

/// `yolink_readings` — one row per sensor sample.
///
/// YoLink's CSV format does not carry a per-sample id, so the PK is
/// synthesized; see [`reading_id_recipe`].
///
/// Columns:
/// - `id` — synthesized composite PK
///   (`"{device_name}#{ts_ms}#{metric}"`). Primary key.
/// - `device_name` — FK into `yolink_devices.id`, the user-chosen
///   YAML `name:`.
/// - `ts_ms` — sample timestamp in Unix milliseconds (parsed from
///   the CSV's `Time` column). This is the event-shaped value the
///   translate / downstream side uses as `GridRow.when_ts`.
/// - `metric` — per-kind metric tag (e.g. `"temperature_c"`,
///   `"humidity_pct"`, `"water_meter_gal"`,
///   `"water_consumption_gal"`).
/// - `value` — sample value as `REAL`. Unit is implicit in
///   `metric`; the parser enforces unit-suffix invariants
///   upstream so a `℃`-tagged column carrying a `℉` row gets
///   rejected, not silently converted.
/// - `payload` — JSON object mapping each column header from the
///   source CSV row to its raw string value (e.g.
///   `{"Time": "…", "Temperature(℃)": "-1.1℃", "Humidity(%RH)":
///   "70.0"}`). YoLink's per-window signed URL is a wire fetch
///   that nothing else preserves — if upstream prunes history,
///   this column is the only place the raw record survives. Two
///   readings derived from the same CSV row (e.g. the temperature
///   and humidity rows) carry the same payload — that
///   denormalization is the cost of staying in the "one row per
///   metric" tall shape rather than splitting per-CSV-row out into
///   its own table.
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
///
/// YoLink's CSV rows carry no per-sample id. We hand-roll a
/// composite PK from `(device_name, ts_ms, metric)` — the only triple
/// guaranteed unique within a single device's stream. Format is
/// `"{device_name}#{ts_ms}#{metric}"`.
///
/// This is YoLink's analogue of the UUIDv5 recipes other providers
/// document under their (eventual, plan §P0.4) `uuid.rs` modules. For
/// now we keep the recipe **here** with the schema it keys into, so
/// that "what does the PK mean?" is one rustdoc-hop from the DDL.
/// When P0.4 lands we'll decide whether to relocate this recipe into
/// a sibling `uuid.rs` or leave it inline.
pub fn reading_id_recipe(device_name: &str, ts_ms: i64, metric: &str) -> String {
    format!("{device_name}#{ts_ms}#{metric}")
}

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
///
/// Schema-local glue, kept here so the "what tables exist?" answer
/// is one function call from this file. Heavier composition (e.g. a
/// repo-wide bookkeeping macro) is deferred to P1.1.
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
