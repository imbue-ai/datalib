//! Zoom → doltlite, for one user's meetings.
//!
//! The user is looked up first (`api.user`, else the sign-in's own).
//! Then three dated listings, each read a window at a time over the days
//! its `coverage` does not hold and over the newest `refresh_days`
//! (`window`): the user's cloud recordings, Zoom's report of their past
//! meetings, and their AI Companion summaries. A window's entries and
//! its coverage are written in one transaction.
//!
//! What is owed is then worked out from what the store holds, never from
//! where a walk got to (docs/dev/data_architecture_ingestion.md, "What
//! is left to fetch"): every file a stored recording names that
//! `recording_files` does not hold at its size, every stored meeting
//! with no `participants`, every listed summary not held at its
//! modified time. Nothing is pruned: a recording deleted in Zoom keeps
//! what was copied of it.

pub mod api;
pub mod db;
pub mod schema_raw;
pub mod shape;
pub mod window;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate};
use serde_json::{json, Value};
use sqlx::{Sqlite, Transaction};

use datalib_etl::control::DownloadControl;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::Progress;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, BatchError, Fetched, Fetcher, Listed, Loop, Outcome};
use datalib_etl_zoom_config::ZoomApi;
use datalib_problems::Reason;

use api::Listing;
pub use db::{db_path_for, RawDb};
use db::{payload, FetchedFile};
use schema_raw::*;
use shape::{str_of, WantedFile};
use window::ymd;

/// How far back Zoom's past-meetings report reaches: "the month must
/// fall within the past six months", with a few days to spare.
const REPORT_DAYS: i64 = 175;

/// How long Zoom keeps who attended a meeting: 15 months.
const PARTICIPANT_DAYS: i64 = 456;

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    pub latchkey: LatchkeySettings,
    pub api: ZoomApi,
    /// `common.blob_size_limit_bytes`: the largest video or audio file
    /// `api.media` fetches.
    pub blob_size_limit_bytes: Option<u64>,
    /// The run's day, in UTC, which Zoom's dates are in.
    pub today: NaiveDate,
    pub progress: Progress,
    pub control: DownloadControl,
    pub sealer: Option<Sealer>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct FetchSummary {
    /// Windows listed whole, across the three listings.
    pub windows: usize,
    pub recordings_listed: usize,
    pub meetings_reported: usize,
    pub summaries_listed: usize,
    pub files: usize,
    /// Files a recording named that Zoom no longer has.
    pub files_gone: usize,
    /// Video and audio files over `blob_size_limit_bytes`.
    pub files_skipped: usize,
    pub participants: usize,
    pub summaries: usize,
    pub failed: usize,
    /// Owed records a stop or a give-up left for the next run.
    pub left: usize,
}

impl FetchSummary {
    pub fn line(&self) -> String {
        format!(
            "windows={} recordings_listed={} meetings_reported={} summaries_listed={} \
             files={} files_gone={} files_skipped={} participants={} summaries={} \
             failed={} left={}",
            self.windows,
            self.recordings_listed,
            self.meetings_reported,
            self.summaries_listed,
            self.files,
            self.files_gone,
            self.files_skipped,
            self.participants,
            self.summaries,
            self.failed,
            self.left,
        )
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| walk(opts, found)).await
}

/// The run's own record in `sync_runs`: its config, its summary, and what
/// it changed in each table.
async fn walk(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let run = DownloadRun::start(opts.db.pool(), &serde_json::to_value(&opts.api)?).await?;
    let mut summary = FetchSummary::default();
    let result = walk_user(&opts, &found, &mut summary).await;
    run.finish(&result, &summary).await;
    result.map(|()| summary)
}

async fn walk_user(opts: &FetchOptions, found: &RunProblems, s: &mut FetchSummary) -> Result<()> {
    opts.progress.set_message("zoom: looking up the user");
    let user = api::resolve_user(opts.api.user.as_deref(), &opts.latchkey).await?;
    let user_id = str_of(&user, "id")
        .context("Zoom's answer for the user carried no id")?
        .to_string();
    db::store_user(opts.db.pool(), &user_id, &user).await?;

    let since = window::date(opts.api.since())?;
    list(opts, found, s, Listing::Recordings, &user_id, since).await?;
    if opts.api.participants {
        let floor = opts.today - Duration::days(REPORT_DAYS);
        list(opts, found, s, Listing::Reports, &user_id, since.max(floor)).await?;
    }
    if opts.api.summaries {
        list(opts, found, s, Listing::Summaries, &user_id, since).await?;
    }

    files(opts, found, s).await?;
    if opts.api.participants {
        participants(opts, found, s).await?;
    }
    if opts.api.summaries {
        summaries(opts, found, s).await?;
    }
    Ok(())
}

// ── the dated listings ───────────────────────────────────────────────

/// Read `listing` over every window this run owes it, from `lo` to
/// today. A window that does not list whole ends the listing, as a
/// problem: the next window would most likely fail the same way, and
/// the windows not read stay owed.
async fn list(
    opts: &FetchOptions,
    found: &RunProblems,
    s: &mut FetchSummary,
    listing: Listing,
    user_id: &str,
    lo: NaiveDate,
) -> Result<()> {
    if opts.control.stop.requested() {
        return Ok(());
    }
    let scope = coverage_scope(listing.name(), user_id);
    let want = Span::new(ymd(lo), ymd(opts.today));
    let held = coverage::held(opts.db.pool(), &scope).await?;
    let refresh_from = opts.today - Duration::days(opts.api.refresh_days().into());
    let mut windows = Vec::new();
    for span in window::to_walk(&want, &held, refresh_from)?.iter().rev() {
        windows.extend(window::windows(span)?);
    }
    opts.progress
        .set_message(&format!("zoom: listing {}", listing.name()));
    opts.progress.set_length(Some(windows.len() as u64));
    for w in windows {
        if opts.control.stop.requested() {
            break;
        }
        let url = |token: Option<&str>| api::listing_url(listing, user_id, &w.lo, &w.hi, token);
        let items = match api::paged(url, listing.items_key(), &opts.latchkey).await {
            Ok(items) => items,
            Err(e) if api::is_service(&e) => return Err(e),
            Err(_) if opts.control.stop.requested() => break,
            Err(e) => {
                found.listing(listing.name(), format!("{}..{}: {e:#}", w.lo, w.hi));
                break;
            }
        };
        let n = store_window(opts, listing, &scope, &w, items).await?;
        match listing {
            Listing::Recordings => s.recordings_listed += n,
            Listing::Reports => s.meetings_reported += n,
            Listing::Summaries => s.summaries_listed += n,
        }
        s.windows += 1;
        opts.progress.inc(1);
    }
    Ok(())
}

/// One window's entries and its coverage, in one transaction. Returns
/// how many entries it held.
async fn store_window(
    opts: &FetchOptions,
    listing: Listing,
    scope: &str,
    w: &Span,
    items: Vec<Value>,
) -> Result<usize> {
    let uuid_key = match listing {
        Listing::Recordings | Listing::Reports => "uuid",
        Listing::Summaries => "meeting_uuid",
    };
    // Two windows share a day, and a meeting on it is in both; within one
    // window an entry listed twice is kept once.
    let by_uuid: BTreeMap<String, Value> = items
        .into_iter()
        .filter_map(|item| Some((str_of(&item, uuid_key)?.to_string(), item)))
        .collect();
    let n = by_uuid.len();
    let start = |v: &Value| str_of(v, "start_time").map(str::to_string);
    let mut tx = opts.db.pool().begin().await.context("begin a window")?;
    match listing {
        Listing::Recordings => {
            let rows = by_uuid
                .into_iter()
                .map(|(uuid, item)| {
                    Ok(RecordingRow {
                        start_time: start(&item),
                        id_and_payload: payload(&uuid, &shape::canonical_recording(item))?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            db::store_listed(&mut tx, &rows).await?;
        }
        Listing::Reports => {
            let rows = by_uuid
                .into_iter()
                .map(|(uuid, item)| {
                    Ok(ReportedMeetingRow {
                        start_time: start(&item),
                        id_and_payload: payload(&uuid, &item)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            db::store_listed(&mut tx, &rows).await?;
        }
        Listing::Summaries => {
            let rows = by_uuid
                .into_iter()
                .map(|(uuid, item)| {
                    Ok(SummaryListingRow {
                        last_modified: str_of(&item, "summary_last_modified_time")
                            .or_else(|| str_of(&item, "summary_created_time"))
                            .map(str::to_string),
                        id_and_payload: payload(&uuid, &item)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            db::store_listed(&mut tx, &rows).await?;
        }
    }
    coverage::cover(&mut tx, scope, w.clone()).await?;
    tx.commit().await.context("commit a window")?;
    if let Some(sealer) = &opts.sealer {
        sealer.wrote(n as u64 + 1).await;
    }
    Ok(n)
}

fn drain_loop<'a>(
    opts: &'a FetchOptions,
    found: &'a RunProblems,
    table: &'static str,
    phase: &'a str,
    concurrency: usize,
) -> Loop<'a> {
    Loop {
        pool: opts.db.pool(),
        table,
        phase,
        stop: &opts.control.stop,
        found,
        sealer: opts.sealer.as_ref(),
        batch: 1,
        concurrency,
        flush: 16,
        flush_bytes: 64 << 20,
        failures_in_a_row: 10,
    }
}

// ── recording files ──────────────────────────────────────────────────

async fn files(opts: &FetchOptions, found: &RunProblems, s: &mut FetchSummary) -> Result<()> {
    if opts.control.stop.requested() {
        return Ok(());
    }
    let mut wanted: HashMap<String, WantedFile> = HashMap::new();
    let mut listed = Vec::new();
    for meeting in opts.db.recordings().await? {
        for file in shape::wanted_files(&meeting, opts.api.media) {
            let key = file.key();
            listed.push(Listed::new(key.clone(), file.size.map(|n| n.to_string())));
            wanted.insert(key, file);
        }
    }
    let owed = owed::owed(opts.db.pool(), RECORDING_FILES, listed).await?;
    if owed.is_empty() {
        return Ok(());
    }
    opts.progress
        .set_message(&format!("zoom: {} recording files", owed.len()));
    opts.progress.set_length(Some(owed.len() as u64));
    let fetcher = Files {
        latchkey: &opts.latchkey,
        db: &opts.db,
        progress: &opts.progress,
        stop: &opts.control.stop,
        cap: opts.blob_size_limit_bytes,
        wanted,
        meetings: Mutex::new(HashMap::new()),
        gone: AtomicUsize::new(0),
    };
    // One at a time: a meeting's files share the token its first file
    // fetched.
    let l = drain_loop(opts, found, RECORDING_FILES, "recording files", 1);
    let drained = owed::drain(&l, owed, &fetcher).await?;
    let gone = fetcher.gone.load(Ordering::Relaxed);
    s.files += drained.got - gone;
    s.files_gone += gone;
    s.files_skipped += drained.skipped;
    s.failed += drained.failed;
    s.left += drained.left;
    Ok(())
}

/// A meeting's fresh recordings answer: the token that downloads its
/// files and their current links. `None` for a meeting Zoom no longer
/// has a recording of.
type MeetingFiles = Option<Arc<(String, Vec<Value>)>>;

struct Files<'a> {
    latchkey: &'a LatchkeySettings,
    db: &'a RawDb,
    progress: &'a Progress,
    stop: &'a StopFlag,
    cap: Option<u64>,
    wanted: HashMap<String, WantedFile>,
    meetings: Mutex<HashMap<String, MeetingFiles>>,
    gone: AtomicUsize,
}

impl Files<'_> {
    async fn meeting(&self, uuid: &str) -> Result<MeetingFiles> {
        if let Some(known) = self.meetings.lock().unwrap().get(uuid) {
            return Ok(known.clone());
        }
        let answer = match api::get_json(&api::meeting_recordings_url(uuid), self.latchkey).await {
            Ok(v) => {
                let token = str_of(&v, "download_access_token").ok_or_else(|| {
                    api::ApiError::Service(
                        "Zoom gave no download_access_token with the meeting's recordings: \
                         the role of the user who signed in needs \"View the recording \
                         content\""
                            .to_string(),
                    )
                })?;
                let files = v
                    .get("recording_files")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Some(Arc::new((token.to_string(), files)))
            }
            Err(e) if api::is_not_found(&e) || api::is_unavailable(&e) => None,
            Err(e) => return Err(e),
        };
        self.meetings
            .lock()
            .unwrap()
            .insert(uuid.to_string(), answer.clone());
        Ok(answer)
    }

    /// The file's bytes, or `None` bytes for one Zoom no longer has.
    async fn file(&self, w: &WantedFile) -> Result<FetchedFile> {
        let mut out = FetchedFile {
            meeting_uuid: w.meeting_uuid.clone(),
            file_key: w.file_key.clone(),
            content_type: None,
            bytes: None,
        };
        let Some(meeting) = self.meeting(&w.meeting_uuid).await? else {
            return Ok(out);
        };
        let (token, files) = meeting.as_ref();
        let Some(file) = shape::file_named(files, &w.file_key) else {
            return Ok(out);
        };
        out.content_type = shape::content_type(file);
        let url = str_of(file, "download_url").context("the file has no download_url")?;
        match api::download(url, token).await {
            Ok(bytes) => out.bytes = Some(bytes),
            Err(e) if api::is_not_found(&e) || api::is_unavailable(&e) => {}
            Err(e) => return Err(e),
        }
        Ok(out)
    }

    fn over_cap(&self, w: &WantedFile) -> Option<String> {
        let cap = self.cap?;
        let size = w.size?;
        (w.media && size > cap).then(|| {
            format!(
                "the {} file is {size} bytes, over blob_size_limit_bytes ({cap})",
                w.file_type
            )
        })
    }
}

#[async_trait]
impl Fetcher<FetchedFile> for Files<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<FetchedFile>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let outcome = match self.wanted.get(&listed.key) {
                None => Outcome::Failed("owed but not wanted".to_string()),
                Some(w) => match self.over_cap(w) {
                    Some(why) => Outcome::Skipped(Reason::OverSizeLimit, why),
                    None => match self.file(w).await {
                        Ok(f) if f.bytes.is_some() => Outcome::Got(f),
                        Ok(f) => {
                            self.gone.fetch_add(1, Ordering::Relaxed);
                            Outcome::Unusable(
                                f,
                                Reason::NotFound,
                                "Zoom no longer has this recording file".to_string(),
                            )
                        }
                        Err(e) if api::is_service(&e) => return Err(BatchError::Terminal(e)),
                        Err(e) if self.stop.requested() => return Err(BatchError::Batch(e)),
                        Err(e) => Outcome::Failed(format!("{e:#}")),
                    },
                },
            };
            self.progress.inc(1);
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<FetchedFile>],
    ) -> Result<()> {
        let files: Vec<&FetchedFile> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(file) | Outcome::Unusable(file, ..) => Some(file),
                _ => None,
            })
            .collect();
        db::store_files(tx, self.db.cas(), &files).await
    }

    fn weight(&self, content: &FetchedFile) -> usize {
        content.bytes.as_ref().map_or(0, Vec::len)
    }
}

// ── participants and summaries ───────────────────────────────────────

async fn participants(
    opts: &FetchOptions,
    found: &RunProblems,
    s: &mut FetchSummary,
) -> Result<()> {
    if opts.control.stop.requested() {
        return Ok(());
    }
    // Zoom forgets who attended after 15 months; a meeting older than
    // that is not asked about, rather than failing on every run.
    let floor = ymd(opts.today - Duration::days(PARTICIPANT_DAYS));
    let listed: Vec<Listed> = opts
        .db
        .meetings()
        .await?
        .into_iter()
        .filter(|(_, start)| start.as_deref().is_some_and(|t| t >= floor.as_str()))
        .map(|(uuid, _)| Listed::new(uuid, None::<String>))
        .collect();
    let owed = owed::owed(opts.db.pool(), PARTICIPANTS, listed).await?;
    if owed.is_empty() {
        return Ok(());
    }
    opts.progress
        .set_message(&format!("zoom: who attended {} meetings", owed.len()));
    opts.progress.set_length(Some(owed.len() as u64));
    let fetcher = Whole {
        latchkey: &opts.latchkey,
        progress: &opts.progress,
        stop: &opts.control.stop,
        kind: WholeKind::Participants,
    };
    let l = drain_loop(opts, found, PARTICIPANTS, "participants", 2);
    let drained = owed::drain(&l, owed, &fetcher).await?;
    s.participants += drained.got;
    s.failed += drained.failed;
    s.left += drained.left;
    Ok(())
}

async fn summaries(opts: &FetchOptions, found: &RunProblems, s: &mut FetchSummary) -> Result<()> {
    if opts.control.stop.requested() {
        return Ok(());
    }
    let listed: Vec<Listed> = opts
        .db
        .summary_listings()
        .await?
        .into_iter()
        .map(|(uuid, modified)| Listed::new(uuid, modified))
        .collect();
    let owed = owed::owed(opts.db.pool(), SUMMARIES, listed).await?;
    if owed.is_empty() {
        return Ok(());
    }
    opts.progress
        .set_message(&format!("zoom: {} meeting summaries", owed.len()));
    opts.progress.set_length(Some(owed.len() as u64));
    let fetcher = Whole {
        latchkey: &opts.latchkey,
        progress: &opts.progress,
        stop: &opts.control.stop,
        kind: WholeKind::Summary,
    };
    let l = drain_loop(opts, found, SUMMARIES, "summaries", 2);
    let drained = owed::drain(&l, owed, &fetcher).await?;
    s.summaries += drained.got;
    s.failed += drained.failed;
    s.left += drained.left;
    Ok(())
}

/// A table each of whose rows is one whole answer about one meeting.
#[derive(Debug, Clone, Copy)]
enum WholeKind {
    Participants,
    Summary,
}

/// A fetched row: its key and payload.
#[derive(Debug, Clone)]
struct Answer {
    uuid: String,
    payload: Value,
}

struct Whole<'a> {
    latchkey: &'a LatchkeySettings,
    progress: &'a Progress,
    stop: &'a StopFlag,
    kind: WholeKind,
}

impl Whole<'_> {
    async fn ask(&self, uuid: &str) -> Result<Value> {
        match self.kind {
            WholeKind::Participants => {
                let url = |token: Option<&str>| api::participants_url(uuid, token);
                let list = api::paged(url, "participants", self.latchkey).await?;
                Ok(json!({ "participants": list }))
            }
            WholeKind::Summary => api::get_json(&api::summary_url(uuid), self.latchkey).await,
        }
    }

    /// What is stored for a meeting Zoom would not answer about.
    fn unavailable(&self, why: &str) -> Value {
        match self.kind {
            WholeKind::Participants => json!({ "participants": [], "unavailable": why }),
            WholeKind::Summary => json!({ "unavailable": why }),
        }
    }
}

#[async_trait]
impl Fetcher<Answer> for Whole<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Answer>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let uuid = listed.key.clone();
            let outcome = match self.ask(&uuid).await {
                Ok(payload) => Outcome::Got(Answer { uuid, payload }),
                Err(e) if api::is_service(&e) || api::needs_paid_plan(&e) => {
                    return Err(BatchError::Terminal(e))
                }
                Err(e) if api::is_not_found(&e) || api::is_unavailable(&e) => {
                    let why = format!("{e:#}");
                    Outcome::Unusable(
                        Answer {
                            payload: self.unavailable(&why),
                            uuid,
                        },
                        Reason::NotFound,
                        why,
                    )
                }
                Err(e) if self.stop.requested() => return Err(BatchError::Batch(e)),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            self.progress.inc(1);
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Answer>],
    ) -> Result<()> {
        let answers: Vec<&Answer> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(a) | Outcome::Unusable(a, ..) => Some(a),
                _ => None,
            })
            .collect();
        match self.kind {
            WholeKind::Participants => {
                let rows = answers
                    .iter()
                    .map(|a| {
                        Ok(ParticipantsRow {
                            id_and_payload: payload(&a.uuid, &a.payload)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                db::store_rows(tx, &rows).await
            }
            WholeKind::Summary => {
                let rows = answers
                    .iter()
                    .map(|a| {
                        Ok(SummaryRow {
                            id_and_payload: payload(&a.uuid, &a.payload)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                db::store_rows(tx, &rows).await
            }
        }
    }
}
