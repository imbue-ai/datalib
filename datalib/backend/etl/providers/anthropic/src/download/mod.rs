//! Anthropic (claude.ai) downloader entry point. Port of
//! `src/download/claude_web.py`.
//!
//! Writes into a single doltlite database file
//! (`<data_root>/<name>/raw/entities.doltlite_db`). Conversations are stored as
//! the **raw** `/api/...` payload — the export-shape normalization
//! used to happen here at fetch time, but now lives in `render`
//! so the raw store stays as close to the wire as possible.

pub mod api;
pub mod db;
pub mod normalize;
pub mod schema_raw;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::http::{latchkey_curl, HttpRequest};
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
///
/// NOTE — this cache was added on a diagnosis that turned out to be wrong,
/// and may not have been necessary. See the sweep comment in `fetch`.
pub const ORGS_TTL: chrono::Duration = chrono::Duration::hours(6);
const ORGS_SWEEP_KEY: &str = "orgs";

/// How long a project's knowledge-doc listing stays good.
///
/// The project *listing* is refetched every run (one request per org),
/// and a changed `updated_at` forces a doc refetch — that is the same
/// incrementality the conversation walk uses. But we have not confirmed
/// that adding or editing a knowledge document bumps the project's
/// `updated_at`, and if it doesn't, an `updated_at`-only rule would let
/// docs go stale forever. This TTL is the floor that bounds that risk:
/// worst case one extra request per project per day.
pub const PROJECT_DOCS_TTL: chrono::Duration = chrono::Duration::hours(24);

/// Per-project sweep-marker key for the docs listing. Namespaced by
/// project UUID so each project ages independently.
fn project_docs_sweep_key(project_uuid: &str) -> String {
    format!("project_docs:{project_uuid}")
}

/// `Default` is hand-written, not derived, so `projects` defaults to
/// `true` and agrees with `ClaudeApiSync`'s serde default. A derived
/// `Default` would make every `..Default::default()` caller silently
/// opt out of the project mirror while their config said otherwise.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Path to the doltlite database file. The entity db lives inside
    /// the per-source directory as `entities.doltlite_db` (the dir is
    /// created if needed). Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. The sync orchestrator pre-
    /// opens at startup so a download isn't started against a DB we
    /// can't write to (and so the post-download commit can run on the
    /// same connection — no reopen race).
    pub db: Option<RawDb>,
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
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: datalib_etl::control::DownloadControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            export_dir: None,
            overlap: 0,
            sleep_between: Duration::ZERO,
            since: None,
            conv_uuids: Vec::new(),
            projects: true,
            project_uuids: Vec::new(),
            progress: Default::default(),
            control: Default::default(),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    /// Listing items ignored because their `updated_at` predates the
    /// configured `since`. Not counted in `skipped` (which means
    /// "in scope and already up to date") or `total`.
    pub out_of_scope: usize,
    pub forbidden_orgs: usize,
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

#[instrument(skip_all, fields(db = %opts.db_path.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = datalib_etl::latchkey::ensure_curl_dispatch();
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path)
            .await
            .with_context(|| format!("open raw db {}", db_path.display()))?,
    };

    if opts.control.reset_and_redownload {
        info!(event = "anthropic_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    if opts.control.refetch_blobs {
        info!(event = "anthropic_refetch_blobs");
        db.clear_blob_hashes()
            .await
            .context("clear anthropic_attachments.blake3 before refetch")?;
    }

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
    let mut client = ClaudeClient::new();
    let mut summary = FetchSummary::default();
    // One `now` per fetch — threaded into every bulk upsert so all
    // `<table>_bookkeeping.fetched_at` stamps from a single sync share
    // a timestamp.
    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    // Run-scoped `(file_uuid → blake3)` cache, loaded once up-front
    // so the per-file dedupe check inside `fetch_files_for` is a
    // HashMap hit instead of a SQLite round trip. Successful
    // downloads insert into it.
    let mut blake3_by_file = db.load_attachment_blake3s().await?;

    let work = async {
        // The org listing goes first on purpose: it doubles as an
        // explicit credential preflight, so a missing latchkey service
        // registration or a dead sessionKey fails the run right here
        // with setup instructions — instead of first emitting a
        // misleading account-fetch warning and then dying on a cryptic
        // curl error.
        //
        // But re-listing on every download is waste: the org set changes
        // maybe once a year, and the manual-e2e golden runs the pipeline
        // three times per invocation. So reuse the stored rows while a
        // completed sweep is younger than ORGS_TTL, mirroring slack's
        // `conversations.list` / `users.list` markers.
        //
        // ── Why this cache exists, and why it may not have needed to ──
        //
        // It was added 2026-08-17 to stop claude.ai returning HTTP 403 on
        // this endpoint during golden runs, on the theory that we were being
        // rate-limited for calling it too often. That theory was wrong.
        //
        // The 403 carries `cf-mitigated: challenge`, `server: cloudflare`,
        // and a `Just a moment...` HTML body, with NO Retry-After and no
        // x-ratelimit-* headers. It is Cloudflare's interactive bot
        // challenge, not a quota — nothing expires, and no amount of backing
        // off clears it. What clears it is looking like a browser: with
        // LATCHKEY_CURL pointed at
        // //datalib/backend/etl:latchkey_curl_impersonate the same request
        // returns 200 immediately, and without it, it 403s no matter how few
        // calls you have made. (The pre-existing note that disabled the
        // anthropic stanza in the manual-e2e config two months earlier
        // blamed "rate limiting" for the same 403 — quite possibly the same
        // misdiagnosis.)
        //
        // The cache is kept because it is independently worth having —
        // one listing per invocation instead of three, for data that is
        // effectively static — but it should not be credited with fixing the
        // 403s, and if it is ever in the way, removing it costs little.
        // Don't let it become load-bearing in someone's mental model of why
        // anthropic downloads work.
        //
        // The preflight survives where it matters: a cold store has no
        // marker, so a first run — the one where a missing registration or
        // dead sessionKey is actually likely — still calls upstream and
        // still fails loudly. Only a warm store, which has already proven
        // the credential once, skips.
        let cached_orgs = match db.sweep_age(ORGS_SWEEP_KEY).await {
            Ok(Some(age)) if age < ORGS_TTL => match db.load_orgs().await {
                // An empty `orgs` table with a fresh marker shouldn't
                // silently yield zero orgs (that would skip every
                // conversation); fall through to the live call.
                Ok(orgs) if !orgs.is_empty() => {
                    info!(
                        event = "anthropic_orgs_skipped",
                        reason = "ttl",
                        age_s = age.num_seconds().max(0),
                        ttl_s = ORGS_TTL.num_seconds(),
                        count = orgs.len(),
                    );
                    Some(orgs)
                }
                Ok(_) => None,
                Err(e) => {
                    warn!(event = "anthropic_orgs_load_failed", error = %e);
                    None
                }
            },
            Ok(_) => None,
            Err(e) => {
                warn!(event = "anthropic_orgs_sweep_age_failed", error = %e);
                None
            }
        };

        let orgs = match cached_orgs {
            Some(orgs) => orgs,
            None => {
                let orgs = client.list_orgs().await.map_err(credential_hint)?;
                info!(event = "anthropic_orgs", count = orgs.len());
                if let Err(e) = upsert_orgs(&db, &orgs, &now).await {
                    warn!(event = "anthropic_orgs_upsert_failed", error = %e);
                } else if let Err(e) = db.record_sweep(ORGS_SWEEP_KEY).await {
                    // Only stamp the marker once the rows are actually
                    // stored — otherwise a failed upsert would leave a
                    // fresh marker pointing at an empty table.
                    warn!(event = "anthropic_orgs_sweep_record_failed", error = %e);
                }
                orgs
            }
        };

        // users.json from the bulk export carries the account.uuid we
        // need on every conversation. If the DB doesn't have any user
        // yet, try to pull it from the export dir before falling back
        // to `/api/account`.
        if !db.has_any_user().await.unwrap_or(false) {
            if let Some(export_dir) = opts.export_dir.as_deref() {
                ingest_export_users(&db, export_dir, &now)
                    .await
                    .unwrap_or_else(|e| {
                        warn!(event = "anthropic_export_users_failed", error = %e);
                    });
            }
        }
        if !db.has_any_user().await.unwrap_or(false) {
            match client.current_account().await {
                Ok(acct) => {
                    let entry = pick_user_fields(&acct);
                    if let Err(e) = upsert_users(&db, &[entry], &now).await {
                        warn!(event = "anthropic_synthesize_user_failed", error = %e);
                    } else {
                        info!(event = "anthropic_users_synthesized");
                    }
                }
                Err(e) => warn!(
                    event = "anthropic_current_account_failed",
                    error = %e,
                    note = "users will be empty"
                ),
            }
        }

        // Projects come before the conversation walk — including the
        // targeted `conv_uuids` walk below — so a rename lands in the
        // same run as the conversations that dereference it. Scoping to
        // specific conversations does not mean wanting their project
        // labels stale: a conversation resolves its `project` grid
        // column through `project_name_by_uuid`, and with no projects
        // mirrored that column falls back to a bare UUID.
        //
        // `project_uuids` is the knob for narrowing this walk; empty
        // means every project. Best-effort: a project failure warns and
        // is counted, but does not abort the chat mirror, which is the
        // main event.
        if opts.projects {
            let only: HashSet<String> = opts
                .project_uuids
                .iter()
                .map(|s| datalib_etl::ids::normalize_id_token(s))
                .collect();
            sync_projects(
                &mut client,
                &db,
                &orgs,
                &only,
                &mut summary,
                &opts.progress,
                &now,
            )
            .await;
        }

        if !opts.conv_uuids.is_empty() {
            opts.progress.set_length(Some(opts.conv_uuids.len() as u64));
            for raw in &opts.conv_uuids {
                opts.progress.inc(1);
                opts.progress.set_message(raw);
                let target = datalib_etl::ids::normalize_id_token(raw);
                fetch_single(
                    &mut client,
                    &db,
                    &orgs,
                    &target,
                    &mut summary,
                    &mut blake3_by_file,
                    &now,
                )
                .await?;
            }
            return Ok::<(), anyhow::Error>(());
        }

        // Pass 1: list every org, classify. Collect the per-org fetch
        // plans so we know the total work up front and can set the
        // progress bar's length exactly once — otherwise a length
        // reset per org makes the bar jump backwards (e.g. `77/58`
        // when the second org's length is smaller than the count
        // already accumulated from the first).
        //
        // No pre-seed: we only ever write a row after a successful
        // detail fetch. The next sync's listing is the source of truth
        // for "what should exist." A previously-failed fetch is
        // naturally retried because no row exists yet.
        struct OrgPlan<'a> {
            org_uuid: String,
            org_name: String,
            ordered: Vec<&'a Value>,
        }
        let mut plans: Vec<OrgPlan> = Vec::new();
        let mut listings_by_org: Vec<(String, String, Vec<Value>)> = Vec::new();
        for org in &orgs {
            let Some((org_uuid, org_name)) = org_identity(org) else {
                continue;
            };
            let listing = match client
                .list_conversations(org_uuid)
                .instrument(info_span!("anthropic_org_listing", org = %org_name))
                .await
            {
                Ok(l) => l,
                Err(ClaudeError::Forbidden(_)) => {
                    info!(
                        event = "anthropic_org_forbidden",
                        org = %org_name,
                        note = "no chat permission for this org"
                    );
                    summary.forbidden_orgs += 1;
                    continue;
                }
                Err(e) => return Err(anyhow::anyhow!("list conversations for {org_name}: {e}")),
            };
            info!(
                event = "anthropic_org_listing_count",
                org = %org_name,
                count = listing.len()
            );
            sleep(SLEEP_BETWEEN).await;
            listings_by_org.push((org_uuid.to_string(), org_name, listing));
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
                if updated_at_in_scope(c.get("updated_at").and_then(|v| v.as_str()), since.as_ref())
                {
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
                event = "anthropic_priority_split",
                org = %org_name,
                missing = missing.len(),
                stale = stale.len(),
                up_to_date = up_to_date,
                out_of_scope = out_of_scope,
            );
            summary.skipped += up_to_date;

            let ordered: Vec<&Value> = missing.into_iter().chain(stale).collect();
            plans.push(OrgPlan {
                org_uuid: org_uuid.clone(),
                org_name: org_name.clone(),
                ordered,
            });
        }

        // Pass 2: fetch. The outer bar's length is the sum across all
        // orgs and advances once per chat — so a quick glance answers
        // "how close is the whole sync to done?". Each org also gets
        // its own inner bar (mirroring the per-channel pattern in
        // slack) so the current-org context stays visible.
        let total: usize = plans.iter().map(|p| p.ordered.len()).sum();
        opts.progress.set_length(Some(total as u64));
        for plan in &plans {
            let inner = opts
                .progress
                .child(&format!("claude org: {}", plan.org_name));
            inner.set_length(Some(plan.ordered.len() as u64));
            for item in &plan.ordered {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                inner.inc(1);
                inner.set_message(uuid);
                opts.progress.inc(1);
                opts.progress
                    .set_message(&format!("{} {uuid}", plan.org_name));
                match get_conversation_with_403_retry(&mut client, &plan.org_uuid, uuid).await {
                    Ok(outcome) => {
                        summary.forbidden_retry_attempts += outcome.retries as u64;
                        if outcome.retries > 0 {
                            summary.forbidden_retry_recoveries += 1;
                        }
                        save_conversation(
                            &db,
                            &plan.org_uuid,
                            &plan.org_name,
                            uuid,
                            &outcome.value,
                            &now,
                        )
                        .await?;
                        summary.fetched += 1;
                        fetch_files_for(
                            &db,
                            &outcome.value,
                            uuid,
                            &mut summary,
                            &mut blake3_by_file,
                            &now,
                        )
                        .await;
                        if opts.sleep_between > Duration::ZERO {
                            sleep(opts.sleep_between).await;
                        }
                    }
                    Err((e, retries)) => {
                        summary.forbidden_retry_attempts += retries as u64;
                        warn!(event = "anthropic_fetch_error", uuid = uuid, error = %e);
                        let _ = db.record_conversation_error(uuid, &e.to_string()).await;
                        summary.errors += 1;
                    }
                }
            }
            inner.finish_and_clear();
        }
        Ok(())
    };

    let result = work.await;
    summary.total = summary.fetched + summary.skipped;
    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// Mirror every org's Claude Projects: the project metadata rows plus
/// each project's knowledge documents.
///
/// Incrementality mirrors the conversation walk — the listing is one
/// request per org and a project whose `updated_at` is unchanged is not
/// re-written — with one addition: knowledge docs live behind a
/// per-project [`PROJECT_DOCS_TTL`] sweep marker, because we have not
/// confirmed that editing a document bumps the project's `updated_at`.
///
/// Best-effort throughout. A 403 on an org means "no project permission
/// here" (the same shape `list_conversations` already handles); any
/// other failure warns, counts into `summary.errors`, and moves on
/// rather than aborting the conversation mirror.
///
/// `only` narrows the walk to a specific set of project UUIDs when
/// non-empty (config `sync.project_uuids`). It filters *after* the
/// listing, not instead of it: the listing is one request per org and
/// it is where the project metadata comes from.
#[allow(clippy::too_many_arguments)]
async fn sync_projects(
    client: &mut ClaudeClient,
    db: &RawDb,
    orgs: &[Value],
    only: &HashSet<String>,
    summary: &mut FetchSummary,
    progress: &datalib_etl::progress::Progress,
    now: &str,
) {
    // Track which requested UUIDs we actually saw, so a typo doesn't
    // silently mirror nothing.
    let mut matched: HashSet<&str> = HashSet::new();

    for org in orgs {
        let Some((org_uuid, org_name)) = org_identity(org) else {
            continue;
        };
        let listing = match client
            .list_projects(org_uuid)
            .instrument(info_span!("anthropic_project_listing", org = %org_name))
            .await
        {
            Ok(l) => l,
            Err(ClaudeError::Forbidden(_)) => {
                info!(
                    event = "anthropic_projects_forbidden",
                    org = %org_name,
                    note = "no project permission for this org"
                );
                continue;
            }
            Err(e) => {
                warn!(event = "anthropic_projects_list_failed", org = %org_name, error = %e);
                summary.errors += 1;
                continue;
            }
        };
        info!(
            event = "anthropic_project_listing_count",
            org = %org_name,
            count = listing.len()
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
                        .is_some_and(|u| only.contains(u))
                })
                .collect()
        };
        for p in &listing {
            if let Some(u) = p.get("uuid").and_then(|v| v.as_str()) {
                // Borrow from `only`, not from the listing, so the set
                // outlives this iteration.
                if let Some(k) = only.get(u) {
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
        let existing = match db.existing_project_updated_at(&listed_ids).await {
            Ok(m) => m,
            Err(e) => {
                warn!(event = "anthropic_projects_skipcheck_failed", error = %e);
                summary.errors += 1;
                continue;
            }
        };

        let inner = progress.child(&format!("claude projects: {org_name}"));
        inner.set_length(Some(listing.len() as u64));
        for project in &listing {
            let Some(uuid) = project.get("uuid").and_then(|v| v.as_str()) else {
                continue;
            };
            inner.inc(1);
            inner.set_message(project.get("name").and_then(|v| v.as_str()).unwrap_or(uuid));

            let api_updated = project
                .get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let metadata_changed = existing.get(uuid).map(String::as_str) != Some(api_updated);
            if metadata_changed {
                match upsert_project(db, project, uuid, org_uuid, &org_name, now).await {
                    Ok(()) => summary.projects_fetched += 1,
                    Err(e) => {
                        warn!(event = "anthropic_project_upsert_failed", uuid = uuid, error = %e);
                        summary.errors += 1;
                        continue;
                    }
                }
            } else {
                summary.projects_skipped += 1;
            }

            if !docs_need_refetch(db, uuid, metadata_changed).await {
                summary.project_docs_skipped += 1;
                continue;
            }
            match client.list_project_docs(org_uuid, uuid).await {
                Ok(docs) => match upsert_project_docs(db, &docs, uuid, now).await {
                    Ok(n) => {
                        summary.project_docs_fetched += n;
                        // Only stamp the marker once the rows are
                        // stored, so an interrupted sweep doesn't
                        // poison the TTL check (same rule as `orgs`).
                        if let Err(e) = db.record_sweep(&project_docs_sweep_key(uuid)).await {
                            warn!(event = "anthropic_project_docs_sweep_failed", error = %e);
                        }
                    }
                    Err(e) => {
                        warn!(event = "anthropic_project_docs_upsert_failed", uuid = uuid, error = %e);
                        summary.errors += 1;
                    }
                },
                Err(ClaudeError::Forbidden(_)) => {
                    info!(event = "anthropic_project_docs_forbidden", uuid = uuid);
                }
                Err(e) => {
                    warn!(event = "anthropic_project_docs_failed", uuid = uuid, error = %e);
                    summary.errors += 1;
                }
            }
            sleep(SLEEP_BETWEEN).await;
        }
        inner.finish_and_clear();
    }

    // A UUID in `sync.project_uuids` that matched nothing is almost
    // always a typo or a project in an org this account can't see.
    // Silently mirroring nothing is the worst outcome, so say it.
    for requested in only {
        if !matched.contains(requested.as_str()) {
            warn!(
                event = "anthropic_project_uuid_not_found",
                uuid = %requested,
                note = "listed in sync.project_uuids but not present in any visible org"
            );
        }
    }
}

/// Whether to re-list one project's knowledge docs. Yes when the
/// project's metadata changed, when no sweep has ever completed for it,
/// or when the last one is older than [`PROJECT_DOCS_TTL`]. A marker
/// read that errors is treated as "refetch" — doing the request is
/// always safe, skipping on a broken read is not.
async fn docs_need_refetch(db: &RawDb, project_uuid: &str, metadata_changed: bool) -> bool {
    if metadata_changed {
        return true;
    }
    match db.sweep_age(&project_docs_sweep_key(project_uuid)).await {
        Ok(Some(age)) => age >= PROJECT_DOCS_TTL,
        Ok(None) => true,
        Err(e) => {
            warn!(event = "anthropic_project_docs_sweep_age_failed", error = %e);
            true
        }
    }
}

async fn upsert_project(
    db: &RawDb,
    payload: &Value,
    uuid: &str,
    org_uuid: &str,
    org_name: &str,
    now: &str,
) -> Result<()> {
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

/// Write one project's knowledge documents. Returns how many rows were
/// written. Docs carry their text inline, so there is nothing to fetch
/// past this listing and nothing to put in the CAS.
async fn upsert_project_docs(
    db: &RawDb,
    docs: &[Value],
    project_uuid: &str,
    now: &str,
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
fn credential_hint(e: ClaudeError) -> anyhow::Error {
    let s = e.to_string();
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

#[allow(clippy::too_many_arguments)]
async fn fetch_single(
    client: &mut ClaudeClient,
    db: &RawDb,
    orgs: &[Value],
    conv_uuid: &str,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &str,
) -> Result<()> {
    for org in orgs {
        let Some((org_uuid, org_name)) = org_identity(org) else {
            continue;
        };
        match client.get_conversation(org_uuid, conv_uuid).await {
            Ok(full) => {
                save_conversation(db, org_uuid, &org_name, conv_uuid, &full, now).await?;
                summary.fetched += 1;
                info!(
                    event = "anthropic_fetch_single_ok",
                    uuid = conv_uuid,
                    org = %org_name
                );
                fetch_files_for(db, &full, conv_uuid, summary, blake3_by_file, now).await;
                return Ok(());
            }
            Err(ClaudeError::Forbidden(_)) => {
                info!(
                    event = "anthropic_fetch_single_forbidden",
                    uuid = conv_uuid,
                    org = %org_name
                );
                continue;
            }
            Err(ClaudeError::Permanent(msg)) if msg.contains("HTTP 404") => {
                info!(
                    event = "anthropic_fetch_single_not_in_org",
                    uuid = conv_uuid,
                    org = %org_name
                );
                continue;
            }
            Err(e) => {
                warn!(event = "anthropic_fetch_error", uuid = conv_uuid, error = %e);
                let _ = db
                    .record_conversation_error(conv_uuid, &e.to_string())
                    .await;
                summary.errors += 1;
                return Err(anyhow::anyhow!("fetch {conv_uuid}: {e}"));
            }
        }
    }
    Err(anyhow::anyhow!(
        "conversation {conv_uuid} not found in any of {} org(s)",
        orgs.len()
    ))
}

/// Backoff delays for transient-403 retries on a single
/// `get_conversation`. claude.ai occasionally returns 403 on detail GETs
/// when listing+detail are issued in rapid succession; the same UUID
/// re-fetched a moment later typically returns 200. Verified by direct
/// probe: a UUID that 403'd inside a run returned 200 to a fresh
/// `latchkey curl` immediately after. We treat Forbidden as transient
/// here (not at the transport layer) so a real org-level permission
/// denial — caught earlier by `list_conversations` — still short-circuits
/// to `anthropic_org_forbidden`.
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
                        event = "anthropic_fetch_403_retry_ok",
                        uuid = conv_uuid,
                        attempt = attempt,
                    );
                }
                return Ok(RetryOutcome {
                    value: v,
                    retries: attempt as u32,
                });
            }
            Err(ClaudeError::Forbidden(msg)) => {
                warn!(
                    event = "anthropic_fetch_403_transient",
                    uuid = conv_uuid,
                    attempt = attempt,
                    error = %msg,
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

async fn save_conversation(
    db: &RawDb,
    org_uuid: &str,
    org_name: &str,
    uuid: &str,
    full: &Value,
    now: &str,
) -> Result<()> {
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

/// `(uuid, display name)` for one `/organizations` entry, or `None`
/// when it carries no uuid.
///
/// The fallback label is the uuid's leading 8 characters — taken with
/// `char_indices` rather than a byte slice, so an unexpected non-ASCII
/// id truncates instead of panicking mid-codepoint. Three call sites
/// (conversation listing, single-conversation fetch, project listing)
/// need exactly this pair.
fn org_identity(org: &Value) -> Option<(&str, String)> {
    let uuid = org.get("uuid").and_then(|v| v.as_str())?;
    let name = match org.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => uuid.char_indices().take(8).map(|(_, c)| c).collect(),
    };
    Some((uuid, name))
}

/// Open a transaction, bulk-upsert `rows`, commit.
///
/// Every entity write in this module has exactly this shape, and
/// `T::TABLE` supplies the error context, so call sites carry only the
/// row construction that actually differs between them.
async fn commit_rows<T: datalib_etl::bulk::BulkUpsertable>(
    db: &RawDb,
    rows: &[T],
    now: &str,
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

/// Bulk-upsert helpers — same `now` as the rest of the fetch so the
/// bookkeeping sidecars all share a timestamp.
async fn upsert_users(db: &RawDb, payloads: &[Value], now: &str) -> Result<()> {
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

async fn upsert_orgs(db: &RawDb, payloads: &[Value], now: &str) -> Result<()> {
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
        let payload_str = serde_json::to_string(payload).context("serialize org")?;
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

/// Pull `users.json` entries from an existing bulk-export directory
/// into the DB. Best-effort: missing file is fine.
async fn ingest_export_users(db: &RawDb, export_dir: &Path, now: &str) -> Result<()> {
    let path = export_dir.join("users.json");
    if !path.exists() {
        return Ok(());
    }
    let txt = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&txt).with_context(|| format!("parse {}", path.display()))?;
    if let Some(arr) = v.as_array() {
        if let Err(e) = upsert_users(db, arr, now).await {
            warn!(event = "anthropic_users_upsert_failed", error = %e);
        }
    }
    Ok(())
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

/// Walk a conversation tree's `chat_messages[*].files[]` and
/// queue every unique attachment for the end-of-conversation CAS
/// flush. Skips files we already have bytes for.
async fn fetch_files_for(
    db: &RawDb,
    conv: &Value,
    conv_uuid: &str,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &str,
) {
    let messages = match conv.get("chat_messages").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return,
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
    let mut attach = datalib_etl::blob_cas::CasEdgeAccumulator::new();
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
            Ok(Some((bytes, content_type))) => {
                let blake3 = datalib_etl::blob_cas::blake3_hex(&bytes);
                blake3_by_file.insert(file_uuid.to_string(), blake3);
                attach.add_fetched(conv_uuid, file_uuid, bytes, content_type, name);
                summary.new_blobs += 1;
            }
            Ok(None) => {
                attach.add_failed(conv_uuid, file_uuid, "no bytes");
                summary.failed_blobs += 1;
            }
            Err(e) => {
                warn!(event = "anthropic_media_unexpected_err", file_uuid = %file_uuid, error = %e);
                attach.add_failed(conv_uuid, file_uuid, e.to_string());
                summary.failed_blobs += 1;
            }
        }
    }
    let flush_result = attach
        .flush(db.pool(), db.cas(), |conv_uuid, file_uuid, blake3| {
            ConversationAttachmentRow {
                id: ConversationAttachmentRow::pk_recipe(conv_uuid, file_uuid),
                conversation_uuid: conv_uuid.to_string(),
                file_uuid: file_uuid.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await;
    if let Err(e) = flush_result {
        warn!(event = "anthropic_attachment_flush_err", conv = %conv_uuid, error = %e);
    }
    let _ = now;
}

async fn download_one_file(file_obj: &Value) -> Result<Option<(Vec<u8>, Option<String>)>> {
    let Some(file_uuid) = file_obj.get("file_uuid").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
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
        });
    let preview_path = match preview_path {
        Some(p) => p,
        None => {
            warn!(
                event = "anthropic_media_no_preview_url",
                file_uuid = file_uuid
            );
            return Ok(None);
        }
    };
    let url = if preview_path.starts_with("http") {
        preview_path.to_string()
    } else {
        format!("{CLAUDE_ORIGIN}{preview_path}")
    };
    let mime = file_obj
        .get("file_kind")
        .and_then(|v| v.as_str())
        .or_else(|| file_obj.get("mime_type").and_then(|v| v.as_str()));

    let req = HttpRequest::get("anthropic", &url).timeout(ATTACH_FILE_TIMEOUT);
    match latchkey_curl(&req).await {
        Ok(resp) if (200..300).contains(&resp.status) => {
            let header_mime = resp.header("content-type").map(String::from);
            let effective_mime = header_mime.as_deref().or(mime);
            Ok(Some((resp.body, effective_mime.map(String::from))))
        }
        Ok(resp) => {
            let msg = format!("HTTP {}", resp.status);
            warn!(
                event = "anthropic_media_failed",
                file_uuid = file_uuid,
                error = %msg,
            );
            Ok(None)
        }
        Err(e) => {
            let msg = e.to_string();
            warn!(
                event = "anthropic_media_failed",
                file_uuid = file_uuid,
                error = %msg,
            );
            Ok(None)
        }
    }
}

/// Parse a `since` config value: full RFC 3339 or bare `YYYY-MM-DD`
/// (assumed UTC midnight). Same accepted forms as slack's `since`.
fn parse_iso_or_utc_date(s: &str) -> Result<DateTime<Utc>> {
    let t = datalib_time::parse_strict(s)
        .or_else(|_| datalib_time::parse_yyyy_mm_dd_assumed_utc(s))
        .with_context(|| format!("expected RFC 3339 or YYYY-MM-DD, got {s:?}"))?;
    Ok(t.inner().with_timezone(&Utc))
}

/// `since` scope check on a listing item's `updated_at`. An item with
/// a missing or unparseable timestamp is conservatively in scope —
/// better to fetch it than to silently drop it.
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
}
