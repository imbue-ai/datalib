//! Keeping one DAV collection (a calendar, an address book) in step with
//! the server over RFC 6578 `sync-collection`, page by page: a fresh
//! listing when the server says the stored token is no longer valid, and
//! `multiget` for what a listing names without its data. A server
//! without `sync-collection` fails the collection; there is no second
//! way to list one. What a listing means for the store is [`super::state`].

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use tracing::info;

use super::{escape_xml, report, DavError, DavProps, DavResponse, Multistatus};
use crate::http::{HttpService, LatchkeySettings};

/// How many truncated `sync-collection` replies one run follows; the
/// next run resumes from the token taken so far.
pub const MAX_SYNC_ROUNDS: usize = 50;

/// How many resources one `multiget` names.
const MULTIGET_BATCH: usize = 100;

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
    /// The object itself, `None` when the listing named it without.
    fn data(&self) -> Option<&str>;
}

/// One reply's worth of changes, ready to store.
#[derive(Debug)]
pub struct Page<P> {
    /// This page is the first of a listing that names everything the
    /// collection holds, not only what changed since a token.
    pub begins_whole: bool,
    /// Every object the page names as present, stored or not.
    pub listed: Vec<String>,
    /// Objects with their data, fetched by `multiget` where the listing
    /// named them without it.
    pub changed: Vec<DavResponse<P>>,
    /// hrefs the server reports gone (404 or 410).
    pub deleted: Vec<String>,
    /// hrefs listed whose data `multiget` did not return either.
    pub unfetched: Vec<String>,
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
                    return self.page(reply, None).await.map(Some);
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
                            return self.page(reply, Some(next)).await.map(Some);
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

    async fn page<P: ObjectProps>(
        &mut self,
        reply: Reply<P>,
        token: Option<String>,
    ) -> Result<Page<P>> {
        let listed: Vec<String> = reply.present.iter().map(|r| r.href.clone()).collect();
        let (mut changed, without): (Vec<_>, Vec<_>) = reply
            .present
            .into_iter()
            .partition(|r| r.props.data().is_some());
        let without: Vec<String> = without.into_iter().map(|r| r.href).collect();
        for chunk in without.chunks(MULTIGET_BATCH) {
            let body = self.kind.body_multiget(chunk);
            let ms = self
                .report::<P>(DEPTH_MULTIGET, &body)
                .await
                .context("multiget REPORT")?;
            changed.extend(
                ms.responses
                    .into_iter()
                    .filter(|r| r.status.is_none() && r.props.data().is_some()),
            );
        }
        let fetched: HashSet<&str> = changed.iter().map(|r| r.href.as_str()).collect();
        let unfetched = without
            .iter()
            .filter(|h| !fetched.contains(h.as_str()))
            .cloned()
            .collect();
        Ok(Page {
            begins_whole: std::mem::take(&mut self.whole_next),
            listed,
            changed,
            deleted: reply.deleted,
            unfetched,
            token,
        })
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
