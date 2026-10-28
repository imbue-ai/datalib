//! Notion downloader: mirror pages via the official API.
//!
//! A run is a listing and five loops. The listing stores page objects
//! held at their `last_edited_time` (`listing`; in roots mode by way of
//! the outline, `scope`, and the objects loop); each loop then fetches
//! what the store lists and does not yet hold at that stamp — bodies,
//! attachment bytes, comments, commented blocks, users — through
//! `datalib_etl_web::owed`, which owns the stop, the failure budget, the
//! flush and what each outcome means for a record. Nothing is marked
//! done: holding the content at the listed stamp is done
//! (docs/dev/data_architecture_ingestion.md, "What is left to fetch").

pub mod db;
mod fetchers;
mod listing;
pub mod markdown;
pub mod official;
pub mod schema_raw;
pub mod scope;
pub mod slots;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::Result;
use datalib_etl::download_problems::{DownloadProblem, RunProblem};
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::RunBar;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl::upstream::Upstream;
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, Fetcher, Listed, Loop};
use serde::Serialize;
use serde_json::json;

pub use db::{db_path_for, AttachmentRow, RawDb};
pub use official::{NotionOfficialClient, NotionOfficialError};

use fetchers::{Anchors, Attachments, Blocks, Bodies, Comments, CommentsForbidden, Objects, Users};
use listing::Listing;
use schema_raw::{
    ATTACHMENTS, COMMENT_ANCHORS, OUTLINE, PAGES, PAGE_COMMENTS, PAGE_MARKDOWN, USERS,
};

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Page or database ids (dashed or undashed), or links: the mirror is
    /// what hangs under them and the pages a root's body links. Empty
    /// means the whole workspace, through search.
    pub subtree_pages: Vec<String>,
    /// In search mode, list this many days of edits below the newest
    /// the store has looked at, so a page shared late is listed once
    /// more. 0 lists only what is newer.
    pub refresh_window_days: u32,
    /// Ignore what the search has covered and walk the whole workspace.
    pub full_sync: bool,
    /// Mirror each page's comment threads.
    pub comments: bool,
    /// Archive attachment bytes into the CAS.
    pub attachments: bool,
    /// Stop listing after this many pages. `None` means no limit — a
    /// whole-workspace mirror is the normal case, and a silent cap
    /// would truncate it without saying so.
    pub max_pages: Option<usize>,
    /// Single-page mode: fetch only this page, and nothing under it.
    pub page: Option<String>,
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
    /// Seals as pages land, when the step driver hands one over.
    pub sealer: Option<Sealer>,
}

impl FetchOptions {
    /// Every field defaulted except the store, which has none to give:
    /// it is a live handle the caller opens and closes.
    pub fn new(db: RawDb) -> Self {
        FetchOptions {
            db,
            latchkey: LatchkeySettings::default(),
            subtree_pages: Vec::new(),
            refresh_window_days: 0,
            full_sync: false,
            comments: true,
            attachments: true,
            max_pages: None,
            page: None,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
            sealer: None,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FetchSummary {
    /// Page objects the listing read this run; in roots mode, the pages
    /// the roots reach.
    pub listed: usize,
    /// In roots mode, the pages and databases the outline walk read.
    pub outlined: usize,
    pub new_pages: usize,
    pub upd_pages: usize,
    /// Listed at the stamp the store already holds them at.
    pub skipped_pages: usize,
    /// Pages in roots mode whose object would not fetch. See the
    /// all-failed check at the end of [`fetch`]: when this is the only
    /// thing that happened, the run fails.
    pub failed_pages: usize,
    /// Page bodies fetched from the markdown endpoint.
    pub bodies: usize,
    /// Bodies that came back empty — the common case for a database
    /// row, whose content is its properties.
    pub empty_bodies: usize,
    pub failed_bodies: usize,
    /// Truncated subtrees fetched as follow-ups.
    pub hole_followups: usize,
    /// Pages with more truncated subtrees than one run will follow.
    pub pages_left_incomplete: usize,
    /// Comment rows stored.
    pub comments: usize,
    pub failed_comment_listings: usize,
    /// Users resolved by id (one request each, once ever).
    pub users_resolved: usize,
    /// Blocks a comment hangs off, fetched for their anchor text.
    pub anchors_resolved: usize,
    pub new_blobs: usize,
    pub failed_blobs: usize,
    pub official_requests: u64,
}

/// True when pages were attempted and not one of them worked out.
fn all_pages_failed(s: &FetchSummary) -> bool {
    s.failed_pages > 0 && s.new_pages == 0 && s.upd_pages == 0 && s.skipped_pages == 0
}

/// The walk ends here: a refused credential or a retry guard that gave
/// up would fail every request left. Raised from any loop and caught by
/// the driver, which keeps what it already stored.
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct RunEnded {
    pub reason: String,
    pub unauthorized: bool,
}

impl RunEnded {
    /// The one row a run that ended early leaves: what was fetched is
    /// kept, and the rest is fetched next run.
    fn problem(&self) -> RunProblem {
        if self.unauthorized {
            RunProblem::phase(
                "credential",
                format!(
                    "Notion refused the credential part-way through, so the run stopped; \
                     what it fetched before is kept: {}",
                    self.reason
                ),
            )
        } else {
            RunProblem::phase(
                "rate_limit",
                format!(
                    "the retry guard gave up on Notion, so the run stopped; the rest is \
                     fetched next run: {}",
                    self.reason
                ),
            )
        }
    }
}

pub(crate) fn run_over(e: NotionOfficialError) -> anyhow::Error {
    RunEnded {
        unauthorized: matches!(e, NotionOfficialError::Unauthorized(_)),
        reason: e.to_string(),
    }
    .into()
}

/// Requests in a row that came to nothing before a loop gives up on
/// this run. The retry guard already ends the run on a rate limit; this
/// ends it on an answer the guard does not retry, such as a `400` on
/// every page from a credential missing its `Notion-Version` header.
const FAILURE_BUDGET: usize = 25;

/// What this run's fetchers count as they go. The loops report what
/// came and what did not; these are the finer kinds the summary names.
#[derive(Default)]
pub(crate) struct Counts {
    pub empty_bodies: AtomicUsize,
    pub hole_followups: AtomicUsize,
    pub pages_left_incomplete: AtomicUsize,
    pub comments: AtomicUsize,
    pub users_resolved: AtomicUsize,
    pub anchors_resolved: AtomicUsize,
    pub new_blobs: AtomicUsize,
}

/// What the listing and every loop of one run share.
pub(crate) struct Ctx<'a> {
    pub client: &'a NotionOfficialClient,
    pub db: &'a RawDb,
    pub opts: &'a FetchOptions,
    pub found: &'a RunProblems,
    /// Per page, the live signed URL of each attachment slot its body
    /// named this run. A signed URL lives only in a body's response and
    /// about an hour, so the attachment loop takes it from here or
    /// reads the body again.
    pub signed: Mutex<HashMap<String, HashMap<String, String>>>,
    /// Pages the roots walk asked for that Notion has not got (404) or
    /// will not show (403).
    pub unreadable: Mutex<HashMap<String, NotionOfficialError>>,
    pub counts: Counts,
    /// One tick per request the run can see waiting: a page object in
    /// the roots walk, a body, a comment listing, an attachment, an
    /// anchor, a user. A page of search results is not one, since it is
    /// known only as it comes.
    pub bar: RunBar,
    /// Per loop table, the ticks the listing announced for it ahead of
    /// the loop, which the loop's owed list counts again.
    foreseen: Mutex<HashMap<&'static str, u64>>,
}

impl Ctx<'_> {
    pub fn stop(&self) -> &StopFlag {
        &self.opts.control.stop
    }

    pub async fn wrote(&self, rows: u64) {
        if let Some(sealer) = &self.opts.sealer {
            sealer.wrote(rows).await;
        }
    }

    fn a_loop(&self, table: &'static str, flush: usize, flush_bytes: usize) -> Loop<'_> {
        Loop {
            pool: self.db.pool(),
            table,
            phase: table,
            stop: self.stop(),
            found: self.found,
            sealer: self.opts.sealer.as_ref(),
            batch: 1,
            concurrency: 1,
            flush,
            flush_bytes,
            failures_in_a_row: FAILURE_BUDGET,
        }
    }

    /// A page stored at a new stamp owes a body, and a comment listing
    /// when comments are mirrored. Announcing them as the listing goes
    /// puts a search walk's work in the queue before its loops start.
    pub fn foresee_pages(&self, pages: u64) {
        let tables: &[&'static str] = if self.opts.comments {
            &[PAGE_MARKDOWN, PAGE_COMMENTS]
        } else {
            &[PAGE_MARKDOWN]
        };
        let mut foreseen = self.foreseen.lock().unwrap();
        for t in tables {
            *foreseen.entry(t).or_default() += pages;
        }
        self.bar.expect(pages * tables.len() as u64);
    }

    async fn drain<T: Send>(
        &self,
        table: &'static str,
        owed: Vec<Listed>,
        f: &impl Fetcher<T>,
        flush: usize,
        flush_bytes: usize,
    ) -> Result<owed::Drained> {
        let foreseen = self.foreseen.lock().unwrap().remove(table).unwrap_or(0);
        self.bar
            .expect((owed.len() as u64).saturating_sub(foreseen));
        self.bar.doing(table);
        let l = self.a_loop(table, flush, flush_bytes);
        owed::drain(&l, owed, f).await
    }

    async fn bodies(&self, s: &mut FetchSummary) -> Result<()> {
        let listed = self.db.pages_listed().await?;
        let owed = owed::owed(self.db.pool(), PAGE_MARKDOWN, listed).await?;
        let drained = self
            .drain(PAGE_MARKDOWN, owed, &Bodies(self), 50, 32 << 20)
            .await?;
        s.bodies += drained.got;
        s.failed_bodies += drained.failed;
        Ok(())
    }

    async fn attachments(&self, s: &mut FetchSummary) -> Result<()> {
        let listed = self.db.attachments_listed().await?;
        let owed = owed::owed(self.db.pool(), ATTACHMENTS, listed).await?;
        let drained = self
            .drain(ATTACHMENTS, owed, &Attachments(self), 8, 32 << 20)
            .await?;
        s.failed_blobs += drained.failed;
        Ok(())
    }

    async fn comments(&self, s: &mut FetchSummary) -> Result<()> {
        let listed = self.db.pages_listed().await?;
        let owed = owed::owed(self.db.pool(), PAGE_COMMENTS, listed).await?;
        match self
            .drain(PAGE_COMMENTS, owed, &Comments(self), 50, 0)
            .await
        {
            Ok(drained) => s.failed_comment_listings += drained.failed,
            Err(e) => match e.downcast::<CommentsForbidden>() {
                Ok(forbidden) => self.found.push(comments_forbidden(&forbidden.0)),
                Err(e) => return Err(e),
            },
        }
        Ok(())
    }

    async fn anchors(&self) -> Result<()> {
        let anchors = self.db.anchors_listed().await?;
        let listed: Vec<Listed> = anchors
            .iter()
            .map(|a| Listed::new(a.block_id.clone(), None::<String>))
            .collect();
        let owed = owed::owed(self.db.pool(), COMMENT_ANCHORS, listed).await?;
        let f = Anchors {
            ctx: self,
            page_by_block: anchors
                .into_iter()
                .map(|a| (a.block_id, a.page_id))
                .collect(),
        };
        self.drain(COMMENT_ANCHORS, owed, &f, 50, 0).await?;
        Ok(())
    }

    async fn users(&self) -> Result<()> {
        let listed = self.db.users_listed().await?;
        let owed = owed::owed(self.db.pool(), USERS, listed).await?;
        self.drain(USERS, owed, &Users(self), 50, 0).await?;
        Ok(())
    }

    /// The pages under the roots, and the pages a root's body links: the
    /// outline listed and its blocks placed, the objects of the pages it
    /// puts under a root, their bodies; then the pages the roots' bodies
    /// link, and theirs. A root the outline does not have is asked for
    /// as a page, so one Notion has not got is a `config:` row.
    async fn roots_mirror(&self, roots: &[String], s: &mut FetchSummary) -> Result<()> {
        self.search_walk(Listing::Outline, s).await?;
        self.place_blocks().await?;
        let outline = self.db.outline().await?;
        let root_set: HashSet<String> = roots.iter().cloned().collect();
        let under = scope::pages_under(&outline, &root_set);
        let unplaced: Vec<String> = roots
            .iter()
            .filter(|r| !outline.contains_key(*r))
            .cloned()
            .collect();
        self.roots_round(&unplaced, s).await?;
        let mut wanted = self
            .db
            .outline_stamps(&under.iter().cloned().collect::<Vec<_>>())
            .await?;
        wanted.sort_by(|a, b| b.version.cmp(&a.version).then(a.key.cmp(&b.key)));
        self.objects(wanted, s).await?;
        self.bodies(s).await?;
        if self.stop().requested() {
            return Ok(());
        }
        let linked: Vec<String> = self
            .db
            .pages_linked_from(roots)
            .await?
            .into_iter()
            .filter(|id| !root_set.contains(id) && !under.contains(id))
            .collect();
        if linked.is_empty() {
            return Ok(());
        }
        let linked = self.db.outline_stamps(&linked).await?;
        self.objects(linked, s).await?;
        self.bodies(s).await
    }

    /// The objects of the pages `listed` names, each at the stamp the
    /// outline lists it at, past `max_pages` cut off with a
    /// `listing:roots` row.
    async fn objects(&self, mut listed: Vec<Listed>, s: &mut FetchSummary) -> Result<()> {
        if let Some(m) = self.opts.max_pages {
            let room = m.saturating_sub(s.listed);
            if listed.len() > room {
                listed.truncate(room);
                self.found.listing(
                    "roots",
                    format!(
                        "the roots reach more than max_pages ({m}) pages, so the rest were \
                         not looked at"
                    ),
                );
            }
        }
        s.listed += listed.len();
        let keys: Vec<&str> = listed.iter().map(|l| l.key.as_str()).collect();
        let had: HashSet<String> = owed::held_versions(self.db.pool(), PAGES, keys)
            .await?
            .into_iter()
            .filter(|(_, held)| held.fetched)
            .map(|(id, _)| id)
            .collect();
        let total = listed.len();
        let owed = owed::owed(self.db.pool(), PAGES, listed).await?;
        s.skipped_pages += total - owed.len();
        let f = Objects {
            ctx: self,
            had,
            new: AtomicUsize::new(0),
            updated: AtomicUsize::new(0),
        };
        let drained = self.drain(PAGES, owed, &f, 50, 0).await?;
        s.new_pages += f.new.load(Ordering::Relaxed);
        s.upd_pages += f.updated.load(Ordering::Relaxed);
        s.failed_pages += drained.failed;
        Ok(())
    }

    /// Where every block a page or a database hangs off hangs in turn,
    /// round by round up to the page that holds it. A round that places
    /// nothing ends it, so a block that will not fetch is not asked for
    /// again until the next run.
    async fn place_blocks(&self) -> Result<()> {
        loop {
            let owed = owed::owed(self.db.pool(), OUTLINE, self.db.blocks_listed().await?).await?;
            if owed.is_empty() || self.stop().requested() {
                return Ok(());
            }
            let drained = self.drain(OUTLINE, owed, &Blocks(self), 50, 0).await?;
            if drained.got == 0 {
                return Ok(());
            }
        }
    }

    /// One page and nothing under it.
    async fn single_page(&self, id: String, s: &mut FetchSummary) -> Result<()> {
        self.roots_round(&[id], s).await?;
        self.bodies(s).await
    }
}

/// Runs one phase unless the run has already ended: an error that ends
/// the run is kept and the phase returns as if done; any other error is
/// the step's failure.
async fn phase(
    ended: &mut Option<RunEnded>,
    run: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    if ended.is_some() {
        return Ok(());
    }
    match run.await {
        Ok(()) => Ok(()),
        Err(e) => match e.downcast::<RunEnded>() {
            Ok(e) => {
                *ended = Some(e);
                Ok(())
            }
            Err(e) => Err(e),
        },
    }
}

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
        "roots": opts.subtree_pages,
        "max_pages": opts.max_pages,
        "page": opts.page,
        "full_sync": opts.full_sync,
        "refresh_window_days": opts.refresh_window_days,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let client = NotionOfficialClient::with_latchkey(opts.latchkey.clone());
    let ctx = Ctx {
        client: &client,
        db: &db,
        opts: &opts,
        found: &found,
        signed: Mutex::new(HashMap::new()),
        unreadable: Mutex::new(HashMap::new()),
        counts: Counts::default(),
        bar: RunBar::new(&opts.progress, 0),
        foreseen: Mutex::new(HashMap::new()),
    };
    let mut s = FetchSummary::default();
    let mut result = phases(&ctx, &mut s).await;
    ctx.bar.finish();
    s.official_requests = client.request_count();
    s.empty_bodies = ctx.counts.empty_bodies.load(Ordering::Relaxed);
    s.hole_followups = ctx.counts.hole_followups.load(Ordering::Relaxed);
    s.pages_left_incomplete = ctx.counts.pages_left_incomplete.load(Ordering::Relaxed);
    s.comments = ctx.counts.comments.load(Ordering::Relaxed);
    s.users_resolved = ctx.counts.users_resolved.load(Ordering::Relaxed);
    s.anchors_resolved = ctx.counts.anchors_resolved.load(Ordering::Relaxed);
    s.new_blobs = ctx.counts.new_blobs.load(Ordering::Relaxed);

    // A page that would not fetch is tolerated, which is right when one
    // page of many is unreadable — but wrong when it is every page. A
    // misconfigured credential fails identically on all of them, and
    // without this the run reports success having stored nothing: the
    // DAG step goes green, render finds an empty raw store, and the
    // provider looks healthy while being completely dead. That is how
    // a missing `Notion-Version` header hid for two months.
    if result.is_ok() && all_pages_failed(&s) {
        result = Err(anyhow::anyhow!(
            "every page fetch failed ({} attempted, 0 succeeded) — the raw store \
             is empty, so render will produce nothing. This is usually a \
             credential problem rather than a per-page one: the `notion` \
             latchkey service must inject BOTH the bearer token and the \
             `Notion-Version` header, e.g.\n  \
             latchkey auth set notion -H \"Authorization: Bearer <token>\" \
             -H \"Notion-Version: 2026-03-11\"\n\
             See the per-page `page fetch failed` warnings above for the \
             underlying error.",
            s.failed_pages
        ));
    }
    run.finish(&result, &s).await;
    result?;
    Ok(s)
}

/// The listing, then the loops, in order; a run that ends early keeps
/// what it has and leaves one `phase:` row.
async fn phases(ctx: &Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    let opts = ctx.opts;
    let mut ended: Option<RunEnded> = None;
    let mut roots: Vec<(&str, String)> = Vec::new();
    if let Some(single) = opts.page.as_deref() {
        let id = format_uuid(single);
        phase(&mut ended, ctx.single_page(id, s)).await?;
    } else if opts.subtree_pages.is_empty() {
        phase(&mut ended, async {
            ctx.search_walk(Listing::Workspace, s).await?;
            ctx.bodies(s).await
        })
        .await?;
    } else {
        let mut ids: Vec<String> = Vec::new();
        for raw in &opts.subtree_pages {
            let id = format_uuid(&datalib_etl::ids::normalize_id_token(raw));
            roots.push((raw, id.clone()));
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        phase(&mut ended, ctx.roots_mirror(&ids, s)).await?;
    }
    if opts.attachments {
        phase(&mut ended, ctx.attachments(s)).await?;
    }
    if opts.comments {
        phase(&mut ended, ctx.comments(s)).await?;
    }
    phase(&mut ended, ctx.anchors()).await?;
    phase(&mut ended, ctx.users()).await?;

    if let Some(e) = &ended {
        if e.unauthorized && s.listed == 0 {
            return Err(anyhow::Error::new(Upstream::CredentialRefused)
                .context(format!("notion: {}", e.reason)));
        }
        let problem = e.problem();
        ctx.found.gave_up(problem.detail.clone());
        ctx.found.push(problem);
    } else if !roots.is_empty() {
        let unreadable = ctx.unreadable.lock().unwrap();
        ctx.found.config(roots_upstream_lacks(&roots, &unreadable));
    }
    if opts.page.is_some() {
        // One page reaches none of a full run's listings and phases.
        ctx.found.cut_short();
    }
    Ok(())
}

fn comments_forbidden(detail: &str) -> RunProblem {
    RunProblem::forbidden(
        "comments",
        format!(
            "this credential may not read comments (give the integration the read-comments \
             capability), so none were mirrored: {detail}"
        ),
    )
}

/// The configured roots Notion says it does not have, or will not show
/// this credential.
fn roots_upstream_lacks(
    roots: &[(&str, String)],
    unreadable: &HashMap<String, NotionOfficialError>,
) -> Vec<DownloadProblem> {
    roots
        .iter()
        .filter_map(|(raw, id)| match unreadable.get(id)? {
            NotionOfficialError::NotFound(d) => Some(DownloadProblem::not_found(
                "roots",
                raw,
                format!("Notion has no page by that id that this credential can see: {d}"),
            )),
            NotionOfficialError::Forbidden(d) => Some(DownloadProblem::forbidden("roots", raw, d)),
            NotionOfficialError::Permanent(_)
            | NotionOfficialError::Transport(..)
            | NotionOfficialError::Unauthorized(_)
            | NotionOfficialError::GaveUp(_) => None,
        })
        .collect()
}

fn format_uuid(s: &str) -> String {
    let raw: String = s.chars().filter(|c| *c != '-').collect();
    if raw.len() != 32 {
        return s.into();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &raw[0..8],
        &raw[8..12],
        &raw[12..16],
        &raw[16..20],
        &raw[20..32]
    )
}

/// The event-store entity names the synthesizer reads a recorded
/// workspace under.
pub const ENTITY_PAGE: &str = "notion_official_page";
pub const ENTITY_MARKDOWN: &str = "notion_page_markdown";
pub const ENTITY_COMMENT: &str = "notion_official_comment";
pub const ENTITY_USER: &str = "notion_user";
pub const ENTITY_ANCHOR_BLOCK: &str = "notion_anchor_block";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uuid_handles_dashed_and_undashed() {
        assert_eq!(
            format_uuid("f9a3f309bde54852944042374cc01dc5"),
            "f9a3f309-bde5-4852-9440-42374cc01dc5"
        );
        let already = "f9a3f309-bde5-4852-9440-42374cc01dc5";
        assert_eq!(format_uuid(already), already);
    }

    /// Nothing attempted is not a failure — an empty roots config,
    /// or a run where every page was already current, must stay green.
    #[test]
    fn all_pages_failed_is_false_when_nothing_failed() {
        assert!(!all_pages_failed(&FetchSummary::default()));
        assert!(!all_pages_failed(&FetchSummary {
            new_pages: 3,
            ..Default::default()
        }));
    }

    /// The regression this guards: a systemic credential failure fails on
    /// every page, and used to be reported as a successful run.
    #[test]
    fn all_pages_failed_is_true_when_every_page_failed() {
        assert!(all_pages_failed(&FetchSummary {
            failed_pages: 1,
            ..Default::default()
        }));
        assert!(all_pages_failed(&FetchSummary {
            failed_pages: 42,
            ..Default::default()
        }));
    }

    /// One bad page among working ones is tolerated — that's the case the
    /// per-page record-and-continue exists for, and it must keep working.
    #[test]
    fn all_pages_failed_is_false_when_some_page_succeeded() {
        for ok in [
            FetchSummary {
                failed_pages: 5,
                new_pages: 1,
                ..Default::default()
            },
            FetchSummary {
                failed_pages: 5,
                upd_pages: 1,
                ..Default::default()
            },
            // A skip only happens after confirming the stored copy is
            // current, which itself proves the credential works.
            FetchSummary {
                failed_pages: 5,
                skipped_pages: 1,
                ..Default::default()
            },
        ] {
            assert!(!all_pages_failed(&ok), "{ok:?} should not be all-failed");
        }
    }
}
