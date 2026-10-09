//! Claude (claude.ai) downloader — the `claude` source type's `api`
//! ingest. Each org's `/chat_conversations` lists its conversations
//! whole, at an `updated_at`; a conversation is held at the one it was
//! listed at, its files as edges its row lists, owed until their bytes
//! land. A project's listing entry is its metadata, held at its
//! `updated_at`; its knowledge docs are a listing of their own, held at
//! the same stamp and due again after a day. Every fetch goes through
//! `datalib_etl_web::owed`, which owns the stop, the failure budget, the
//! flush and what each outcome means for a record. Nothing is marked
//! done: holding the content at the listed version is done
//! (docs/dev/data_architecture_ingestion.md, "What is left to fetch").

pub mod api;
pub mod db;
pub mod export;
mod fetchers;
pub(crate) use fetchers::file_url;
pub mod normalize;
pub mod schema_raw;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl::download_problems::{DownloadProblem, RunProblem};
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::RunBar;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, Fetcher, Held, Listed, Loop};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

pub use api::{ClaudeClient, ClaudeError};
use db::ProjectUpsert;
pub use db::{db_path_for, Conversation, LoadedConversation, LoadedRaw, RawDb};
use fetchers::{Conversations, Docs, Files, ProjectListed};
use schema_raw::{OrgRow, UserRow, ATTACHMENTS, CONVERSATIONS, PROJECTS, PROJECT_DOCS_LISTINGS};

/// Between one org's listings and the next's.
pub const SLEEP_BETWEEN: Duration = Duration::from_millis(400);
pub const DEFAULT_OVERLAP: usize = 3;
pub(crate) const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);
pub(crate) const CLAUDE_ORIGIN: &str = "https://claude.ai";

/// How long a completed `/organizations` listing stays good. Matches
/// slack's `MANIFEST_TTL`, and for the same reason: the org set is
/// near-static, so re-listing it on every download is pure waste.
pub const ORGS_TTL: chrono::Duration = chrono::Duration::hours(6);
pub(crate) const ORGS_SWEEP_KEY: &str = "orgs";

/// How long a project's knowledge-doc listing stays good: editing a doc
/// has not been seen to move its project's `updated_at`.
pub const PROJECT_DOCS_TTL: chrono::Duration = chrono::Duration::hours(24);

pub(crate) fn project_docs_sweep_key(project_uuid: &str) -> String {
    format!("project_docs:{project_uuid}")
}

/// Requests in a row that came to nothing before a loop gives up on
/// this run. A rate limit ends the run on its own; this ends it on an
/// answer the retry guard does not retry, such as a `500` on every
/// conversation.
const FAILURE_BUDGET: usize = 25;

/// `Default` is hand-written, not derived, so `projects` defaults to
/// `true` and agrees with `ClaudeApiSync`'s serde default. A derived
/// `Default` would make every `..Default::default()` caller silently
/// opt out of the project mirror while their config said otherwise.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block. Default = the only stored
    /// account for the service.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Path to a bulk-export directory (`users.json` and friends). If
    /// set and the DB is missing users, we pre-seed them from here.
    pub export_dir: Option<PathBuf>,
    /// The N most recently updated conversations of each org are
    /// fetched every run, held or not: a check against the live copy.
    pub overlap: usize,
    /// Between detail fetches.
    pub sleep_between: Duration,
    /// Only sync conversations whose listing `updated_at` is at or
    /// after this instant (RFC 3339 or `YYYY-MM-DD`, assumed UTC).
    /// Older conversations are never detail-fetched — the listing walk
    /// itself is one request per org and stays unbounded. `None` →
    /// sync everything. Ignored in `conv_uuids` mode.
    pub since: Option<String>,
    /// When non-empty, fetch only these conversation UUIDs. The
    /// listing walk is skipped entirely.
    pub conv_uuids: Vec<String>,
    /// Also mirror Claude Projects (metadata + knowledge docs).
    pub projects: bool,
    /// When non-empty, mirror only these project UUIDs. The per-org
    /// listing still runs; everything outside the set is skipped.
    pub project_uuids: Vec<String>,
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
    /// Seals as flushes land, when the step driver hands one over.
    pub sealer: Option<datalib_etl::raw_store::Sealer>,
    /// The run-pinned `--now`, which the sweep TTLs are measured from, so
    /// whether a listing is requested does not depend on the wall clock.
    /// `None` samples the clock.
    pub now: Option<String>,
}

impl FetchOptions {
    /// Every field defaulted except the store, which has none to give:
    /// it is a live handle the caller opens and closes.
    pub fn new(db: RawDb) -> Self {
        Self {
            latchkey: LatchkeySettings::default(),
            db,
            export_dir: None,
            overlap: 0,
            sleep_between: Duration::ZERO,
            since: None,
            conv_uuids: Vec::new(),
            projects: true,
            project_uuids: Vec::new(),
            sealer: None,
            progress: Default::default(),
            control: Default::default(),
            now: None,
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct FetchSummary {
    /// Configured `conv_uuids` and `project_uuids` upstream has not got.
    /// Reported rather than fatal: one dead link costs that entry, not
    /// the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<DownloadProblem>,
    pub fetched: usize,
    /// Listed, in scope, and already held at the version listed.
    pub skipped: usize,
    /// Listing items ignored because their `updated_at` predates the
    /// configured `since`. Not counted in `skipped` (which means
    /// "in scope and already up to date") or `total`.
    pub out_of_scope: usize,
    /// Orgs whose `capabilities` leave out `chat` (an API-console org):
    /// not asked for anything.
    pub non_chat_orgs: usize,
    pub forbidden_orgs: usize,
    /// Conversations an org's complete listing did not name — deleted on
    /// claude.ai. Never counts rows a `claude_export` ingest wrote (those
    /// carry a NULL `org_uuid` and are out of an API sync's scope).
    pub pruned: usize,
    /// Fetch failures across every loop — conversations, docs listings.
    pub errors: usize,
    pub total: usize,
    /// Projects whose metadata row was written this run.
    pub projects_fetched: usize,
    /// Projects already held at the `updated_at` listed.
    pub projects_skipped: usize,
    /// Knowledge documents written this run, across every project.
    pub project_docs_fetched: usize,
    /// Projects whose docs listing was held and under a day old.
    pub project_docs_skipped: usize,
    pub new_blobs: usize,
    /// Edges satisfied from bytes the CAS already held.
    pub skipped_blobs: usize,
    pub failed_blobs: usize,
    pub requests: u64,
    pub network_seconds: f64,
    /// Total number of extra `get_conversation` attempts spent on
    /// transient-403 retries (does not count the initial attempt).
    pub forbidden_retry_attempts: u64,
    /// Conversations that ultimately succeeded only after at least one
    /// retry. `forbidden_retry_attempts > 0` with `_recovered == 0`
    /// would mean every retry path exhausted without success.
    pub forbidden_retry_recoveries: u64,
}

#[instrument(skip_all, fields(
    db = %opts.db.pool().connect_options().get_filename().display()
))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| download(opts, found))
        .await
}

async fn download(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let _ = datalib_etl_web::latchkey::ensure_curl_router();
    let db = opts.db.clone();
    let run_config = json!({
        "overlap": opts.overlap,
        "since": opts.since,
        "conv_uuids": opts.conv_uuids,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let client = ClaudeClient::with_latchkey(opts.latchkey.clone());
    // One `now` per fetch — threaded into every bulk upsert so all
    // `<table>_bookkeeping.fetched_at_utc` stamps from a single sync share
    // a timestamp.
    let now = IsoOffsetTimestamp::now_local();
    let run_now = match &opts.now {
        Some(s) => datalib_time::parse_strict(s).context("--now")?,
        None => now.clone(),
    };
    // Nothing is known before the org listing returns, so the bar
    // starts with no total rather than a guess.
    let bar = RunBar::new(&opts.progress, 0);
    let ctx = Ctx {
        client: &client,
        db: &db,
        opts: &opts,
        found: &found,
        bar: &bar,
        now,
        run_now,
        files: Mutex::new(HashMap::new()),
        forbidden_orgs: Mutex::new(BTreeMap::new()),
        config_problems: Mutex::new(Vec::new()),
        counts: Counts::default(),
    };
    let mut summary = FetchSummary::default();
    let result = phases(&ctx, &mut summary).await;
    bar.finish();
    summary.total = summary.fetched + summary.skipped;
    summary.requests = client.requests();
    summary.network_seconds = client.network_seconds();
    summary.problems = std::mem::take(&mut *ctx.config_problems.lock().unwrap());
    summary.forbidden_orgs = ctx.forbidden_orgs.lock().unwrap().len();
    summary.project_docs_fetched = ctx.counts.project_docs.load(Ordering::Relaxed);
    summary.new_blobs = ctx.counts.new_blobs.load(Ordering::Relaxed);
    summary.skipped_blobs = ctx.counts.skipped_blobs.load(Ordering::Relaxed);
    summary.forbidden_retry_attempts =
        ctx.counts.forbidden_retry_attempts.load(Ordering::Relaxed) as u64;
    summary.forbidden_retry_recoveries = ctx
        .counts
        .forbidden_retry_recoveries
        .load(Ordering::Relaxed) as u64;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// What this run's fetchers count as they go.
#[derive(Default)]
pub(crate) struct Counts {
    pub project_docs: AtomicUsize,
    pub new_blobs: AtomicUsize,
    pub skipped_blobs: AtomicUsize,
    pub forbidden_retry_attempts: AtomicUsize,
    pub forbidden_retry_recoveries: AtomicUsize,
}

/// What the listings and every loop of one run share.
pub(crate) struct Ctx<'a> {
    pub client: &'a ClaudeClient,
    pub db: &'a RawDb,
    pub opts: &'a FetchOptions,
    pub found: &'a RunProblems,
    pub bar: &'a RunBar,
    /// Stamps the bookkeeping of the rows written outside the loops.
    pub now: IsoOffsetTimestamp,
    /// What the sweep markers are stamped with and aged against.
    pub run_now: IsoOffsetTimestamp,
    /// Per conversation fetched this run, the file objects it names:
    /// what the file loop asks for without reading the row back.
    pub files: Mutex<HashMap<String, Vec<Value>>>,
    /// An org that refuses one request refuses them all: once it has
    /// answered 403 past the transient retries it is not asked again
    /// this run. The listing walk learns this from `list_conversations`;
    /// the one-by-one path has no listing, so it learns it from the
    /// detail fetch.
    pub forbidden_orgs: Mutex<BTreeMap<String, String>>,
    /// The configured entries upstream has not got.
    pub config_problems: Mutex<Vec<DownloadProblem>>,
    pub counts: Counts,
}

impl Ctx<'_> {
    pub fn stop(&self) -> &StopFlag {
        &self.opts.control.stop
    }

    async fn wrote(&self, rows: u64) {
        if let Some(sealer) = &self.opts.sealer {
            sealer.wrote(rows).await;
        }
    }

    pub fn remember_files(&self, c: &Conversation) {
        self.files
            .lock()
            .unwrap()
            .insert(c.uuid.clone(), c.files.clone());
    }

    /// The conversation's file object for one of its files: from this
    /// run's fetch of it, or the stored row. `None` when it names no
    /// such file.
    pub async fn file_object(&self, conv_uuid: &str, file_uuid: &str) -> Result<Option<Value>> {
        let known = self
            .files
            .lock()
            .unwrap()
            .get(conv_uuid)
            .map(|files| fetchers::find_file(files, file_uuid).cloned());
        if let Some(found) = known {
            return Ok(found);
        }
        let Some(payload) = self.db.load_conversation_payload(conv_uuid).await? else {
            return Ok(None);
        };
        Ok(fetchers::find_file(&db::files_of(&payload), file_uuid).cloned())
    }

    /// The give-up guard tripped: every further request would be refused
    /// the same way, so the run does no more of them.
    fn give_up(&self, at: &str, reason: &str) {
        self.found.phase(
            at,
            format!("stopped at the rate limit; the rest is left for the next run: {reason}"),
        );
    }

    /// One row per org this credential cannot read, so the Manage row
    /// says why a whole org is missing. Keyed by the org's uuid, since
    /// two orgs of one account can share a name.
    fn refused_orgs(&self) -> Vec<RunProblem> {
        let org = |(uuid, name): (&String, &String)| {
            RunProblem::forbidden(
                &format!("org:{uuid}"),
                format!(
                    "the org {name:?} refuses the credential's requests (conversations and \
                     projects); nothing from it is mirrored"
                ),
            )
        };
        self.forbidden_orgs
            .lock()
            .unwrap()
            .iter()
            .map(org)
            .collect()
    }

    /// The orgs worth walking: every one but those whose capabilities
    /// say they have no chat.
    fn chat_orgs(&self, orgs: Vec<Value>, s: &mut FetchSummary) -> Vec<Value> {
        let (skipped, walked): (Vec<Value>, Vec<Value>) = orgs.into_iter().partition(lacks_chat);
        for org in &skipped {
            info!(
                event = "claude_org_not_chat",
                org = %org_identity(org).map(|(_, name)| name).unwrap_or_default(),
                capabilities = %org.get("capabilities").cloned().unwrap_or_default(),
                "this org has no chat (an API-console org); not walking it"
            );
        }
        s.non_chat_orgs = skipped.len();
        walked
    }

    async fn drain<T: Send>(
        &self,
        table: &'static str,
        phase: &str,
        owed: Vec<Listed>,
        f: &impl Fetcher<T>,
        flush: usize,
    ) -> Result<owed::Drained> {
        let l = Loop {
            pool: self.db.pool(),
            table,
            phase,
            stop: self.stop(),
            found: self.found,
            sealer: self.opts.sealer.as_ref(),
            batch: 1,
            concurrency: 1,
            flush,
            flush_bytes: 32 << 20,
            failures_in_a_row: FAILURE_BUDGET,
        };
        owed::drain(&l, owed, f).await
    }

    /// The orgs this credential can see: the stored rows while the
    /// listing is under its TTL, else `/organizations`, which doubles as
    /// the credential preflight — a missing latchkey registration or a
    /// dead sessionKey fails the run right here with setup instructions.
    async fn orgs(&self) -> Result<Vec<Value>> {
        let db = self.db;
        if let Some(age) = db.sweep_age(ORGS_SWEEP_KEY, &self.run_now).await? {
            if age < ORGS_TTL {
                let orgs = db.load_orgs().await?;
                // An empty `orgs` table with a fresh marker must not
                // silently yield zero orgs (that would skip every
                // conversation); fall through to the live call.
                if !orgs.is_empty() {
                    info!(
                        event = "claude_orgs_skipped",
                        reason = "ttl",
                        age_s = age.num_seconds().max(0),
                        ttl_s = ORGS_TTL.num_seconds(),
                        count = orgs.len(),
                        "the org listing is fresh enough; not re-listing"
                    );
                    return Ok(orgs);
                }
            }
        }
        let orgs = self.client.list_orgs().await.map_err(credential_hint)?;
        info!(
            event = "claude_orgs",
            count = orgs.len(),
            "listed the orgs this credential can see"
        );
        let mut rows: Vec<OrgRow> = Vec::with_capacity(orgs.len());
        for payload in &orgs {
            let Some(id) = payload.get("uuid").and_then(Value::as_str) else {
                continue;
            };
            rows.push(OrgRow {
                id_and_payload: WirePayload {
                    id: id.to_string(),
                    payload: serde_json::to_string(&canonicalize_org_payload(payload))
                        .context("serialize org")?,
                },
                name: payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(String::from),
            });
        }
        let mut tx = db.pool().begin().await.context("begin orgs tx")?;
        db.store_orgs(&mut tx, &rows, &self.now, &self.run_now)
            .await?;
        tx.commit().await.context("commit orgs tx")?;
        Ok(orgs)
    }

    /// The account, once: users.json from the bulk export carries the
    /// account.uuid every conversation needs, and `/account` is the
    /// fallback.
    async fn users(&self) -> Result<()> {
        let db = self.db;
        if !db.has_any_user().await? {
            if let Some(export_dir) = self.opts.export_dir.as_deref() {
                match read_export_users(export_dir) {
                    Ok(Some(users)) => upsert_users(db, &users, &self.now).await?,
                    Ok(None) => {}
                    Err(e) => warn!(
                        event = "claude_export_users_unreadable",
                        error = %format!("{e:#}"),
                        "the export's users.json could not be read; asking /account instead"
                    ),
                }
            }
        }
        if !db.has_any_user().await? {
            match self.client.current_account().await {
                Ok(acct) => {
                    upsert_users(db, &[pick_user_fields(&acct)], &self.now).await?;
                    info!(
                        event = "claude_users_synthesized",
                        "synthesized the users from the account"
                    );
                }
                // Asked again next run, since there is still no user.
                Err(e) => self.found.phase("account", e.to_string()),
            }
        }
        Ok(())
    }

    /// Every org's projects, stored at the `updated_at` listed, then the
    /// knowledge docs of every project not held at that stamp or last
    /// listed over a day ago. Whether a rate limit ended it.
    async fn projects(&self, orgs: &[Value], s: &mut FetchSummary) -> Result<bool> {
        // Normalized → as configured, so a miss is reported the way the
        // config spelled it.
        let only: HashMap<String, &str> = self
            .opts
            .project_uuids
            .iter()
            .map(|p| (datalib_etl::ids::normalize_id_token(p), p.as_str()))
            .collect();
        let mut matched: HashSet<&str> = HashSet::new();
        let mut unlisted_orgs = 0usize;
        let mut listed: Vec<Listed> = Vec::new();
        let mut by_project: HashMap<String, ProjectListed> = HashMap::new();
        for org in orgs {
            let Some((org_uuid, org_name)) = org_identity(org) else {
                continue;
            };
            let listing = match self
                .client
                .list_projects(org_uuid)
                .instrument(info_span!("claude_project_listing", org = %org_name))
                .await
            {
                Ok(l) => l,
                Err(_) if self.stop().requested() => return Ok(false),
                Err(ClaudeError::RateLimited(reason)) => {
                    self.give_up("projects", &reason);
                    return Ok(true);
                }
                Err(ClaudeError::Forbidden(_)) => {
                    info!(
                        event = "claude_projects_forbidden",
                        org = %org_name,
                        "this org refuses project listings; its conversations are not \
                         asked for one by one either"
                    );
                    self.forbidden_orgs
                        .lock()
                        .unwrap()
                        .insert(org_uuid.to_string(), org_name.clone());
                    unlisted_orgs += 1;
                    continue;
                }
                Err(e) => {
                    self.found.listing(
                        &format!("projects org:{org_uuid}"),
                        format!("org {org_name:?}: {e}"),
                    );
                    s.errors += 1;
                    unlisted_orgs += 1;
                    continue;
                }
            };
            info!(
                event = "claude_project_listing_count",
                org = %org_name,
                count = listing.len(),
                "listed one org's projects"
            );
            let mut rows: Vec<ProjectUpsert> = Vec::new();
            for p in &listing {
                let Some(uuid) = p.get("uuid").and_then(Value::as_str) else {
                    continue;
                };
                if !only.is_empty() {
                    let Some((k, _)) = only.get_key_value(uuid) else {
                        continue;
                    };
                    matched.insert(k.as_str());
                }
                let payload = canonicalize_project_payload(p);
                let updated_at = payload
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .map(String::from);
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .map(String::from);
                listed.push(Listed::new(uuid, updated_at.clone()));
                by_project.insert(
                    uuid.to_string(),
                    ProjectListed {
                        org_uuid: org_uuid.to_string(),
                        label: name.clone().unwrap_or_else(|| uuid.to_string()),
                    },
                );
                rows.push(ProjectUpsert {
                    uuid: uuid.to_string(),
                    org_uuid: org_uuid.to_string(),
                    org_name: org_name.clone(),
                    name,
                    updated_at,
                    payload: serde_json::to_string(&payload).context("serialize project")?,
                });
            }
            let held = owed::held_versions(
                self.db.pool(),
                PROJECTS,
                rows.iter().map(|r| r.uuid.as_str()),
            )
            .await?;
            let (write, held_already) = not_yet_held(rows, &held);
            s.projects_skipped += held_already;
            if !write.is_empty() {
                let mut tx = self.db.pool().begin().await.context("begin projects tx")?;
                self.db.store_projects(&mut tx, &write).await?;
                tx.commit().await.context("commit projects tx")?;
                self.wrote(write.len() as u64).await;
                s.projects_fetched += write.len();
            }
        }
        // A UUID in `api.project_uuids` that matched nothing is almost
        // always a typo or a project in an org this account can't see.
        // Silently mirroring nothing is the worst outcome, so say it.
        for (normalized, raw) in &only {
            if !matched.contains(normalized.as_str()) {
                let detail = if unlisted_orgs == 0 {
                    format!("no project with this id in any of {} org(s)", orgs.len())
                } else {
                    format!(
                        "no project with this id in the orgs that listed their projects; \
                         {unlisted_orgs} of {} org(s) did not",
                        orgs.len()
                    )
                };
                self.config_problems
                    .lock()
                    .unwrap()
                    .push(DownloadProblem::not_found("project_uuids", raw, detail));
            }
        }

        let by_version: HashSet<String> =
            owed::owed(self.db.pool(), PROJECT_DOCS_LISTINGS, listed.clone())
                .await?
                .into_iter()
                .map(|l| l.key)
                .collect();
        let ages = self.db.project_docs_sweep_ages(&self.run_now).await?;
        let owed: Vec<Listed> = listed
            .into_iter()
            .filter(|l| {
                by_version.contains(&l.key)
                    || ages.get(&l.key).is_none_or(|age| *age >= PROJECT_DOCS_TTL)
            })
            .collect();
        s.project_docs_skipped = by_project.len() - owed.len();
        let drained = self
            .drain(
                PROJECT_DOCS_LISTINGS,
                "projects",
                owed,
                &Docs {
                    ctx: self,
                    by_project,
                },
                10,
            )
            .await?;
        s.errors += drained.failed;
        Ok(drained.terminal.is_some())
    }

    /// `conv_uuids`: exactly the named conversations, each fetched every
    /// run, since no listing says whether one has moved. Each org is
    /// tried in turn; what is fetched is held at the `updated_at` its
    /// detail names, so a later listing run leaves it alone.
    async fn named(&self, orgs: &[Value], s: &mut FetchSummary) -> Result<bool> {
        let named = &self.opts.conv_uuids;
        self.bar.expect(named.len() as u64);
        for raw in named {
            if self.stop().requested() {
                break;
            }
            self.bar.did(1);
            self.bar.doing(raw);
            let target = datalib_etl::ids::normalize_id_token(raw);
            match self.fetch_single(orgs, &target, s).await? {
                SingleOutcome::Fetched | SingleOutcome::Failed => {}
                SingleOutcome::Stopped => break,
                SingleOutcome::Cut(reason) => {
                    self.give_up("conversations", &reason);
                    return Ok(true);
                }
                SingleOutcome::NotFoundInAnyOrg => {
                    self.config_problems
                        .lock()
                        .unwrap()
                        .push(DownloadProblem::not_found(
                            "conv_uuids",
                            raw,
                            format!(
                                "no conversation with this id in any of {} org(s)",
                                orgs.len()
                            ),
                        ));
                }
                SingleOutcome::ForbiddenInSomeOrg { refused, not_found } => {
                    self.config_problems
                        .lock()
                        .unwrap()
                        .push(DownloadProblem::forbidden(
                            "conv_uuids",
                            raw,
                            format!(
                                "refused by {} after retries, and not in the {not_found} \
                                 org(s) this credential can read; it may exist where the \
                                 credential cannot look",
                                refused.join(", ")
                            ),
                        ));
                }
            }
        }
        Ok(false)
    }

    async fn fetch_single(
        &self,
        orgs: &[Value],
        conv_uuid: &str,
        s: &mut FetchSummary,
    ) -> Result<SingleOutcome> {
        let mut refused: Vec<String> = Vec::new();
        let mut not_found = 0usize;
        for org in orgs {
            let Some((org_uuid, org_name)) = org_identity(org) else {
                continue;
            };
            if self.forbidden_orgs.lock().unwrap().contains_key(org_uuid) {
                refused.push(org_name);
                continue;
            }
            // Same retry the listing walk uses. Without it a transient 403
            // — which claude.ai issues routinely on a detail GET — reads as
            // "not in this org", and the uuid is reported missing.
            match get_conversation_with_403_retry(self.client, org_uuid, conv_uuid)
                .await
                .map(|o| o.value)
                .map_err(|(e, _)| e)
            {
                Ok(full) => {
                    let c = parse_conversation(org_uuid, &org_name, conv_uuid, &full)?;
                    let mut tx = self.db.pool().begin().await?;
                    self.db.store_conversation(&mut tx, &c).await?;
                    owed::hold(&mut tx, CONVERSATIONS, conv_uuid, c.updated_at.as_deref()).await?;
                    tx.commit().await?;
                    self.remember_files(&c);
                    s.fetched += 1;
                    self.wrote(1).await;
                    info!(
                        event = "claude_fetch_single_ok",
                        uuid = conv_uuid,
                        org = %org_name,
                        "fetched one conversation by id"
                    );
                    return Ok(SingleOutcome::Fetched);
                }
                Err(_) if self.stop().requested() => return Ok(SingleOutcome::Stopped),
                Err(ClaudeError::RateLimited(reason)) => return Ok(SingleOutcome::Cut(reason)),
                Err(ClaudeError::Forbidden(_)) => {
                    warn!(
                        event = "claude_fetch_single_forbidden",
                        uuid = conv_uuid,
                        org = %org_name,
                        "still 403 after the transient retries; not asking this org again",
                    );
                    self.forbidden_orgs
                        .lock()
                        .unwrap()
                        .insert(org_uuid.to_string(), org_name.clone());
                    refused.push(org_name);
                    continue;
                }
                Err(ClaudeError::Permanent(msg)) if msg.contains("HTTP 404") => {
                    info!(
                        event = "claude_fetch_single_not_in_org",
                        uuid = conv_uuid,
                        org = %org_name,
                        "this org has no conversation with that id"
                    );
                    not_found += 1;
                    continue;
                }
                Err(e) => {
                    warn!(event = "claude_fetch_error", uuid = conv_uuid, error = %e, "a conversation could not be fetched");
                    let mut tx = self.db.pool().begin().await?;
                    dr::record_object_error(&mut tx, CONVERSATIONS, conv_uuid, &e.to_string())
                        .await?;
                    tx.commit().await?;
                    s.errors += 1;
                    return Ok(SingleOutcome::Failed);
                }
            }
        }
        Ok(if refused.is_empty() {
            SingleOutcome::NotFoundInAnyOrg
        } else {
            SingleOutcome::ForbiddenInSomeOrg { refused, not_found }
        })
    }

    /// List every org's conversations, prune what a complete listing no
    /// longer names, then fetch what is listed and not held at the
    /// `updated_at` listed, the never-fetched first. Whether a rate
    /// limit ended it.
    async fn listed(
        &self,
        orgs: &[Value],
        since: Option<&DateTime<Utc>>,
        s: &mut FetchSummary,
    ) -> Result<bool> {
        let mut listings_by_org: Vec<(String, String, Vec<Value>)> = Vec::new();
        // A stub may belong to any org, so they are pruned only when every
        // org listed.
        let mut every_org_listed = true;
        let mut refused: Option<ClaudeError> = None;
        let mut failed: Option<String> = None;
        for org in orgs {
            let Some((org_uuid, org_name)) = org_identity(org) else {
                continue;
            };
            let listing = match self
                .client
                .list_conversations(org_uuid)
                .instrument(info_span!("claude_org_listing", org = %org_name))
                .await
            {
                Ok(l) => l,
                Err(_) if self.stop().requested() => return Ok(false),
                // Nothing is pruned on a run that listed nothing whole.
                Err(ClaudeError::RateLimited(reason)) => {
                    self.give_up("conversations", &reason);
                    return Ok(true);
                }
                Err(e @ ClaudeError::Forbidden(_)) => {
                    info!(
                        event = "claude_org_forbidden",
                        org = %org_name,
                        note = "no chat permission for this org",
                        "this org refuses conversation listings; skipping it"
                    );
                    self.forbidden_orgs
                        .lock()
                        .unwrap()
                        .insert(org_uuid.to_string(), org_name.clone());
                    every_org_listed = false;
                    refused = Some(e);
                    continue;
                }
                // Not pruned: it never reaches `listings_by_org`.
                Err(e) => {
                    self.found.listing(
                        &format!("conversations org:{org_uuid}"),
                        format!("org {org_name:?}: {e}"),
                    );
                    every_org_listed = false;
                    failed = Some(e.to_string());
                    continue;
                }
            };
            info!(
                event = "claude_org_listing_count",
                org = %org_name,
                count = listing.len(),
                "listed one org's conversations"
            );
            sleep(SLEEP_BETWEEN).await;
            listings_by_org.push((org_uuid.to_string(), org_name, listing));
        }

        // No org listed: the credential is not working (the org listing it
        // passed may be hours old), and that is not a partial sync.
        if listings_by_org.is_empty() {
            match (failed, refused) {
                (None, Some(e)) => {
                    return Err(
                        credential_hint(e).context("every org refused its conversation listing")
                    )
                }
                (Some(e), _) => {
                    anyhow::bail!("no org's conversations could be listed; the last: {e}")
                }
                (None, None) => {}
            }
        }

        let mut listed: Vec<Listed> = Vec::new();
        let mut overlap: HashSet<String> = HashSet::new();
        let mut org_of: HashMap<String, (String, String)> = HashMap::new();
        for (org_uuid, org_name, listing) in &listings_by_org {
            // `since` gates fetching only: rows already stored stay, so
            // moving it further back later lists the older conversations
            // as owed. Out-of-scope items are invisible to the overlap too.
            let mut in_scope: Vec<&Value> = Vec::new();
            for c in listing {
                if updated_at_in_scope(c.get("updated_at").and_then(Value::as_str), since) {
                    in_scope.push(c);
                } else {
                    s.out_of_scope += 1;
                }
            }
            let mut sorted: Vec<&Value> = in_scope.clone();
            sorted.sort_by(|a, b| {
                let ka = a.get("updated_at").and_then(Value::as_str).unwrap_or("");
                let kb = b.get("updated_at").and_then(Value::as_str).unwrap_or("");
                kb.cmp(ka)
            });
            for c in sorted.iter().take(self.opts.overlap) {
                if let Some(u) = c.get("uuid").and_then(Value::as_str) {
                    overlap.insert(u.into());
                }
            }
            for item in &in_scope {
                let Some(uuid) = item.get("uuid").and_then(Value::as_str) else {
                    continue;
                };
                let updated_at = item.get("updated_at").and_then(Value::as_str);
                listed.push(Listed::new(uuid, updated_at));
                org_of.insert(uuid.to_string(), (org_uuid.clone(), org_name.clone()));
            }

            // `/chat_conversations` returns this org's whole list in one
            // response, so a conversation we hold for this org that the
            // listing did not name has been deleted on claude.ai. The
            // pruning set is the *unfiltered* listing, not `in_scope`:
            // `since` narrows what we re-fetch, and treating what it
            // excluded as deleted would delete the entire archive older
            // than the cutoff. Only orgs that listed successfully reach
            // here, so a lost permission or a failed listing on one org
            // cannot be read as its conversations having been deleted.
            let keep: HashSet<String> = listing
                .iter()
                .filter_map(|c| c.get("uuid").and_then(Value::as_str))
                .map(String::from)
                .collect();
            s.pruned += self.db.prune_org_conversations(org_uuid, &keep).await?;
        }
        if every_org_listed {
            let keep: HashSet<String> = listings_by_org
                .iter()
                .flat_map(|(_, _, l)| l.iter())
                .filter_map(|c| c.get("uuid").and_then(Value::as_str))
                .map(String::from)
                .collect();
            s.pruned += self.db.prune_unlisted_stubs(&keep).await?;
        }

        let in_scope = listed.len();
        let mut owed = owed_missing_first(self.db.pool(), CONVERSATIONS, listed.clone()).await?;
        // The overlap is looked at every run, held or not.
        for l in listed {
            if overlap.contains(&l.key) && !owed.iter().any(|o| o.key == l.key) {
                owed.push(l);
            }
        }
        s.skipped = in_scope - owed.len();
        info!(
            event = "claude_priority_split",
            owed = owed.len(),
            up_to_date = s.skipped,
            out_of_scope = s.out_of_scope,
            "sorted the listings into what to fetch"
        );
        // The sum across all orgs, ticking once per chat, so a glance
        // answers "how close is the whole sync to done?".
        self.bar.expect(owed.len() as u64);
        let drained = self
            // One conversation per transaction: each is one slow request,
            // and a seal may follow every one, so a long first sync
            // reaches the grid as it goes.
            .drain(
                CONVERSATIONS,
                "conversations",
                owed,
                &Conversations { ctx: self, org_of },
                1,
            )
            .await?;
        s.fetched = drained.got;
        s.errors += drained.failed;
        Ok(drained.terminal.is_some())
    }

    /// Every attachment edge the store lists and does not hold at its
    /// conversation's version. Whether a rate limit ended it.
    async fn attachments(&self, s: &mut FetchSummary) -> Result<bool> {
        let listed = self.db.attachments_listed().await?;
        let owed = owed::owed(self.db.pool(), ATTACHMENTS, listed).await?;
        self.bar.doing("attachments");
        let drained = self
            .drain(ATTACHMENTS, "attachments", owed, &Files(self), 8)
            .await?;
        s.failed_blobs = drained.failed;
        Ok(drained.terminal.is_some())
    }
}

/// The orgs, the account, the projects, the conversations, then their
/// files. A rate limit anywhere ends the run's requests: the loop it
/// struck leaves a `phase:` row, and the `config:` rows stand, since not
/// every configured entry was checked.
async fn phases(ctx: &Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    let since = ctx
        .opts
        .since
        .as_deref()
        .map(parse_iso_or_utc_date)
        .transpose()
        .with_context(|| format!("sync.since {:?}", ctx.opts.since))?;
    let orgs = ctx.orgs().await?;
    let orgs = ctx.chat_orgs(orgs, s);
    ctx.users().await?;
    // Projects come before the conversation walk — the `conv_uuids` one
    // too — so a rename lands in the same run as the conversations that
    // dereference it through `project_name_by_uuid`.
    let mut rate_limited = false;
    if ctx.opts.projects {
        rate_limited = ctx.projects(&orgs, s).await?;
    }
    if !rate_limited {
        rate_limited = if ctx.opts.conv_uuids.is_empty() {
            ctx.listed(&orgs, since.as_ref(), s).await?
        } else {
            ctx.named(&orgs, s).await?
        };
    }
    if !rate_limited && !ctx.stop().requested() {
        rate_limited = ctx.attachments(s).await?;
    }
    ctx.found.extend(ctx.refused_orgs());
    if rate_limited {
        ctx.found.cut_short();
    } else {
        ctx.found
            .config(ctx.config_problems.lock().unwrap().clone());
    }
    Ok(())
}

/// Of `listed`, what `table` does not hold at the version listed: the
/// records never fetched first, then the stale, so a run cut short
/// spent its budget on new work.
async fn owed_missing_first(
    pool: &sqlx::SqlitePool,
    table: &str,
    listed: Vec<Listed>,
) -> Result<Vec<Listed>> {
    let held = owed::held_versions(pool, table, listed.iter().map(|l| l.key.as_str())).await?;
    let (missing, stale): (Vec<Listed>, Vec<Listed>) = listed
        .into_iter()
        .filter(|l| !held.get(&l.key).is_some_and(|h| h.satisfies(&l.version)))
        .partition(|l| !held.get(&l.key).is_some_and(|h| h.fetched));
    Ok(missing.into_iter().chain(stale).collect())
}

/// The projects of `rows` not already held at their `updated_at`, and
/// how many were.
fn not_yet_held(
    rows: Vec<ProjectUpsert>,
    held: &HashMap<String, Held>,
) -> (Vec<ProjectUpsert>, usize) {
    let mut skipped = 0;
    let keep: Vec<ProjectUpsert> = rows
        .into_iter()
        .filter(|r| {
            let satisfied = held
                .get(&r.uuid)
                .is_some_and(|h| h.satisfies(&r.updated_at));
            skipped += usize::from(satisfied);
            !satisfied
        })
        .collect();
    (keep, skipped)
}

pub(crate) fn canonicalize_project_payload(payload: &Value) -> Value {
    with_bag_sorted(payload, "permissions")
}

/// An org's `capabilities` is a set the API returns in no fixed order.
pub(crate) fn canonicalize_org_payload(payload: &Value) -> Value {
    with_bag_sorted(payload, "capabilities")
}

fn with_bag_sorted(payload: &Value, field: &str) -> Value {
    let mut out = payload.clone();
    if let Some(bag) = out.get_mut(field).and_then(Value::as_array_mut) {
        // Sort by the rendered string so mixed types (which would make
        // `as_str` sort unstable) still get a total order.
        bag.sort_by_key(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        });
    }
    out
}

/// Wrap the preflight's failure in setup instructions when it looks
/// like latchkey can't authenticate to claude.ai at all: the service
/// was never registered ("No service matches URL"), or the sessionKey
/// cookie is missing/expired (401/403). Anything else (network,
/// claude.ai outage) passes through unembellished.
pub fn credential_hint(e: ClaudeError) -> anyhow::Error {
    let s = e.to_string();
    // Cloudflare's challenge is a 403 too, and no credential gets past
    // it: a sign-in recipe would send the person after the wrong fix.
    if s.contains(r#"cf-mitigated=Some("challenge")"#) {
        return anyhow::anyhow!("claude.ai's bot protection blocked the request: {s}");
    }
    let setup_problem = s.contains("No service matches URL")
        || s.to_ascii_lowercase().contains("no credentials")
        || s.contains("HTTP 401")
        || s.contains("HTTP 403");
    if !setup_problem {
        return anyhow::anyhow!("list orgs: {s}");
    }
    let lk = datalib_etl_web::latchkey::latchkey_cli_hint();
    anyhow::anyhow!(
        "claude.ai credentials are not set up: {s}\n\
         The credential is the `sessionKey` cookie. Copy the one your\n\
         browser already has:\n\
         1. Register the service (once):\n\
              {lk} services register claude-ai --base-api-url=\"https://claude.ai/\"\n\
         2. Open https://claude.ai signed in; DevTools -> Application ->\n\
            Cookies -> claude.ai, copy the `sessionKey` value.\n\
         3. Store it (`$(pbpaste)` keeps the secret out of shell history):\n\
              {lk} auth set claude-ai -H \"Cookie: sessionKey=$(pbpaste)\"\n\
         4. Smoke-test:\n\
              {lk} curl -s https://claude.ai/api/organizations\n\
\n\
         Already set it and still seeing this? Check the stored value's\n\
         shape before re-pasting. Quoting `$(pbpaste)` in SINGLE quotes\n\
         stores the literal 10 characters `$(pbpaste)` — the shell never\n\
         expands it, and claude.ai answers `account_session_invalid`,\n\
         which reads exactly like an expired key. A real sessionKey\n\
         starts `sk-ant-sid01-` and is >100 chars. This prints only the\n\
         length, never the secret (the response is irrelevant, only the\n\
         request header `-v` echoes is read):\n\
              {lk} curl -v \\\n\
                https://claude.ai/api/organizations 2>&1 >/dev/null |\\\n\
                sed -n 's/.*sessionKey=\\([^;]*\\).*/\\1/p' |\\\n\
                awk '{{print \"sessionKey length: \" length($0)}}'\n\
\n\
         There is also a browser login — register with\n\
         `--login-url=\"https://claude.ai/login\" --login-flow=cookie-capture\n\
         --login-flow-params='{{\"cookieKeys\": [\"sessionKey\"]}}'`, then\n\
         `{lk} auth browser claude-ai`. It works, but it signs in a second\n\
         time, and claude.ai appears to invalidate the older session when it\n\
         does: observed 2026-08-31, the captured cookie and the browser you\n\
         normally use kept evicting each other, logging both out repeatedly.\n\
         Prefer the paste above until that is understood.\n\
         See docs/user/getting_your_data.md for the full walkthrough."
    )
}

/// Whether a targeted `conv_uuids` fetch found its conversation.
///
/// A separate outcome rather than an `Err`: a uuid no org will serve is
/// a config problem the caller reports and steps over, and a fetch that
/// failed is that conversation's own problem row; neither stops the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleOutcome {
    Fetched,
    /// Every org answered 404. The id is wrong, or the conversation is
    /// gone.
    NotFoundInAnyOrg,
    /// At least one org kept answering 403 after the transient-403
    /// retries. The conversation may well exist — this credential
    /// cannot read it — so it must not be reported as missing, which
    /// would send the reader hunting for a deleted chat. `refused`
    /// names those orgs; `not_found` counts the ones that said 404.
    ForbiddenInSomeOrg {
        refused: Vec<String>,
        not_found: usize,
    },
    /// The fetch failed otherwise; it is recorded on the conversation's
    /// bookkeeping row.
    Failed,
    /// Asked to stop before it was done; nothing of it was written.
    Stopped,
    /// The give-up guard tripped, with why; nothing of it was written.
    Cut(String),
}

/// Backoff delays for transient-403 retries on a single
/// `get_conversation`. claude.ai occasionally returns 403 on detail GETs
/// when listing+detail are issued in rapid succession; the same UUID
/// re-fetched a moment later typically returns 200. Verified by direct
/// probe: a UUID that 403'd inside a run returned 200 to a fresh
/// `latchkey curl` immediately after. We treat Forbidden as transient
/// here (not at the transport layer) so a real org-level permission
/// denial — caught earlier by `list_conversations` — still short-circuits
/// to `claude_org_forbidden`.
const FORBIDDEN_RETRY_BACKOFFS: &[Duration] = &[Duration::from_millis(500), Duration::from_secs(2)];

/// Outcome of a 403-retrying detail fetch. `retries` counts the
/// *additional* attempts after the first (so 0 = first try succeeded).
pub(crate) struct RetryOutcome {
    pub value: Value,
    pub retries: u32,
}

pub(crate) async fn get_conversation_with_403_retry(
    client: &ClaudeClient,
    org_uuid: &str,
    conv_uuid: &str,
) -> Result<RetryOutcome, (ClaudeError, u32)> {
    let mut last_err: Option<ClaudeError> = None;
    for (attempt, delay) in std::iter::once(None)
        .chain(FORBIDDEN_RETRY_BACKOFFS.iter().copied().map(Some))
        .enumerate()
    {
        if let Some(d) = delay {
            sleep(d).await;
        }
        match client.get_conversation(org_uuid, conv_uuid).await {
            Ok(v) => {
                if attempt > 0 {
                    info!(
                        event = "claude_fetch_403_retry_ok",
                        uuid = conv_uuid,
                        attempt = attempt,
                        "the retry after a 403 succeeded"
                    );
                }
                return Ok(RetryOutcome {
                    value: v,
                    retries: attempt as u32,
                });
            }
            Err(ClaudeError::Forbidden(msg)) => {
                info!(
                    event = "claude_fetch_403_transient",
                    uuid = conv_uuid,
                    attempt = attempt,
                    error = %msg,
                    "403 on a detail fetch; retrying in case it is transient",
                );
                last_err = Some(ClaudeError::Forbidden(msg));
            }
            Err(other) => {
                return Err((other, attempt as u32));
            }
        }
    }
    Err((
        last_err.expect("at least one attempt"),
        FORBIDDEN_RETRY_BACKOFFS.len() as u32,
    ))
}

/// claude.ai's replicas disagree about whether a field with no value
/// is spelled `key: null` or left out — the same untouched conversation
/// came back both ways five minutes apart, on `chat_messages[].content[]`
/// down to `display_content.link.*`. Every reader here treats the two
/// alike, so absent is the stored spelling. Array elements stay:
/// `[null, 1]` is positional.
pub(crate) fn canonicalize_conversation_payload(payload: &Value) -> Value {
    let mut out = payload.clone();
    drop_null_keys(&mut out);
    out
}

fn drop_null_keys(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, v| !v.is_null());
            m.values_mut().for_each(drop_null_keys);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_null_keys),
        _ => {}
    }
}

/// One conversation as the detail endpoint answered it, ready to store.
pub(crate) fn parse_conversation(
    org_uuid: &str,
    org_name: &str,
    uuid: &str,
    full: &Value,
) -> Result<Conversation> {
    let full = canonicalize_conversation_payload(full);
    Ok(Conversation {
        uuid: uuid.to_string(),
        org_uuid: org_uuid.to_string(),
        org_name: org_name.to_string(),
        name: full.get("name").and_then(Value::as_str).map(String::from),
        updated_at: full
            .get("updated_at")
            .and_then(Value::as_str)
            .map(String::from),
        files: db::files_of(&full),
        payload: serde_json::to_string(&full).context("serialize conversation")?,
    })
}

/// An org whose `capabilities` list is there and leaves out `chat` — an
/// API-console org — answers 403 to every chat and project request. One
/// with no list, or something other than a list, is walked as before.
fn lacks_chat(org: &Value) -> bool {
    org.get("capabilities")
        .and_then(Value::as_array)
        .is_some_and(|caps| !caps.iter().any(|c| c.as_str() == Some("chat")))
}

fn org_identity(org: &Value) -> Option<(&str, String)> {
    let uuid = org.get("uuid").and_then(Value::as_str)?;
    let name = match org.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => uuid.char_indices().take(8).map(|(_, c)| c).collect(),
    };
    Some((uuid, name))
}

async fn upsert_users(db: &RawDb, payloads: &[Value], now: &IsoOffsetTimestamp) -> Result<()> {
    let mut rows: Vec<UserRow> = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let Some(id) = payload.get("uuid").and_then(Value::as_str) else {
            continue;
        };
        rows.push(UserRow {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: serde_json::to_string(payload).context("serialize user")?,
            },
            email: payload
                .get("email_address")
                .and_then(Value::as_str)
                .map(String::from),
            full_name: payload
                .get("full_name")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool().begin().await.context("begin users tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, now).await?;
    tx.commit().await.context("commit users tx")
}

/// The export's `users.json`, `None` when there is none.
fn read_export_users(export_dir: &Path) -> Result<Option<Vec<Value>>> {
    let path = export_dir.join("users.json");
    if !path.exists() {
        return Ok(None);
    }
    let txt = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&txt).with_context(|| format!("parse {}", path.display()))?;
    Ok(v.as_array().cloned())
}

fn pick_user_fields(acct: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    for key in ["uuid", "email_address", "full_name"] {
        if let Some(v) = acct.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    Value::Object(obj)
}

fn parse_iso_or_utc_date(s: &str) -> Result<DateTime<Utc>> {
    let t = datalib_time::parse_strict(s)
        .or_else(|_| datalib_time::parse_yyyy_mm_dd_assumed_utc(s))
        .with_context(|| format!("expected RFC 3339 or YYYY-MM-DD, got {s:?}"))?;
    Ok(t.inner().with_timezone(&Utc))
}

fn updated_at_in_scope(updated_at: Option<&str>, since: Option<&DateTime<Utc>>) -> bool {
    let Some(since) = since else {
        return true;
    };
    let Some(s) = updated_at else {
        return true;
    };
    match datalib_time::parse_strict(s) {
        Ok(t) => t.inner().with_timezone(&Utc) >= *since,
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 403 from claude.ai reads as "expired key" but is equally the
    /// signature of a *malformed stored* key — observed 2026-08-31, where
    /// `auth set` had been run with `'$(pbpaste)'` in single quotes and
    /// latchkey had been sending those 10 literal characters as the cookie
    /// for hours. The hint has to name that case, because the symptom
    /// (`account_session_invalid`) points the other way and re-pasting a
    /// perfectly good browser key does not fix it.
    #[test]
    fn auth_hint_names_the_single_quote_trap_and_offers_a_shape_check() {
        let hint = credential_hint(ClaudeError::Forbidden("HTTP 403".into())).to_string();
        assert!(
            hint.contains("SINGLE quotes"),
            "hint must name the quoting trap; got:\n{hint}"
        );
        assert!(
            hint.contains("sk-ant-sid01-"),
            "hint must say what a real key looks like; got:\n{hint}"
        );
        // The shape check reads the request header back out of `-v`.
        assert!(
            hint.contains("curl -v"),
            "shape-check command must run curl verbosely; got:\n{hint}"
        );
        // ...and it must never suggest printing the secret itself.
        assert!(
            hint.contains("length"),
            "shape check must report length, not the value; got:\n{hint}"
        );
    }

    /// The embellishment is scoped: a claude.ai outage or a network blip is
    /// not a credentials problem, and dumping setup instructions on one
    /// sends you off fixing something that isn't broken.
    #[test]
    fn auth_hint_passes_through_non_setup_failures() {
        let hint =
            credential_hint(ClaudeError::Permanent("HTTP 502 bad gateway".into())).to_string();
        assert_eq!(hint, "list orgs: HTTP 502 bad gateway");
    }

    /// Cloudflare's challenge is a 403 that no credential gets past; the
    /// sign-in recipe would send the person after the wrong fix.
    #[test]
    fn a_cloudflare_challenge_gets_no_sign_in_recipe() {
        let hint = credential_hint(ClaudeError::Forbidden(
            r#"GET /account -> HTTP 403 cf-mitigated=Some("challenge")"#.into(),
        ))
        .to_string();
        assert!(hint.contains("bot protection"), "{hint}");
        assert!(!hint.contains("sessionKey"), "{hint}");
    }

    #[test]
    fn since_parses_date_and_rfc3339() {
        let d = parse_iso_or_utc_date("2026-01-15").unwrap();
        assert_eq!(d.to_rfc3339(), "2026-01-15T00:00:00+00:00");
        let t = parse_iso_or_utc_date("2026-01-15T12:30:00Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-01-15T12:30:00+00:00");
        assert!(parse_iso_or_utc_date("not-a-date").is_err());
    }

    #[test]
    fn no_since_means_everything_in_scope() {
        assert!(updated_at_in_scope(Some("2001-01-01T00:00:00Z"), None));
        assert!(updated_at_in_scope(None, None));
    }

    #[test]
    fn since_boundary_is_inclusive() {
        let since = parse_iso_or_utc_date("2026-01-15").unwrap();
        assert!(updated_at_in_scope(
            Some("2026-01-15T00:00:00Z"),
            Some(&since)
        ));
        assert!(updated_at_in_scope(
            Some("2026-02-01T09:00:00+00:00"),
            Some(&since)
        ));
        assert!(!updated_at_in_scope(
            Some("2026-01-14T23:59:59Z"),
            Some(&since)
        ));
    }

    #[test]
    fn since_respects_offsets_and_tolerates_garbage() {
        let since = parse_iso_or_utc_date("2026-01-15").unwrap();
        // 2026-01-15T01:00:00+02:00 is 2026-01-14T23:00:00Z — out.
        assert!(!updated_at_in_scope(
            Some("2026-01-15T01:00:00+02:00"),
            Some(&since)
        ));
        // Missing/garbage updated_at stays in scope (fetch, don't drop).
        assert!(updated_at_in_scope(None, Some(&since)));
        assert!(updated_at_in_scope(Some("garbage"), Some(&since)));
    }

    /// A project held at the `updated_at` listed is not written again;
    /// one held at an older stamp, or never, is.
    #[test]
    fn a_project_held_at_its_stamp_is_not_written_again() {
        let project = |uuid: &str, updated_at: &str| ProjectUpsert {
            uuid: uuid.into(),
            org_uuid: "org-a".into(),
            org_name: "A".into(),
            name: None,
            updated_at: Some(updated_at.into()),
            payload: "{}".into(),
        };
        let held: HashMap<String, Held> = [
            (
                "p-1".to_string(),
                Held {
                    fetched: true,
                    version: Some("T2".into()),
                },
            ),
            (
                "p-2".to_string(),
                Held {
                    fetched: true,
                    version: Some("T1".into()),
                },
            ),
        ]
        .into();
        let (write, skipped) = not_yet_held(
            vec![
                project("p-1", "T2"),
                project("p-2", "T2"),
                project("p-3", "T1"),
            ],
            &held,
        );
        let ids: Vec<&str> = write.iter().map(|p| p.uuid.as_str()).collect();
        assert_eq!(ids, ["p-2", "p-3"]);
        assert_eq!(skipped, 1);
    }

    /// An API-console org 403s every chat listing; it read as a refused
    /// work org. Only a capabilities list that is there and lacks `chat`
    /// skips an org: one with no list, or a malformed one, is walked.
    #[test]
    fn only_an_org_whose_capabilities_lack_chat_is_skipped() {
        let caps = |v: Value| json!({"uuid": "o1", "name": "Starfleet", "capabilities": v});
        assert!(lacks_chat(&caps(json!(["api", "customer_terms:standard"]))));
        assert!(lacks_chat(&caps(json!([]))));
        assert!(!lacks_chat(&caps(json!(["chat", "raven"]))));
        assert!(!lacks_chat(&json!({"uuid": "o1", "name": "Starfleet"})));
        assert!(!lacks_chat(&caps(json!("api"))));
        assert!(!lacks_chat(&caps(Value::Null)));
    }

    // ── unordered bags from the API ──────────────────────────────────

    /// Two fetches of an unchanged project must serialize identically.
    #[test]
    fn project_permissions_are_stored_in_a_stable_order() {
        let one = serde_json::json!({
            "uuid": "p1",
            "permissions": ["chat_project:view", "chat_project:chat:create"],
        });
        let other_order = serde_json::json!({
            "uuid": "p1",
            "permissions": ["chat_project:chat:create", "chat_project:view"],
        });
        assert_eq!(
            canonicalize_project_payload(&one),
            canonicalize_project_payload(&other_order),
            "the same permissions in a different order must canonicalize the same"
        );
    }

    /// Sorting, not dropping: losing a permission is a real change and
    /// must still show up as one.
    #[test]
    fn project_permissions_keep_their_contents() {
        let full = serde_json::json!({"permissions": ["b", "a", "c"]});
        let fewer = serde_json::json!({"permissions": ["a", "b"]});
        assert_eq!(
            canonicalize_project_payload(&full)["permissions"],
            serde_json::json!(["a", "b", "c"]),
            "every permission is kept, in sorted order"
        );
        assert_ne!(
            canonicalize_project_payload(&full),
            canonicalize_project_payload(&fewer),
            "a removed permission must still read as a change"
        );
    }

    /// A project without the field, or with something unexpected in it,
    /// passes through rather than panicking — this runs on every project
    /// of every sync.
    #[test]
    fn canonicalize_tolerates_a_missing_or_odd_permissions_field() {
        let none = serde_json::json!({"uuid": "p1"});
        assert_eq!(canonicalize_project_payload(&none), none);
        let odd = serde_json::json!({"permissions": "not-an-array"});
        assert_eq!(canonicalize_project_payload(&odd), odd);
        let mixed = serde_json::json!({"permissions": [2, "a", 1]});
        assert_eq!(
            canonicalize_project_payload(&mixed)["permissions"],
            serde_json::json!([1, 2, "a"]),
            "mixed types still get a total order rather than panicking"
        );
    }

    /// An org's capabilities arrive in no fixed order, and an unchanged
    /// org must serialize identically.
    #[test]
    fn org_capabilities_are_stored_in_a_stable_order() {
        let one = serde_json::json!({"uuid": "o1", "capabilities": ["warp", "bridge"]});
        let other_order = serde_json::json!({"uuid": "o1", "capabilities": ["bridge", "warp"]});
        assert_eq!(
            canonicalize_org_payload(&one),
            canonicalize_org_payload(&other_order)
        );
        assert_eq!(
            canonicalize_org_payload(&one)["capabilities"],
            serde_json::json!(["bridge", "warp"])
        );
    }

    // ── null-vs-absent from the API ──────────────────────────────────

    /// The two spellings claude.ai used for the same untouched
    /// conversation on the 2026-09-18 bake: one replica sent every
    /// no-value field of a content block as `null`, the other left it
    /// out. `stop_timestamp: null` is the deliberate kind — an in-flight
    /// message — and reads the same either way.
    fn conversation_with_nulls() -> Value {
        json!({
            "uuid": "e2afda7d-3b67-42a7-90e6-e84017fee652",
            "name": "Warp core diagnostics",
            "updated_at": "2026-09-18T10:00:00Z",
            "chat_messages": [{
                "uuid": "m1",
                "sender": "assistant",
                "stop_timestamp": null,
                "content": [{
                    "type": "text",
                    "text": "Running level-3 diagnostic.",
                    "flags": null,
                    "alternative_display_type": null,
                    "approval_key": null,
                    "approval_options": null,
                    "context": null,
                    "integration_icon_url": null,
                    "integration_name": null,
                    "is_mcp_app": null,
                    "mcp_server_url": null,
                    "message": null,
                    "meta": null,
                    "structured_content": null,
                    "display_content": {
                        "type": "link",
                        "link": {"url": "https://x.test", "resource_type": null, "subtitles": null}
                    },
                    "citations": [null, {"uuid": "cite-1"}]
                }]
            }]
        })
    }

    fn conversation_without_nulls() -> Value {
        json!({
            "uuid": "e2afda7d-3b67-42a7-90e6-e84017fee652",
            "name": "Warp core diagnostics",
            "updated_at": "2026-09-18T10:00:00Z",
            "chat_messages": [{
                "uuid": "m1",
                "sender": "assistant",
                "content": [{
                    "type": "text",
                    "text": "Running level-3 diagnostic.",
                    "display_content": {
                        "type": "link",
                        "link": {"url": "https://x.test"}
                    },
                    "citations": [null, {"uuid": "cite-1"}]
                }]
            }]
        })
    }

    #[test]
    fn null_and_absent_canonicalize_the_same() {
        assert_eq!(
            canonicalize_conversation_payload(&conversation_with_nulls()),
            canonicalize_conversation_payload(&conversation_without_nulls()),
            "a key present as null and a key left out must store identically"
        );
        assert_eq!(
            canonicalize_conversation_payload(&conversation_without_nulls()),
            conversation_without_nulls(),
            "a payload with no nulls is stored as-is"
        );
    }

    /// Only object keys go. A null *element* is positional, and a real
    /// value in place of the null is still a change.
    #[test]
    fn canonicalize_keeps_array_nulls_and_real_values() {
        let out = canonicalize_conversation_payload(&conversation_with_nulls());
        assert_eq!(
            out["chat_messages"][0]["content"][0]["citations"],
            json!([null, {"uuid": "cite-1"}])
        );
        let mut valued = conversation_without_nulls();
        valued["chat_messages"][0]["content"][0]["message"] = json!("approved");
        assert_ne!(
            canonicalize_conversation_payload(&valued),
            canonicalize_conversation_payload(&conversation_with_nulls()),
            "a field that gained a value must still read as a change"
        );
    }

    /// The bake's symptom, end to end: two fetches of an unchanged
    /// conversation, one spelling each, must leave one row and no
    /// `modified` delta between the commit and the working set.
    #[tokio::test]
    async fn refetch_with_the_other_null_spelling_is_not_a_modification() {
        use datalib_etl::doltlite_raw::commit_run;

        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        let uuid = "e2afda7d-3b67-42a7-90e6-e84017fee652";
        for (k, v) in [
            ("user.name", "null-test"),
            ("user.email", "null-test@datalib.local"),
        ] {
            sqlx::query("SELECT dolt_config(?, ?)")
                .bind(k)
                .bind(v)
                .execute(db.pool())
                .await
                .unwrap();
        }
        let db = &db;
        let store = |full: Value| async move {
            let c = parse_conversation("org-a", "A", uuid, &full).unwrap();
            let mut tx = db.pool().begin().await.unwrap();
            db.store_conversation(&mut tx, &c).await.unwrap();
            tx.commit().await.unwrap();
        };

        store(conversation_with_nulls()).await;
        let head = commit_run(db.pool(), "first fetch").await.unwrap();
        store(conversation_without_nulls()).await;

        let stored: Vec<String> = sqlx::query_scalar("SELECT json(payload) FROM conversations")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(stored.len(), 1, "one row for one conversation");
        assert_eq!(
            serde_json::from_str::<Value>(&stored[0]).unwrap(),
            canonicalize_conversation_payload(&conversation_without_nulls())
        );

        let head = head.expect("doltlite is linked into this test binary");
        let modified: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM dolt_diff_conversations \
              WHERE from_ref = ? AND to_ref = 'WORKING'",
        )
        .bind(head)
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            modified, 0,
            "the second spelling of the same conversation must not dirty the row"
        );
        db.close().await;
    }
}
