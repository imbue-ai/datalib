//! Raw-store schema for the `garmin` provider. Every table keys on the
//! id Garmin itself uses, except the two whose rows Garmin never
//! numbers: a per-day metric is `<metric>#<date>`, and the account
//! singletons are named for the endpoint they came from.

use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
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
/// asked" and a re-walk rewrites nothing. `fetched_on` is the run's date
/// when the row was fetched: a day fetched before it had settled is
/// fetched again (`ingest::settles_on`).
#[derive(Debug, Clone, RawTable)]
#[raw_table(
    table = "garmin_daily",
    index = "garmin_daily_by_metric_date:metric,calendar_date"
)]
pub struct DailyRow {
    pub id_and_payload: WirePayload,
    pub metric: String,
    pub calendar_date: String,
    pub fetched_on: Option<String>,
}

impl DailyRow {
    pub fn id_for(metric: &str, calendar_date: &str) -> String {
        format!("{metric}#{calendar_date}")
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
/// `listing_hash` is the `garmin_activities.listing_hash` the detail was
/// fetched for; a detail whose activity now lists differently is owed.
/// An activity Garmin has no detail for holds JSON `null`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "garmin_activity_details")]
pub struct ActivityDetailRow {
    pub id_and_payload: WirePayload,
    pub listing_hash: Option<String>,
}

/// `garmin_activity_files` — an activity's original FIT file in the CAS;
/// `file_kind` is `fit`. `listing_hash` is the listing version the
/// download was answered for, whatever the answer: the file (`blake3`
/// set), no file at all, or one that could not be read (both `blake3`
/// NULL). It is NULL when the request itself failed, which leaves the
/// file owed.
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
    pub listing_hash: Option<String>,
}

/// `garmin_wellness_files` — one row per calendar day the wellness
/// bundle was asked for: the zip in the CAS (`blake3` set), or no bundle
/// that day (`blake3` NULL). `fetched_on` is the run's date when Garmin
/// answered, read as `garmin_daily.fetched_on` is; NULL when the request
/// failed.
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
    pub fetched_on: Option<String>,
}

pub const FILE_KIND_FIT: &str = "fit";
pub const FILE_KIND_WELLNESS_ZIP: &str = "wellness_zip";

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

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        AccountRow::ddl(),
        DeviceRow::ddl(),
        WeighInRow::ddl(),
        WEIGH_INS_BY_DATE_INDEX_DDL.to_string(),
        ActivityRow::ddl(),
        ACTIVITIES_BY_START_INDEX_DDL.to_string(),
        ActivityDetailRow::ddl(),
        datalib_etl::coverage::DDL.to_string(),
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
