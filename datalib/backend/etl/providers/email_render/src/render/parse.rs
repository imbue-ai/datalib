//! Parse the email raw store, driven by **`dolt_diff_<table>`**, a page
//! of threads at a time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{self, BlobBundle, CasEdgeRow};
use datalib_etl::doltlite_raw::Reader;
use datalib_etl::read_marks::{self, ReadMark};
use datalib_etl_render::inputs::{Inputs, RawRange};
use datalib_etl_render::pages::{BucketPages, KeyColumn};
use datalib_etl_render::processor::Unparsed;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_email::ingest::db::{db_path_for, EmailJoins, LoadedEmail};
use datalib_etl_email::ingest::schema_raw::EmlBlobRow;

/// SQL projection from the `email_blobs` edge's `blake3` to `.eml`
/// bytes. Consumed by [`BlobBundle::load_many`]. We only load `.eml`s:
/// attachment parts are mail-parsed out of their bytes, and a page links
/// each part inside its stored `.eml`. `DISTINCT` because several emails
/// can edge to the same `.eml` blob.
const EML_PROJECTION_SQL: &str = "
    SELECT DISTINCT blob_id AS ref_id, blake3,
           'message/rfc822' AS content_type,
           NULL AS upstream_name
      FROM email_blobs
     WHERE blob_id IN ({placeholders}) AND blake3 IS NOT NULL";

/// A page is the threads of a run of `thread_id`s: every account's
/// emails under one id land in the same page.
const THREAD_KEY: KeyColumn = KeyColumn {
    table: "emails",
    column: "thread_id",
};

/// Under a label filter, a mailbox added, removed, renamed or moved can
/// change which threads the filter keeps, so it renders every thread.
/// Any other change to a mailbox row (a key of the server's own that
/// nothing reads) reaches only the threads that looked that mailbox up.
const MAILBOX_TREE_MOVED: &str = "
    SELECT 1 FROM dolt_diff_mailboxes
     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
       AND (diff_type != 'modified'
            OR from_name IS NOT to_name
            OR from_parent_id IS NOT to_parent_id)
     LIMIT 1";

/// The threads with an email filed under one of the mailboxes in `?1`:
/// the ones a label filter keeps, whole.
const KEPT_THREADS: &str = "
    SELECT DISTINCT e.account_id, e.thread_id
      FROM email_mailboxes m JOIN emails e ON e.id = m.email_id
     WHERE m.mailbox_id IN (SELECT value FROM json_each(?1))
       AND e.account_id != '' AND e.thread_id != ''";

/// [`KEPT_THREADS`] among the thread ids in `?2`.
const KEPT_THREADS_AMONG: &str = "
    SELECT DISTINCT e.account_id, e.thread_id
      FROM email_mailboxes m JOIN emails e ON e.id = m.email_id
     WHERE m.mailbox_id IN (SELECT value FROM json_each(?1))
       AND e.thread_id IN (SELECT value FROM json_each(?2))
       AND e.account_id != '' AND e.thread_id != ''";

/// Every thread with an email.
const ALL_THREADS: &str = "
    SELECT DISTINCT account_id, thread_id FROM emails
     WHERE account_id != '' AND thread_id != ''";

/// The threads with an email among the thread ids in `?1`.
const THREADS_AMONG: &str = "
    SELECT DISTINCT account_id, thread_id FROM emails
     WHERE thread_id IN (SELECT value FROM json_each(?1))
       AND account_id != '' AND thread_id != ''";

type Pair = (String, String);

/// Which threads a run reads. The label filter is decided here, in SQL,
/// so a thread it keeps out is never loaded.
enum Selection {
    /// Every thread: no filter, nothing narrowed.
    Every,
    /// Every thread the label filter keeps, by thread id.
    Kept(BTreeMap<String, Vec<String>>),
    /// The threads the diff and the driver found stale; `left_out` holds
    /// those of them that have emails and that the label filter keeps out.
    Stale {
        pairs: HashSet<Pair>,
        left_out: HashSet<Pair>,
    },
}

/// Result of the dolt_diff scan.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → load only threads whose `(account_id, thread_id)`
    /// is in `set`: the ones the driver found stale through their
    /// declared inputs, plus the ones the diff named. `None` → cold
    /// start, render every thread.
    pub render: Option<HashSet<(String, String)>>,
    /// Bucket keys the driver found stale that no email still belongs
    /// to — declared with nothing so their documents go.
    pub gone: Vec<String>,
    /// The HEAD commit hash at scan time. `None` if `dolt_log()` was
    /// unavailable (non-doltlite sqlite); cursor stays unwritten.
    pub new_head: Option<String>,
    /// Wall-clock time spent in the union query. `None` on cold
    /// start (no diff was issued).
    pub scan_elapsed: Option<Duration>,
}

/// The rows every thread shares, read whole up front: small next to the
/// threads.
#[derive(Debug, Clone, Default)]
pub struct Shared {
    /// `(account id, payload)`. The id rides beside the payload rather
    /// than inside it: a JMAP `Account` object has no `id` field — the
    /// id is the key it sits under in the session's `accounts` map.
    pub accounts: Vec<(String, Value)>,
    pub mailboxes: Vec<Value>,
}

/// The raw store and its blob CAS, open for one render: what every
/// thread shares, what the diff said, and the threads themselves read a
/// page at a time ([`EmailStore::next_page`]) with their `.eml`s read
/// later still, a batch at a time ([`EmailStore::load_emls`]).
pub struct EmailStore {
    reader: Reader,
    cas: Option<blob_cas::BlobCas>,
    pub shared: Shared,
    pub scan: ScanResult,
    selection: Selection,
    /// Threads this run will look at: every one on a full walk (every one
    /// the label filter keeps, under one), the narrowed set otherwise.
    pub threads: usize,
    /// On a full walk under a label filter, the threads the filter keeps
    /// out, by key alone: never loaded, declared so their documents go.
    pub left_out: Vec<Pair>,
    /// Rows whose stored payload would not read as JSON, dropped —
    /// reported by the processor rather than skipped in silence.
    pub unparsed: Vec<Unparsed>,
    /// Every email's read mark, by email id.
    marks: BTreeMap<String, ReadMark>,
}

/// One page of threads, with every email row and join they read and no
/// `.eml` bytes yet.
pub struct Page {
    pub docs: Vec<EmailThreadBucket>,
    /// On a narrowed run, the stale threads this page looked at, which
    /// it declares empty before the ones it renders: a thread no email
    /// belongs to any more builds nothing. Empty on a full walk, whose
    /// threads all come from rows.
    pub looked_at: Vec<(String, String)>,
    /// On a narrowed run, the stale threads on this page that the label
    /// filter keeps out: not loaded.
    pub left_out: Vec<(String, String)>,
}

impl EmailStore {
    /// The `thread_id`s to render, a page of `page` at a time.
    pub fn pages(&self, page: usize) -> BucketPages {
        match &self.selection {
            Selection::Every => BucketPages::walk(THREAD_KEY, page),
            Selection::Kept(kept) => BucketPages::listed(kept.keys().cloned(), page),
            Selection::Stale { pairs, .. } => {
                BucketPages::listed(pairs.iter().map(|(_, t)| t.clone()), page)
            }
        }
    }

    /// The next page's threads, or `None` once every page is read.
    pub fn next_page(&self, pages: &mut BucketPages) -> Result<Option<Page>> {
        block_on(async {
            let pool = self.reader.pool();
            let Some(thread_ids) = pages.next(pool).await? else {
                return Ok(None);
            };
            let (wanted, looked_at, left_out) = match &self.selection {
                Selection::Every => (None, Vec::new(), Vec::new()),
                Selection::Kept(kept) => {
                    let wanted: Vec<Pair> = thread_ids
                        .iter()
                        .flat_map(|t| {
                            kept.get(t)
                                .into_iter()
                                .flatten()
                                .map(move |a| (a.clone(), t.clone()))
                        })
                        .collect();
                    (Some(wanted), Vec::new(), Vec::new())
                }
                Selection::Stale { pairs, left_out } => {
                    let ids: HashSet<&str> = thread_ids.iter().map(String::as_str).collect();
                    let mut looked_at: Vec<Pair> = pairs
                        .iter()
                        .filter(|(_, t)| ids.contains(t.as_str()))
                        .cloned()
                        .collect();
                    looked_at.sort();
                    let (out, wanted): (Vec<Pair>, Vec<Pair>) = looked_at
                        .iter()
                        .cloned()
                        .partition(|p| left_out.contains(p));
                    (Some(wanted), looked_at, out)
                }
            };
            // Only the threads a page renders are read: one the filter
            // keeps out is never loaded.
            let load_ids: Vec<String> = match &wanted {
                None => thread_ids,
                Some(wanted) => wanted
                    .iter()
                    .map(|(_, t)| t.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            };
            let docs = load_buckets(pool, &load_ids, wanted.as_deref(), &self.marks).await?;
            Ok(Some(Page {
                docs,
                looked_at,
                left_out,
            }))
        })
    }

    /// Copies of `batch` with their `.eml`s loaded and their attachment
    /// parts listed.
    pub fn load_emls(&self, batch: &[&EmailThreadBucket]) -> Result<Vec<EmailThreadBucket>> {
        block_on(async {
            let mut docs: Vec<EmailThreadBucket> = batch.iter().map(|b| (*b).clone()).collect();
            let refs = docs
                .iter()
                .enumerate()
                .map(|(i, bucket)| (i, bucket.emails.iter().map(|em| em.blob_id.as_str())));
            let mut blobs = BlobBundle::load_many_with_bytes(
                self.reader.pool(),
                self.cas.as_ref(),
                EML_PROJECTION_SQL,
                refs,
            )
            .await?;
            for (i, bucket) in docs.iter_mut().enumerate() {
                if let Some(b) = blobs.remove(&i) {
                    bucket.blobs = b;
                    extract_attachments_from_emls(bucket);
                }
            }
            Ok(docs)
        })
    }

    pub fn close(self) {
        block_on(async {
            self.reader.close().await;
            if let Some(cas) = self.cas {
                cas.close().await;
            }
        })
    }
}

fn block_on<F: Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// One rendered-markdown bucket: every email in a single JMAP Thread
/// plus its joins. Its [`BlobBundle`] (the `.eml`s) is empty until
/// [`EmailStore::load_emls`] fills a copy.
#[derive(Debug, Clone, Default)]
pub struct EmailThreadBucket {
    pub account_id: String,
    pub thread_id: String,
    pub emails: Vec<LoadedEmail>,
    pub joins: EmailJoins,
    pub blobs: BlobBundle,
    /// Each email's read mark, by email id.
    pub marks: HashMap<String, ReadMark>,
    /// Every raw row this thread reads — its emails, their mailbox,
    /// keyword and `.eml` edges and read marks from the load; its account
    /// and the mailboxes it is filed under as render looks them up.
    pub inputs: Inputs,
}

impl EmailThreadBucket {
    /// How many bytes of `.eml` loading it costs.
    pub fn eml_bytes(&self) -> u64 {
        self.emails
            .iter()
            .map(|em| em.size.unwrap_or(0).max(0) as u64)
            .sum()
    }
}

/// `only_labels` is the render step's label filter, empty for none: a
/// thread renders when any of its emails is filed under one of them.
/// `None` when there is no store, or nothing committed in it, to read.
pub fn open(
    input: &Path,
    source_id: &str,
    range: RawRange<'_>,
    only_labels: &[String],
) -> Result<Option<EmailStore>> {
    let db_path = db_path_for(input);
    if !db_path.is_file() {
        return Ok(None);
    }
    block_on(open_async(&db_path, source_id, range, only_labels))
}

async fn open_async(
    db_path: &Path,
    source_id: &str,
    range: RawRange<'_>,
    only_labels: &[String],
) -> Result<Option<EmailStore>> {
    // Opened at the driver's commit, else HEAD. No commit means nothing
    // has been committed here to render: emptiness, not a reason to read
    // the working set.
    let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, range.pin)
        .await
        .with_context(|| format!("open raw doltlite for render at {}", db_path.display()))?
    else {
        return Ok(None);
    };
    let pool = reader.pool().clone();
    let pin = reader.pin().clone();

    let mut unparsed: Vec<Unparsed> = Vec::new();
    let accounts = load_accounts(&pool, &mut unparsed).await?;
    let mailboxes = load_payloads(&pool, "mailboxes", &mut unparsed).await?;
    let allowed = (!only_labels.is_empty()).then(|| resolve_labels(&mailboxes, only_labels).ids);
    let scan = scan_diff(&pool, source_id, range, &pin, allowed.is_some()).await?;
    let marks = read_marks::load(&pool).await?;
    let (selection, left_out) = select(&pool, &scan, allowed.as_ref()).await?;
    let threads = match &selection {
        Selection::Every => thread_count(&pool).await?,
        Selection::Kept(kept) => kept.values().map(Vec::len).sum(),
        Selection::Stale { pairs, .. } => pairs.len(),
    };
    let cas = blob_cas::open_cas_for_render(db_path)
        .await
        .with_context(|| format!("open the blob store beside {}", db_path.display()))?;
    Ok(Some(EmailStore {
        reader,
        cas,
        shared: Shared {
            accounts,
            mailboxes,
        },
        scan,
        selection,
        threads,
        left_out,
        unparsed,
        marks,
    }))
}

/// Mail-parse each `.eml` already loaded into the bucket's
/// `BlobBundle` and list its attachment-style parts in the bucket's
/// joins. Each is named by the hash of its decoded bytes, which is how a
/// page's address finds it inside the stored `.eml`
/// (`datalib_blob_store::part_of` hashes them the same way).
fn extract_attachments_from_emls(bucket: &mut EmailThreadBucket) {
    for em in &bucket.emails {
        let Some(eml_blob) = bucket.blobs.get(&em.blob_id) else {
            continue;
        };
        let atts = attachments_in(eml_blob.bytes.as_deref().unwrap_or_default());
        if !atts.is_empty() {
            bucket.joins.attachments.insert(em.id.clone(), atts);
        }
    }
}

fn attachments_in(eml: &[u8]) -> Vec<datalib_etl_email::ingest::db::LoadedAttachment> {
    use mail_parser::{MessageParser, PartKind};

    let Some(msg) = MessageParser::default().parse(eml) else {
        return Vec::new();
    };
    {
        // A part with no name is labelled by its place among the
        // message's own parts, the root counted as 0.
        let place: HashMap<_, usize> = msg
            .root()
            .parts()
            .enumerate()
            .map(|(i, part)| (part.id(), i))
            .collect();
        // `html_body` too: an inline image inside `multipart/related`
        // may be listed only there. A text or HTML body part is skipped
        // unless something links it by its Content-ID.
        let mut seen = HashSet::new();
        let mut atts: Vec<datalib_etl_email::ingest::db::LoadedAttachment> = Vec::new();
        for part in msg.attachments().chain(msg.html_body()) {
            if !seen.insert(part.id()) {
                continue;
            }
            let in_body = part.in_text_body() || part.in_html_body();
            if in_body && part.content_id().is_none() {
                continue;
            }
            if part.is_text() && part.content_id().is_none() && part.attachment_name().is_none() {
                continue;
            }
            let bytes: Vec<u8> = match part.kind() {
                PartKind::Binary | PartKind::InlineBinary | PartKind::Message(_) => {
                    part.decoded().into_owned()
                }
                PartKind::Text | PartKind::Html => {
                    part.text().unwrap_or_default().into_owned().into_bytes()
                }
                _ => continue,
            };
            let name = part
                .attachment_name()
                .map(str::to_string)
                .or_else(|| part.is_message().then(|| "attached-email.eml".to_string()));
            let cid = part.content_id().map(str::to_string);
            let disposition = part.content_disposition().map(|cd| cd.ctype().to_string());
            let content_type = part.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            });
            atts.push(datalib_etl_email::ingest::db::LoadedAttachment {
                part_id: format!("p{}", place.get(&part.id()).copied().unwrap_or_default()),
                blob_id: blob_cas::blake3_hex(&bytes),
                name,
                content_type,
                size: Some(bytes.len() as i64),
                disposition,
                cid,
            });
        }
        atts
    }
}

/// The configured label paths against the mailbox tree. Resolution and
/// exact-match semantics are shared with the download filter via
/// [`datalib_etl_email::mailbox_labels`], so a given label means the
/// same thing at both phases and across JMAP / mbox sources.
pub(crate) fn resolve_labels(
    mailboxes: &[Value],
    only_labels: &[String],
) -> datalib_etl_email::mailbox_labels::Resolved {
    let nodes: Vec<datalib_etl_email::mailbox_labels::MailboxNode> = mailboxes
        .iter()
        .filter_map(datalib_etl_email::mailbox_labels::MailboxNode::from_payload)
        .collect();
    datalib_etl_email::mailbox_labels::resolve(&nodes, only_labels)
}

/// What the run reads, and on a full walk under a label filter the
/// threads that filter keeps out.
async fn select(
    pool: &SqlitePool,
    scan: &ScanResult,
    allowed: Option<&HashSet<String>>,
) -> Result<(Selection, Vec<Pair>)> {
    let Some(allowed) = allowed else {
        return Ok(match &scan.render {
            None => (Selection::Every, Vec::new()),
            Some(pairs) => (
                Selection::Stale {
                    pairs: pairs.clone(),
                    left_out: HashSet::new(),
                },
                Vec::new(),
            ),
        });
    };
    let allowed = serde_json::to_string(allowed)?;
    match &scan.render {
        None => {
            let kept: HashSet<Pair> = sqlx::query_as(KEPT_THREADS)
                .bind(&allowed)
                .fetch_all(pool)
                .await
                .context("the threads the label filter keeps")?
                .into_iter()
                .collect();
            let every: Vec<Pair> = sqlx::query_as(ALL_THREADS)
                .fetch_all(pool)
                .await
                .context("every thread")?;
            let left_out = every.into_iter().filter(|p| !kept.contains(p)).collect();
            let mut by_thread: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (a, t) in kept {
                by_thread.entry(t).or_default().push(a);
            }
            Ok((Selection::Kept(by_thread), left_out))
        }
        Some(pairs) => {
            let ids: Vec<&str> = pairs
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            let ids = serde_json::to_string(&ids)?;
            let kept: HashSet<Pair> = sqlx::query_as(KEPT_THREADS_AMONG)
                .bind(&allowed)
                .bind(&ids)
                .fetch_all(pool)
                .await
                .context("the stale threads the label filter keeps")?
                .into_iter()
                .collect();
            let left_out = sqlx::query_as::<_, Pair>(THREADS_AMONG)
                .bind(&ids)
                .fetch_all(pool)
                .await
                .context("the stale threads with an email")?
                .into_iter()
                .filter(|p| pairs.contains(p) && !kept.contains(p))
                .collect();
            Ok((
                Selection::Stale {
                    pairs: pairs.clone(),
                    left_out,
                },
                Vec::new(),
            ))
        }
    }
}

/// Per-bucket dolt_diff scan. Delegates to the shared
/// [`datalib_etl::doltlite_raw::scan_buckets`] helper, which is
/// the same primitive every other provider uses (slack / chatgpt /
/// claude / signal). Bucket key shape is `"<account_id>|<thread_id>"`
/// so it fits the helper's `HashSet<String>` API; we split it back
/// into a `(String, String)` pair locally for the load step. An
/// account or mailbox change reaches a thread through the inputs it
/// declared — except under a label filter, where a change to the
/// mailbox tree ([`MAILBOX_TREE_MOVED`]) renders everything. A read
/// mark that moved renders the thread of its email.
async fn scan_diff(
    pool: &SqlitePool,
    source_id: &str,
    range: RawRange<'_>,
    pin: &datalib_etl::pin::Pin,
    label_filter: bool,
) -> Result<ScanResult> {
    let mut scan = datalib_etl::doltlite_raw::scan_buckets(
        pool,
        range.cursor,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            fanout_query: label_filter.then_some(MAILBOX_TREE_MOVED),
            bucket_query: "
                SELECT DISTINCT account_id || '|' || thread_id AS bucket_key FROM (
                    SELECT to_account_id  AS account_id, to_thread_id  AS thread_id
                      FROM dolt_diff_emails
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT from_account_id, from_thread_id
                      FROM dolt_diff_emails
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT emails.account_id, emails.thread_id
                      FROM dolt_diff_email_mailboxes d
                      JOIN emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                    UNION
                    SELECT emails.account_id, emails.thread_id
                      FROM dolt_diff_email_keywords d
                      JOIN emails ON emails.id = coalesce(d.to_email_id, d.from_email_id)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                    UNION
                    SELECT t.account_id,
                           coalesce(dt.to_id, dt.from_id) AS thread_id
                      FROM dolt_diff_threads dt
                      JOIN threads t ON t.id = coalesce(dt.to_id, dt.from_id)
                     WHERE dt.from_ref = ?1 AND dt.to_ref = ?2 AND dt.diff_type != 'unchanged'
                )
                WHERE account_id IS NOT NULL AND thread_id IS NOT NULL
            ",
        },
    )
    .await?;
    if let (Some(from), Some(render)) = (range.cursor, scan.render.as_mut()) {
        render.extend(threads_of_moved_marks(pool, from, pin.commit()).await?);
    }
    // The driver names buckets by the thread uuid; the load wants the
    // `(account, thread)` pair. Only a run that narrows maps them.
    let by_uuid: HashMap<String, String> = match (range.stale, &scan.render) {
        (Some(stale), Some(_)) => pairs_named(pool, source_id, stale).await?,
        _ => HashMap::new(),
    };
    let narrowed = range.narrow_by(scan.render.as_ref(), |key| by_uuid.get(key).cloned());
    let split = |set: HashSet<String>| {
        set.into_iter()
            .filter_map(|key| {
                let (a, t) = key.split_once('|')?;
                Some((a.to_string(), t.to_string()))
            })
            .collect::<HashSet<(String, String)>>()
    };
    Ok(ScanResult {
        render: narrowed.render.map(split),
        gone: narrowed.gone,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
    })
}

/// The `account|thread` key of the thread of each email whose read mark
/// moved between the two commits.
async fn threads_of_moved_marks(pool: &SqlitePool, from: &str, to: &str) -> Result<Vec<String>> {
    let moved: Vec<String> = read_marks::moved(pool, from, to)
        .await?
        .into_iter()
        .map(|m| m.id)
        .collect();
    if moved.is_empty() {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT account_id, thread_id FROM emails
          WHERE id IN (SELECT value FROM json_each(?))",
    )
    .bind(serde_json::to_string(&moved)?)
    .fetch_all(pool)
    .await
    .context("the threads of the emails whose read mark moved")?;
    Ok(rows.into_iter().map(|(a, t)| format!("{a}|{t}")).collect())
}

/// The `account|thread` key of each thread whose uuid is in `uuids`.
async fn pairs_named(
    pool: &SqlitePool,
    source_id: &str,
    uuids: &HashSet<String>,
) -> Result<HashMap<String, String>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT DISTINCT account_id, thread_id FROM emails")
            .fetch_all(pool)
            .await
            .context("load all (account_id, thread_id) pairs")?;
    Ok(rows
        .into_iter()
        .filter(|(a, t)| !a.is_empty() && !t.is_empty())
        .filter_map(|(a, t)| {
            let uuid = super::ids::thread(source_id, &a, &t).uuid;
            uuids.contains(&uuid).then(|| (uuid, format!("{a}|{t}")))
        })
        .collect())
}

async fn thread_count(pool: &SqlitePool) -> Result<usize> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM (SELECT DISTINCT account_id, thread_id FROM emails
                                WHERE account_id != '' AND thread_id != '')",
    )
    .fetch_one(pool)
    .await
    .context("count threads")?;
    Ok(n as usize)
}

async fn load_accounts(
    pool: &SqlitePool,
    unparsed: &mut Vec<Unparsed>,
) -> Result<Vec<(String, Value)>> {
    let table = "accounts";
    let sql = format!("SELECT id, json(payload) AS payload FROM {table} WHERE payload IS NOT NULL");
    // Audited: `table` is a literal.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("load accounts")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: String = r.try_get("id").unwrap_or_default();
        let s: String = r.try_get("payload").unwrap_or_default();
        match serde_json::from_str::<Value>(&s) {
            Ok(v) => out.push((id, v)),
            Err(_) => unparsed.push(Unparsed::new("accounts", &id, &s)),
        }
    }
    Ok(out)
}

async fn load_payloads(
    pool: &SqlitePool,
    table: &'static str,
    unparsed: &mut Vec<Unparsed>,
) -> Result<Vec<Value>> {
    let pinned = table;
    let sql =
        format!("SELECT id, json(payload) AS payload FROM {pinned} WHERE payload IS NOT NULL");
    // Audited: `table` is a literal at every callsite.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .with_context(|| format!("load_payloads {table}"))?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let id: String = r.try_get("id").unwrap_or_default();
        let s: String = r.try_get("payload").unwrap_or_default();
        match serde_json::from_str::<Value>(&s) {
            Ok(v) => out.push(v),
            Err(_) => unparsed.push(Unparsed::new(table, &id, &s)),
        }
    }
    Ok(out)
}

/// The threads under `thread_ids`, with their emails and joins: every
/// `(account, thread)` the rows hold, or on a narrowed run only the
/// `wanted` ones — each of which comes back, emails or none.
async fn load_buckets(
    pool: &SqlitePool,
    thread_ids: &[String],
    wanted: Option<&[(String, String)]>,
    marks: &BTreeMap<String, ReadMark>,
) -> Result<Vec<EmailThreadBucket>> {
    let mut by_key: BTreeMap<(String, String), EmailThreadBucket> = BTreeMap::new();
    let new_bucket = |account_id: &str, thread_id: &str| EmailThreadBucket {
        account_id: account_id.to_string(),
        thread_id: thread_id.to_string(),
        ..Default::default()
    };
    for (a, t) in wanted.into_iter().flatten() {
        by_key.insert((a.clone(), t.clone()), new_bucket(a, t));
    }
    if thread_ids.is_empty() {
        return Ok(by_key.into_values().collect());
    }

    let erows = sqlx::query(
        "SELECT id, account_id, thread_id, blob_id, message_id, in_reply_to, references_header,
                received_at, sent_at, size, subject, from_json, to_json, cc_json, has_attachment
           FROM emails
          WHERE thread_id IN (SELECT value FROM json_each(?))
          ORDER BY thread_id, received_at, id",
    )
    .bind(serde_json::to_string(thread_ids)?)
    .fetch_all(pool)
    .await
    .context("phase 2 emails select")?;
    let mut email_to_bucket: HashMap<String, (String, String)> =
        HashMap::with_capacity(erows.len());
    for r in &erows {
        let id: String = r.try_get("id").unwrap_or_default();
        let account_id: String = r.try_get("account_id").unwrap_or_default();
        let thread_id: String = r.try_get("thread_id").unwrap_or_default();
        if account_id.is_empty() || thread_id.is_empty() {
            continue;
        }
        let key = (account_id.clone(), thread_id.clone());
        let bucket = match by_key.get_mut(&key) {
            Some(bucket) => bucket,
            None if wanted.is_none() => by_key
                .entry(key.clone())
                .or_insert_with(|| new_bucket(&account_id, &thread_id)),
            None => continue,
        };
        email_to_bucket.insert(id.clone(), key);
        let blob_id: String = r.try_get("blob_id").unwrap_or_default();
        bucket.inputs.read("emails", &id);
        bucket
            .inputs
            .read("email_blobs", &EmlBlobRow::pk_recipe(&id, &blob_id));
        bucket.inputs.read(read_marks::TABLE, &id);
        if let Some(mark) = marks.get(&id) {
            bucket.marks.insert(id.clone(), mark.clone());
        }
        bucket.emails.push(LoadedEmail {
            id,
            account_id,
            thread_id,
            blob_id,
            message_id: r.try_get::<Option<String>, _>("message_id").unwrap_or(None),
            in_reply_to: r
                .try_get::<Option<String>, _>("in_reply_to")
                .unwrap_or(None),
            references: r
                .try_get::<Option<String>, _>("references_header")
                .unwrap_or(None),
            received_at: r
                .try_get::<Option<String>, _>("received_at")
                .unwrap_or(None),
            sent_at: r.try_get::<Option<String>, _>("sent_at").unwrap_or(None),
            size: r.try_get::<Option<i64>, _>("size").unwrap_or(None),
            subject: r.try_get::<Option<String>, _>("subject").unwrap_or(None),
            from_json: r.try_get::<Option<String>, _>("from_json").unwrap_or(None),
            to_json: r.try_get::<Option<String>, _>("to_json").unwrap_or(None),
            cc_json: r.try_get::<Option<String>, _>("cc_json").unwrap_or(None),
            has_attachment: r
                .try_get::<Option<i64>, _>("has_attachment")
                .unwrap_or(None)
                .unwrap_or(0)
                != 0,
        });
    }
    drop(erows);

    if email_to_bucket.is_empty() {
        return Ok(by_key.into_values().collect());
    }
    let email_ids = serde_json::to_string(&email_to_bucket.keys().collect::<Vec<_>>())?;

    let mailbox_rows = sqlx::query(
        "SELECT id, email_id, mailbox_id FROM email_mailboxes
          WHERE email_id IN (SELECT value FROM json_each(?))",
    )
    .bind(&email_ids)
    .fetch_all(pool)
    .await
    .context("phase 2 email_mailboxes select")?;
    for r in mailbox_rows {
        let e: String = r.try_get("email_id").unwrap_or_default();
        let m: String = r.try_get("mailbox_id").unwrap_or_default();
        let Some(bucket) = email_to_bucket.get(&e).and_then(|k| by_key.get_mut(k)) else {
            continue;
        };
        bucket.inputs.read(
            "email_mailboxes",
            &r.try_get::<String, _>("id").unwrap_or_default(),
        );
        bucket.joins.mailboxes.entry(e).or_default().push(m);
    }

    let keyword_rows = sqlx::query(
        "SELECT id, email_id, keyword FROM email_keywords
          WHERE email_id IN (SELECT value FROM json_each(?))",
    )
    .bind(&email_ids)
    .fetch_all(pool)
    .await
    .context("phase 2 email_keywords select")?;
    for r in keyword_rows {
        let e: String = r.try_get("email_id").unwrap_or_default();
        let k: String = r.try_get("keyword").unwrap_or_default();
        let Some(bucket) = email_to_bucket.get(&e).and_then(|k| by_key.get_mut(k)) else {
            continue;
        };
        bucket.inputs.read(
            "email_keywords",
            &r.try_get::<String, _>("id").unwrap_or_default(),
        );
        bucket.joins.keywords.entry(e).or_default().push(k);
    }

    // Attachments are extracted later by mail-parsing each loaded
    // `.eml` — see `extract_attachments_from_emls`. No
    // `email_attachments` table to query after the eml-as-canonical
    // port.

    Ok(by_key.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::bulk::SQLITE_MAX_VARIABLES;
    use datalib_etl_email::ingest::schema_raw::{EmailKeywordRow, EmailMailboxRow, EmailRow};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// A full render of a mailbox with more threads than SQLite binds in
    /// one statement (#1156): each thread, its email and the email's
    /// mailbox and keyword all come back.
    /// An email attached to an email, a forward or a bounce, was skipped:
    /// the page showed the outer one with no sign it held another.
    #[test]
    fn an_email_attached_to_an_email_is_an_attachment() {
        let inner = "From: Data <data@enterprise.ufp>\r\n\
                     Subject: Warp core report\r\n\r\nAll nominal.\r\n";
        let eml = format!(
            "From: Picard <picard@enterprise.ufp>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
             --b\r\nContent-Type: text/plain\r\n\r\nSee attached.\r\n\
             --b\r\nContent-Type: message/rfc822\r\n\r\n{inner}\r\n--b--\r\n"
        );
        let atts = attachments_in(eml.as_bytes());
        assert_eq!(atts.len(), 1, "{atts:?}");
        assert_eq!(atts[0].name.as_deref(), Some("attached-email.eml"));
        assert_eq!(atts[0].content_type.as_deref(), Some("message/rfc822"));
        assert!(atts[0].size.unwrap() > 0);
    }

    #[tokio::test]
    async fn loads_more_threads_than_sqlite_binds_in_one_statement() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        let ddl = [
            EmailRow::all_ddl(),
            EmailMailboxRow::all_ddl(),
            EmailKeywordRow::all_ddl(),
        ];
        for sql in ddl.concat() {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&pool)
                .await
                .unwrap();
        }
        let n = SQLITE_MAX_VARIABLES + 1;
        // `?1` rows numbered from 1, for each table.
        let numbered = "WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < ?1)";
        for insert in [
            "INSERT INTO emails (id, account_id, thread_id, blob_id, references_header)
             SELECT 'e' || value, 'acct', 't' || value, 'b' || value, '<r' || value || '>' FROM s",
            "INSERT INTO email_mailboxes (id, email_id, mailbox_id)
             SELECT 'e' || value || '#inbox', 'e' || value, 'inbox' FROM s",
            "INSERT INTO email_keywords (id, email_id, keyword)
             SELECT 'e' || value || '#$flagged', 'e' || value, '$flagged' FROM s",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!("{numbered} {insert}")))
                .bind(n as i64)
                .execute(&pool)
                .await
                .unwrap();
        }
        let thread_ids: Vec<String> = (1..=n).map(|i| format!("t{i}")).collect();

        let docs = load_buckets(&pool, &thread_ids, None, &BTreeMap::new())
            .await
            .unwrap();

        assert_eq!(docs.len(), n);
        let last = docs
            .iter()
            .find(|d| d.thread_id == format!("t{n}"))
            .unwrap();
        let email = &last.emails[..];
        assert_eq!(email.len(), 1);
        assert_eq!(
            email[0].references.as_deref(),
            Some(format!("<r{n}>").as_str())
        );
        assert_eq!(last.joins.mailboxes[&email[0].id], vec!["inbox"]);
        assert_eq!(last.joins.keywords[&email[0].id], vec!["$flagged"]);
    }

    /// An email read upstream moves its read mark and nothing else of it.
    /// That moves the store's version, and the next render renders the
    /// thread holding that email and no other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_moved_read_mark_renders_only_the_thread_of_its_email() {
        use datalib_etl::doltlite_raw::{commit_run, content_commit, head_commit};
        use datalib_etl_email::ingest::db::RawDb;
        use datalib_etl_email::ingest::listed::{same_id, write_batch_in_tx};
        use datalib_etl_email::ingest::schema_raw::FetchedEmail;

        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&db_path_for(dir.path())).await.unwrap();
        let write = |seen: &'static [&'static str]| {
            let db = db.clone();
            async move {
                let got: Vec<FetchedEmail> = ["E1", "E2"]
                    .into_iter()
                    .map(|id| {
                        let keywords: serde_json::Map<String, Value> = seen
                            .contains(&id)
                            .then(|| ("$seen".to_string(), Value::Bool(true)))
                            .into_iter()
                            .collect();
                        let envelope = serde_json::json!({
                            "id": id, "blobId": format!("B{id}"), "threadId": format!("T{id}"),
                            "keywords": keywords, "mailboxIds": {"M1": true},
                        });
                        FetchedEmail::from_jmap_envelope("A1", &envelope).unwrap()
                    })
                    .collect();
                let mut tx = db.pool().begin().await.unwrap();
                let now = datalib_time::IsoOffsetTimestamp::now_local();
                write_batch_in_tx(&mut tx, &now, got, &[], same_id)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                commit_run(db.pool(), "sync").await.unwrap();
            }
        };
        write(&[]).await;
        let rendered = head_commit(db.pool()).await.unwrap().unwrap();
        let version = content_commit(db.pool()).await.unwrap();
        write(&["E1"]).await;
        assert_ne!(content_commit(db.pool()).await.unwrap(), version);
        db.close().await;

        let store = open(
            dir.path(),
            "mail",
            RawRange {
                cursor: Some(&rendered),
                pin: None,
                // The driver found nothing stale by what the thread declared.
                stale: Some(&HashSet::new()),
            },
            &[],
        )
        .unwrap()
        .unwrap();
        let render = store.scan.render.clone();
        store.close();
        assert_eq!(
            render,
            Some(HashSet::from([("A1".to_string(), "TE1".to_string())]))
        );
    }

    /// A raw store holding mailboxes M1 "Inbox" and M2 "Spam", and one
    /// email in each (E1 in thread TE1 under M1, E2 in TE2 under M2),
    /// committed; `seen` emails carry `$seen`.
    async fn two_mailbox_store(
        db: &datalib_etl_email::ingest::db::RawDb,
        spam: Value,
        seen: &'static [&'static str],
    ) -> String {
        use datalib_etl::bulk::bulk_upsert_in_tx;
        use datalib_etl::doltlite_raw::{commit_run, head_commit};
        use datalib_etl_email::ingest::listed::{same_id, write_batch_in_tx};
        use datalib_etl_email::ingest::schema_raw::{FetchedEmail, MailboxRow};

        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = db.pool().begin().await.unwrap();
        let mailboxes = [
            MailboxRow::from_jmap_payload("A1", &serde_json::json!({"id": "M1", "name": "Inbox"}))
                .unwrap(),
            MailboxRow::from_jmap_payload("A1", &spam).unwrap(),
        ];
        bulk_upsert_in_tx(&mut tx, &mailboxes, &now).await.unwrap();
        let got: Vec<FetchedEmail> = [("E1", "M1"), ("E2", "M2")]
            .into_iter()
            .map(|(id, mailbox)| {
                let keywords: serde_json::Map<String, Value> = seen
                    .contains(&id)
                    .then(|| ("$seen".to_string(), Value::Bool(true)))
                    .into_iter()
                    .collect();
                let envelope = serde_json::json!({
                    "id": id, "blobId": format!("B{id}"), "threadId": format!("T{id}"),
                    "keywords": keywords, "mailboxIds": {mailbox: true},
                });
                FetchedEmail::from_jmap_envelope("A1", &envelope).unwrap()
            })
            .collect();
        write_batch_in_tx(&mut tx, &now, got, &[], same_id)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        commit_run(db.pool(), "sync").await.unwrap();
        head_commit(db.pool()).await.unwrap().unwrap()
    }

    /// What a run under the "Inbox" filter from `cursor` would render:
    /// `None` for every thread.
    fn inbox_render_from(dir: &Path, cursor: &str) -> Option<HashSet<(String, String)>> {
        let store = open(
            dir,
            "mail",
            RawRange {
                cursor: Some(cursor),
                pin: None,
                stale: Some(&HashSet::new()),
            },
            &["Inbox".to_string()],
        )
        .unwrap()
        .unwrap();
        let render = store.scan.render.clone();
        store.close();
        render
    }

    /// Fastmail dropped keys of its own (`autoLearn`, `hidden`, …) from one
    /// mailbox between two syncs, and under a label filter the render
    /// walked all 200k threads of the account to change 51 documents. Only
    /// a change to which mailbox is which renders everything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_mailbox_key_the_filter_does_not_read_renders_no_thread() {
        use datalib_etl::doltlite_raw::content_commit;
        use datalib_etl_email::ingest::db::RawDb;

        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&db_path_for(dir.path())).await.unwrap();
        let spam = serde_json::json!({"id": "M2", "name": "Spam", "hidden": false});
        let first = two_mailbox_store(&db, spam, &[]).await;
        let version = content_commit(db.pool()).await.unwrap();
        two_mailbox_store(&db, serde_json::json!({"id": "M2", "name": "Spam"}), &[]).await;
        assert_ne!(
            content_commit(db.pool()).await.unwrap(),
            version,
            "the mailbox row changed"
        );
        db.close().await;

        assert_eq!(inbox_render_from(dir.path(), &first), Some(HashSet::new()));
    }

    /// A mailbox renamed can move a thread into or out of the filter, so
    /// every thread renders.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_renamed_mailbox_renders_every_thread_under_a_label_filter() {
        use datalib_etl_email::ingest::db::RawDb;

        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&db_path_for(dir.path())).await.unwrap();
        let spam = serde_json::json!({"id": "M2", "name": "Spam"});
        let first = two_mailbox_store(&db, spam, &[]).await;
        let renamed = serde_json::json!({"id": "M2", "name": "Inbox"});
        two_mailbox_store(&db, renamed, &[]).await;
        db.close().await;

        assert_eq!(inbox_render_from(dir.path(), &first), None);
    }

    /// A narrowed run under a label filter names a stale thread the filter
    /// keeps out, and never loads it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stale_thread_the_filter_keeps_out_is_named_and_not_loaded() {
        use datalib_etl_email::ingest::db::RawDb;

        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&db_path_for(dir.path())).await.unwrap();
        let spam = || serde_json::json!({"id": "M2", "name": "Spam"});
        let first = two_mailbox_store(&db, spam(), &[]).await;
        two_mailbox_store(&db, spam(), &["E1", "E2"]).await;
        db.close().await;

        let store = open(
            dir.path(),
            "mail",
            RawRange {
                cursor: Some(&first),
                pin: None,
                stale: Some(&HashSet::new()),
            },
            &["Inbox".to_string()],
        )
        .unwrap()
        .unwrap();
        let mut pages = store.pages(10);
        let page = store.next_page(&mut pages).unwrap().unwrap();
        assert!(store.next_page(&mut pages).unwrap().is_none());
        store.close();
        let pair = |t: &str| ("A1".to_string(), t.to_string());
        assert_eq!(page.looked_at, vec![pair("TE1"), pair("TE2")]);
        assert_eq!(page.left_out, vec![pair("TE2")]);
        let loaded: Vec<&str> = page.docs.iter().map(|d| d.thread_id.as_str()).collect();
        assert_eq!(loaded, vec!["TE1"]);
    }

    /// A narrowed page returns each wanted thread, one with no email
    /// left included, and nothing else under the same ids.
    #[tokio::test]
    async fn a_narrowed_page_returns_exactly_the_wanted_threads() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        for sql in [
            EmailRow::all_ddl(),
            EmailMailboxRow::all_ddl(),
            EmailKeywordRow::all_ddl(),
        ]
        .concat()
        {
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO emails (id, account_id, thread_id, blob_id)
             VALUES ('e1', 'a1', 't1', 'b1'), ('e2', 'a2', 't1', 'b2')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let wanted = [
            ("a1".to_string(), "t1".to_string()),
            ("a1".to_string(), "t2".to_string()),
        ];
        let docs = load_buckets(
            &pool,
            &["t1".into(), "t2".into()],
            Some(&wanted),
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        let got: Vec<(&str, &str, usize)> = docs
            .iter()
            .map(|d| (d.account_id.as_str(), d.thread_id.as_str(), d.emails.len()))
            .collect();
        assert_eq!(got, vec![("a1", "t1", 1), ("a1", "t2", 0)]);
    }
}
