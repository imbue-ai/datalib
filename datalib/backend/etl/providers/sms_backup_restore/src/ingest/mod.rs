//! "SMS Backup & Restore" export ingester.

pub mod parse;
pub mod schema_raw;

use datalib_etl_files::fingerprint_cache::FingerprintCache;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::download_problems::RunProblem;
use datalib_etl::progress::Progress;
use datalib_etl::prune;
use datalib_etl::read_marks::{self, ReadMarkRow};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_files::file_changes_scan;
use datalib_etl_files::file_checkpoint;
use datalib_problems::{Outcome, Problem, Reason, Severity};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::json;
use sqlx::{Sqlite, Transaction};

use self::parse::{CallRecord, MmsRecord, RootKind, SmsRecord};
use self::schema_raw::{
    full_ddl, ns_id, sha8, SmsAttachmentRow, SmsCallRow, SmsMessageRow, LADDER,
};

pub use datalib_etl::doltlite_raw::db_path_for;

const SCOPE: &str = "sms_backup_restore/xml";

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Root of the user's export (the directory holding `sms-*.xml` /
    /// `calls-*.xml`). A single file path is also accepted.
    pub input_path: PathBuf,
    /// Host-wide fingerprint cache: the shared answer to "did this
    /// file change?", so an unchanged export costs a `stat`.
    pub cache: FingerprintCache,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    pub files: usize,
    pub sms: usize,
    pub mms: usize,
    pub calls: usize,
    pub attachments: usize,
    pub blobs_stored: usize,
    pub parse_errors: usize,
    /// Messages and calls deleted because no file still holds them.
    pub removed: usize,
    /// Backup files that are gone since the last run.
    pub files_removed: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_backups(opts, found)).await
}

async fn read_backups(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db.clone();

    // One scan answers what `.xml` files are there and which have
    // changed since the last pass — walk, hash, and the decision not
    // to re-hash what the host cache can vouch for, all in the utility.
    let scan = file_changes_scan::scan(
        &opts.cache,
        &opts.input_path,
        &file_changes_scan::ScanOptions {
            progress: opts.progress.clone(),
            ..Default::default()
        },
        |p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xml")),
    )
    .await?;
    let prev = file_checkpoint::load_cursor(db.pool(), SCOPE).await?;
    let changes = scan.changes_since(&prev);
    // Backups are snapshots and several can hold one message, so only a read
    // of every file says which records left the input.
    let read_all = changes.may_have_dropped_records()
        || changes.needs_reading().count() == scan.files.len()
        || an_mms_lacks_its_addrs(db.pool()).await?;
    let to_read: Vec<&file_changes_scan::ScannedFile> = if read_all {
        scan.files.iter().collect()
    } else {
        changes.needs_reading().collect()
    };

    let mut message_rows: Vec<SmsMessageRow> = Vec::new();
    let mut reads: Vec<Read> = Vec::new();
    let mut call_rows: Vec<SmsCallRow> = Vec::new();
    let mut acc = CasEdgeAccumulator::new();
    let mut done: Vec<&file_changes_scan::ScannedFile> = Vec::new();
    let mut summary = FetchSummary::default();
    // Files that would not open. They are left unstamped, so the next run
    // reads them again.
    let mut unread: Vec<RunProblem> = Vec::new();
    // Files that opened and would not parse. Reading them again gets the
    // same answer, so they are stamped with their problem and read again
    // only once they change.
    let mut unparsed: Vec<(&file_changes_scan::ScannedFile, String)> = Vec::new();
    // XML that is not an export: stamped with its problem the same way.
    let mut foreign: Vec<&file_changes_scan::ScannedFile> = Vec::new();

    let mut parts_without_data: HashMap<&str, usize> = HashMap::new();
    for f in to_read {
        let path = &f.path;
        let xml = match std::fs::read_to_string(path) {
            Ok(x) => x,
            Err(e) => {
                unread.push(RunProblem::listing(
                    &format!("file {}", f.rel),
                    e.to_string(),
                ));
                summary.parse_errors += 1;
                continue;
            }
        };
        // The parsers read whatever the file holds, so a panic in one is
        // this file's failure, not the run's.
        let smses = || run_problems::catching_panics(|| parse::parse_smses(&xml));
        let calls = || run_problems::catching_panics(|| parse::parse_calls(&xml));
        match run_problems::catching_panics(|| parse::detect_root(&xml)) {
            Ok(RootKind::Smses) => match smses() {
                Ok((smses, mmses)) => {
                    for s in smses {
                        ingest_sms(&s, &mut message_rows, &mut reads);
                        summary.sms += 1;
                    }
                    for m in mmses {
                        *parts_without_data.entry(f.rel.as_str()).or_default() +=
                            m.parts_without_data;
                        ingest_mms(
                            &m,
                            &mut message_rows,
                            &mut reads,
                            &mut acc,
                            &mut summary.attachments,
                        );
                        summary.mms += 1;
                    }
                    done.push(f);
                    summary.files += 1;
                }
                Err(e) => {
                    unparsed.push((f, format!("{e:#}")));
                    summary.parse_errors += 1;
                }
            },
            Ok(RootKind::Calls) => match calls() {
                Ok(calls) => {
                    for c in calls {
                        ingest_call(&c, &mut call_rows);
                        summary.calls += 1;
                    }
                    done.push(f);
                    summary.files += 1;
                }
                Err(e) => {
                    unparsed.push((f, format!("{e:#}")));
                    summary.parse_errors += 1;
                }
            },
            Ok(RootKind::Other) => foreign.push(f),
            Err(e) => {
                unparsed.push((f, format!("{e:#}")));
                summary.parse_errors += 1;
            }
        }
        opts.progress.set_message(&format!(
            "sms_backup_restore: {} sms / {} mms / {} calls ({} files)",
            summary.sms, summary.mms, summary.calls, summary.files,
        ));
    }

    summary.blobs_stored = acc.fetched_len();

    // Whether this run may delete what no file holds: only right after
    // reading every one of them. A file there and unread may hold any of
    // them.
    let deletes = read_all
        && changes.walk_errors == 0
        && summary.parse_errors == 0
        && scan.present_unread.is_empty();
    // A rewritten file keeps its old stamp on a run that deleted nothing,
    // so the next run still sees it rewritten and reads every file again.
    let rewritten: HashSet<&str> = changes.modified.iter().map(|f| f.rel.as_str()).collect();

    // The attachments land before the files that name them are stamped:
    // a flush that fails leaves the files to be read again.
    acc.flush(db.pool(), db.cas(), |owning, ref_id, blake3| {
        SmsAttachmentRow {
            id: SmsAttachmentRow::pk_recipe(owning, ref_id),
            message_id: owning.to_string(),
            ref_name: ref_id.to_string(),
            blake3: blake3.map(str::to_string),
        }
    })
    .await?;

    // The prune lands with the stamps: a stamp that committed without it
    // would tell the next run the rewritten file was already dealt with.
    let now = IsoOffsetTimestamp::now_local();
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin sms_backup_restore tx")?;
    bulk_upsert_in_tx(&mut tx, &message_rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &call_rows, &now).await?;
    let (marks, unknown) = last_word_on_each(&reads);
    read_marks::write(&mut tx, &marks).await?;
    read_marks::forget(&mut tx, &unknown).await?;
    for f in done
        .iter()
        .filter(|f| deletes || !rewritten.contains(f.rel.as_str()))
    {
        // One row a file: a backup taken without media is missing every part.
        match parts_without_data.get(f.rel.as_str()) {
            Some(&n) if n > 0 => {
                let problem = Problem::explained(
                    Reason::DeliberateLoss,
                    None,
                    &format!(
                        "{n} MMS part(s) hold no data, as in a backup taken without media, so \
                         they are not in the mirror"
                    ),
                )
                .severity(Severity::Warning);
                file_checkpoint::record_file_with_problem(
                    &mut tx,
                    SCOPE,
                    f,
                    Some((Outcome::Nulled, problem)),
                )
                .await?;
            }
            _ => file_checkpoint::record_file(&mut tx, SCOPE, f).await?,
        }
    }
    for (f, why) in &unparsed {
        let problem = Problem::record(
            Reason::Undeserializable,
            &format!("the file did not parse, so none of its records were read: {why}"),
        );
        file_checkpoint::record_file_with_problem(
            &mut tx,
            SCOPE,
            f,
            Some((Outcome::Dropped, problem)),
        )
        .await?;
    }
    for f in &foreign {
        let problem = Problem::record(
            Reason::Undeserializable,
            "the file is XML but not an <smses> or <calls> export, so nothing in it was read",
        )
        .severity(Severity::Warning);
        file_checkpoint::record_file_with_problem(
            &mut tx,
            SCOPE,
            f,
            Some((Outcome::Dropped, problem)),
        )
        .await?;
    }
    if deletes {
        summary.removed = prune_unseen(&mut tx, &message_rows, &call_rows).await?;
        let gone = changes.gone();
        summary.files_removed = gone.len();
        for rel in gone {
            file_checkpoint::forget_file(&mut tx, SCOPE, rel).await?;
        }
    }
    tx.commit().await.context("commit sms_backup_restore tx")?;

    scan.report_problems(&found, "files");
    let mut problems = unread;
    if !deletes && read_all && changes.walk_errors == 0 && changes.may_have_dropped_records() {
        problems.push(file_changes_scan::Scan::deletions_held_back(
            summary.parse_errors + scan.present_unread.len(),
        ));
    }
    found.extend(problems);

    Ok(summary)
}

/// Delete the messages and calls no file holds, with their attachment
/// edges. Only right after reading every file. Returns how
/// many records went.
/// An MMS stored before its `addrs` were kept: every file is read again,
/// once, so a root from before has them too.
async fn an_mms_lacks_its_addrs(pool: &sqlx::SqlitePool) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sms_messages \
         WHERE kind = 'mms' AND json_extract(payload, '$.addrs') IS NULL)",
    )
    .fetch_one(pool)
    .await
    .context("look for an MMS stored without its addrs")
}

async fn prune_unseen(
    tx: &mut Transaction<'_, Sqlite>,
    messages: &[SmsMessageRow],
    calls: &[SmsCallRow],
) -> Result<usize> {
    let keep_messages: HashSet<String> = messages
        .iter()
        .map(|r| r.id_and_payload.id.clone())
        .collect();
    let keep_calls: HashSet<String> = calls.iter().map(|r| r.id_and_payload.id.clone()).collect();
    let gone_messages = prune::prune_scope_in_tx(tx, "sms_messages", &[], &keep_messages).await?;
    prune::delete_owned_in_tx(tx, "sms_attachments", "message_id", &gone_messages).await?;
    read_marks::forget(tx, &gone_messages).await?;
    let gone_calls = prune::prune_scope_in_tx(tx, "sms_calls", &[], &keep_calls).await?;
    prune::record(
        "sms_messages",
        keep_messages.len() + gone_messages.len(),
        gone_messages.len(),
    );
    prune::record(
        "sms_calls",
        keep_calls.len() + gone_calls.len(),
        gone_calls.len(),
    );
    Ok(gone_messages.len() + gone_calls.len())
}

/// What one backup file says of whether a message was read; `None` when
/// it says nothing.
type Read = (String, Option<bool>);

/// The marks to write and the ids to forget. A message is in several
/// backups; the file read last has the last word, as it does on the
/// message's row.
fn last_word_on_each(reads: &[Read]) -> (Vec<ReadMarkRow>, Vec<String>) {
    let last: std::collections::BTreeMap<&str, Option<bool>> = reads
        .iter()
        .map(|(id, read)| (id.as_str(), *read))
        .collect();
    let mut marks = Vec::new();
    let mut unknown = Vec::new();
    for (id, read) in last {
        match read {
            Some(read) => marks.push(ReadMarkRow::read(id, read)),
            None => unknown.push(id.to_string()),
        }
    }
    (marks, unknown)
}

fn ingest_sms(s: &SmsRecord, rows: &mut Vec<SmsMessageRow>, reads: &mut Vec<Read>) {
    let tel = normalize_tel(&s.address);
    let display = conversation_display(&tel, s.contact_name.as_deref());
    let is_me = s.type_ == 2;
    let box_kind = direction(s.type_);
    let when = ms_to_rfc3339(s.date_ms);
    let id = ns_id(&format!(
        "sms:{tel}:{}:{}:{}",
        s.date_ms,
        s.type_,
        sha8(&s.body)
    ));
    let payload = json!({
        "id": id,
        "kind": "sms",
        "conversation_key": tel,
        "conversation_display": display,
        "when": when,
        "date": s.date_ms,
        "box": box_kind,
        "is_me": is_me,
        "address": s.address,
        "body": s.body,
        "date_sent": s.date_sent_ms,
        "readable_date": s.readable_date,
        "contact_name": s.contact_name,
        "attachments": Vec::<String>::new(),
    });
    reads.push((id.clone(), s.read));
    rows.push(SmsMessageRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(tel),
        when_ts: when,
        kind: Some("sms".to_string()),
        box_kind: Some(box_kind.to_string()),
    });
}

fn ingest_mms(
    m: &MmsRecord,
    rows: &mut Vec<SmsMessageRow>,
    reads: &mut Vec<Read>,
    acc: &mut CasEdgeAccumulator,
    n_attachments: &mut usize,
) {
    let tel = normalize_tel(&m.address);
    let display = conversation_display(&tel, m.contact_name.as_deref());
    let is_me = m.msg_box == 2;
    let box_kind = direction_box(m.msg_box);
    let when = ms_to_rfc3339(m.date_ms);
    // Prefer the carrier message-id, then the transaction id, then a
    // hash of the body text — whichever is the most stable available
    // discriminator for this (address, date) pair.
    let stable = m
        .m_id
        .clone()
        .or_else(|| m.tr_id.clone())
        .unwrap_or_else(|| sha8(&m.text));
    let id = ns_id(&format!("mms:{tel}:{}:{stable}", m.date_ms));

    let mut attachment_refs: Vec<String> = Vec::new();
    for blob in &m.blobs {
        // Prefix with the message id so the app's non-unique part names
        // (image000000.jpg, recording000000.m4a, …) can't collide in a
        // conversation's blob bundle at render time.
        let ref_name = format!("{id}/{}", blob.name);
        acc.add_fetched(
            &id,
            &ref_name,
            blob.bytes.clone(),
            Some(blob.content_type.clone()),
        );
        *n_attachments += 1;
        attachment_refs.push(ref_name);
    }
    // Listed with the rest, so render draws the gap where the part was.
    for (name, why) in &m.failed_blobs {
        let ref_name = format!("{id}/{name}");
        acc.add_failed(
            &id,
            &ref_name,
            format!("the part's base64 did not decode: {why}"),
        );
        attachment_refs.push(ref_name);
    }

    let payload = json!({
        "id": id,
        "kind": "mms",
        "conversation_key": tel,
        "conversation_display": display,
        "when": when,
        "date": m.date_ms,
        "box": box_kind,
        "is_me": is_me,
        "address": m.address,
        "text": m.text,
        "m_id": m.m_id,
        "tr_id": m.tr_id,
        "date_sent": m.date_sent_ms,
        "readable_date": m.readable_date,
        "contact_name": m.contact_name,
        "attachments": attachment_refs,
        "addrs": m
            .addrs
            .iter()
            .map(|a| json!({"address": a.address, "type": a.type_}))
            .collect::<Vec<_>>(),
    });
    reads.push((id.clone(), m.read));
    rows.push(SmsMessageRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(tel),
        when_ts: when,
        kind: Some("mms".to_string()),
        box_kind: Some(box_kind.to_string()),
    });
}

fn ingest_call(c: &CallRecord, rows: &mut Vec<SmsCallRow>) {
    let tel = normalize_tel(&c.number);
    let display = conversation_display(&tel, c.contact_name.as_deref());
    let when = ms_to_rfc3339(c.date_ms);
    let call_type = call_type_str(c.type_);
    let id = ns_id(&format!(
        "call:{tel}:{}:{}:{}",
        c.date_ms, c.type_, c.duration_s
    ));
    let payload = json!({
        "id": id,
        "kind": "call",
        "conversation_key": tel,
        "conversation_display": display,
        "when": when,
        "date": c.date_ms,
        "call_type": call_type,
        "number": c.number,
        "duration": c.duration_s,
        "readable_date": c.readable_date,
        "contact_name": c.contact_name,
    });
    rows.push(SmsCallRow {
        id_and_payload: WirePayload {
            id,
            payload: payload.to_string(),
        },
        conversation_key: Some(tel),
        when_ts: when,
        call_type: Some(call_type.to_string()),
    });
}

// ── helpers ─────────────────────────────────────────────────────────

/// Canonicalize a phone number / address to `+` and digits so the same
/// contact keys to one conversation regardless of source formatting.
/// Short codes (e.g. `8880`) and alphanumeric senders pass through
/// digits-only. Empty input → `"unknown"`.
fn normalize_tel(addr: &str) -> String {
    let trimmed = addr.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    let plus = trimmed.starts_with('+');
    let digits: String = trimmed.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        // Alphanumeric sender id (e.g. "VERIZON"): keep as-is.
        return trimmed.to_string();
    }
    if plus {
        format!("+{digits}")
    } else {
        digits
    }
}

fn conversation_display(tel: &str, contact_name: Option<&str>) -> String {
    clean_contact(contact_name).unwrap_or_else(|| tel.to_string())
}

fn clean_contact(contact_name: Option<&str>) -> Option<String> {
    contact_name
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "(Unknown)" && *s != "null")
        .map(str::to_string)
}

fn direction(sms_type: i64) -> &'static str {
    if sms_type == 2 {
        "sent"
    } else {
        "inbox"
    }
}

fn direction_box(msg_box: i64) -> &'static str {
    if msg_box == 2 {
        "sent"
    } else {
        "inbox"
    }
}

fn call_type_str(t: i64) -> &'static str {
    match t {
        1 => "incoming",
        2 => "outgoing",
        3 => "missed",
        4 => "voicemail",
        5 => "rejected",
        6 => "blocked",
        _ => "call",
    }
}

fn ms_to_rfc3339(ms: i64) -> Option<String> {
    use chrono::TimeZone;
    if ms <= 0 {
        return None;
    }
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_tel_keeps_plus_and_digits() {
        assert_eq!(normalize_tel("+1 (647) 449-5789"), "+16474495789");
        assert_eq!(normalize_tel("7786121000"), "7786121000");
        assert_eq!(normalize_tel("8880"), "8880");
        assert_eq!(normalize_tel("VERIZON"), "VERIZON");
        assert_eq!(normalize_tel(""), "unknown");
    }

    #[test]
    fn call_and_sms_with_same_number_share_conversation_key() {
        let mut msgs = Vec::new();
        ingest_sms(
            &SmsRecord {
                address: "+1 (647) 449-5789".into(),
                date_ms: 1778277198761,
                type_: 1,
                body: "hi".into(),
                ..Default::default()
            },
            &mut msgs,
            &mut Vec::new(),
        );
        let mut calls = Vec::new();
        ingest_call(
            &CallRecord {
                number: "+16474495789".into(),
                duration_s: 0,
                date_ms: 1778698683617,
                type_: 3,
                ..Default::default()
            },
            &mut calls,
        );
        assert_eq!(msgs[0].conversation_key, calls[0].conversation_key);
    }

    #[test]
    fn ms_to_rfc3339_has_explicit_offset() {
        let s = ms_to_rfc3339(1781547510000).unwrap();
        assert!(s.ends_with("+00:00"), "explicit offset: {s}");
        assert!(datalib_time::parse_strict(&s).is_ok());
        assert_eq!(ms_to_rfc3339(0), None);
    }

    #[test]
    fn clean_contact_drops_placeholders() {
        assert_eq!(clean_contact(Some("(Unknown)")), None);
        assert_eq!(clean_contact(Some("")), None);
        assert_eq!(
            clean_contact(Some("Jean-Luc Picard")).as_deref(),
            Some("Jean-Luc Picard")
        );
    }
}
