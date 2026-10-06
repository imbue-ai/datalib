//! Claude (claude.ai) downloader entry point — the `claude_api`
//! source type's ingest wave. Port of `src/ingest/claude_web.py`.

pub mod api;
pub mod db;
pub mod export;
pub mod normalize;
pub mod schema_raw;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use datalib_etl::blob_cas::CasEdgeAccumulator;
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::download_problems::{DownloadProblem, RunProblem};
use datalib_etl::download_run::DownloadRun;
use datalib_etl::http::{latchkey_curl, HttpError, HttpRequest, HttpService, LatchkeySettings};
use datalib_etl::progress::RunBar;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_problems::Reason;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

pub use api::{ClaudeClient, ClaudeError};
use datalib_etl::blob_cas::CasEdgeRow as _;
pub use db::{db_path_for, LoadedConversation, LoadedRaw, RawDb};
use schema_raw::{
    ConversationAttachmentRow, ConversationRow as ConversationRowSchema, OrgRow, ProjectDocRow,
    ProjectRow, UserRow,
};

pub const SLEEP_BETWEEN: Duration = Duration::from_millis(400);
pub const DEFAULT_OVERLAP: usize = 3;
const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);
const CLAUDE_ORIGIN: &str = "https://claude.ai";

/// How long a completed `/organizations` listing stays good. Matches
/// slack's `MANIFEST_TTL`, and for the same reason: the org set is
/// near-static, so re-listing it on every download is pure waste.
pub const ORGS_TTL: chrono::Duration = chrono::Duration::hours(6);
const ORGS_SWEEP_KEY: &str = "orgs";

/// How long a project's knowledge-doc listing stays good.
pub const PROJECT_DOCS_TTL: chrono::Duration = chrono::Duration::hours(24);

fn project_docs_sweep_key(project_uuid: &str) -> String {
    format!("project_docs:{project_uuid}")
}

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
    pub overlap: usize,
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
    /// Ignored in `conv_uuids` mode, which skips the listing walk.
    pub projects: bool,
    /// When non-empty, mirror only these project UUIDs. The per-org
    /// listing still runs; everything outside the set is skipped.
    pub project_uuids: Vec<String>,
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
    /// Seals what has been written so far, so render can start on the early
    /// conversations while the rest are still arriving. `None` -- the
    /// default, and what every test uses -- commits once at the end.
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
    /// Configured `conv_uuids` no org has. Reported rather than fatal:
    /// one dead link costs that conversation, not the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<DownloadProblem>,
    pub fetched: usize,
    pub skipped: usize,
    /// Listing items ignored because their `updated_at` predates the
    /// configured `since`. Not counted in `skipped` (which means
    /// "in scope and already up to date") or `total`.
    pub out_of_scope: usize,
    pub forbidden_orgs: usize,
    /// Conversations an org's complete listing did not name — deleted on
    /// claude.ai. Never counts rows a `claude_export` ingest wrote (those
    /// carry a NULL `org_uuid` and are out of an API sync's scope).
    pub pruned: usize,
    /// Fetch failures across both walks — conversations and projects.
    pub errors: usize,
    pub total: usize,
    /// Projects whose metadata row was written this run.
    pub projects_fetched: usize,
    /// Projects whose metadata was already current (no `updated_at`
    /// change) — counted separately from conversations' `skipped`.
    pub projects_skipped: usize,
    /// Knowledge documents written this run, across every project.
    pub project_docs_fetched: usize,
    /// Projects whose docs listing was served by a fresh sweep marker
    /// instead of a request.
    pub project_docs_skipped: usize,
    pub new_blobs: usize,
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
    let _ = datalib_etl::latchkey::ensure_curl_router();
    let db = opts.db.clone();

    let since = opts
        .since
        .as_deref()
        .map(parse_iso_or_utc_date)
        .transpose()
        .with_context(|| format!("sync.since {:?}", opts.since))?;

    let run_config = json!({
        "overlap": opts.overlap,
        "since": opts.since,
        "conv_uuids": opts.conv_uuids,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let mut client = ClaudeClient::with_latchkey(opts.latchkey.clone());
    let mut summary = FetchSummary::default();
    // One `now` per fetch — threaded into every bulk upsert so all
    // `<table>_bookkeeping.fetched_at_utc` stamps from a single sync share
    // a timestamp.
    let now = IsoOffsetTimestamp::now_local();
    let run_now = match &opts.now {
        Some(s) => datalib_time::parse_strict(s).context("--now")?,
        None => now.clone(),
    };
    // Run-scoped `(file_uuid → blake3)` cache, loaded once up-front
    // so the per-file dedupe check inside `fetch_files` is a
    // HashMap hit instead of a SQLite round trip. Successful
    // downloads insert into it.
    let mut blake3_by_file = db.load_attachment_blake3s().await?;

    // Nothing is known before the org listing returns, so the bar
    // starts with no total rather than a guess.
    let bar = RunBar::new(&opts.progress, 0);

    let work = async {
        let stop = &opts.control.stop;
        // The org listing goes first on purpose: it doubles as an
        // explicit credential preflight, so a missing latchkey service
        // registration or a dead sessionKey fails the run right here
        // with setup instructions — instead of first emitting a
        // misleading account-fetch warning and then dying on a cryptic
        // curl error.
        let cached_orgs = match db.sweep_age(ORGS_SWEEP_KEY, &run_now).await? {
            Some(age) if age < ORGS_TTL => {
                let orgs = db.load_orgs().await?;
                // An empty `orgs` table with a fresh marker shouldn't
                // silently yield zero orgs (that would skip every
                // conversation); fall through to the live call.
                if orgs.is_empty() {
                    None
                } else {
                    info!(
                        event = "claude_orgs_skipped",
                        reason = "ttl",
                        age_s = age.num_seconds().max(0),
                        ttl_s = ORGS_TTL.num_seconds(),
                        count = orgs.len(),
                        "the org listing is fresh enough; not re-listing"
                    );
                    Some(orgs)
                }
            }
            _ => None,
        };

        let orgs = match cached_orgs {
            Some(orgs) => orgs,
            None => {
                let orgs = client.list_orgs().await.map_err(credential_hint)?;
                info!(
                    event = "claude_orgs",
                    count = orgs.len(),
                    "listed the orgs this credential can see"
                );
                upsert_orgs(&db, &orgs, &now).await?;
                // Only once the rows are stored, so a failed upsert never
                // leaves a fresh marker over an empty table.
                db.record_sweep(ORGS_SWEEP_KEY, &run_now).await?;
                orgs
            }
        };

        let mut walk = Walk::new(found);

        // users.json from the bulk export carries the account.uuid we
        // need on every conversation. If the DB doesn't have any user
        // yet, try to pull it from the export dir before falling back
        // to `/api/account`.
        if !db.has_any_user().await? {
            if let Some(export_dir) = opts.export_dir.as_deref() {
                match read_export_users(export_dir) {
                    Ok(Some(users)) => upsert_users(&db, &users, &now).await?,
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
            match client.current_account().await {
                Ok(acct) => {
                    upsert_users(&db, &[pick_user_fields(&acct)], &now).await?;
                    info!(
                        event = "claude_users_synthesized",
                        "synthesized the users from the account"
                    );
                }
                // Asked again next run, since there is still no user.
                Err(e) => walk.found.phase("account", e.to_string()),
            }
        }

        // Projects come before the conversation walk — including the
        // targeted `conv_uuids` walk below — so a rename lands in the
        // same run as the conversations that dereference it. Scoping to
        // specific conversations does not mean wanting their project
        // labels stale: a conversation resolves its `project` grid
        // column through `project_name_by_uuid`, and with no projects
        // mirrored that column falls back to a bare UUID.
        if opts.projects {
            sync_projects(
                &mut client,
                &db,
                &orgs,
                &opts.project_uuids,
                &mut summary,
                &bar,
                &now,
                &run_now,
                stop,
                &mut walk,
            )
            .await?;
        }

        if walk.rate_limited {
            // Every request now would be refused the same way.
        } else if !opts.conv_uuids.is_empty() {
            bar.expect(opts.conv_uuids.len() as u64);
            for raw in &opts.conv_uuids {
                if stop.requested() {
                    break;
                }
                bar.did(1);
                bar.doing(raw);
                let target = datalib_etl::ids::normalize_id_token(raw);
                let outcome = fetch_single(
                    &mut client,
                    &db,
                    &orgs,
                    &target,
                    &mut summary,
                    &mut blake3_by_file,
                    &now,
                    stop,
                    &mut walk.forbidden_orgs,
                )
                .await?;
                match outcome {
                    SingleOutcome::Fetched => {
                        walk.attachments_tried.insert(target);
                    }
                    // Its own `conversations:<id>` row says why.
                    SingleOutcome::Failed => {}
                    SingleOutcome::Stopped => break,
                    SingleOutcome::Cut(reason) => {
                        walk.give_up("conversations", &reason);
                        break;
                    }
                    SingleOutcome::NotFoundInAnyOrg => {
                        summary.problems.push(DownloadProblem::not_found(
                            "conv_uuids",
                            raw,
                            format!(
                                "no conversation with this id in any of {} org(s)",
                                orgs.len()
                            ),
                        ));
                    }
                    SingleOutcome::ForbiddenInSomeOrg { refused, not_found } => {
                        summary.problems.push(DownloadProblem::forbidden(
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
        } else {
            walk_listings(
                &mut client,
                &db,
                &opts,
                &orgs,
                since.as_ref(),
                &mut summary,
                &mut blake3_by_file,
                &bar,
                &now,
                &mut walk,
            )
            .await?;
        }

        if !stop.requested() && !walk.rate_limited {
            retry_attachments(&db, &opts, &mut summary, &mut blake3_by_file, &mut walk).await?;
        }
        walk.found.extend(walk.refused_orgs());
        // A rate limit may have left configured entries unchecked, so
        // their `config:` rows stand, and it ended the run before every
        // listing and phase was tried.
        if walk.rate_limited {
            walk.found.cut_short();
        } else {
            walk.found.config(summary.problems.clone());
        }
        Ok::<(), anyhow::Error>(())
    };

    let result = work.await;
    bar.finish();
    summary.total = summary.fetched + summary.skipped;
    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// What one run could not do, and what the retry pass needs to know
/// about the walk.
struct Walk {
    found: RunProblems,
    /// An org that refuses one request refuses them all: once it has
    /// answered 403 past the transient retries it is not asked again
    /// this run. The listing walk learns this from `list_conversations`;
    /// the one-by-one path has no listing, so it learns it from the
    /// detail fetch.
    forbidden_orgs: BTreeMap<String, String>,
    /// Conversations whose attachments the walk already tried this run.
    attachments_tried: HashSet<String>,
    /// The shared give-up guard tripped; every further request would be
    /// refused the same way, so the run does no more of them.
    rate_limited: bool,
}

impl Walk {
    fn new(found: RunProblems) -> Self {
        Self {
            found,
            forbidden_orgs: BTreeMap::new(),
            attachments_tried: HashSet::new(),
            rate_limited: false,
        }
    }

    fn give_up(&mut self, at: &str, reason: &str) {
        self.rate_limited = true;
        self.found.phase(
            at,
            format!("stopped at the rate limit; the rest is left for the next run: {reason}"),
        );
    }

    /// One row per org this credential cannot read, keyed
    /// `listing:org:<name>`, so the Manage row says why a whole org is
    /// missing.
    fn refused_orgs(&self) -> Vec<RunProblem> {
        let org = |name: &String| {
            RunProblem::forbidden(
                &format!("org:{name}"),
                "this org refuses the credential's requests (conversations and projects); \
                 nothing from it is mirrored",
            )
        };
        self.forbidden_orgs.values().map(org).collect()
    }
}

/// List every org's conversations, prune what a complete listing no
/// longer names, then fetch what is missing or stale.
#[allow(clippy::too_many_arguments)]
async fn walk_listings(
    client: &mut ClaudeClient,
    db: &RawDb,
    opts: &FetchOptions,
    orgs: &[Value],
    since: Option<&DateTime<Utc>>,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    bar: &RunBar,
    now: &IsoOffsetTimestamp,
    walk: &mut Walk,
) -> Result<()> {
    let stop = &opts.control.stop;
    // Pass 1: list every org, classify. Collect the per-org fetch
    // plans so we know the total work up front and can set the
    // progress bar's length exactly once — otherwise a length
    // reset per org makes the bar jump backwards (e.g. `77/58`
    // when the second org's length is smaller than the count
    // already accumulated from the first).
    struct OrgPlan<'a> {
        org_uuid: String,
        org_name: String,
        ordered: Vec<&'a Value>,
    }
    let mut plans: Vec<OrgPlan> = Vec::new();
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
        let listing = match client
            .list_conversations(org_uuid)
            .instrument(info_span!("claude_org_listing", org = %org_name))
            .await
        {
            Ok(l) => l,
            Err(_) if stop.requested() => return Ok(()),
            // Nothing is pruned on a run that listed nothing whole.
            Err(ClaudeError::RateLimited(reason)) => {
                walk.give_up("conversations", &reason);
                return Ok(());
            }
            Err(e @ ClaudeError::Forbidden(_)) => {
                info!(
                    event = "claude_org_forbidden",
                    org = %org_name,
                    note = "no chat permission for this org",
                    "this org refuses conversation listings; skipping it"
                );
                summary.forbidden_orgs += 1;
                walk.forbidden_orgs
                    .insert(org_uuid.to_string(), org_name.clone());
                every_org_listed = false;
                refused = Some(e);
                continue;
            }
            // Not pruned: it never reaches `listings_by_org`.
            Err(e) => {
                walk.found
                    .listing(&format!("conversations org:{org_name}"), e.to_string());
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
                return Err(credential_hint(e).context("every org refused its conversation listing"))
            }
            (Some(e), _) => anyhow::bail!("no org's conversations could be listed; the last: {e}"),
            (None, None) => {}
        }
    }

    for (org_uuid, org_name, listing) in &listings_by_org {
        // `since` scope filter comes first: out-of-scope items are
        // invisible to overlap selection and classification alike,
        // so they are never detail-fetched. The filter only gates
        // fetching — rows already in the DB are left untouched —
        // and moving `since` further back later backfills the
        // newly-in-scope conversations as `missing` on that run.
        let mut in_scope: Vec<&Value> = Vec::new();
        let mut out_of_scope: usize = 0;
        for c in listing {
            if updated_at_in_scope(c.get("updated_at").and_then(|v| v.as_str()), since) {
                in_scope.push(c);
            } else {
                out_of_scope += 1;
            }
        }
        summary.out_of_scope += out_of_scope;

        let mut missing: Vec<&Value> = Vec::new();
        let mut stale: Vec<&Value> = Vec::new();
        let mut overlap_force: HashSet<String> = HashSet::new();
        {
            let mut sorted: Vec<&Value> = in_scope.clone();
            sorted.sort_by(|a, b| {
                let ka = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                let kb = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                kb.cmp(ka)
            });
            for c in sorted.iter().take(opts.overlap) {
                if let Some(u) = c.get("uuid").and_then(|v| v.as_str()) {
                    overlap_force.insert(u.into());
                }
            }
        }
        let listed_ids: Vec<&str> = in_scope
            .iter()
            .filter_map(|c| c.get("uuid").and_then(|v| v.as_str()))
            .collect();
        let existing = db.existing_updated_at(&listed_ids).await?;
        let mut up_to_date: usize = 0;
        for &item in &in_scope {
            let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                continue;
            };
            let api_updated = item
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match existing.get(uuid) {
                Some(stored) if !overlap_force.contains(uuid) => {
                    if stored.as_str() == api_updated {
                        up_to_date += 1;
                    } else {
                        stale.push(item);
                    }
                }
                Some(_) => stale.push(item),
                None => missing.push(item),
            }
        }
        info!(
            event = "claude_priority_split",
            org = %org_name,
            missing = missing.len(),
            stale = stale.len(),
            up_to_date = up_to_date,
            out_of_scope = out_of_scope,
            "sorted one org's listing into what to fetch"
        );
        summary.skipped += up_to_date;

        let ordered: Vec<&Value> = missing.into_iter().chain(stale).collect();
        plans.push(OrgPlan {
            org_uuid: org_uuid.clone(),
            org_name: org_name.clone(),
            ordered,
        });

        // `/chat_conversations` returns this org's whole list in one
        // response, so a conversation we hold for this org that the
        // listing did not name has been deleted on claude.ai. The
        // pruning set is the *unfiltered* listing, not `in_scope`:
        // `since` narrows what we re-fetch, and treating what it
        // excluded as deleted would delete the entire archive older
        // than the cutoff.
        //
        // Only orgs that listed successfully reach here, so a lost
        // permission or a failed listing on one org cannot be read as
        // its conversations having been deleted.
        let listed: HashSet<String> = listing
            .iter()
            .filter_map(|c| c.get("uuid").and_then(|v| v.as_str()))
            .map(String::from)
            .collect();
        summary.pruned += db.prune_org_conversations(org_uuid, &listed).await?;
    }
    if every_org_listed {
        let listed: HashSet<String> = listings_by_org
            .iter()
            .flat_map(|(_, _, l)| l.iter())
            .filter_map(|c| c.get("uuid").and_then(|v| v.as_str()))
            .map(String::from)
            .collect();
        summary.pruned += db.prune_unlisted_stubs(&listed).await?;
    }

    // Pass 2: fetch. The sum across all orgs, ticking once per chat,
    // so a glance answers "how close is the whole sync to done?".
    // The org is the bar's message, not a bar of its own — that is
    // the backwards jump Pass 1 above is written to avoid.
    let total: usize = plans.iter().map(|p| p.ordered.len()).sum();
    bar.expect(total as u64);
    'orgs: for plan in &plans {
        for item in &plan.ordered {
            // Asked to stop: the conversation that just landed sealed
            // with its blobs, so end here.
            if stop.requested() {
                info!(event = "claude_interrupted", org = %plan.org_name, "told to stop; leaving the rest of this org for the next run");
                break 'orgs;
            }
            let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                continue;
            };
            bar.did(1);
            bar.doing(&format!("{} {uuid}", plan.org_name));
            match get_conversation_with_403_retry(client, &plan.org_uuid, uuid).await {
                Ok(outcome) => {
                    summary.forbidden_retry_attempts += outcome.retries as u64;
                    if outcome.retries > 0 {
                        summary.forbidden_retry_recoveries += 1;
                    }
                    let saved = save_with_files(
                        db,
                        &plan.org_uuid,
                        &plan.org_name,
                        uuid,
                        &outcome.value,
                        summary,
                        blake3_by_file,
                        now,
                        stop,
                    )
                    .await?;
                    match saved {
                        None => {}
                        Some(Cut::Stopped) => break 'orgs,
                        Some(Cut::RateLimited(reason)) => {
                            walk.give_up("attachments", &reason);
                            break 'orgs;
                        }
                    }
                    walk.attachments_tried.insert(uuid.to_string());
                    if let Some(sealer) = opts.sealer.as_ref() {
                        sealer.wrote(1).await;
                    }
                    if opts.sleep_between > Duration::ZERO {
                        sleep(opts.sleep_between).await;
                    }
                }
                Err((_, retries)) if stop.requested() => {
                    summary.forbidden_retry_attempts += retries as u64;
                    break 'orgs;
                }
                Err((ClaudeError::RateLimited(reason), retries)) => {
                    summary.forbidden_retry_attempts += retries as u64;
                    walk.give_up("conversations", &reason);
                    break 'orgs;
                }
                Err((e, retries)) => {
                    summary.forbidden_retry_attempts += retries as u64;
                    warn!(event = "claude_fetch_error", uuid = uuid, error = %e, "a conversation could not be fetched");
                    db.record_conversation_error(uuid, &e.to_string()).await?;
                    summary.errors += 1;
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn sync_projects(
    client: &mut ClaudeClient,
    db: &RawDb,
    orgs: &[Value],
    project_uuids: &[String],
    summary: &mut FetchSummary,
    bar: &RunBar,
    now: &IsoOffsetTimestamp,
    run_now: &IsoOffsetTimestamp,
    stop: &StopFlag,
    walk: &mut Walk,
) -> Result<()> {
    // Normalized → as configured, so a miss is reported the way the
    // config spelled it.
    let only: HashMap<String, &str> = project_uuids
        .iter()
        .map(|s| (datalib_etl::ids::normalize_id_token(s), s.as_str()))
        .collect();
    // Track which requested UUIDs we actually saw, so a typo doesn't
    // silently mirror nothing.
    let mut matched: HashSet<&str> = HashSet::new();
    let mut unlisted_orgs = 0usize;

    for org in orgs {
        let Some((org_uuid, org_name)) = org_identity(org) else {
            continue;
        };
        let listing = match client
            .list_projects(org_uuid)
            .instrument(info_span!("claude_project_listing", org = %org_name))
            .await
        {
            Ok(l) => l,
            Err(_) if stop.requested() => return Ok(()),
            Err(ClaudeError::RateLimited(reason)) => {
                walk.give_up("projects", &reason);
                return Ok(());
            }
            Err(ClaudeError::Forbidden(_)) => {
                info!(
                    event = "claude_projects_forbidden",
                    org = %org_name,
                    "this org refuses project listings; its conversations are not \
                     asked for one by one either"
                );
                walk.forbidden_orgs
                    .insert(org_uuid.to_string(), org_name.clone());
                unlisted_orgs += 1;
                continue;
            }
            Err(e) => {
                walk.found
                    .listing(&format!("projects org:{org_name}"), e.to_string());
                summary.errors += 1;
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

        // Narrow before the skip-check so a filtered run doesn't even
        // read rows it will never touch.
        let listing: Vec<Value> = if only.is_empty() {
            listing
        } else {
            listing
                .into_iter()
                .filter(|p| {
                    p.get("uuid")
                        .and_then(|v| v.as_str())
                        .is_some_and(|u| only.contains_key(u))
                })
                .collect()
        };
        for p in &listing {
            if let Some(u) = p.get("uuid").and_then(|v| v.as_str()) {
                // Borrow from `only`, not from the listing, so the set
                // outlives this iteration.
                if let Some((k, _)) = only.get_key_value(u) {
                    matched.insert(k.as_str());
                }
            }
        }
        if listing.is_empty() {
            continue;
        }

        let listed_ids: Vec<&str> = listing
            .iter()
            .filter_map(|p| p.get("uuid").and_then(|v| v.as_str()))
            .collect();
        let existing = db.existing_project_updated_at(&listed_ids).await?;

        // `Projects/list` is what first says how many there are, so the
        // number arrives one org at a time.
        bar.expect(listing.len() as u64);
        for project in &listing {
            let Some(uuid) = project.get("uuid").and_then(|v| v.as_str()) else {
                continue;
            };
            let label = project.get("name").and_then(|v| v.as_str()).unwrap_or(uuid);
            bar.did(1);
            bar.doing(label);

            let api_updated = project
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let metadata_changed = existing.get(uuid).map(String::as_str) != Some(api_updated);
            if metadata_changed {
                upsert_project(db, project, uuid, org_uuid, &org_name, now).await?;
                summary.projects_fetched += 1;
            } else {
                summary.projects_skipped += 1;
            }

            if !docs_need_refetch(db, uuid, metadata_changed, run_now).await? {
                summary.project_docs_skipped += 1;
                continue;
            }
            let docs = client.list_project_docs(org_uuid, uuid).await;
            if docs.is_err() && !stop.requested() {
                // Due again next run, even with its metadata stored and a
                // sweep marker under a day old.
                db.forget_sweep(&project_docs_sweep_key(uuid)).await?;
            }
            match docs {
                Ok(docs) => {
                    summary.project_docs_fetched +=
                        upsert_project_docs(db, &docs, uuid, now).await?;
                    // Only stamp the marker once the rows are stored, so
                    // an interrupted sweep doesn't poison the TTL check
                    // (same rule as `orgs`).
                    db.record_sweep(&project_docs_sweep_key(uuid), run_now)
                        .await?;
                }
                Err(_) if stop.requested() => return Ok(()),
                Err(ClaudeError::RateLimited(reason)) => {
                    walk.give_up("projects", &reason);
                    return Ok(());
                }
                Err(e @ ClaudeError::Forbidden(_)) => {
                    walk.found.push(RunProblem::forbidden(
                        &format!("project_docs {label}"),
                        e.to_string(),
                    ));
                }
                Err(e) => {
                    walk.found
                        .listing(&format!("project_docs {label}"), e.to_string());
                    summary.errors += 1;
                }
            }
            sleep(SLEEP_BETWEEN).await;
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
            summary
                .problems
                .push(DownloadProblem::not_found("project_uuids", raw, detail));
        }
    }
    Ok(())
}

/// Whether to re-list one project's knowledge docs. Yes when the
/// project's metadata changed, when no sweep has ever completed for it,
/// or when the last one is older than [`PROJECT_DOCS_TTL`] at the run's
/// now.
async fn docs_need_refetch(
    db: &RawDb,
    project_uuid: &str,
    metadata_changed: bool,
    run_now: &IsoOffsetTimestamp,
) -> Result<bool> {
    if metadata_changed {
        return Ok(true);
    }
    Ok(
        match db
            .sweep_age(&project_docs_sweep_key(project_uuid), run_now)
            .await?
        {
            Some(age) => age >= PROJECT_DOCS_TTL,
            None => true,
        },
    )
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

async fn upsert_project(
    db: &RawDb,
    payload: &Value,
    uuid: &str,
    org_uuid: &str,
    org_name: &str,
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    let payload = &canonicalize_project_payload(payload);
    let row = ProjectRow {
        id_and_payload: WirePayload {
            id: uuid.to_string(),
            payload: serde_json::to_string(payload).context("serialize project")?,
        },
        org_uuid: Some(org_uuid.to_string()),
        org_name: Some(org_name.to_string()),
        name: payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from),
        updated_at: payload
            .get("updated_at")
            .and_then(|v| v.as_str())
            .map(String::from),
    };
    commit_rows(db, &[row], now).await
}

async fn upsert_project_docs(
    db: &RawDb,
    docs: &[Value],
    project_uuid: &str,
    now: &IsoOffsetTimestamp,
) -> Result<usize> {
    let mut rows: Vec<ProjectDocRow> = Vec::with_capacity(docs.len());
    for doc in docs {
        let Some(id) = doc.get("uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        rows.push(ProjectDocRow {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: serde_json::to_string(doc).context("serialize project doc")?,
            },
            project_uuid: Some(project_uuid.to_string()),
            file_name: doc
                .get("file_name")
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: doc
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }
    let n = rows.len();
    commit_rows(db, &rows, now).await?;
    Ok(n)
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
    let lk = datalib_etl::latchkey::latchkey_cli_hint();
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

#[allow(clippy::too_many_arguments)]
async fn fetch_single(
    client: &mut ClaudeClient,
    db: &RawDb,
    orgs: &[Value],
    conv_uuid: &str,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &IsoOffsetTimestamp,
    stop: &StopFlag,
    forbidden_orgs: &mut BTreeMap<String, String>,
) -> Result<SingleOutcome> {
    let mut refused: Vec<String> = Vec::new();
    let mut not_found = 0usize;
    for org in orgs {
        let Some((org_uuid, org_name)) = org_identity(org) else {
            continue;
        };
        if forbidden_orgs.contains_key(org_uuid) {
            refused.push(org_name);
            continue;
        }
        // Same retry the listing walk uses. Without it a transient 403
        // — which claude.ai issues routinely on a detail GET — reads as
        // "not in this org", and the uuid is reported missing.
        match get_conversation_with_403_retry(client, org_uuid, conv_uuid)
            .await
            .map(|o| o.value)
            .map_err(|(e, _)| e)
        {
            Ok(full) => {
                let saved = save_with_files(
                    db,
                    org_uuid,
                    &org_name,
                    conv_uuid,
                    &full,
                    summary,
                    blake3_by_file,
                    now,
                    stop,
                )
                .await?;
                match saved {
                    None => {}
                    Some(Cut::Stopped) => return Ok(SingleOutcome::Stopped),
                    Some(Cut::RateLimited(reason)) => return Ok(SingleOutcome::Cut(reason)),
                }
                info!(
                    event = "claude_fetch_single_ok",
                    uuid = conv_uuid,
                    org = %org_name,
                    "fetched one conversation by id"
                );
                return Ok(SingleOutcome::Fetched);
            }
            Err(_) if stop.requested() => return Ok(SingleOutcome::Stopped),
            Err(ClaudeError::RateLimited(reason)) => return Ok(SingleOutcome::Cut(reason)),
            Err(ClaudeError::Forbidden(_)) => {
                warn!(
                    event = "claude_fetch_single_forbidden",
                    uuid = conv_uuid,
                    org = %org_name,
                    "still 403 after the transient retries; not asking this org again",
                );
                forbidden_orgs.insert(org_uuid.to_string(), org_name.clone());
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
                db.record_conversation_error(conv_uuid, &e.to_string())
                    .await?;
                summary.errors += 1;
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
struct RetryOutcome {
    value: Value,
    retries: u32,
}

async fn get_conversation_with_403_retry(
    client: &mut ClaudeClient,
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

async fn save_conversation(
    db: &RawDb,
    org_uuid: &str,
    org_name: &str,
    uuid: &str,
    full: &Value,
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    let full = &canonicalize_conversation_payload(full);
    let payload = serde_json::to_string(full).context("serialize conversation")?;
    let name = full.get("name").and_then(|v| v.as_str()).map(String::from);
    let updated_at = full
        .get("updated_at")
        .and_then(|v| v.as_str())
        .map(String::from);
    let row = ConversationRowSchema {
        id_and_payload: WirePayload {
            id: uuid.to_string(),
            payload,
        },
        org_uuid: Some(org_uuid.to_string()),
        org_name: Some(org_name.to_string()),
        name,
        updated_at,
    };
    commit_rows(db, &[row], now).await
}

fn org_identity(org: &Value) -> Option<(&str, String)> {
    let uuid = org.get("uuid").and_then(|v| v.as_str())?;
    let name = match org.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => uuid.char_indices().take(8).map(|(_, c)| c).collect(),
    };
    Some((uuid, name))
}

async fn commit_rows<T: datalib_etl::bulk::BulkUpsertable>(
    db: &RawDb,
    rows: &[T],
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = db
        .pool()
        .begin()
        .await
        .with_context(|| format!("begin {} upsert tx", T::TABLE))?;
    bulk_upsert_in_tx(&mut tx, rows, now).await?;
    tx.commit()
        .await
        .with_context(|| format!("commit {} upsert tx", T::TABLE))
}

async fn upsert_users(db: &RawDb, payloads: &[Value], now: &IsoOffsetTimestamp) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let mut rows: Vec<UserRow> = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let Some(id) = payload.get("uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        let email = payload
            .get("email_address")
            .and_then(|v| v.as_str())
            .map(String::from);
        let full_name = payload
            .get("full_name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let payload_str = serde_json::to_string(payload).context("serialize user")?;
        rows.push(UserRow {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: payload_str,
            },
            email,
            full_name,
        });
    }
    commit_rows(db, &rows, now).await
}

async fn upsert_orgs(db: &RawDb, payloads: &[Value], now: &IsoOffsetTimestamp) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let mut rows: Vec<OrgRow> = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let Some(id) = payload.get("uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let payload_str =
            serde_json::to_string(&canonicalize_org_payload(payload)).context("serialize org")?;
        rows.push(OrgRow {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: payload_str,
            },
            name,
        });
    }
    commit_rows(db, &rows, now).await
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

/// Why the files of one conversation were not all tried.
enum Cut {
    Stopped,
    /// The give-up guard tripped, with why.
    RateLimited(String),
}

/// Store one fetched conversation with the attachments it names. A [`Cut`]
/// when the attachments were cut short: nothing of it is written, so the
/// next run finds it missing or stale and starts it over.
#[allow(clippy::too_many_arguments)]
async fn save_with_files(
    db: &RawDb,
    org_uuid: &str,
    org_name: &str,
    uuid: &str,
    full: &Value,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &IsoOffsetTimestamp,
    stop: &StopFlag,
) -> Result<Option<Cut>> {
    let attach = match fetch_files(full, uuid, summary, blake3_by_file, stop).await {
        Ok(attach) => attach,
        Err(cut) => return Ok(Some(cut)),
    };
    save_conversation(db, org_uuid, org_name, uuid, full, now).await?;
    summary.fetched += 1;
    flush_files(db, &attach).await?;
    Ok(None)
}

/// The walk reaches a conversation's files only while it fetches that
/// conversation, and an unchanged one is not fetched again. So after the
/// walk, every file that has not landed is tried again from the
/// conversation the store already holds.
async fn retry_attachments(
    db: &RawDb,
    opts: &FetchOptions,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    walk: &mut Walk,
) -> Result<()> {
    let pending: Vec<String> = db
        .conversations_with_unfetched_attachments()
        .await?
        .into_iter()
        .filter(|uuid| !walk.attachments_tried.contains(uuid))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    info!(
        event = "claude_attachment_retry",
        conversations = pending.len(),
        "trying again the attachments earlier runs did not land"
    );
    for uuid in &pending {
        if opts.control.stop.requested() {
            break;
        }
        let Some(conv) = db.load_conversation_payload(uuid).await? else {
            continue;
        };
        let attach =
            match fetch_files(&conv, uuid, summary, blake3_by_file, &opts.control.stop).await {
                Ok(attach) => attach,
                Err(Cut::Stopped) => break,
                Err(Cut::RateLimited(reason)) => {
                    walk.give_up("attachments", &reason);
                    break;
                }
            };
        flush_files(db, &attach).await?;
        if let Some(sealer) = opts.sealer.as_ref() {
            sealer.wrote(1).await;
        }
    }
    Ok(())
}

/// Every `chat_messages[].files[]` the conversation names, each once. A
/// file we have bytes for is not fetched again; one that fails becomes
/// its edge's `last_error`, and one that is not there to fetch a warning
/// the retry pass leaves alone. A [`Cut`] when a stop or the give-up
/// guard ended it: what it fetched is dropped rather than recorded.
async fn fetch_files(
    conv: &Value,
    conv_uuid: &str,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    stop: &StopFlag,
) -> std::result::Result<CasEdgeAccumulator, Cut> {
    let mut attach = CasEdgeAccumulator::new();
    let Some(messages) = conv.get("chat_messages").and_then(|v| v.as_array()) else {
        return Ok(attach);
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut targets: Vec<Value> = Vec::new();
    for msg in messages {
        if let Some(files) = msg.get("files").and_then(|v| v.as_array()) {
            for f in files {
                if let Some(id) = f.get("file_uuid").and_then(|v| v.as_str()) {
                    if seen.insert(id.to_string()) {
                        targets.push(f.clone());
                    }
                }
            }
        }
    }
    for f in &targets {
        let Some(file_uuid) = f.get("file_uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(blake3) = blake3_by_file.get(file_uuid) {
            attach.add_known(conv_uuid, file_uuid, blake3.clone());
            summary.skipped_blobs += 1;
            continue;
        }
        let name = f
            .get("file_name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        match download_one_file(f).await {
            Ok((bytes, content_type)) => {
                let blake3 = datalib_etl::blob_cas::blake3_hex(&bytes);
                blake3_by_file.insert(file_uuid.to_string(), blake3);
                attach.add_fetched(conv_uuid, file_uuid, bytes, content_type, name);
                summary.new_blobs += 1;
            }
            Err(_) if stop.requested() => return Err(Cut::Stopped),
            Err(FileError::RateLimited(reason)) => return Err(Cut::RateLimited(reason)),
            Err(FileError::NotThere(reason)) => {
                attach.add_skipped(conv_uuid, file_uuid, Reason::NotFound, reason);
                summary.failed_blobs += 1;
            }
            Err(FileError::Failed(reason)) => {
                attach.add_failed(conv_uuid, file_uuid, reason);
                summary.failed_blobs += 1;
            }
        }
    }
    Ok(attach)
}

async fn flush_files(db: &RawDb, attach: &CasEdgeAccumulator) -> Result<()> {
    attach
        .flush(db.pool(), db.cas(), |conv_uuid, file_uuid, blake3| {
            ConversationAttachmentRow {
                id: ConversationAttachmentRow::pk_recipe(conv_uuid, file_uuid),
                conversation_uuid: conv_uuid.to_string(),
                file_uuid: file_uuid.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await
        .context("write a conversation's attachments")
}

/// Why a file has no bytes; the text becomes the edge's problem.
enum FileError {
    /// Nothing to fetch: claude.ai no longer has it, or the payload names
    /// no URL. Trying again cannot help until the conversation changes,
    /// and a refetch of it tries its files again.
    NotThere(String),
    /// The give-up guard tripped.
    RateLimited(String),
    /// Anything else, which the next run tries again.
    Failed(String),
}

/// One file's bytes and content type, or why there are none.
async fn download_one_file(file_obj: &Value) -> Result<(Vec<u8>, Option<String>), FileError> {
    let preview_path = file_obj
        .get("preview_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file_obj
                .get("document_asset")
                .and_then(|d| d.get("url"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| FileError::NotThere("the file has no preview URL".into()))?;
    let url = if preview_path.starts_with("http") {
        preview_path.to_string()
    } else {
        format!("{CLAUDE_ORIGIN}{preview_path}")
    };
    let mime = file_obj
        .get("file_kind")
        .and_then(|v| v.as_str())
        .or_else(|| file_obj.get("mime_type").and_then(|v| v.as_str()));

    let req = HttpRequest::get(HttpService::Claude, &url).timeout(ATTACH_FILE_TIMEOUT);
    match latchkey_curl(&req).await {
        Ok(resp) if (200..300).contains(&resp.status) => {
            let header_mime = resp.header("content-type").map(String::from);
            let effective_mime = header_mime.as_deref().or(mime);
            Ok((resp.body, effective_mime.map(String::from)))
        }
        Ok(resp) if matches!(resp.status, 404 | 410) => Err(FileError::NotThere(format!(
            "HTTP {}, claude.ai no longer has it: GET {url}",
            resp.status
        ))),
        Ok(resp) => Err(FileError::Failed(format!(
            "HTTP {}: GET {url}",
            resp.status
        ))),
        // Its message carries the tape's path on this machine. The reason
        // leads: the sample is cut at 80 characters.
        Err(HttpError::PlaybackMiss(_)) => Err(FileError::Failed(format!(
            "no recorded response: GET {url}"
        ))),
        Err(e @ HttpError::GaveUp { .. }) => Err(FileError::RateLimited(e.to_string())),
        Err(e) => Err(FileError::Failed(e.to_string())),
    }
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
        let now = datalib_time::parse_strict("2026-09-18T10:05:00-07:00").unwrap();
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

        save_conversation(&db, "org-a", "A", uuid, &conversation_with_nulls(), &now)
            .await
            .unwrap();
        let head = commit_run(db.pool(), "first fetch").await.unwrap();
        save_conversation(&db, "org-a", "A", uuid, &conversation_without_nulls(), &now)
            .await
            .unwrap();

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
