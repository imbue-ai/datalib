//! What each loop fetches and how a flush of it is stored, one
//! [`Fetcher`] per kind: a page's body, an attachment's bytes, a page's
//! comments listed whole, a commented block, a user. Each is one request
//! per record; the loop (`datalib_etl_web::owed`) owns the stop, the
//! failure budget, the flush and what each outcome means for the
//! record's sidecar.

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use anyhow::Result;
use async_trait::async_trait;
use datalib_etl::blob_cas::CasInsert;
use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService};
use datalib_etl_web::owed::{BatchError, Fetched, Fetcher, Listed, Outcome};
use serde_json::Value;
use sqlx::{Sqlite, Transaction};

use super::db::{CommentAnchorUpsert, CommentUpsert, PageMarkdownUpsert};
use super::official::NotionOfficialError;
use super::schema_raw::split_edge_key;
use super::{markdown, run_over, slots, Ctx};

/// Follow-up fetches for subtrees the markdown response was too large to
/// inline, appended to the body in the order the holes appeared.
///
/// Only `alt`-less holes are followed: an `alt`-bearing one names a
/// block type markdown cannot express, and fetching it returns a stub
/// carrying the same id — an infinite regress. The cap bounds the pass
/// so a single pathological page (one measured page wanted ~1,385 of
/// these) cannot dominate a run.
pub const MAX_HOLE_FOLLOWUPS: usize = 64;

/// The credential may not read comments: an integration without the
/// capability. One row for the run, and no page the worse for it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CommentsForbidden(pub String);

/// What a request of one record came to, before the loop's outcomes: an
/// error here is the run's to decide on, not the record's.
fn outcome<T>(
    ctx: &Ctx<'_>,
    answer: Result<Outcome<T>, NotionOfficialError>,
) -> Result<Outcome<T>, BatchError> {
    match answer {
        Ok(outcome) => Ok(outcome),
        Err(e) if e.ends_the_run() => Err(BatchError::Abort(run_over(e))),
        // The transport refuses every request once a stop is asked for;
        // the record is the next run's.
        Err(e) if ctx.stop().requested() => Err(BatchError::Batch(e.into())),
        Err(e) => Ok(Outcome::Failed(e.to_string())),
    }
}

// ── bodies ─────────────────────────────────────────────────────────────

/// A page's body as `page_markdown` stores it, and what it lists.
pub struct Body {
    row: PageMarkdownUpsert,
    /// The attachment slots the body names, each an edge to list.
    slots: Vec<String>,
    /// Whether every subtree the response left out was fetched: an
    /// incomplete body may be missing links, so its old edges stand.
    complete: bool,
}

pub(crate) struct Bodies<'a>(pub(crate) &'a Ctx<'a>);

#[async_trait]
impl Fetcher<Body> for Bodies<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Body>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let answer = self.one(&listed.key).await;
            out.push(Fetched {
                outcome: outcome(self.0, answer)?,
                listed,
            });
            self.0.opts.progress.inc(1);
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Body>],
    ) -> Result<()> {
        for f in batch {
            if let Outcome::Got(body) | Outcome::Unusable(body, ..) = &f.outcome {
                self.0
                    .db
                    .store_body(tx, &body.row, &body.slots, body.complete)
                    .await?;
            }
        }
        Ok(())
    }

    fn weight(&self, body: &Body) -> usize {
        body.row.markdown.len()
    }
}

impl Bodies<'_> {
    /// One page's body: its holes followed, its signed URLs kept for the
    /// attachment loop and reduced to slots in what is stored. A 404 is
    /// an empty body, held: Notion answers it for a page deleted or no
    /// longer shared, and the page is asked again only once it is edited.
    async fn one(&self, pid: &str) -> Result<Outcome<Body>, NotionOfficialError> {
        let ctx = self.0;
        let resp = match ctx.client.get_page_markdown(pid).await {
            Ok(resp) => resp,
            Err(NotionOfficialError::NotFound(_)) => {
                return Ok(Outcome::Got(Body {
                    row: PageMarkdownUpsert {
                        id: pid.to_string(),
                        ..Default::default()
                    },
                    slots: Vec::new(),
                    complete: true,
                }));
            }
            Err(e) => return Err(e),
        };
        let mut body = markdown::parse(&resp);
        let holes = if body.truncated {
            self.fill_holes(&mut body).await?
        } else {
            Holes::default()
        };
        if let Some((block, e)) = holes.failed.first() {
            return Ok(Outcome::Failed(format!(
                "{} truncated subtree(s) did not fetch, so the body is incomplete; \
                 first: block {block}: {e}",
                holes.failed.len()
            )));
        }
        let permanent = body.permanent_holes();
        if !permanent.is_empty() {
            tracing::info!(
                event = "notion_unrepresentable_blocks",
                page = pid,
                count = permanent.len(),
                "page contains block types markdown cannot express"
            );
        }
        // Signed URLs are captured before the rewrite strips them: they
        // are the only way to fetch the bytes, and they live only here.
        ctx.signed
            .lock()
            .unwrap()
            .insert(pid.to_string(), signed_by_slot(&body.markdown));
        let (stable, slots) = slots::rewrite(&body.markdown);
        let unresolved: Vec<&str> = body
            .unresolved
            .iter()
            .map(|u| u.block_id.as_str())
            .collect();
        if stable.is_empty() {
            ctx.counts.empty_bodies.fetch_add(1, Ordering::Relaxed);
        }
        let fetched = Body {
            row: PageMarkdownUpsert {
                id: pid.to_string(),
                markdown: stable,
                truncated: body.truncated,
                unresolved_block_ids: (!unresolved.is_empty())
                    .then(|| serde_json::to_string(&unresolved).unwrap_or_default()),
            },
            slots,
            complete: holes.over_cap.is_none(),
        };
        Ok(match holes.over_cap {
            Some(wanted) => {
                ctx.counts
                    .pages_left_incomplete
                    .fetch_add(1, Ordering::Relaxed);
                Outcome::Unusable(
                    fetched,
                    datalib_problems::Reason::DeliberateLoss,
                    format!(
                        "the body has {wanted} truncated subtrees and one run follows \
                         {MAX_HOLE_FOLLOWUPS}, so the rest is missing"
                    ),
                )
            }
            None => Outcome::Got(fetched),
        })
    }

    async fn fill_holes(
        &self,
        body: &mut markdown::PageBody,
    ) -> Result<Holes, NotionOfficialError> {
        let ctx = self.0;
        let mut holes = Holes::default();
        let todo: Vec<String> = body
            .fetchable_holes()
            .map(|u| u.block_id.clone())
            .take(MAX_HOLE_FOLLOWUPS)
            .collect();
        if todo.is_empty() {
            return Ok(holes);
        }
        let total_fetchable = body.fetchable_holes().count();
        if total_fetchable > MAX_HOLE_FOLLOWUPS {
            holes.over_cap = Some(total_fetchable);
        }
        for id in todo {
            match ctx.client.get_page_markdown(&id).await {
                Ok(resp) => {
                    let part = markdown::parse(&resp);
                    if part.markdown.is_empty() {
                        continue;
                    }
                    body.markdown.push('\n');
                    body.markdown.push_str(&part.markdown);
                    for c in part.child_pages {
                        if !body.child_pages.contains(&c) {
                            body.child_pages.push(c);
                        }
                    }
                    ctx.counts.hole_followups.fetch_add(1, Ordering::Relaxed);
                }
                // The block went since the body was read.
                Err(NotionOfficialError::NotFound(_)) => {}
                Err(e) if e.ends_the_run() || ctx.stop().requested() => return Err(e),
                Err(e) => holes.failed.push((id, e.to_string())),
            }
        }
        Ok(holes)
    }
}

/// What the follow-ups for a truncated body could not fill.
#[derive(Default)]
struct Holes {
    /// `(block id, error)` for each follow-up that failed.
    failed: Vec<(String, String)>,
    /// How many follow-ups the body wanted, when that was more than
    /// one run follows.
    over_cap: Option<usize>,
}

/// Map each slot back to the live signed URL it came from, so the bytes
/// can still be fetched after the markdown has been rewritten.
fn signed_by_slot(raw_markdown: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = raw_markdown;
    while let Some(i) = rest.find("http") {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '<' | '>'))
            .unwrap_or(tail.len());
        let url = &tail[..end];
        if let Some(slot) = slots::slot_of(url) {
            out.entry(slot).or_insert_with(|| url.to_string());
        }
        rest = &tail[end.max(1)..];
    }
    out
}

// ── attachments ────────────────────────────────────────────────────────

/// What one edge's fetch came to.
pub enum Blob {
    /// The CAS holds the bytes already, under another page, by this hash.
    Held(String),
    /// Bytes this run fetched, and their type. The CAS names them.
    Fetched(Vec<u8>, Option<String>),
}

pub(crate) struct Attachments<'a>(pub(crate) &'a Ctx<'a>);

#[async_trait]
impl Fetcher<Blob> for Attachments<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Blob>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let answer = self.one(&listed.key).await;
            out.push(Fetched {
                outcome: outcome(self.0, answer)?,
                listed,
            });
            self.0.opts.progress.inc(1);
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Blob>],
    ) -> Result<()> {
        let inserts: Vec<CasInsert<'_, &str>> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(Blob::Fetched(bytes, content_type))
                | Outcome::Unusable(Blob::Fetched(bytes, content_type), ..) => Some(CasInsert {
                    id: f.listed.key.as_str(),
                    bytes,
                    content_type: content_type.as_deref(),
                }),
                _ => None,
            })
            .collect();
        let stored = self.0.db.cas().put_many(inserts).await?;
        for f in batch {
            match &f.outcome {
                Outcome::Got(blob) | Outcome::Unusable(blob, ..) => {
                    let blake3 = match blob {
                        Blob::Held(held) => held,
                        Blob::Fetched(..) => &stored[f.listed.key.as_str()],
                    };
                    self.0.db.store_blob(tx, &f.listed.key, blake3).await?;
                }
                Outcome::Gone => self.0.db.forget_attachment(tx, &f.listed.key).await?,
                Outcome::Failed(_) | Outcome::Skipped(..) => {}
            }
        }
        Ok(())
    }

    fn weight(&self, blob: &Blob) -> usize {
        match blob {
            Blob::Fetched(bytes, _) => bytes.len(),
            Blob::Held(_) => 0,
        }
    }
}

impl Attachments<'_> {
    /// One edge's bytes. The signed URL lives only in the body's
    /// response: the one the body loop read this run, or a fresh read
    /// of the body. A slot the body no longer links, or a file Notion
    /// no longer serves, is gone.
    async fn one(&self, key: &str) -> Result<Outcome<Blob>, NotionOfficialError> {
        let ctx = self.0;
        let Some((pid, slot)) = split_edge_key(key) else {
            return Ok(Outcome::Failed(format!("{key}: not an attachment key")));
        };
        if let Some(blake3) = ctx.db.blake3_of_slot(slot).await.map_err(store_failed)? {
            return Ok(Outcome::Got(Blob::Held(blake3)));
        }
        let Some(url) = self.signed_url(pid, slot).await? else {
            return Ok(Outcome::Gone);
        };
        let mut req = HttpRequest::get(HttpService::Notion, &url);
        if !host_is_notion(&url) {
            req = req.plain();
        }
        match latchkey_curl(&req).await {
            Ok(resp) if (200..300).contains(&resp.status) => {
                let content_type = resp.header("content-type").map(String::from);
                ctx.counts.new_blobs.fetch_add(1, Ordering::Relaxed);
                Ok(Outcome::Got(Blob::Fetched(resp.body, content_type)))
            }
            Ok(resp) if matches!(resp.status, 404 | 410) => Ok(Outcome::Gone),
            Ok(resp) => Ok(Outcome::Failed(format!("HTTP {}", resp.status))),
            Err(e @ HttpError::GaveUp { .. }) => Err(NotionOfficialError::GaveUp(e.to_string())),
            Err(e) => Err(NotionOfficialError::Permanent(e.to_string())),
        }
    }

    async fn signed_url(
        &self,
        pid: &str,
        slot: &str,
    ) -> Result<Option<String>, NotionOfficialError> {
        let ctx = self.0;
        if let Some(by_slot) = ctx.signed.lock().unwrap().get(pid) {
            return Ok(by_slot.get(slot).cloned());
        }
        let resp = match ctx.client.get_page_markdown(pid).await {
            Ok(resp) => resp,
            Err(NotionOfficialError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let by_slot = signed_by_slot(&markdown::parse(&resp).markdown);
        let url = by_slot.get(slot).cloned();
        ctx.signed.lock().unwrap().insert(pid.to_string(), by_slot);
        Ok(url)
    }
}

fn store_failed(e: anyhow::Error) -> NotionOfficialError {
    NotionOfficialError::Permanent(format!("the store would not answer: {e:#}"))
}

/// True when `url`'s host is a Notion-owned domain (and so its fetch
/// should go through latchkey). Everything else — chiefly the pre-signed
/// S3 links Notion hands out for uploaded files — is fetched with plain
/// curl. Crude host extraction (no `url` crate dep).
fn host_is_notion(url: &str) -> bool {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    host == "notion.so"
        || host.ends_with(".notion.so")
        || host == "notion.com"
        || host.ends_with(".notion.com")
}

// ── comments ───────────────────────────────────────────────────────────

pub(crate) struct Comments<'a>(pub(crate) &'a Ctx<'a>);

#[async_trait]
impl Fetcher<Vec<CommentUpsert>> for Comments<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> Result<Vec<Fetched<Vec<CommentUpsert>>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let answer = match self.all_comments(&listed.key).await {
                Ok(comments) => Ok(Outcome::Got(comments)),
                Err(NotionOfficialError::NotFound(_)) => Ok(Outcome::Got(Vec::new())),
                Err(NotionOfficialError::Forbidden(d)) => {
                    return Err(BatchError::Abort(CommentsForbidden(d).into()));
                }
                Err(e) => Err(e),
            };
            out.push(Fetched {
                outcome: outcome(self.0, answer)?,
                listed,
            });
            self.0.opts.progress.inc(1);
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Vec<CommentUpsert>>],
    ) -> Result<()> {
        for f in batch {
            if let Outcome::Got(rows) | Outcome::Unusable(rows, ..) = &f.outcome {
                self.0.db.store_comments(tx, &f.listed.key, rows).await?;
                self.0
                    .counts
                    .comments
                    .fetch_add(rows.len(), Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

impl Comments<'_> {
    /// Every page of `GET /v1/comments?block_id=`: the page's whole
    /// discussion set, block-anchored threads included.
    async fn all_comments(&self, pid: &str) -> Result<Vec<CommentUpsert>, NotionOfficialError> {
        let mut out: Vec<CommentUpsert> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let resp = self.0.client.get_comments(pid, cursor.as_deref()).await?;
            if let Some(arr) = resp.get("results").and_then(Value::as_array) {
                out.extend(
                    arr.iter()
                        .filter_map(|c| CommentUpsert::from_object(c, pid)),
                );
            }
            if !resp
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Ok(out);
            }
            match resp.get("next_cursor").and_then(Value::as_str) {
                Some(c) => cursor = Some(c.to_string()),
                None => {
                    return Err(NotionOfficialError::Permanent(format!(
                    "GET /comments?block_id={pid}: has_more with no cursor; the rest was not read"
                )))
                }
            }
        }
    }
}

// ── anchors and users ──────────────────────────────────────────────────

/// A commented block's type and text; both `None` for a block Notion
/// no longer has, which comments say with `original_content_deleted`.
pub struct Anchor {
    block_type: Option<String>,
    plain_text: Option<String>,
}

pub(crate) struct Anchors<'a> {
    pub(crate) ctx: &'a Ctx<'a>,
    /// The page each listed block's comment is on.
    pub(crate) page_by_block: HashMap<String, Option<String>>,
}

#[async_trait]
impl Fetcher<Anchor> for Anchors<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<Anchor>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let answer = match self.ctx.client.get_block(&listed.key).await {
                Ok(b) => Ok(Outcome::Got(Anchor {
                    block_type: b.get("type").and_then(Value::as_str).map(String::from),
                    plain_text: block_plain_text(&b),
                })),
                Err(NotionOfficialError::NotFound(_)) => Ok(Outcome::Got(Anchor {
                    block_type: None,
                    plain_text: None,
                })),
                Err(e) => Err(e),
            };
            out.push(Fetched {
                outcome: outcome(self.ctx, answer)?,
                listed,
            });
            self.ctx.opts.progress.inc(1);
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Anchor>],
    ) -> Result<()> {
        for f in batch {
            if let Outcome::Got(anchor) | Outcome::Unusable(anchor, ..) = &f.outcome {
                self.ctx
                    .db
                    .store_anchor(
                        tx,
                        &CommentAnchorUpsert {
                            id: f.listed.key.clone(),
                            page_id: self.page_by_block.get(&f.listed.key).cloned().flatten(),
                            block_type: anchor.block_type.clone(),
                            plain_text: anchor.plain_text.clone(),
                        },
                    )
                    .await?;
                self.ctx
                    .counts
                    .anchors_resolved
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

/// Plain text of a block, whatever its type carries rich text under.
fn block_plain_text(block: &Value) -> Option<String> {
    let t = block.get("type")?.as_str()?;
    let rt = block.get(t)?.get("rich_text")?.as_array()?;
    let s: String = rt
        .iter()
        .filter_map(|x| x.get("plain_text").and_then(Value::as_str))
        .collect();
    (!s.trim().is_empty()).then_some(s)
}

/// A user as Notion describes one, or nothing for an id it no longer
/// has.
pub struct User {
    name: Option<String>,
    payload: Option<String>,
}

pub(crate) struct Users<'a>(pub(crate) &'a Ctx<'a>);

#[async_trait]
impl Fetcher<User> for Users<'_> {
    async fn fetch(&self, batch: Vec<Listed>) -> Result<Vec<Fetched<User>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let answer = match self.0.client.get_user(&listed.key).await {
                Ok(u) => Ok(Outcome::Got(User {
                    name: u.get("name").and_then(Value::as_str).map(String::from),
                    payload: Some(u.to_string()),
                })),
                Err(NotionOfficialError::NotFound(_)) => Ok(Outcome::Got(User {
                    name: None,
                    payload: None,
                })),
                Err(e) => Err(e),
            };
            out.push(Fetched {
                outcome: outcome(self.0, answer)?,
                listed,
            });
            self.0.opts.progress.inc(1);
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<User>],
    ) -> Result<()> {
        for f in batch {
            if let Outcome::Got(user) | Outcome::Unusable(user, ..) = &f.outcome {
                self.0
                    .db
                    .store_user(
                        tx,
                        &f.listed.key,
                        user.name.as_deref(),
                        user.payload.as_deref(),
                    )
                    .await?;
                self.0.counts.users_resolved.fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notion_hosts_go_through_latchkey_and_s3_does_not() {
        assert!(host_is_notion("https://www.notion.so/image/x.png"));
        assert!(host_is_notion("https://api.notion.com/v1/pages/x"));
        assert!(!host_is_notion(
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/x.png?X-Amz-Signature=a"
        ));
    }

    #[test]
    fn a_signed_url_is_found_by_its_slot() {
        let signed =
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/a.png?X-Amz-Signature=abc";
        let md = format!("![a]({signed}) and [doc](https://docs.example.test/x?gid=1)");
        let by_slot = signed_by_slot(&md);
        assert_eq!(
            by_slot
                .get("https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/a.png")
                .map(String::as_str),
            Some(signed)
        );
        assert_eq!(by_slot.len(), 1);
    }
}
