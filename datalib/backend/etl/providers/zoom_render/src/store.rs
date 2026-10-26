//! What a render reads from the `zoom` raw store at its pin: every
//! meeting's rows, small enough to read whole.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use datalib_etl_zoom::ingest::schema_raw::{
    PARTICIPANTS, RECORDINGS, REPORTED_MEETINGS, SUMMARIES,
};
use datalib_etl_zoom::ingest::RawDb;
use serde_json::Value;
use sqlx::Row as _;

/// Every table a meeting's document reads; the forward scan diffs each.
/// Every key in them is the meeting's UUID.
pub const TABLES: [&str; 4] = [RECORDINGS, REPORTED_MEETINGS, PARTICIPANTS, SUMMARIES];

/// Everything the store holds about one meeting.
#[derive(Debug, Clone, Default)]
pub struct Meeting {
    pub uuid: String,
    pub recording: Option<Value>,
    pub reported: Option<Value>,
    pub participants: Option<Value>,
    pub summary: Option<Value>,
}

impl Meeting {
    /// A field of the meeting, from its recording entry or else from its
    /// report entry.
    pub fn field(&self, key: &str) -> Option<&Value> {
        self.recording
            .as_ref()
            .and_then(|r| r.get(key))
            .or_else(|| self.reported.as_ref().and_then(|r| r.get(key)))
    }

    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.field(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    /// The files Zoom holds of the meeting's recording, as its listing
    /// entry names them. None of them is downloaded.
    pub fn recording_files(&self) -> &[Value] {
        self.recording
            .as_ref()
            .and_then(|r| r.get("recording_files"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[derive(Debug, Default)]
pub struct Listing {
    /// Every meeting the store has a row for, by UUID.
    pub meetings: BTreeMap<String, Meeting>,
    /// The user being copied, by Zoom user id: their email.
    pub user_emails: HashMap<String, String>,
    /// The meetings whose rows the diff names; `None` renders every one.
    pub changed: Option<HashSet<String>>,
}

pub async fn load(db: &RawDb, range: RawRange<'_>) -> Result<Listing> {
    let pin = db.pin().expect("open_reader returns a pinned handle");
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    let mut out = Listing {
        changed: changed.map(|by_table| by_table.into_values().flatten().collect()),
        ..Listing::default()
    };

    for row in sqlx::query("SELECT id, json(payload) AS payload FROM users")
        .fetch_all(db.pool())
        .await
        .context("read users")?
    {
        let id: String = row.try_get("id")?;
        let payload: Option<String> = row.try_get("payload")?;
        let email = payload
            .and_then(|p| serde_json::from_str::<Value>(&p).ok())
            .and_then(|v| v.get("email").and_then(Value::as_str).map(str::to_string));
        if let Some(email) = email {
            out.user_emails.insert(id, email);
        }
    }

    for table in TABLES {
        // `table` is one of the four names in TABLES.
        let sql = format!("SELECT id, json(payload) AS payload FROM {table}");
        for row in sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(db.pool())
            .await
            .with_context(|| format!("read {table}"))?
        {
            let id: String = row.try_get("id")?;
            let Some(payload) = row.try_get::<Option<String>, _>("payload")? else {
                continue;
            };
            let value: Value = serde_json::from_str(&payload)
                .with_context(|| format!("{table} row {id} is not JSON"))?;
            let meeting = out.meetings.entry(id.clone()).or_insert_with(|| Meeting {
                uuid: id,
                ..Meeting::default()
            });
            match table {
                RECORDINGS => meeting.recording = Some(value),
                REPORTED_MEETINGS => meeting.reported = Some(value),
                PARTICIPANTS => meeting.participants = Some(value),
                _ => meeting.summary = Some(value),
            }
        }
    }
    Ok(out)
}
