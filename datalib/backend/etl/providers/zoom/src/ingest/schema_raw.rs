//! Raw-store schema for the `zoom` provider. Every meeting table keys on
//! Zoom's meeting UUID, which names one meeting instance (a recurring
//! meeting's every occurrence has its own); a recording file is keyed
//! `{meeting uuid}#{file key}`. The bytes of every file are in the blob
//! CAS beside the store.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};

pub const USERS: &str = "users";
pub const RECORDINGS: &str = "recordings";
pub const REPORTED_MEETINGS: &str = "reported_meetings";
pub const PARTICIPANTS: &str = "participants";
pub const SUMMARY_LISTINGS: &str = "summary_listings";
pub const SUMMARIES: &str = "summaries";
pub const RECORDING_FILES: &str = "recording_files";

pub const DATA_TABLES: &[&str] = &[
    USERS,
    RECORDINGS,
    REPORTED_MEETINGS,
    PARTICIPANTS,
    SUMMARY_LISTINGS,
    SUMMARIES,
    RECORDING_FILES,
];

/// `users` — the user whose meetings this source copies, keyed by Zoom's
/// user id: `GET /users/{user}` as it answered.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "users")]
pub struct UserRow {
    pub id_and_payload: WirePayload,
}

/// Fields of a user that move without anything about the person
/// changing.
pub const USER_VOLATILE_PATHS: &[dr::VolatilePath] =
    &[&["last_login_time"], &["last_client_version"]];

/// `recordings` — one row per recorded meeting, keyed by its UUID: the
/// entry `GET /users/{user}/recordings` lists for it, its
/// `recording_files` inside. Rewritten whenever a later listing differs
/// (a transcript that finished processing adds a file); never deleted,
/// so a recording deleted in Zoom keeps its row and its files here.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "recordings")]
pub struct RecordingRow {
    pub id_and_payload: WirePayload,
    pub start_time: Option<String>,
}

/// `reported_meetings` — one row per meeting Zoom's past-meetings report
/// lists for the user (`GET /report/users/{user}/meetings`), keyed by its
/// UUID: the meetings that were not recorded, and the ones that were.
/// Zoom reports only meetings with two or more participants.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "reported_meetings")]
pub struct ReportedMeetingRow {
    pub id_and_payload: WirePayload,
    pub start_time: Option<String>,
}

/// `participants` — who attended one meeting, keyed by its UUID:
/// `{"participants": [...]}`, every page of
/// `GET /past_meetings/{uuid}/participants` in order. Fetched once, at
/// no version; empty where Zoom would not say.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "participants")]
pub struct ParticipantsRow {
    pub id_and_payload: WirePayload,
}

/// `summary_listings` — one row per AI Companion summary
/// `GET /users/{user}/meeting_summaries` lists, keyed by the meeting's
/// UUID. `last_modified` is the version the summary is held at.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "summary_listings")]
pub struct SummaryListingRow {
    pub id_and_payload: WirePayload,
    pub last_modified: Option<String>,
}

/// `summaries` — one meeting's summary, keyed by its UUID:
/// `GET /meetings/{uuid}/meeting_summary` as it answered, or
/// `{"unavailable": "<why>"}` for one Zoom listed and would not give.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "summaries")]
pub struct SummaryRow {
    pub id_and_payload: WirePayload,
}

/// `recording_files` — one recording file's bytes in the CAS, keyed
/// `{meeting_uuid}#{file_key}`. `file_key` is Zoom's file id, or for the
/// kinds Zoom gives none (a closed-caption or timeline file) the file
/// type in lower case. `blake3` is NULL for a file Zoom no longer has;
/// a warning on the edge says so.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "recording_files")]
pub struct RecordingFileRow {
    pub id: String,
    pub meeting_uuid: String,
    pub file_key: String,
    pub blake3: Option<String>,
}

/// The coverage scopes, one per listing and user: what part of the
/// calendar that listing has been read for.
pub fn coverage_scope(listing: &str, user_id: &str) -> String {
    format!("{listing}:{user_id}")
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[];

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        UserRow::ddl(),
        RecordingRow::ddl(),
        ReportedMeetingRow::ddl(),
        ParticipantsRow::ddl(),
        SummaryListingRow::ddl(),
        SummaryRow::ddl(),
    ];
    out.extend(RecordingFileRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out.push(datalib_etl_web::coverage::DDL.to_string());
    out
}
