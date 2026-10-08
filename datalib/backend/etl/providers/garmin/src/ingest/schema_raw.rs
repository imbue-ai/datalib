//! Raw-store schema for the `garmin` provider. Every table keys on the
//! id Garmin itself uses, except the two whose rows Garmin never
//! numbers: a per-day metric is `<metric>#<date>`, and the account
//! singletons are named for the endpoint they came from. What a row is
//! held *for* — the date a day is final from, the listing version a
//! detail or a file was answered for — is `held_version` in the table's
//! `_bookkeeping` sidecar (`datalib_etl_web::owed`), never a column here.

use anyhow::Result;
use sqlx::SqliteConnection;

use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::{RawTable, WirePayloadRow};

pub const DATA_TABLES: &[&str] = &[
    "garmin_account",
    "garmin_devices",
    "garmin_daily",
    "garmin_weigh_ins",
    "garmin_activities",
    "garmin_activity_details",
    "garmin_activity_files",
    "garmin_wellness_files",
    "garmin_items",
];

/// `garmin_account` — the account's singletons, one row each:
/// `social_profile` (`/userprofile-service/socialProfile`, the row every
/// per-user path needs `displayName` from) and `user_settings`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "garmin_account")]
pub struct AccountRow {
    pub id_and_payload: WirePayload,
    pub display_name: Option<String>,
}

pub const ACCOUNT_SOCIAL_PROFILE: &str = "social_profile";
pub const ACCOUNT_USER_SETTINGS: &str = "user_settings";

/// `garmin_devices` — one row per registered device, keyed on
/// Garmin's `deviceId`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "garmin_devices")]
pub struct DeviceRow {
    pub id_and_payload: WirePayload,
    pub product_display_name: Option<String>,
}

/// `garmin_daily` — one row per (metric, calendar day): the JSON that
/// metric's endpoint returned for that day, verbatim. A day the
/// endpoint had nothing for (204, 404, or an empty body) is stored as
/// JSON `null`, so "asked and empty" is distinguishable from "never
/// asked" and a re-walk rewrites nothing.
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "garmin_daily",
    index = "garmin_daily_by_metric_date:metric,calendar_date"
)]
pub struct DailyRow {
    pub id_and_payload: WirePayload,
    pub metric: String,
    pub calendar_date: String,
}

impl DailyRow {
    pub fn id_for(metric: &str, calendar_date: &str) -> String {
        format!("{metric}#{calendar_date}")
    }

    /// The calendar date of an id [`Self::id_for`] built.
    pub fn date_of(id: &str) -> &str {
        id.rsplit('#').next().unwrap_or(id)
    }
}

/// `garmin_weigh_ins` — one row per weigh-in, keyed on Garmin's
/// `samplePk`. `weight_g` is grams, as the wire carries it.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "garmin_weigh_ins")]
pub struct WeighInRow {
    pub id_and_payload: WirePayload,
    pub calendar_date: Option<String>,
    pub timestamp_gmt: Option<i64>,
    pub weight_g: Option<f64>,
    pub source_type: Option<String>,
}

pub const WEIGH_INS_BY_DATE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS garmin_weigh_ins_by_date ON garmin_weigh_ins(calendar_date)";

/// `garmin_activities` — one row per activity as the listing describes
/// it, keyed on `activityId`. `listing_hash` is the blake3 of the payload:
/// the version of the activity its detail and file are fetched for.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "garmin_activities")]
pub struct ActivityRow {
    pub id_and_payload: WirePayload,
    pub start_time_gmt: Option<String>,
    pub activity_type: Option<String>,
    pub name: Option<String>,
    pub listing_hash: Option<String>,
}

pub const ACTIVITIES_BY_START_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS garmin_activities_by_start ON garmin_activities(start_time_gmt)";

/// `garmin_activity_details` — `/activity-service/activity/<id>`, the
/// fuller record (device, sensors, gear, summary), keyed like the list.
/// An activity Garmin has no detail for holds JSON `null`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "garmin_activity_details")]
pub struct ActivityDetailRow {
    pub id_and_payload: WirePayload,
}

/// `garmin_activity_files` — an activity's original FIT file in the CAS;
/// `file_kind` is `fit`. `blake3` is NULL when Garmin answered with no
/// file, or with one that could not be read.
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "garmin_activity_files",
    index = "garmin_activity_files_by_activity_id:activity_id",
    index = "garmin_activity_files_by_file_kind:file_kind,blake3"
)]
pub struct ActivityFileRow {
    pub id: String,
    pub activity_id: String,
    pub file_kind: String,
    pub blake3: Option<String>,
}

/// `garmin_wellness_files` — one row per calendar day the wellness
/// bundle was asked for: the zip in the CAS (`blake3` set), or no bundle
/// that day (`blake3` NULL).
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "garmin_wellness_files",
    index = "garmin_wellness_files_by_calendar_date:calendar_date",
    index = "garmin_wellness_files_by_file_kind:file_kind,blake3"
)]
pub struct WellnessFileRow {
    pub id: String,
    pub calendar_date: String,
    pub file_kind: String,
    pub blake3: Option<String>,
}

pub const FILE_KIND_FIT: &str = "fit";
pub const FILE_KIND_WELLNESS_ZIP: &str = "wellness_zip";

/// The CAS bundle keys its blobs by ref id, so each record's file needs
/// a ref of its own; the edge row's id is that ref.
pub fn file_ref(owner: &str, file_kind: &str) -> String {
    format!("{owner}#{file_kind}")
}

/// The owner of a ref [`file_ref`] built: the activity id, or the day.
pub fn file_owner(file_ref: &str) -> &str {
    file_ref.split('#').next().unwrap_or(file_ref)
}

/// `garmin_items` — the small whole-account listings that have no date
/// axis and are re-read complete every run: personal records, gear,
/// earned badges, workouts, goals. `kind` names the listing, `upstream_id`
/// is Garmin's id inside it, and the row id joins the two.
#[derive(Debug, Clone, RawTable)]
#[raw_table(table = "garmin_items", index = "garmin_items_by_kind:kind")]
pub struct ItemRow {
    pub id_and_payload: WirePayload,
    pub kind: String,
    pub upstream_id: String,
}

impl ItemRow {
    pub fn id_for(kind: &str, upstream_id: &str) -> String {
        format!("{kind}#{upstream_id}")
    }
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "what a row is held for moves to its bookkeeping sidecar",
    apply: |conn| Box::pin(held_versions_to_the_sidecar(conn)),
}];

/// The columns an earlier build kept the held version in, by table.
const HELD_COLUMNS: &[(&str, &str)] = &[
    ("garmin_daily", "fetched_on"),
    ("garmin_wellness_files", "fetched_on"),
    ("garmin_activity_details", "listing_hash"),
    ("garmin_activity_files", "listing_hash"),
];

/// Each held column's values go to its table's sidecar as they are,
/// and the column goes. A day's `fetched_on` is not the date the day
/// settled, which is what the sidecar holds from now on; a day fetched
/// on the day it settled matches and is not fetched again, one fetched
/// later is fetched once more. The rung cannot do better: the settle
/// date depends on `refresh_days`, which is in the config, not the
/// store. A store from before the columns has nothing to carry.
async fn held_versions_to_the_sidecar(conn: &mut SqliteConnection) -> Result<()> {
    for (table, column) in HELD_COLUMNS {
        if !has_column(conn, table, column).await? {
            continue;
        }
        let sidecar = format!("{table}_bookkeeping");
        if !has_column(conn, &sidecar, "held_version").await? {
            // Audited: `sidecar` is built from a literal of HELD_COLUMNS.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE {sidecar} ADD COLUMN held_version TEXT NULL"
            )))
            .execute(&mut *conn)
            .await?;
        }
        // Audited: every name is a literal of HELD_COLUMNS.
        for sql in [
            format!(
                "UPDATE {sidecar} SET held_version = \
                 (SELECT {column} FROM {table} t WHERE t.id = {sidecar}.id)"
            ),
            format!("ALTER TABLE {table} DROP COLUMN {column}"),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}

async fn has_column(conn: &mut SqliteConnection, table: &str, column: &str) -> Result<bool> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(column)
        .fetch_one(&mut *conn)
        .await?;
    Ok(n > 0)
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        AccountRow::ddl(),
        DeviceRow::ddl(),
        WeighInRow::ddl(),
        WEIGH_INS_BY_DATE_INDEX_DDL.to_string(),
        ActivityRow::ddl(),
        ACTIVITIES_BY_START_INDEX_DDL.to_string(),
        ActivityDetailRow::ddl(),
        datalib_etl_web::coverage::DDL.to_string(),
    ];
    out.extend(DailyRow::all_ddl());
    out.extend(ItemRow::all_ddl());
    out.extend(ActivityFileRow::all_ddl());
    out.extend(WellnessFileRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
