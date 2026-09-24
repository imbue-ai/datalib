//! Raw-store schema for the `calendar` provider. Each download method
//! keeps what its upstream sent: an iCalendar object per event for
//! CalDAV and `.ics` files, Google's JSON per event for Google.

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::WirePayloadRow;
use sqlx::query::Query;
use sqlx::sqlite::SqliteArguments;
use sqlx::Sqlite;

pub const DATA_TABLES: &[&str] = &["accounts", "calendars", "ics_objects", "google_events"];

/// `accounts` — the login this store mirrors. One row.
pub const ACCOUNTS_DDL: &str = "CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    method TEXT NOT NULL,
    server_url TEXT NULL,
    principal_href TEXT NULL,
    login TEXT NULL
)";

#[derive(Debug, Clone, Default)]
pub struct AccountRow {
    /// The server's host for CalDAV, `google` for Google, `ics` for files.
    pub id: String,
    /// `caldav`, `google` or `ics`.
    pub method: String,
    pub server_url: Option<String>,
    pub principal_href: Option<String>,
    /// Who the calendars belong to: the CalDAV principal's user, the
    /// Google account's primary calendar id (its address).
    pub login: Option<String>,
}

impl BulkUpsertable for AccountRow {
    const TABLE: &'static str = "accounts";
    const TYPED_COLUMNS: &'static [&'static str] =
        &["method", "server_url", "principal_href", "login"];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(&self.method)
            .bind(self.server_url.as_deref())
            .bind(self.principal_href.as_deref())
            .bind(self.login.as_deref())
    }
}

/// `calendars` — one row per calendar the account lists.
///
/// `id` is the calendar's own id, short enough to lead an event's key:
/// the last segment of a CalDAV collection's href (Fastmail and iCloud
/// mint a UUID there), Google's calendar id, an `.ics` file's path
/// under the configured directory without its extension.
pub const CALENDARS_DDL: &str = "CREATE TABLE IF NOT EXISTS calendars (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    href TEXT NULL,
    display_name TEXT NULL,
    description TEXT NULL,
    color TEXT NULL,
    time_zone TEXT NULL,
    sync_token TEXT NULL
)";

#[derive(Debug, Clone, Default)]
pub struct CalendarRow {
    pub id: String,
    pub account_id: String,
    pub href: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    /// The IANA zone a floating time on this calendar is in, where the
    /// server says (`calendar-timezone`, Google's `timeZone`,
    /// `X-WR-TIMEZONE`).
    pub time_zone: Option<String>,
}

impl BulkUpsertable for CalendarRow {
    const TABLE: &'static str = "calendars";
    // `sync_token` moves only after a calendar's changes are all stored
    // (`RawDb::set_sync_token`), so the upsert leaves it alone.
    const TYPED_COLUMNS: &'static [&'static str] = &[
        "account_id",
        "href",
        "display_name",
        "description",
        "color",
        "time_zone",
    ];
    const PAYLOAD_COLUMN: Option<&'static str> = None;
    fn id(&self) -> &str {
        &self.id
    }
    fn bind_into<'q>(
        &'q self,
        q: Query<'q, Sqlite, SqliteArguments>,
    ) -> Query<'q, Sqlite, SqliteArguments> {
        q.bind(&self.id)
            .bind(&self.account_id)
            .bind(self.href.as_deref())
            .bind(self.display_name.as_deref())
            .bind(self.description.as_deref())
            .bind(self.color.as_deref())
            .bind(self.time_zone.as_deref())
    }
}

/// `ics_objects` — one iCalendar object per event `UID`: the series or
/// single event and every changed occurrence of it, the way CalDAV
/// stores a resource. Payload `{"ics": "<VCALENDAR text>"}`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "ics_objects")]
pub struct IcsObjectRow {
    pub id_and_payload: WirePayload,
    pub calendar_id: String,
    pub uid: String,
    /// The CalDAV resource href; `None` for an `.ics` file's event.
    pub href: Option<String>,
    pub etag: Option<String>,
}

impl IcsObjectRow {
    pub fn new(
        calendar_id: &str,
        uid: &str,
        href: Option<String>,
        etag: Option<String>,
        ics: &str,
    ) -> Self {
        Self {
            id_and_payload: WirePayload {
                id: event_pk(calendar_id, uid),
                payload: serde_json::json!({ "ics": ics }).to_string(),
            },
            calendar_id: calendar_id.to_string(),
            uid: uid.to_string(),
            href,
            etag,
        }
    }
}

/// `google_events` — one Google Calendar event resource, verbatim. A
/// changed or cancelled occurrence of a series is its own resource,
/// naming the series in `recurring_event_id`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "google_events")]
pub struct GoogleEventRow {
    pub id_and_payload: WirePayload,
    pub calendar_id: String,
    pub event_id: String,
    pub recurring_event_id: Option<String>,
    pub status: Option<String>,
}

impl GoogleEventRow {
    pub fn new(calendar_id: &str, event: &serde_json::Value) -> Option<Self> {
        let event_id = event.get("id")?.as_str()?.to_string();
        let str_of = |k: &str| event.get(k).and_then(|v| v.as_str()).map(str::to_string);
        Some(Self {
            id_and_payload: WirePayload {
                id: event_pk(calendar_id, &event_id),
                payload: event.to_string(),
            },
            calendar_id: calendar_id.to_string(),
            recurring_event_id: str_of("recurringEventId"),
            status: str_of("status"),
            event_id,
        })
    }
}

/// An event row's key: `"{calendar_id}#{uid or Google event id}"`.
pub fn event_pk(calendar_id: &str, event_key: &str) -> String {
    format!("{calendar_id}#{event_key}")
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        ACCOUNTS_DDL.to_string(),
        CALENDARS_DDL.to_string(),
        IcsObjectRow::ddl(),
        "CREATE INDEX IF NOT EXISTS ics_objects_by_calendar ON ics_objects(calendar_id)"
            .to_string(),
        "CREATE INDEX IF NOT EXISTS ics_objects_by_href ON ics_objects(calendar_id, href)"
            .to_string(),
        GoogleEventRow::ddl(),
        "CREATE INDEX IF NOT EXISTS google_events_by_calendar ON google_events(calendar_id)"
            .to_string(),
        // The `.ics` method's resume cursor: a file whose size and mtime
        // have not moved is not read again.
        datalib_etl::file_checkpoint::INGESTED_FILES_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
