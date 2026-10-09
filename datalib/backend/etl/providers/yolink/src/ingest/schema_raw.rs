//! Raw-store schema for the YoLink provider.

use std::collections::BTreeMap;

use anyhow::Result;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::WirePayloadRow;
use datalib_etl_web::coverage;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::{Sqlite, SqliteConnection};

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &["yolink_devices", "yolink_readings"];

/// `yolink_devices` — one row per configured device, carrying its
/// per-device config snapshot. Which stretches of its history have been
/// walked is in `coverage`, under [`device_scope`].
pub const YOLINK_DEVICES_DDL: &str = "CREATE TABLE IF NOT EXISTS yolink_devices (
    id TEXT PRIMARY KEY,
    family_device_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    start_ms INTEGER NOT NULL
)";

/// Row matching [`YOLINK_DEVICES_DDL`]. Hand-rolled `BulkUpsertable`
/// (no payload column — every field is a typed column).
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
/// "newest reading of this device" lookup and the "readings for
/// device X over a time range" queries downstream consumers run.
pub const YOLINK_READINGS_BY_DEVICE_TS_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS yolink_readings_by_device_ts
        ON yolink_readings(device_name, ts_ms)";

/// Recipe for the synthesized [`YOLINK_READINGS_DDL`] primary key.
pub fn reading_id_recipe(device_name: &str, ts_ms: i64, metric: &str) -> String {
    format!("{device_name}#{ts_ms}#{metric}")
}

/// The `coverage` scope a device's history is walked under.
pub fn device_scope(device_name: &str) -> String {
    format!("device:{device_name}")
}

/// A span end in `coverage`: epoch milliseconds, zero-padded so text
/// order is time order.
pub fn span_end(ms: i64) -> String {
    format!("{ms:016}")
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
        coverage::DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "coverage spans replace the resume cursor and the failed-window table",
    apply: |conn| Box::pin(coverage_from_the_cursor(conn)),
}];

/// An earlier build resumed each device from `yolink_devices.last_ts_ms`,
/// the newest stored reading, and kept the windows that failed behind it
/// in `yolink_windows`. That walk had covered everything from the
/// device's start up to the cursor except those windows, so that is the
/// coverage the rung records; the rest of the device's range is owed and
/// walked, which is what the old code did too. The start is the one the
/// cursor was walked under: the row's, unless `sync_scope_config` holds
/// a later one, which means the row's start was widened by a run that
/// did not finish the backfill.
async fn coverage_from_the_cursor(conn: &mut SqliteConnection) -> Result<()> {
    sqlx::query(coverage::DDL).execute(&mut *conn).await?;
    if has_column(conn, "yolink_devices", "last_ts_ms").await? {
        let prior_starts = prior_starts(conn).await?;
        let devices: Vec<(String, i64, Option<i64>)> =
            sqlx::query_as("SELECT id, start_ms, last_ts_ms FROM yolink_devices")
                .fetch_all(&mut *conn)
                .await?;
        let failed: Vec<(String, i64, i64)> = if has_column(conn, "yolink_windows", "id").await? {
            sqlx::query_as("SELECT device_name, start_ms, end_ms FROM yolink_windows")
                .fetch_all(&mut *conn)
                .await?
        } else {
            Vec::new()
        };
        for (name, start_ms, cursor) in devices {
            let Some(cursor) = cursor else {
                continue;
            };
            let lo = prior_starts
                .get(&name)
                .map_or(start_ms, |prior| start_ms.max(*prior));
            let holes: Vec<(i64, i64)> = failed
                .iter()
                .filter(|(device, _, _)| *device == name)
                .map(|(_, s, e)| (*s, *e))
                .collect();
            for (span_lo, span_hi) in walked(lo, cursor, &holes) {
                sqlx::query("INSERT INTO coverage (scope, lo, hi) VALUES (?, ?, ?)")
                    .bind(device_scope(&name))
                    .bind(span_end(span_lo))
                    .bind(span_end(span_hi))
                    .execute(&mut *conn)
                    .await?;
            }
        }
        sqlx::query("ALTER TABLE yolink_devices DROP COLUMN last_ts_ms")
            .execute(&mut *conn)
            .await?;
    }
    for sql in [
        "DROP TABLE IF EXISTS yolink_windows",
        "DROP TABLE IF EXISTS yolink_windows_bookkeeping",
    ] {
        sqlx::query(sql).execute(&mut *conn).await?;
    }
    if has_column(conn, "problems", "scope_key").await? {
        sqlx::query("DELETE FROM problems WHERE scope_key LIKE 'yolink_windows:%'")
            .execute(&mut *conn)
            .await?;
    }
    if has_column(conn, "sync_scope_config", "scope").await? {
        sqlx::query("DELETE FROM sync_scope_config WHERE scope = 'yolink:download'")
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// `[lo, hi]` less each of `holes`, lowest first.
fn walked(lo: i64, hi: i64, holes: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let mut holes: Vec<(i64, i64)> = holes.iter().copied().filter(|(a, b)| a < b).collect();
    holes.sort_unstable();
    let mut out = Vec::new();
    let mut from = lo;
    for (a, b) in holes {
        if b <= from {
            continue;
        }
        if a > hi {
            break;
        }
        if a > from {
            out.push((from, a));
        }
        from = from.max(b);
    }
    if from < hi {
        out.push((from, hi));
    }
    out
}

/// The start each device's cursor was walked under, as the earlier
/// build recorded it in `sync_scope_config` once a run satisfied the
/// config: `{"device_starts": {"<name>": "YYYY-MM-DD"}}`.
async fn prior_starts(conn: &mut SqliteConnection) -> Result<BTreeMap<String, i64>> {
    let mut out = BTreeMap::new();
    if !has_column(conn, "sync_scope_config", "scope").await? {
        return Ok(out);
    }
    let blob: Option<String> =
        sqlx::query_scalar("SELECT config FROM sync_scope_config WHERE scope = 'yolink:download'")
            .fetch_optional(&mut *conn)
            .await?;
    let Some(blob) = blob else {
        return Ok(out);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&blob) else {
        return Ok(out);
    };
    let Some(starts) = value.get("device_starts").and_then(|v| v.as_object()) else {
        return Ok(out);
    };
    for (name, start) in starts {
        if let Some(ms) = start.as_str().and_then(crate::ingest::start_of_day_ms) {
            out.insert(name.clone(), ms);
        }
    }
    Ok(out)
}

async fn has_column(conn: &mut SqliteConnection, table: &str, column: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(&mut *conn)
        .await?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rung's arithmetic: the cursor's range less the failed windows
    /// inside it, holes outside it ignored, touching holes merged.
    #[test]
    fn a_cursors_range_less_its_failed_windows() {
        assert_eq!(walked(10, 90, &[]), [(10, 90)]);
        assert_eq!(walked(10, 90, &[(30, 40)]), [(10, 30), (40, 90)]);
        assert_eq!(walked(10, 90, &[(0, 5), (95, 99)]), [(10, 90)]);
        assert_eq!(walked(10, 90, &[(0, 20), (80, 99)]), [(20, 80)]);
        assert_eq!(
            walked(10, 90, &[(40, 50), (30, 40), (60, 60)]),
            [(10, 30), (50, 90)]
        );
        assert_eq!(walked(10, 90, &[(10, 90)]), []);
        assert_eq!(walked(90, 10, &[]), []);
    }

    #[test]
    fn span_ends_sort_as_time_does() {
        assert!(span_end(999) < span_end(1_000));
        assert!(span_end(12_601_526_400_000) < span_end(12_601_526_400_001));
    }
}
