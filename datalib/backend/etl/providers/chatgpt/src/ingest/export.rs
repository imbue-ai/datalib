//! Ingest a ChatGPT **data export** (chatgpt.com → Settings → Data
//! controls → Export data), zipped or unzipped, into the raw store the API download
//! writes: the same rows, each held at its detail's `update_time`. No
//! request is made, so no rate limit applies. An `api` run given an
//! export too takes from it what it would otherwise fetch
//! ([`ExportCopy`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use datalib_etl::blob_cas::{CasEdgeRow as _, CasInsert};
use datalib_etl::bulk::bulk_upsert_first_seen_in_tx;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_web::owed::{self, Listed};
use serde::Serialize;
use serde_json::Value;
use tracing::{info, instrument};

use super::db::{FileRef, RawDb};
use super::export_archive::ExportArchive;
use super::schema_raw::{ConversationAttachmentRow, MeRow, ATTACHMENTS, CONVERSATIONS};
use super::{attachment_targets, parse_conversation, version_of};

/// Attachment bytes read before they are written: a few large files at
/// a time, not the whole export, in memory.
const BLOB_BATCH_BYTES: usize = 32 << 20;

#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// The export's `.zip`, or the folder it was unzipped into
    /// (`export.path`).
    pub input_path: PathBuf,
    /// Run timestamp, shared by every bookkeeping stamp this ingest
    /// writes.
    pub now: String,
    pub progress: datalib_etl::progress::Progress,
    pub control: datalib_etl::control::DownloadControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct IngestSummary {
    /// Conversations written: new, or at a version the store did not hold.
    pub conversations: usize,
    /// Conversations the store already held at the export's version.
    pub unchanged: usize,
    /// Conversations dropped because the export no longer has them.
    pub pruned: usize,
    /// Conversations the export holds with no id to store them under.
    pub without_id: usize,
    pub new_blobs: usize,
    /// Edges satisfied from bytes the CAS already held.
    pub skipped_blobs: usize,
    /// Files a conversation names that the export has no copy of.
    pub not_in_export: usize,
}

#[instrument(skip_all, fields(export = %opts.input_path.display()))]
pub async fn ingest(opts: IngestOptions) -> Result<IngestSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_export(opts, found)).await
}

async fn read_export(opts: IngestOptions, found: RunProblems) -> Result<IngestSummary> {
    // Read before writing, so a malformed export fails the run without
    // having touched the store.
    let mut archive = ExportArchive::open(&opts.input_path)?;
    let conversations = read_conversations(&mut archive)?;
    let user = archive
        .read_json("user.json")?
        .filter(|u| u.get("id").and_then(Value::as_str).is_some());
    check_account(&opts.db, user.as_ref()).await?;

    let mut summary = IngestSummary::default();
    let files =
        conversations_in_one_tx(&opts, &conversations, user.as_ref(), &mut summary, &found).await?;
    drop(conversations);
    attachments(&opts, &mut archive, &files, &mut summary, &found).await?;
    Ok(summary)
}

/// Every conversation, from each part `conversations.json` is split
/// into. A part the manifest lists and the export lacks fails the run:
/// read as a smaller export, it would prune what the part holds.
fn read_conversations(archive: &mut ExportArchive) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for part in archive.conversation_parts()? {
        match archive.read_json(&part)? {
            Some(Value::Array(a)) => out.extend(a),
            Some(_) => bail!("{part} must hold a JSON array"),
            None => bail!("the export has no {part}; is it whole?"),
        }
    }
    Ok(out)
}

/// An export of one account read into a store holding another would
/// prune every conversation the store holds.
async fn check_account(db: &RawDb, user: Option<&Value>) -> Result<()> {
    let (Some(user), Some(held)) = (user, db.load_me().await?) else {
        return Ok(());
    };
    let (theirs, ours) = (user["id"].as_str(), held["id"].as_str());
    if ours.is_some() && theirs != ours {
        bail!(
            "this export is of account {theirs:?}, and this source holds {ours:?}; \
             add a new source for that account's export"
        );
    }
    Ok(())
}

fn id_of(conv: &Value) -> Option<&str> {
    conv.get("conversation_id")
        .or_else(|| conv.get("id"))
        .and_then(Value::as_str)
}

/// The conversations and the prune in one transaction, so render never
/// sees half an export. Answers what files each conversation names,
/// for the attachment pass.
async fn conversations_in_one_tx(
    opts: &IngestOptions,
    conversations: &[Value],
    user: Option<&Value>,
    summary: &mut IngestSummary,
    found: &RunProblems,
) -> Result<HashMap<String, Vec<FileRef>>> {
    let db = &opts.db;
    let listed: Vec<(Listed, &Value)> = conversations
        .iter()
        .filter_map(|c| Some((Listed::new(id_of(c)?, version_of(c.get("update_time"))), c)))
        .collect();
    summary.without_id = conversations.len() - listed.len();
    let files: HashMap<String, Vec<FileRef>> = listed
        .iter()
        .map(|(l, c)| (l.key.clone(), attachment_targets(c)))
        .collect();
    let held = owed::held_versions(
        db.pool(),
        CONVERSATIONS,
        listed.iter().map(|(l, _)| l.key.as_str()),
    )
    .await?;
    let has_me = db.load_me().await?.is_some();

    let now = datalib_time::parse_strict(&opts.now).context("run-pinned now")?;
    opts.progress.set_length(Some(listed.len() as u64));
    let mut tx = db.pool().begin().await.context("begin export ingest tx")?;
    if let (Some(user), false) = (user, has_me) {
        bulk_upsert_first_seen_in_tx(&mut tx, &[me_row(user)?], &now).await?;
    }
    for (l, c) in &listed {
        opts.progress.inc(1);
        if held.get(&l.key).is_some_and(|h| h.satisfies(&l.version)) {
            summary.unchanged += 1;
            continue;
        }
        db.store_conversation(&mut tx, &parse_conversation(&l.key, c)?)
            .await?;
        owed::hold(&mut tx, CONVERSATIONS, &l.key, l.version.as_deref()).await?;
        summary.conversations += 1;
    }
    if conversations.is_empty() || !listed.is_empty() {
        let keep: HashSet<String> = files.keys().cloned().collect();
        let gone =
            datalib_etl::prune::prune_scope_in_tx(&mut tx, CONVERSATIONS, &[], &keep).await?;
        datalib_etl::prune::delete_owned_in_tx(&mut tx, ATTACHMENTS, "conversation_id", &gone)
            .await?;
        summary.pruned = gone.len();
    } else {
        found.phase(
            "conversations.json",
            format!(
                "none of its {} entries has an id, a shape this reader does not know, \
                 so nothing stored was deleted",
                conversations.len()
            ),
        );
    }
    tx.commit().await.context("commit export ingest tx")?;

    if summary.without_id > 0 {
        found.phase(
            "export",
            format!(
                "{} conversations in the export have no id; they were not stored",
                summary.without_id
            ),
        );
    }
    if summary.pruned > 0 {
        // Loud on purpose: the one path here that removes stored rows.
        info!(
            event = "chatgpt_export_pruned",
            rows = summary.pruned,
            "dropped conversations the export no longer contains",
        );
    }
    Ok(files)
}

fn me_row(user: &Value) -> Result<MeRow> {
    let str_at = |k: &str| user.get(k).and_then(Value::as_str).map(String::from);
    Ok(MeRow {
        id_and_payload: WirePayload {
            id: str_at("id").unwrap_or_default(),
            payload: serde_json::to_string(user).context("serialize user.json")?,
        },
        email: str_at("email"),
        name: str_at("name"),
    })
}

/// Every attachment edge the store owes, its bytes taken from the
/// export's copy of the file. An edge the export has no copy of stays
/// owed, so an `api` run fetches it.
async fn attachments(
    opts: &IngestOptions,
    archive: &mut ExportArchive,
    files: &HashMap<String, Vec<FileRef>>,
    summary: &mut IngestSummary,
    found: &RunProblems,
) -> Result<()> {
    let db = &opts.db;
    let owed = owed::owed(db.pool(), ATTACHMENTS, db.attachments_listed().await?).await?;
    if owed.is_empty() {
        return Ok(());
    }
    let in_export = archive.files_by_id();
    opts.progress.set_message("attachments");
    opts.progress.set_length(Some(owed.len() as u64));

    let mut batch: Vec<(Listed, Bytes)> = Vec::new();
    let mut batch_bytes = 0usize;
    for edge in owed {
        if opts.control.stop.requested() {
            break;
        }
        opts.progress.inc(1);
        let (Some(conv_id), Some((_, file_id))) = (
            ConversationAttachmentRow::owning_id_of(&edge.key),
            edge.key.rsplit_once('#'),
        ) else {
            continue;
        };
        let bytes = if let Some(blake3) = db.blake3_of_file(file_id).await? {
            summary.skipped_blobs += 1;
            Bytes::Held(blake3)
        } else if let Some(name) = in_export.get(file_id) {
            let read = archive
                .read(name)?
                .with_context(|| format!("{name} went from the export as it was read"))?;
            let mime = files
                .get(conv_id)
                .and_then(|fs| fs.iter().find(|f| f.id == file_id))
                .and_then(|f| f.mime.clone());
            batch_bytes += read.len();
            summary.new_blobs += 1;
            Bytes::Read(read, mime)
        } else {
            summary.not_in_export += 1;
            continue;
        };
        batch.push((edge, bytes));
        if batch_bytes >= BLOB_BATCH_BYTES {
            store_blobs(db, std::mem::take(&mut batch)).await?;
            batch_bytes = 0;
        }
    }
    store_blobs(db, batch).await?;
    if summary.not_in_export > 0 {
        found.phase(
            "attachments",
            format!(
                "{} attached files are not in the export; an `api` run fetches them",
                summary.not_in_export
            ),
        );
    }
    Ok(())
}

/// An export an `api` run asks before chatgpt.com: a conversation it
/// holds at or after the version the listing names, and a file's bytes. Never a
/// census: what it lacks, or holds and the listing does not name, says
/// nothing about what exists, so it neither prunes nor adds.
pub struct ExportCopy {
    conversations: HashMap<String, Value>,
    account: Option<String>,
    files: HashMap<String, String>,
    archive: Mutex<ExportArchive>,
}

impl ExportCopy {
    /// Reads every conversation up front, so a malformed export fails
    /// the run before it has written anything.
    pub fn open(path: &Path) -> Result<Self> {
        let mut archive = ExportArchive::open(path)?;
        let conversations = read_conversations(&mut archive)?
            .into_iter()
            .filter_map(|c| Some((id_of(&c)?.to_string(), c)))
            .collect();
        let account = archive
            .read_json("user.json")?
            .and_then(|u| u.get("id")?.as_str().map(String::from));
        let files = archive.files_by_id();
        Ok(Self {
            conversations,
            account,
            files,
            archive: Mutex::new(archive),
        })
    }

    /// The export's copy of `listed`, when it is no older than the
    /// version listed. The export carries the detail's `update_time`,
    /// and the listing's can trail it by seconds to days (INGEST.md,
    /// "The listing can trail the detail"), so the two are compared as
    /// instants, not matched as text.
    pub fn conversation_at(&self, listed: &Listed) -> Option<&Value> {
        let c = self.conversations.get(&listed.key)?;
        let ours = version_of(c.get("update_time"));
        let secs = |v: &Option<String>| v.as_deref()?.parse::<i64>().ok();
        let current = match (secs(&ours), secs(&listed.version)) {
            (Some(ours), Some(listed)) => ours >= listed,
            _ => listed.version.is_some() && ours == listed.version,
        };
        current.then_some(c)
    }

    pub fn file(&self, file_id: &str) -> Result<Option<Vec<u8>>> {
        let Some(name) = self.files.get(file_id) else {
            return Ok(None);
        };
        self.archive.lock().unwrap().read(name)
    }

    /// An export of another account than the one signed in would never
    /// match a listed conversation; it is a config mistake, said as one.
    pub fn check_account(&self, me: &Value) -> Result<()> {
        let (Some(theirs), Some(ours)) = (self.account.as_deref(), me["id"].as_str()) else {
            return Ok(());
        };
        if theirs != ours {
            bail!(
                "`export.path` is an export of account {theirs:?}, and `api` is signed in as \
                 {ours:?}; point it at this account's export"
            );
        }
        Ok(())
    }
}

enum Bytes {
    /// The CAS already holds the file, under another conversation.
    Held(String),
    Read(Vec<u8>, Option<String>),
}

async fn store_blobs(db: &RawDb, batch: Vec<(Listed, Bytes)>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let inserts: Vec<CasInsert<'_, &str>> = batch
        .iter()
        .filter_map(|(edge, b)| match b {
            Bytes::Read(bytes, mime) => Some(CasInsert {
                id: edge.key.as_str(),
                bytes,
                content_type: mime.as_deref(),
            }),
            Bytes::Held(_) => None,
        })
        .collect();
    let stored = db.cas().put_many(inserts).await?;
    let mut tx = db.pool().begin().await.context("begin export blobs tx")?;
    for (edge, b) in &batch {
        let blake3 = match b {
            Bytes::Held(held) => held,
            Bytes::Read(..) => &stored[edge.key.as_str()],
        };
        db.store_blob(&mut tx, &edge.key, blake3).await?;
        owed::hold(&mut tx, ATTACHMENTS, &edge.key, edge.version.as_deref()).await?;
    }
    tx.commit().await.context("commit export blobs tx")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::db::db_path_for;
    use super::*;
    use serde_json::json;

    const NOW: &str = "2026-10-22T00:00:00-07:00";

    fn write(dir: &Path, rel: &str, v: &Value) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    fn opts(db: &RawDb, export: &Path) -> IngestOptions {
        IngestOptions {
            db: db.clone(),
            input_path: export.to_path_buf(),
            now: NOW.to_string(),
            progress: Default::default(),
            control: Default::default(),
        }
    }

    /// One conversation, naming `files` as uploads on its one message.
    fn conv(id: &str, update_time: f64, files: &[&str]) -> Value {
        let atts: Vec<Value> = files
            .iter()
            .map(|f| json!({"id": f, "name": format!("{f}.txt"), "mime_type": "text/plain"}))
            .collect();
        json!({
            "id": id,
            "conversation_id": id,
            "title": format!("Log {id}"),
            "create_time": update_time - 60.0,
            "update_time": update_time,
            "current_node": "m1",
            "mapping": {
                "m1": {"id": "m1", "parent": null, "children": [], "message": {
                    "id": "m1", "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["Engage."]},
                    "metadata": {"attachments": atts},
                }},
            },
        })
    }

    async fn ids(db: &RawDb) -> Vec<String> {
        sqlx::query_scalar("SELECT id FROM conversations ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    async fn problem_keys(db: &RawDb) -> Vec<String> {
        sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY 1")
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    /// The export's conversations, its account and its files land in
    /// the tables the API download writes, each held at the version an
    /// API listing would name, so an API run after it fetches nothing.
    #[tokio::test]
    async fn an_export_fills_the_store_and_holds_what_it_wrote() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([
                conv("c1", 1_710_959_331.42, &["file-AbC1"]),
                conv("c2", 1_711_000_000.0, &[])
            ]),
        );
        write(
            ex.path(),
            "user.json",
            &json!({"id": "user-data", "email": "data@enterprise.test"}),
        );
        std::fs::create_dir_all(ex.path().join("user-data")).unwrap();
        std::fs::write(
            ex.path().join("user-data/file-AbC1-log.txt"),
            b"Captain's log",
        )
        .unwrap();

        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        let s = ingest(opts(&db, ex.path())).await.unwrap();
        assert_eq!((s.conversations, s.new_blobs, s.not_in_export), (2, 1, 0));
        assert_eq!(ids(&db).await, ["c1", "c2"]);
        assert_eq!(db.load_me().await.unwrap().unwrap()["id"], "user-data");

        let listed = vec![
            Listed::new("c1", Some("1710959331")),
            Listed::new("c2", Some("1711000000")),
        ];
        assert!(owed::owed(db.pool(), CONVERSATIONS, listed)
            .await
            .unwrap()
            .is_empty());
        let edges = db.attachments_listed().await.unwrap();
        assert!(owed::owed(db.pool(), ATTACHMENTS, edges)
            .await
            .unwrap()
            .is_empty());
        let blake3 = db.blake3_of_file("file-AbC1").await.unwrap().unwrap();
        let blob = db.cas().get(&blake3).await.unwrap().unwrap();
        assert_eq!(blob.content_type.as_deref(), Some("text/plain"));
        db.close().await;
    }

    /// Every run reads the whole export, so reading an unchanged one
    /// again must leave the store as it was: a re-stamped sidecar is a
    /// commit, and a bigger store, on every sync.
    #[tokio::test]
    async fn reading_an_unchanged_export_again_commits_nothing() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", 1_710_959_331.0, &["file-AbC1"])]),
        );
        write(ex.path(), "user.json", &json!({"id": "user-data"}));
        std::fs::write(ex.path().join("file-AbC1-log.txt"), b"Captain's log").unwrap();
        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        let mut commits = Vec::new();
        for now in [NOW, "2026-10-22T00:05:00-07:00"] {
            let s = ingest(IngestOptions {
                now: now.to_string(),
                ..opts(&db, ex.path())
            })
            .await
            .unwrap();
            commits.push((
                s.unchanged,
                datalib_etl::doltlite_raw::commit_run(db.pool(), "test")
                    .await
                    .unwrap(),
            ));
        }
        db.close().await;
        assert_eq!(commits[0].0, 0);
        assert!(commits[0].1.is_some());
        assert_eq!(commits[1], (1, None), "an unchanged export changes nothing");
    }

    /// An export is a complete snapshot, so a conversation it stops
    /// listing was deleted on chatgpt.com, and goes with its edges.
    #[tokio::test]
    async fn a_shrinking_export_deletes_what_it_dropped() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", 1.0, &[]), conv("c2", 2.0, &["file-AbC2"])]),
        );
        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        ingest(opts(&db, ex.path())).await.unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", 1.0, &[])]),
        );
        let s = ingest(opts(&db, ex.path())).await.unwrap();
        assert_eq!(s.pruned, 1);
        assert_eq!(ids(&db).await, ["c1"]);
        assert!(db.attachments_listed().await.unwrap().is_empty());
        let orphans: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations_bookkeeping WHERE id = 'c2'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(orphans, 0, "the sidecar went with its row");
        db.close().await;
    }

    /// A file the export has no copy of stays owed, for an `api` run to
    /// fetch, and the Manage row says how many.
    #[tokio::test]
    async fn a_file_missing_from_the_export_stays_owed() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", 1.0, &["file-Gone"])]),
        );
        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        let s = ingest(opts(&db, ex.path())).await.unwrap();
        assert_eq!(s.not_in_export, 1);
        let edges = db.attachments_listed().await.unwrap();
        assert_eq!(
            owed::owed(db.pool(), ATTACHMENTS, edges)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(problem_keys(&db).await, ["phase:attachments"]);
        db.close().await;
    }

    /// Pointing the source at the wrong directory says so, rather than
    /// quietly ingesting nothing.
    #[tokio::test]
    async fn a_directory_without_conversations_json_fails_loudly() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        let err = format!("{:#}", ingest(opts(&db, ex.path())).await.unwrap_err());
        assert!(err.contains("conversations.json"), "{err}");
        assert!(err.contains("export.path"), "{err}");
        db.close().await;
    }

    /// Another account's export would replace this account's mirror
    /// with its own: refused before anything is written.
    #[tokio::test]
    async fn another_accounts_export_is_refused() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", 1.0, &[])]),
        );
        write(ex.path(), "user.json", &json!({"id": "user-data"}));
        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        ingest(opts(&db, ex.path())).await.unwrap();

        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c9", 1.0, &[])]),
        );
        write(ex.path(), "user.json", &json!({"id": "user-lore"}));
        let err = format!("{:#}", ingest(opts(&db, ex.path())).await.unwrap_err());
        assert!(err.contains("user-lore"), "{err}");
        assert_eq!(ids(&db).await, ["c1"]);
        db.close().await;
    }

    /// Beside `api`, the export answers for a conversation it holds at
    /// or after the version the listing names: the listing's
    /// `update_time` can trail the detail's, which is the export's.
    /// Anything older is fetched live.
    #[test]
    fn an_export_copy_answers_at_or_after_the_listed_version() {
        let ex = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", 1_710_959_331.42, &[])]),
        );
        let copy = ExportCopy::open(ex.path()).unwrap();
        let at = |version: Option<&str>| copy.conversation_at(&Listed::new("c1", version));
        assert!(at(Some("1710959331")).is_some());
        assert!(at(Some("1710959320")).is_some(), "a listing that trails");
        assert!(at(Some("1710959932")).is_none(), "moved since the export");
        assert!(at(None).is_none(), "a listing with no version");
        assert!(copy
            .conversation_at(&Listed::new("c2", Some("1710959331")))
            .is_none());
    }

    /// Another account's export beside `api` would never match a
    /// listed conversation; the run says so instead.
    #[test]
    fn an_export_copy_of_another_account_is_refused() {
        let ex = tempfile::tempdir().unwrap();
        write(ex.path(), "conversations.json", &json!([]));
        write(ex.path(), "user.json", &json!({"id": "user-lore"}));
        let copy = ExportCopy::open(ex.path()).unwrap();
        copy.check_account(&json!({"id": "user-lore"})).unwrap();
        let err = copy
            .check_account(&json!({"id": "user-data"}))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("user-lore") && err.contains("user-data"),
            "{err}"
        );
    }

    /// Entries none of which has an id are an export in a shape this
    /// reader does not know, not one that deleted every conversation.
    #[tokio::test]
    async fn an_export_with_no_id_anywhere_deletes_nothing() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", 1.0, &[])]),
        );
        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        ingest(opts(&db, ex.path())).await.unwrap();

        let mut unkeyed = conv("c1", 2.0, &[]);
        let o = unkeyed.as_object_mut().unwrap();
        o.remove("id");
        o.remove("conversation_id");
        write(ex.path(), "conversations.json", &json!([unkeyed]));
        let s = ingest(opts(&db, ex.path())).await.unwrap();
        assert_eq!((s.pruned, s.without_id), (0, 1));
        assert_eq!(ids(&db).await, ["c1"]);
        assert_eq!(
            problem_keys(&db).await,
            ["phase:conversations.json", "phase:export"]
        );

        write(ex.path(), "conversations.json", &json!([]));
        ingest(opts(&db, ex.path())).await.unwrap();
        assert!(
            ids(&db).await.is_empty(),
            "an export that lists nothing empties it"
        );
        db.close().await;
    }
}
