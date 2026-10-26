//! What a render reads from the `zoom` raw store at its pin: every
//! meeting's rows, small enough to read whole, and one meeting's
//! transcript bytes at a time, as it renders.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use datalib_etl_zoom::ingest::schema_raw::{
    PARTICIPANTS, RECORDINGS, RECORDING_FILES, REPORTED_MEETINGS, SUMMARIES,
};
use datalib_etl_zoom::ingest::RawDb;
use datalib_schema::problems::{Problem, Reason};
use serde_json::Value;
use sqlx::Row as _;

/// Every table a meeting's document reads; the forward scan diffs each.
/// Every key in them is the meeting's UUID, or starts with it and `#`.
pub const TABLES: [&str; 5] = [
    RECORDINGS,
    REPORTED_MEETINGS,
    PARTICIPANTS,
    SUMMARIES,
    RECORDING_FILES,
];

/// One stored recording file: its edge id, its key within the meeting,
/// and its hash, `None` for one Zoom no longer had.
#[derive(Debug, Clone)]
pub struct File {
    pub edge_id: String,
    pub file_key: String,
    pub blake3: Option<String>,
}

/// Everything the store holds about one meeting.
#[derive(Debug, Clone, Default)]
pub struct Meeting {
    pub uuid: String,
    pub recording: Option<Value>,
    pub reported: Option<Value>,
    pub participants: Option<Value>,
    pub summary: Option<Value>,
    pub files: Vec<File>,
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

    /// The listing entry of the file stored under `file_key`.
    pub fn file_entry(&self, file_key: &str) -> Option<&Value> {
        self.recording
            .as_ref()?
            .get("recording_files")?
            .as_array()?
            .iter()
            .find(|f| {
                let id = f
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty());
                let ty = f.get("file_type").and_then(Value::as_str);
                id == Some(file_key)
                    || (id.is_none()
                        && ty.map(str::to_ascii_lowercase).as_deref() == Some(file_key))
            })
    }

    /// The stored transcript file: the `TRANSCRIPT` one the recording
    /// lists.
    pub fn transcript(&self) -> Option<&File> {
        self.files.iter().find(|f| {
            self.file_entry(&f.file_key)
                .and_then(|e| e.get("file_type"))
                .and_then(Value::as_str)
                == Some("TRANSCRIPT")
        })
    }
}

#[derive(Debug, Default)]
pub struct Listing {
    /// Every meeting the store has a row for, by UUID.
    pub meetings: BTreeMap<String, Meeting>,
    /// The user being copied, by Zoom user id: their email.
    pub user_emails: HashMap<String, String>,
    /// The meetings to render; `None` renders every one.
    pub render: Option<HashSet<String>>,
}

/// The meeting a raw key belongs to.
pub fn meeting_of(key: &str) -> &str {
    key.split_once('#').map_or(key, |(uuid, _)| uuid)
}

pub async fn load(db: &RawDb, range: RawRange<'_>) -> Result<Listing> {
    let pin = db.pin().expect("open_reader returns a pinned handle");
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    let forward = changed.map(|by_table| {
        by_table
            .values()
            .flatten()
            .map(|key| meeting_of(key).to_string())
            .collect::<HashSet<String>>()
    });
    let mut out = Listing {
        render: forward,
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

    for table in [RECORDINGS, REPORTED_MEETINGS, PARTICIPANTS, SUMMARIES] {
        // `table` is one of the four names above.
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

    for row in
        sqlx::query("SELECT id, meeting_uuid, file_key, blake3 FROM recording_files ORDER BY id")
            .fetch_all(db.pool())
            .await
            .context("read recording_files")?
    {
        let uuid: String = row.try_get("meeting_uuid")?;
        if let Some(meeting) = out.meetings.get_mut(&uuid) {
            meeting.files.push(File {
                edge_id: row.try_get("id")?,
                file_key: row.try_get("file_key")?,
                blake3: row.try_get("blake3")?,
            });
        }
    }
    Ok(out)
}

/// A meeting's transcript as text, or why the store cannot give it. The
/// error is the store's; the problem is the meeting's.
pub async fn transcript(db: &RawDb, file: &File) -> Result<Result<String, Problem>> {
    let missing = |why: String| Ok(Err(Problem::field("transcript", Reason::BlobMissing, &why)));
    let Some(blake3) = file.blake3.as_deref() else {
        return missing("Zoom no longer had the transcript when it was fetched".to_string());
    };
    let Some(object) = db.cas().get(blake3).await? else {
        return missing(format!("the blob store has lost {blake3}"));
    };
    Ok(Ok(String::from_utf8_lossy(&object.bytes).into_owned()))
}

/// The bytes the recording's files link to, for a meeting's page.
pub const FILES_SQL: &str = "
    SELECT id AS ref_id, blake3, NULL AS content_type, file_key AS upstream_name
      FROM recording_files
     WHERE id IN ({placeholders}) AND blake3 IS NOT NULL";
