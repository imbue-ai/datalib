//! Keeping one DAV collection (a calendar, an address book) in step with
//! the server over RFC 6578 `sync-collection`, page by page: a fresh
//! listing when the server says the stored token is no longer valid.
//! A server without `sync-collection` fails the collection; there is no
//! second way to list one.
//!
//! A listing names objects, most with their data and some without. Each
//! page is one transaction: what it names is listed ([`super::state`]),
//! what came with its data is stored and held at its etag, what it
//! reports deleted goes, and the token moves. What is listed and not
//! held — named without data, or fetched and failed — is then owed, and
//! [`crate::owed::drain`] fetches it by `multiget`. The provider is an
//! [`ObjectStore`]: how one object becomes its row, and how that row is
//! written, removed, and its collection's token kept.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use sqlx::{Sqlite, SqlitePool, Transaction};
use tracing::info;

use super::state::{self, Resource, RESOURCES};
use super::{escape_xml, report, DavError, DavProps, DavResponse, Multistatus};
use crate::http::{HttpService, LatchkeySettings};
use crate::owed::{self, BatchError, Fetched, Fetcher, Listed, Loop, Outcome};
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::RunProblems;
use datalib_etl::stop::StopFlag;

/// How many truncated `sync-collection` replies one run follows; the
/// next run resumes from the token taken so far.
pub const MAX_SYNC_ROUNDS: usize = 50;

/// How many resources one `multiget` names.
const MULTIGET_BATCH: usize = 100;

/// `multiget` requests in a row that came to nothing before the run
/// leaves the rest for the next one.
const MULTIGET_FAILURE_BUDGET: usize = 3;

/// RFC 6578 defines `sync-collection` only at Depth 0, and a `multiget`
/// names its resources itself. A `calendar-query` searches the members,
/// which is Depth 1 (RFC 4791 §7.8). Fastmail answers all three the same
/// at any Depth, measured live.
const DEPTH_SYNC: &str = "0";
const DEPTH_MULTIGET: &str = "0";
const DEPTH_QUERY: &str = "1";

/// What differs between CalDAV and CardDAV on the wire.
#[derive(Debug, Clone, Copy)]
pub struct CollectionKind {
    pub service: HttpService,
    /// Declares the prefix `data_prop` and `multiget` use.
    pub ns_decl: &'static str,
    /// The property holding an object: `C:calendar-data`.
    pub data_prop: &'static str,
    /// The `multiget` REPORT's root element: `C:calendar-multiget`.
    pub multiget: &'static str,
}

impl CollectionKind {
    pub fn body_sync_collection(&self, prev_token: &str) -> String {
        super::body_sync_collection(prev_token, self.ns_decl, self.data_prop)
    }

    pub fn body_multiget(&self, hrefs: &[String]) -> String {
        let hrefs: String = hrefs
            .iter()
            .map(|h| format!("  <href>{}</href>\n", escape_xml(h)))
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<{root} xmlns="DAV:" {ns}>
  <prop>
    <getetag/>
    <{data}/>
  </prop>
{hrefs}</{root}>
"#,
            root = self.multiget,
            ns = self.ns_decl,
            data = self.data_prop,
        )
    }
}

/// The properties of one stored object: what `multiget` is for.
pub trait ObjectProps: DavProps {
    fn etag(&self) -> Option<&str>;
    /// The object itself, `None` when the listing named it without.
    fn data(&self) -> Option<&str>;
}

/// How a provider stores one collection's objects.
#[async_trait]
pub trait ObjectStore: Sync {
    type Props: ObjectProps + Send;
    type Row: Send + Sync;

    /// The row the object at `href` becomes, or why it cannot become
    /// one (no UID): it is then held at its etag with that as a warning,
    /// and not asked for again until the listing moves.
    fn row(
        &self,
        collection: &str,
        href: &str,
        etag: Option<&str>,
        data: &str,
    ) -> std::result::Result<Self::Row, String>;

    async fn put(&self, tx: &mut Transaction<'_, Sqlite>, rows: &[&Self::Row]) -> Result<()>;

    /// Drop whatever is stored for the object at `href`. Returns how
    /// many objects went.
    async fn remove(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        collection: &str,
        href: &str,
    ) -> Result<u64>;

    /// The collection's `sync-collection` token; `None` clears it.
    async fn set_token(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        collection: &str,
        token: Option<&str>,
    ) -> Result<()>;
}

/// What one run shares across the collections it syncs.
pub struct Run<'a> {
    pub pool: &'a SqlitePool,
    pub stop: &'a StopFlag,
    pub found: &'a RunProblems,
    pub sealer: Option<&'a Sealer>,
}

impl Run<'_> {
    async fn wrote(&self, rows: u64) {
        if let Some(sealer) = self.sealer {
            sealer.wrote(rows).await;
        }
    }
}

/// What syncing one collection came to.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Synced {
    /// Objects stored for the first time.
    pub new: usize,
    /// Objects stored again at a new etag.
    pub updated: usize,
    pub deleted: usize,
    /// Objects fetched that could not become a row.
    pub unusable: usize,
    /// Objects the listing named that would not come; still owed.
    pub failed: usize,
    pub requests: usize,
    /// Why the listing stopped before its end, if it did.
    pub cut_short: Option<String>,
}

/// Keeps `collection` in step with `listing`: its pages, each one
/// transaction; the prune when the listing reached its end; then a
/// `multiget` of what is listed and not held. A stop ends it with what
/// it has, as no failure.
pub async fn sync_collection<'a, S: ObjectStore>(
    run: &'a Run<'a>,
    store: &'a S,
    collection: &'a str,
    label: &'a str,
    mut listing: CollectionSync<'a>,
) -> Result<Synced> {
    let mut synced = Synced::default();
    let stopped = loop {
        let page = match listing.next_page::<S::Props>().await {
            Ok(Some(page)) => page,
            Ok(None) => break false,
            Err(_) if run.stop.requested() => break true,
            Err(e) => {
                synced.requests += listing.requests();
                return Err(e);
            }
        };
        store_page(run, store, collection, page, &mut synced).await?;
    };
    synced.requests += listing.requests();
    if stopped {
        return Ok(synced);
    }
    synced.cut_short = listing.cut_short().map(str::to_string);
    if synced.cut_short.is_none() {
        let mut tx = run.pool.begin().await.context("begin the prune")?;
        let gone = state::finish_listing(&mut tx, collection).await?;
        for href in &gone {
            synced.deleted += store.remove(&mut tx, collection, href).await? as usize;
            state::forget(&mut tx, collection, href).await?;
        }
        tx.commit().await.context("commit the prune")?;
        run.wrote(gone.len() as u64).await;
    }

    let owed = state::owed(run.pool, collection).await?;
    if owed.is_empty() {
        return Ok(synced);
    }
    let keys: Vec<Listed> = owed.iter().map(|r| r.listed(collection)).collect();
    let ids: Vec<String> = keys.iter().map(|k| k.key.clone()).collect();
    let fetcher = Multiget {
        kind: listing.kind,
        url: listing.url,
        latchkey: listing.latchkey,
        store,
        collection,
        href_of: owed
            .iter()
            .map(|r| (state::resource_id(collection, &r.href), r.href.clone()))
            .collect(),
        stored_before: state::stored(run.pool, &ids).await?,
        counts: Default::default(),
    };
    let phase = format!("multiget {label}");
    let l = Loop {
        pool: run.pool,
        table: RESOURCES,
        phase: &phase,
        stop: run.stop,
        found: run.found,
        sealer: run.sealer,
        batch: MULTIGET_BATCH,
        concurrency: 1,
        flush: MULTIGET_BATCH,
        flush_bytes: 0,
        failures_in_a_row: MULTIGET_FAILURE_BUDGET,
    };
    let drained = owed::drain(&l, keys, &fetcher).await?;
    synced.failed += drained.failed;
    synced.requests += fetcher.counts.requests.load(Ordering::Relaxed);
    synced.new += fetcher.counts.new.load(Ordering::Relaxed);
    synced.updated += fetcher.counts.updated.load(Ordering::Relaxed);
    synced.unusable += fetcher.counts.unusable.load(Ordering::Relaxed);
    Ok(synced)
}

/// One page, one transaction: the listing, the objects that came with
/// their data held at their etag, the deletions, the settled hrefs and
/// the token.
async fn store_page<S: ObjectStore>(
    run: &Run<'_>,
    store: &S,
    collection: &str,
    page: Page<S::Props>,
    synced: &mut Synced,
) -> Result<()> {
    let listed: Vec<Resource> = page
        .present
        .iter()
        .map(|r| Resource {
            href: r.href.clone(),
            etag: r.props.etag().map(str::to_string),
        })
        .collect();
    let ids: Vec<String> = listed
        .iter()
        .map(|r| state::resource_id(collection, &r.href))
        .collect();
    let stored_before = state::stored(run.pool, &ids).await?;

    let mut tx = run.pool.begin().await.context("begin a page")?;
    if page.begins_whole {
        state::begin_whole(&mut tx, collection).await?;
    }
    state::list(&mut tx, collection, &listed).await?;
    let mut rows: Vec<S::Row> = Vec::new();
    for (r, id) in page.present.iter().zip(&ids) {
        let Some(data) = r.props.data() else { continue };
        let etag = r.props.etag();
        let outcome = match store.row(collection, &r.href, etag, data) {
            Ok(row) => {
                rows.push(row);
                Outcome::Got(())
            }
            Err(why) => Outcome::Unusable((), datalib_problems::Reason::NoIdentity, why),
        };
        hold(&mut tx, id, etag, &outcome).await?;
        synced.count(stored_before.contains(id), &outcome);
    }
    store.put(&mut tx, &rows.iter().collect::<Vec<_>>()).await?;
    for href in &page.deleted {
        synced.deleted += store.remove(&mut tx, collection, href).await? as usize;
        state::forget(&mut tx, collection, href).await?;
    }
    let named: Vec<&str> = listed
        .iter()
        .map(|r| r.href.as_str())
        .chain(page.deleted.iter().map(String::as_str))
        .collect();
    state::settle(&mut tx, collection, &named).await?;
    store
        .set_token(&mut tx, collection, page.token.as_deref())
        .await?;
    tx.commit().await.context("commit a page")?;
    run.wrote((rows.len() + page.deleted.len()) as u64).await;
    Ok(())
}

/// The sidecar half of storing an object that came with the listing:
/// what [`owed::drain`] writes for a fetched one.
async fn hold(
    tx: &mut Transaction<'_, Sqlite>,
    id: &str,
    etag: Option<&str>,
    outcome: &Outcome<()>,
) -> Result<()> {
    owed::hold(tx, RESOURCES, id, etag).await?;
    if let Outcome::Unusable(_, reason, why) = outcome {
        datalib_etl::doltlite_raw::record_object_unusable(tx, RESOURCES, id, *reason, why).await?;
    }
    Ok(())
}

impl Synced {
    fn count<T>(&mut self, stored_before: bool, outcome: &Outcome<T>) {
        match outcome {
            Outcome::Got(_) if stored_before => self.updated += 1,
            Outcome::Got(_) => self.new += 1,
            Outcome::Unusable(..) => self.unusable += 1,
            _ => {}
        }
    }
}

#[derive(Default)]
struct Counts {
    requests: AtomicUsize,
    new: AtomicUsize,
    updated: AtomicUsize,
    unusable: AtomicUsize,
}

/// Fetches what a listing named without its data, a batch per
/// `multiget`. An object the server does not return is left out of the
/// answer, which the loop reads as failed: it stays owed.
struct Multiget<'a, S: ObjectStore> {
    kind: &'a CollectionKind,
    url: &'a str,
    latchkey: &'a LatchkeySettings,
    store: &'a S,
    collection: &'a str,
    href_of: HashMap<String, String>,
    stored_before: HashSet<String>,
    counts: Counts,
}

#[async_trait]
impl<S: ObjectStore> Fetcher<Option<S::Row>> for Multiget<'_, S> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Option<S::Row>>>, BatchError> {
        let hrefs: Vec<String> = batch
            .iter()
            .filter_map(|l| self.href_of.get(&l.key).cloned())
            .collect();
        let body = self.kind.body_multiget(&hrefs);
        self.counts.requests.fetch_add(1, Ordering::Relaxed);
        let ms: Multistatus<S::Props> = report(
            self.kind.service,
            self.url,
            DEPTH_MULTIGET,
            &body,
            self.latchkey,
        )
        .await
        .map_err(|e| match e.status() {
            Some(401 | 403) => BatchError::Terminal(e.into()),
            _ => BatchError::Batch(e.into()),
        })?;
        let mut by_href: HashMap<String, DavResponse<S::Props>> = ms
            .responses
            .into_iter()
            .map(|r| (r.href.clone(), r))
            .collect();
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let Some(href) = self.href_of.get(&listed.key) else {
                continue;
            };
            let outcome = match by_href.remove(href) {
                Some(r) if matches!(r.status, Some(404 | 410)) => Outcome::Gone,
                Some(r) => match r.props.data() {
                    Some(data) => {
                        match self
                            .store
                            .row(self.collection, href, listed.version.as_deref(), data)
                        {
                            Ok(row) => Outcome::Got(Some(row)),
                            Err(why) => {
                                Outcome::Unusable(None, datalib_problems::Reason::NoIdentity, why)
                            }
                        }
                    }
                    None => Outcome::Failed(format!(
                        "the server answered the multiget without the object's data{}",
                        r.status.map(|s| format!(" (HTTP {s})")).unwrap_or_default()
                    )),
                },
                None => Outcome::Failed(
                    "the collection listed this object, but did not return it when asked"
                        .to_string(),
                ),
            };
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Option<S::Row>>],
    ) -> Result<()> {
        let mut rows: Vec<&S::Row> = Vec::new();
        for f in batch {
            match &f.outcome {
                Outcome::Got(Some(row)) | Outcome::Unusable(Some(row), ..) => rows.push(row),
                Outcome::Gone => {
                    if let Some(href) = self.href_of.get(&f.listed.key) {
                        self.store.remove(tx, self.collection, href).await?;
                        state::unlist(tx, self.collection, href).await?;
                    }
                }
                _ => {}
            }
            let mut counted = Synced::default();
            counted.count(self.stored_before.contains(&f.listed.key), &f.outcome);
            self.counts.new.fetch_add(counted.new, Ordering::Relaxed);
            self.counts
                .updated
                .fetch_add(counted.updated, Ordering::Relaxed);
            self.counts
                .unusable
                .fetch_add(counted.unusable, Ordering::Relaxed);
        }
        self.store.put(tx, &rows).await
    }
}

/// One reply's worth of a listing, ready to store.
#[derive(Debug)]
pub struct Page<P> {
    /// This page is the first of a listing that names everything the
    /// collection holds, not only what changed since a token.
    pub begins_whole: bool,
    /// Every object the page names as present, with its data or not.
    pub present: Vec<DavResponse<P>>,
    /// hrefs the server reports gone (404 or 410).
    pub deleted: Vec<String>,
    /// The token to store once the page is applied; `None` stores none,
    /// as a query leaves nothing to resume from.
    pub token: Option<String>,
}

enum Mode {
    Sync { token: String, rounds: usize },
    Query { body: String },
    Done,
}

/// Drives one collection's listing. Call [`Self::next_page`] until it
/// returns `None`, storing each page and its token as it comes, then
/// [`Self::cut_short`] says whether the listing reached its end.
pub struct CollectionSync<'a> {
    kind: &'a CollectionKind,
    url: &'a str,
    latchkey: &'a LatchkeySettings,
    mode: Mode,
    /// The next page begins a whole listing.
    whole_next: bool,
    cut_short: Option<String>,
    /// The server refused a token this run, so the listing restarted
    /// from nothing; a second refusal fails rather than loop.
    token_refused: bool,
    requests: usize,
}

impl<'a> CollectionSync<'a> {
    /// `sync-collection` from `token`; `None` or empty lists whole.
    pub fn new(
        kind: &'a CollectionKind,
        url: &'a str,
        token: Option<String>,
        latchkey: &'a LatchkeySettings,
    ) -> Self {
        let token = token.unwrap_or_default();
        Self {
            kind,
            url,
            latchkey,
            whole_next: token.is_empty(),
            mode: Mode::Sync { token, rounds: 0 },
            cut_short: None,
            token_refused: false,
            requests: 0,
        }
    }

    /// One query naming everything in a scope `sync-collection` cannot
    /// bound, such as a calendar's window. No token is kept.
    pub fn query(
        kind: &'a CollectionKind,
        url: &'a str,
        body: String,
        latchkey: &'a LatchkeySettings,
    ) -> Self {
        Self {
            kind,
            url,
            latchkey,
            whole_next: true,
            mode: Mode::Query { body },
            cut_short: None,
            token_refused: false,
            requests: 0,
        }
    }

    pub fn requests(&self) -> usize {
        self.requests
    }

    /// Why the listing stopped before its end, once `next_page` has
    /// returned `None`; `None` when it reached the end.
    pub fn cut_short(&self) -> Option<&str> {
        self.cut_short.as_deref()
    }

    pub async fn next_page<P: ObjectProps>(&mut self) -> Result<Option<Page<P>>> {
        loop {
            match std::mem::replace(&mut self.mode, Mode::Done) {
                Mode::Done => return Ok(None),
                Mode::Query { body } => {
                    let ms = self
                        .report::<P>(DEPTH_QUERY, &body)
                        .await
                        .context("query REPORT")?;
                    let reply = Reply::of(ms, self.url);
                    if reply.truncated {
                        self.cut_short = Some(CUT_SHORT.to_string());
                    }
                    return Ok(Some(self.page(reply, None)));
                }
                Mode::Sync { token, rounds } => {
                    if rounds == MAX_SYNC_ROUNDS {
                        self.cut_short = Some(format!(
                            "the listing was still unfinished after {MAX_SYNC_ROUNDS} pages; \
                             the next run carries on from there"
                        ));
                        return Ok(None);
                    }
                    let body = self.kind.body_sync_collection(&token);
                    let refused = match self.report::<P>(DEPTH_SYNC, &body).await {
                        Ok(ms) => {
                            let reply = Reply::of(ms, self.url);
                            let Some(next) = reply.token.clone() else {
                                bail!("sync-collection reply carried no sync-token");
                            };
                            match after_sync_page(&token, &next, reply.truncated) {
                                AfterPage::Done => {}
                                AfterPage::NextPage => {
                                    self.mode = Mode::Sync {
                                        token: next.clone(),
                                        rounds: rounds + 1,
                                    }
                                }
                                AfterPage::CutShort => {
                                    self.cut_short = Some(format!(
                                        "{CUT_SHORT}, and would not page past where it stopped"
                                    ))
                                }
                            }
                            return Ok(Some(self.page(reply, Some(next))));
                        }
                        Err(e) => e,
                    };
                    let (status, precondition) = match &refused {
                        DavError::Http {
                            status,
                            precondition,
                            ..
                        } => (*status, precondition.as_deref()),
                        _ => return Err(refused).context("sync-collection REPORT"),
                    };
                    match on_refusal(status, precondition, &token, self.token_refused) {
                        Refusal::ListWhole => {
                            info!(
                                event = "dav_sync_token_refused",
                                url = %self.url,
                                "the server no longer honours the stored sync token; listing the collection whole"
                            );
                            self.token_refused = true;
                            self.whole_next = true;
                            self.mode = Mode::Sync {
                                token: String::new(),
                                rounds: 0,
                            };
                        }
                        Refusal::Fail => {
                            return Err(refused).context(if self.token_refused {
                                "sync-collection REPORT, after listing from nothing \
                                 because the stored token was refused"
                            } else {
                                "sync-collection REPORT"
                            })
                        }
                    }
                }
            }
        }
    }

    async fn report<P: DavProps>(
        &mut self,
        depth: &str,
        body: &str,
    ) -> Result<Multistatus<P>, DavError> {
        self.requests += 1;
        report(self.kind.service, self.url, depth, body, self.latchkey).await
    }

    fn page<P: ObjectProps>(&mut self, reply: Reply<P>, token: Option<String>) -> Page<P> {
        Page {
            begins_whole: std::mem::take(&mut self.whole_next),
            present: reply.present,
            deleted: reply.deleted,
            token,
        }
    }
}

const CUT_SHORT: &str = "the server stopped the listing short (507)";

/// One listing reply, sorted.
struct Reply<P> {
    /// Objects the reply names as present, the collection itself left out.
    present: Vec<DavResponse<P>>,
    deleted: Vec<String>,
    /// A 507 anywhere: RFC 6578 §3.6 puts it on the collection when the
    /// server stops a listing short, and no 507 means a whole reply.
    truncated: bool,
    token: Option<String>,
}

impl<P> Reply<P> {
    fn of(ms: Multistatus<P>, collection_url: &str) -> Self {
        let own = collection_url.trim_end_matches('/');
        let is_collection = |href: &str| {
            super::absolutize(collection_url, href).is_some_and(|u| u.trim_end_matches('/') == own)
        };
        let mut out = Reply {
            present: Vec::new(),
            deleted: Vec::new(),
            truncated: false,
            token: ms.sync_token,
        };
        for r in ms.responses {
            match r.status {
                Some(507) => out.truncated = true,
                Some(404 | 410) => out.deleted.push(r.href),
                _ if is_collection(&r.href) => {}
                _ => out.present.push(r),
            }
        }
        out
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AfterPage {
    Done,
    NextPage,
    CutShort,
}

fn after_sync_page(prev_token: &str, next_token: &str, truncated: bool) -> AfterPage {
    if !truncated {
        AfterPage::Done
    } else if next_token == prev_token {
        AfterPage::CutShort
    } else {
        AfterPage::NextPage
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// Drop the token and list from nothing.
    ListWhole,
    Fail,
}

/// RFC 6578 §3.2: a token the server no longer honours is a 403 naming
/// the `valid-sync-token` precondition, and the client lists again from
/// none. Anything else is an error, and so is a second refusal in one
/// run: a server that refuses the token it just handed out would
/// otherwise be listed from nothing forever.
fn on_refusal(
    status: u16,
    precondition: Option<&str>,
    token: &str,
    refused_before: bool,
) -> Refusal {
    let token_expired = status == 403 && precondition == Some("valid-sync-token");
    if token_expired && !token.is_empty() && !refused_before {
        Refusal::ListWhole
    } else {
        Refusal::Fail
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct Props;

    impl DavProps for Props {
        fn leaf(&mut self, _name: &str, _parent: &str, _text: String) {}
    }

    fn response(href: &str, status: Option<u16>) -> DavResponse<Props> {
        DavResponse {
            href: href.into(),
            status,
            props: Props,
        }
    }

    #[test]
    fn a_reply_sorts_present_deleted_and_truncated_and_drops_the_collection() {
        let ms = Multistatus {
            responses: vec![
                response("/cal/", None),
                response("/cal/a.ics", None),
                response("/cal/b.ics", Some(404)),
                response("/cal/c.ics", Some(410)),
                response("/cal/", Some(507)),
            ],
            sync_token: Some("t2".into()),
        };
        let reply = Reply::of(ms, "https://dav.test/cal");
        let present: Vec<&str> = reply.present.iter().map(|r| r.href.as_str()).collect();
        assert_eq!(present, vec!["/cal/a.ics"]);
        assert_eq!(reply.deleted, vec!["/cal/b.ics", "/cal/c.ics"]);
        assert!(reply.truncated);
        assert_eq!(reply.token.as_deref(), Some("t2"));
    }

    #[test]
    fn a_truncated_page_pages_on_only_while_its_token_moves() {
        assert_eq!(after_sync_page("t1", "t1", false), AfterPage::Done);
        assert_eq!(after_sync_page("t1", "t2", true), AfterPage::NextPage);
        assert_eq!(after_sync_page("t2", "t2", true), AfterPage::CutShort);
        assert_eq!(after_sync_page("", "t1", true), AfterPage::NextPage);
    }

    #[test]
    fn only_a_token_the_server_calls_invalid_lists_whole_again() {
        let expired = Some("valid-sync-token");
        assert_eq!(on_refusal(403, expired, "t1", false), Refusal::ListWhole);
        assert_eq!(on_refusal(403, None, "t1", false), Refusal::Fail);
        assert_eq!(
            on_refusal(403, Some("supported-report"), "", false),
            Refusal::Fail
        );
        assert_eq!(on_refusal(409, None, "t1", false), Refusal::Fail);
        assert_eq!(on_refusal(410, None, "t1", false), Refusal::Fail);
        assert_eq!(on_refusal(501, None, "", false), Refusal::Fail);
    }

    /// A server that refuses every token it hands out, even mid-way
    /// through a fresh listing, would otherwise restart that listing
    /// forever.
    #[test]
    fn a_second_refused_token_fails_instead_of_restarting() {
        assert_eq!(
            on_refusal(403, Some("valid-sync-token"), "t2", true),
            Refusal::Fail
        );
    }

    #[test]
    fn a_multiget_names_each_href_escaped() {
        let kind = CollectionKind {
            service: HttpService::Caldav,
            ns_decl: r#"xmlns:C="urn:ietf:params:xml:ns:caldav""#,
            data_prop: "C:calendar-data",
            multiget: "C:calendar-multiget",
        };
        let body = kind.body_multiget(&["/cal/a&b.ics".into()]);
        assert!(
            body.contains("<C:calendar-multiget xmlns=\"DAV:\" xmlns:C="),
            "{body}"
        );
        assert!(body.contains("<href>/cal/a&amp;b.ics</href>"), "{body}");
        assert!(body.contains("<C:calendar-data/>"), "{body}");
    }
}
