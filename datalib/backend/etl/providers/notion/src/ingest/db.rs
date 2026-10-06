//! Doltlite-backed raw store for the Notion provider.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::Row;

use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::doltlite_raw::{self as dr};

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::{full_ddl, NotionAttachmentRow};

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl());

/// One row for [`RawDb::upsert_pages`]. `payload` is `None` for a
/// discovery upsert that records a page exists without clobbering a
/// body already fetched.
#[derive(Debug, Clone, Default)]
pub struct PageUpsert {
    pub id: String,
    pub parent_type: Option<String>,
    pub parent_id: Option<String>,
    pub in_trash: bool,
    pub created_time: Option<String>,
    pub last_edited_time: Option<String>,
    pub url: Option<String>,
    pub payload: Option<String>,
}

/// What the download loop wants to know about a page before it decides
/// whether to issue a detail fetch.
#[derive(Debug, Clone)]
pub struct PageState {
    pub last_edited_time: Option<String>,
    pub has_payload: bool,
    /// The `last_edited_time` the stored body was fetched at. Behind the
    /// page's own when a run stored the page and not its body.
    pub body_edited_time: Option<String>,
}

/// One page's body for [`RawDb::upsert_page_markdown`].
///
/// `markdown` must already have had its attachment URLs reduced to
/// slots (`ingest::slots::rewrite`). Storing what the API returned
/// would make an unchanged page differ from itself on every run.
#[derive(Debug, Clone, Default)]
pub struct PageMarkdownUpsert {
    pub id: String,
    pub markdown: String,
    pub truncated: bool,
    /// JSON array of block ids the response could not inline.
    pub unresolved_block_ids: Option<String>,
    /// The `last_edited_time` this body was fetched at, so a later run
    /// can tell whether the stored body is current without re-fetching.
    pub source_last_edited_time: Option<String>,
}

/// One anchor for [`RawDb::upsert_comment_anchors`]: the block a
/// comment hangs off, and the text it hangs off of.
#[derive(Debug, Clone, Default)]
pub struct CommentAnchorUpsert {
    pub id: String,
    pub page_id: Option<String>,
    pub block_type: Option<String>,
    pub plain_text: Option<String>,
}

/// One comment for [`RawDb::upsert_comments`].
#[derive(Debug, Clone, Default)]
pub struct CommentUpsert {
    pub id: String,
    pub discussion_id: Option<String>,
    pub parent_type: Option<String>,
    pub parent_id: Option<String>,
    pub page_id: Option<String>,
    pub created_time: Option<String>,
    pub last_edited_time: Option<String>,
    pub payload: String,
}

impl RawDb {
    pub async fn page_states(&self) -> Result<std::collections::HashMap<String, PageState>> {
        let rows = sqlx::query(
            "SELECT p.id, p.last_edited_time, p.payload IS NOT NULL AS has_payload, \
                    m.source_last_edited_time AS body_edited_time \
             FROM pages p LEFT JOIN page_markdown m ON m.id = p.id",
        )
        .fetch_all(self.pool())
        .await
        .context("select page_states")?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let last: Option<String> = r.try_get("last_edited_time").ok();
            let has: i64 = r.try_get("has_payload").unwrap_or(0);
            out.insert(
                id,
                PageState {
                    last_edited_time: last,
                    has_payload: has != 0,
                    body_edited_time: r.try_get("body_edited_time").ok().flatten(),
                },
            );
        }
        Ok(out)
    }

    pub async fn ensure_id(&self, table: &str, id: &str) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin ensure_id tx")?;
        dr::ensure_object_row(&mut tx, table, id).await?;
        tx.commit().await.context("commit ensure_id tx")?;
        Ok(())
    }

    /// Batch upsert pages. We compare-on-upsert: if the stored
    /// `last_edited_time` already matches, we leave payload alone (the
    /// list pass shouldn't clobber a freshly-fetched detail body with a
    /// truncated list-only payload). When the incoming
    /// `last_edited_time` differs, payload is overwritten verbatim.
    pub async fn upsert_pages(&self, rows: &[PageUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await.context("begin pages tx")?;
        for r in rows {
            let sql = if r.payload.is_some() {
                "INSERT INTO pages (id, parent_type, parent_id, in_trash, created_time, last_edited_time, url, payload)
                 VALUES (?, ?, ?, ?, ?, ?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    parent_type = COALESCE(excluded.parent_type, pages.parent_type),
                    parent_id = COALESCE(excluded.parent_id, pages.parent_id),
                    in_trash = excluded.in_trash,
                    created_time = COALESCE(excluded.created_time, pages.created_time),
                    last_edited_time = excluded.last_edited_time,
                    url = COALESCE(excluded.url, pages.url),
                    payload = excluded.payload"
            } else {
                "INSERT INTO pages (id, parent_type, parent_id, in_trash, created_time, last_edited_time, url)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    parent_type = COALESCE(excluded.parent_type, pages.parent_type),
                    parent_id = COALESCE(excluded.parent_id, pages.parent_id),
                    in_trash = excluded.in_trash,
                    created_time = COALESCE(excluded.created_time, pages.created_time),
                    last_edited_time = COALESCE(excluded.last_edited_time, pages.last_edited_time),
                    url = COALESCE(excluded.url, pages.url)"
            };
            let mut q = sqlx::query(sql)
                .bind(&r.id)
                .bind(&r.parent_type)
                .bind(&r.parent_id)
                .bind(r.in_trash as i64)
                .bind(&r.created_time)
                .bind(&r.last_edited_time)
                .bind(&r.url);
            if let Some(p) = &r.payload {
                q = q.bind(p);
            }
            q.execute(&mut *tx)
                .await
                .with_context(|| format!("upsert page {}", r.id))?;
            if r.payload.is_some() {
                dr::record_object_attempt(&mut tx, "pages", &r.id, None).await?;
            } else {
                sqlx::query("INSERT OR IGNORE INTO pages_bookkeeping (id) VALUES (?)")
                    .bind(&r.id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| format!("pre-seed pages_bookkeeping {}", r.id))?;
            }
        }
        tx.commit().await.context("commit pages tx")?;
        Ok(())
    }

    pub async fn upsert_page_markdown(&self, rows: &[PageMarkdownUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool()
            .begin()
            .await
            .context("begin page_markdown tx")?;
        for r in rows {
            sqlx::query(
                "INSERT INTO page_markdown (id, markdown, truncated, unresolved_block_ids, source_last_edited_time)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    markdown = excluded.markdown,
                    truncated = excluded.truncated,
                    unresolved_block_ids = excluded.unresolved_block_ids,
                    source_last_edited_time = excluded.source_last_edited_time",
            )
            .bind(&r.id)
            .bind(&r.markdown)
            .bind(r.truncated as i64)
            .bind(&r.unresolved_block_ids)
            .bind(&r.source_last_edited_time)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert page_markdown {}", r.id))?;
            dr::record_object_attempt(&mut tx, "page_markdown", &r.id, None).await?;
        }
        tx.commit().await.context("commit page_markdown tx")?;
        Ok(())
    }

    pub async fn upsert_comments(&self, rows: &[CommentUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await.context("begin comments tx")?;
        for r in rows {
            sqlx::query(
                "INSERT INTO comments (id, discussion_id, parent_type, parent_id, page_id, created_time, last_edited_time, payload)
                 VALUES (?, ?, ?, ?, ?, ?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    discussion_id = COALESCE(excluded.discussion_id, comments.discussion_id),
                    parent_type = excluded.parent_type,
                    parent_id = excluded.parent_id,
                    page_id = COALESCE(excluded.page_id, comments.page_id),
                    created_time = COALESCE(excluded.created_time, comments.created_time),
                    last_edited_time = excluded.last_edited_time,
                    payload = excluded.payload",
            )
            .bind(&r.id)
            .bind(&r.discussion_id)
            .bind(&r.parent_type)
            .bind(&r.parent_id)
            .bind(&r.page_id)
            .bind(&r.created_time)
            .bind(&r.last_edited_time)
            .bind(&r.payload)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert comment {}", r.id))?;
            dr::record_object_attempt(&mut tx, "comments", &r.id, None).await?;
        }
        tx.commit().await.context("commit comments tx")?;
        Ok(())
    }

    pub async fn upsert_users(&self, rows: &[(String, Option<String>, String)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await.context("begin users tx")?;
        for (id, name, payload) in rows {
            sqlx::query(
                "INSERT INTO users (id, name, payload) VALUES (?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    name = COALESCE(excluded.name, users.name),
                    payload = excluded.payload",
            )
            .bind(id)
            .bind(name)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert user {id}"))?;
            dr::record_object_attempt(&mut tx, "users", id, None).await?;
        }
        tx.commit().await.context("commit users tx")?;
        Ok(())
    }

    /// Ids already stored, so a run only spends a request on a user it
    /// has never seen.
    pub async fn known_user_ids(&self) -> Result<HashSet<String>> {
        let rows = sqlx::query("SELECT id FROM users WHERE payload IS NOT NULL")
            .fetch_all(self.pool())
            .await
            .context("select known user ids")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get::<String, _>("id").ok())
            .collect())
    }

    /// `(user_id, display name)` for every user we resolved.
    pub async fn load_user_names(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query("SELECT id, name FROM users WHERE name IS NOT NULL")
            .fetch_all(self.pool())
            .await
            .context("select user names")?;
        let mut out = HashMap::new();
        for r in rows {
            if let (Ok(id), Ok(name)) =
                (r.try_get::<String, _>("id"), r.try_get::<String, _>("name"))
            {
                out.insert(id, name);
            }
        }
        Ok(out)
    }

    pub async fn upsert_comment_anchors(&self, rows: &[CommentAnchorUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self
            .pool()
            .begin()
            .await
            .context("begin comment_anchors tx")?;
        for CommentAnchorUpsert {
            id,
            page_id,
            block_type,
            plain_text: text,
        } in rows
        {
            sqlx::query(
                "INSERT INTO comment_anchors (id, page_id, block_type, plain_text)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    page_id = COALESCE(excluded.page_id, comment_anchors.page_id),
                    block_type = excluded.block_type,
                    plain_text = excluded.plain_text",
            )
            .bind(id)
            .bind(page_id)
            .bind(block_type)
            .bind(text)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert comment anchor {id}"))?;
            dr::record_object_attempt(&mut tx, "comment_anchors", id, None).await?;
        }
        tx.commit().await.context("commit comment_anchors tx")?;
        Ok(())
    }

    pub async fn known_anchor_ids(&self) -> Result<HashSet<String>> {
        let rows = sqlx::query("SELECT id FROM comment_anchors")
            .fetch_all(self.pool())
            .await
            .context("select known anchor ids")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.try_get::<String, _>("id").ok())
            .collect())
    }

    /// `(block_id, anchor text)` for every block a comment hangs off.
    pub async fn load_comment_anchors(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query("SELECT id, plain_text FROM comment_anchors WHERE plain_text IS NOT NULL AND plain_text <> ''")
        .fetch_all(self.pool())
        .await
        .context("select comment anchors")?;
        let mut out = HashMap::new();
        for r in rows {
            if let (Ok(id), Ok(t)) = (
                r.try_get::<String, _>("id"),
                r.try_get::<String, _>("plain_text"),
            ) {
                out.insert(id, t);
            }
        }
        Ok(out)
    }

    pub async fn record_page_error(&self, id: &str, err: &str) -> Result<()> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .context("begin record_page_error tx")?;
        dr::record_object_error(&mut tx, "pages", id, err).await?;
        tx.commit().await.context("commit record_page_error tx")?;
        Ok(())
    }

    pub async fn failed_page_ids(&self) -> Result<Vec<String>> {
        dr::failed_ids(self.pool(), "pages").await
    }

    pub async fn load_pages(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "pages").await
    }

    /// Child pages linked from `page_id`'s stored body.
    ///
    /// Used by the "unchanged page" skip path: when a page's
    /// `last_edited_time` hasn't moved we don't re-fetch its markdown,
    /// but the walk still has to descend into known children in case a
    /// *child* moved when the parent didn't.
    pub async fn stored_child_pages(&self, page_id: &str) -> Result<Vec<String>> {
        let row = sqlx::query("SELECT markdown FROM page_markdown WHERE id = ?")
            .bind(page_id)
            .fetch_optional(self.pool())
            .await
            .context("select stored markdown for child discovery")?;
        let Some(row) = row else {
            return Ok(Vec::new());
        };
        let md: String = row.try_get("markdown").unwrap_or_default();
        Ok(super::markdown::parse(&serde_json::json!({ "markdown": md })).child_pages)
    }

    /// Every stored page body, as `(page_id, markdown)`.
    pub async fn load_page_markdown(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT id, markdown FROM page_markdown ORDER BY id")
            .fetch_all(self.pool())
            .await
            .context("select page_markdown")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let (Ok(id), Ok(md)) = (
                r.try_get::<String, _>("id"),
                r.try_get::<String, _>("markdown"),
            ) else {
                continue;
            };
            out.push((id, md));
        }
        Ok(out)
    }

    pub async fn load_comments(&self) -> Result<Vec<(Value, Option<String>)>> {
        let rows = sqlx::query("SELECT json(payload) AS payload, page_id FROM comments WHERE payload IS NOT NULL ORDER BY id")
        .fetch_all(self.pool())
        .await
        .context("select comments")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let page_id: Option<String> = r.try_get("page_id").ok();
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.push((v, page_id));
            }
        }
        Ok(out)
    }

    /// Every `notion_attachments` row, bytes fetched or not: a page
    /// declares them all, so the fetch landing re-renders it.
    pub async fn load_attachments(&self) -> Result<Vec<AttachmentRow>> {
        let rows = sqlx::query(
            "SELECT id, page_id, ref_id, blake3 FROM notion_attachments ORDER BY page_id, ref_id",
        )
        .fetch_all(self.pool())
        .await
        .context("select notion_attachments for render")?;
        Ok(rows
            .into_iter()
            .map(|r| AttachmentRow {
                id: r.try_get("id").unwrap_or_default(),
                page_id: r.try_get("page_id").unwrap_or_default(),
                ref_id: r.try_get("ref_id").unwrap_or_default(),
                blake3: r.try_get("blake3").ok().flatten(),
            })
            .collect())
    }

    /// Have we already stored bytes for this image-block's ref_id?
    /// One SELECT against `notion_attachments` — the universal
    /// CAS-edge "have we got these bytes yet?" skip-check shape every
    /// other ported provider uses (`wa_media_files`, `slack_attachments`,
    /// …). NULL `blake3` means "we know the ref exists but haven't
    /// fetched bytes yet" — returns false so the caller fetches.
    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM notion_attachments \
             WHERE ref_id = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(ref_id)
        .fetch_optional(self.pool())
        .await
        .context("notion_attachments skip-check")?;
        Ok(row.is_some())
    }

    /// Land one page's attachment edges and the bytes that came back,
    /// stamping the ones that did not as failed so a later run retries
    /// them ([`Self::pages_to_refetch`]).
    pub async fn flush_attachments(&self, acc: &CasEdgeAccumulator) -> Result<()> {
        acc.flush(self.pool(), self.cas(), |page_id, ref_id, blake3| {
            NotionAttachmentRow {
                id: NotionAttachmentRow::pk_recipe(page_id, ref_id),
                page_id: page_id.to_string(),
                ref_id: ref_id.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await
    }

    /// Drop the failed edges of `page_id` whose slot `keep` turns down:
    /// one its body no longer links, or one upstream says is gone. Nothing
    /// will fetch them again, so their rows would stand for good and keep
    /// the page in the retry set.
    pub async fn forget_failed_attachments(
        &self,
        page_id: &str,
        keep_slot: impl Fn(&str) -> bool,
    ) -> Result<()> {
        let rows = sqlx::query(
            "SELECT a.id, a.ref_id, b.last_error IS NOT NULL AS failed \
             FROM notion_attachments a \
             LEFT JOIN notion_attachments_bookkeeping b ON b.id = a.id \
             WHERE a.page_id = ?",
        )
        .bind(page_id)
        .fetch_all(self.pool())
        .await
        .context("select the attachment edges of a page")?;
        let mut keep = HashSet::new();
        let mut gone = false;
        for r in rows {
            let id: String = r.try_get("id")?;
            let ref_id: String = r.try_get("ref_id")?;
            let failed: bool = r.try_get("failed")?;
            if failed && !keep_slot(&ref_id) {
                gone = true;
            } else {
                keep.insert(id);
            }
        }
        if gone {
            datalib_etl::prune::prune_scope(
                self.pool(),
                "notion_attachments",
                &[("page_id", page_id)],
                &keep,
            )
            .await?;
        }
        Ok(())
    }

    /// A page Notion answered 404 for: deleted, or no longer shared with
    /// this credential. The ingest deletes nothing, so a page the store
    /// holds stays as it was; what goes is every failure that would have
    /// it fetched again, and a stub that never fetched. Its body is
    /// marked current, so a body left behind by an earlier run does not
    /// keep it in [`Self::pages_to_refetch`] either.
    pub async fn retire_page(&self, page_id: &str) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin retire page tx")?;
        let stub: bool = sqlx::query_scalar("SELECT payload IS NULL FROM pages WHERE id = ?")
            .bind(page_id)
            .fetch_optional(&mut *tx)
            .await
            .context("probe a retired page")?
            .unwrap_or(false);
        if stub {
            for sql in [
                "DELETE FROM pages WHERE id = ?",
                "DELETE FROM pages_bookkeeping WHERE id = ?",
            ] {
                sqlx::query(sql)
                    .bind(page_id)
                    .execute(&mut *tx)
                    .await
                    .context("drop a page stub")?;
            }
        }
        clear_failure(&mut tx, "pages", page_id).await?;
        sqlx::query(
            "INSERT INTO page_markdown (id, source_last_edited_time) \
             SELECT id, last_edited_time FROM pages WHERE id = ? \
             ON CONFLICT(id) DO UPDATE SET \
                source_last_edited_time = excluded.source_last_edited_time",
        )
        .bind(page_id)
        .execute(&mut *tx)
        .await
        .context("mark a retired page's body current")?;
        clear_failure(&mut tx, "page_markdown", page_id).await?;
        tx.commit().await.context("commit retire page tx")?;
        self.forget_failed_attachments(page_id, |_| false).await
    }

    /// A body Notion answered 404 for, though the page fetched: marked
    /// current at `edited` with whatever body is stored, so it is asked
    /// for again only once the page is edited.
    pub async fn settle_body(&self, page_id: &str, edited: Option<&str>) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin settle body tx")?;
        sqlx::query(
            "INSERT INTO page_markdown (id, source_last_edited_time) VALUES (?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                source_last_edited_time = excluded.source_last_edited_time",
        )
        .bind(page_id)
        .bind(edited)
        .execute(&mut *tx)
        .await
        .context("mark a body current")?;
        clear_failure(&mut tx, "page_markdown", page_id).await?;
        tx.commit().await.context("commit settle body tx")?;
        Ok(())
    }

    /// A user Notion answered 404 for. A stub that never fetched goes; a
    /// user the store holds stays, with its failure cleared.
    pub async fn forget_user(&self, user_id: &str) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin forget user tx")?;
        let stub: bool = sqlx::query_scalar("SELECT payload IS NULL FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await
            .context("probe a gone user")?
            .unwrap_or(false);
        if stub {
            for sql in [
                "DELETE FROM users WHERE id = ?",
                "DELETE FROM users_bookkeeping WHERE id = ?",
            ] {
                sqlx::query(sql)
                    .bind(user_id)
                    .execute(&mut *tx)
                    .await
                    .context("drop a user stub")?;
            }
        }
        clear_failure(&mut tx, "users", user_id).await?;
        tx.commit().await.context("commit forget user tx")?;
        Ok(())
    }

    /// A fetch of `id` in `table` that failed, kept on its sidecar and as
    /// a `problems` row until the same record fetches.
    pub async fn record_fetch_error(&self, table: &str, id: &str, err: &str) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin fetch error tx")?;
        dr::record_object_error(&mut tx, table, id, err).await?;
        tx.commit().await.context("commit fetch error tx")?;
        Ok(())
    }

    /// A body stored short on purpose: a warning that stands until the
    /// page is fetched whole.
    pub async fn record_body_cut_short(&self, page_id: &str, detail: &str) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin body skip tx")?;
        dr::record_object_skipped(
            &mut tx,
            "page_markdown",
            page_id,
            datalib_problems::Reason::DeliberateLoss,
            detail,
        )
        .await?;
        tx.commit().await.context("commit body skip tx")?;
        Ok(())
    }

    /// Pages a run must fetch again although upstream has not moved them:
    /// one whose object, comments, body or an attachment failed last
    /// time, or whose stored body is older than its stored object.
    ///
    /// A body cut short by the follow-up cap is left out: fetching it
    /// again gets the same body. So are failed attachments when the run
    /// will not fetch attachments.
    pub async fn pages_to_refetch(&self, attachments: bool) -> Result<HashSet<String>> {
        let mut out: HashSet<String> = self.failed_page_ids().await?.into_iter().collect();
        let behind: Vec<String> = sqlx::query_scalar(
            "SELECT p.id FROM pages p LEFT JOIN page_markdown m ON m.id = p.id \
             WHERE p.payload IS NOT NULL \
               AND (m.id IS NULL OR m.source_last_edited_time IS NOT p.last_edited_time)",
        )
        .fetch_all(self.pool())
        .await
        .context("select pages whose body is behind")?;
        out.extend(behind);
        let bodies: Vec<String> = sqlx::query_scalar(
            "SELECT b.id FROM page_markdown_bookkeeping b \
             WHERE b.last_error IS NOT NULL \
               AND NOT EXISTS (SELECT 1 FROM problems p \
                   WHERE p.scope_kind = ? AND p.scope_key = 'page_markdown:' || b.id \
                     AND p.reason = ?)",
        )
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(datalib_problems::Reason::DeliberateLoss.as_str())
        .fetch_all(self.pool())
        .await
        .context("select page bodies that failed")?;
        out.extend(bodies);
        if !attachments {
            return Ok(out);
        }
        let owners: Vec<String> = sqlx::query_scalar(
            "SELECT a.page_id FROM notion_attachments a \
             JOIN notion_attachments_bookkeeping b ON b.id = a.id \
             WHERE b.last_error IS NOT NULL",
        )
        .fetch_all(self.pool())
        .await
        .context("select pages whose attachments failed")?;
        out.extend(owners);
        Ok(out)
    }

    /// What the last run said under a run-level problem `key`, if anything.
    pub async fn run_problem_sample(&self, key: &str) -> Result<Option<String>> {
        sqlx::query_scalar("SELECT sample FROM problems WHERE scope_kind = ? AND scope_key = ?")
            .bind(datalib_problems::ScopeKind::Entity.as_str())
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .context("read a run problem")
    }

    pub async fn failed_user_ids(&self) -> Result<Vec<String>> {
        dr::failed_ids(self.pool(), "users").await
    }
}

/// `table:id` is no longer a failure: its sidecar's error and its fetch
/// problem go, and nothing else about the row changes.
async fn clear_failure(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    table: &'static str,
    id: &str,
) -> Result<()> {
    // Audited: `table` is a `&'static str` at every callsite; `id` is bound.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {table}_bookkeeping SET last_error = NULL WHERE id = ?"
    )))
    .bind(id)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("clear the failure of {table}:{id}"))?;
    sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ? AND stage = ?")
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(format!("{table}:{id}"))
        .bind(datalib_problems::Stage::Fetch.as_str())
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear the problem of {table}:{id}"))?;
    Ok(())
}

/// One `notion_attachments` row as render reads it.
#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub id: String,
    pub page_id: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn open_creates_file_and_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        assert!(db_file.exists());
        let pages = db.load_pages().await.unwrap();
        assert!(pages.is_empty());
    }

    #[tokio::test]
    async fn upsert_page_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("x.doltlite_db"))
            .await
            .unwrap();
        db.upsert_pages(&[PageUpsert {
            id: "p1".into(),
            parent_id: Some("root".into()),
            last_edited_time: Some("2026-05-21T19:37:00Z".into()),
            payload: Some(serde_json::to_string(&json!({"id": "p1", "title": "hi"})).unwrap()),
            ..Default::default()
        }])
        .await
        .unwrap();
        let states = db.page_states().await.unwrap();
        assert!(states.get("p1").unwrap().has_payload);
        let pages = db.load_pages().await.unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["title"], "hi");
    }

    #[tokio::test]
    async fn store_blob_succeeds_and_records_edge() {
        // Regression: `notion_attachments` was created without its
        // paired `_bookkeeping` sidecar (the table was missing from
        // DATA_TABLES, unlike every other provider's CAS-edge table). So
        // `store_blob` -> `bulk_upsert_in_tx` -> `bulk_upsert_bookkeeping`
        // failed with "no such table: notion_attachments_bookkeeping"
        // and the fetched image bytes were dropped on every run.
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("blob.doltlite_db"))
            .await
            .unwrap();
        let block_id = "364a550f-af95-8007-9bac-f40d5d9eb53c";
        let ref_id = format!("{block_id}:image");
        assert!(
            !db.blob_exists(&ref_id).await.unwrap(),
            "edge should not exist before store"
        );
        let mut acc = CasEdgeAccumulator::new();
        acc.add_fetched(
            block_id,
            &ref_id,
            b"\x89PNG fake bytes".to_vec(),
            Some("image/png".into()),
            None,
        );
        db.flush_attachments(&acc)
            .await
            .expect("the flush should succeed once the bookkeeping sidecar exists");
        assert!(
            db.blob_exists(&ref_id).await.unwrap(),
            "edge with non-null blake3 should exist after store"
        );
    }

    async fn fetch_problems(db: &RawDb) -> Vec<(String, String)> {
        sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await
            .unwrap()
    }

    /// An attachment whose bytes did not come back used to be upserted
    /// like a fetched one, which stamped it fetched and cleared its
    /// problem: the mirror lacked the file and nothing said so.
    #[tokio::test]
    async fn a_failed_attachment_is_a_problem_until_its_bytes_land() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("fail.doltlite_db"))
            .await
            .unwrap();
        let page = "page-enterprise";
        let slot = "https://prod-files-secure.s3.us-west-2.amazonaws.com/x/warp.png";
        let edge = NotionAttachmentRow::pk_recipe(page, slot);

        let mut acc = CasEdgeAccumulator::new();
        acc.add_failed(page, slot, "HTTP 403");
        db.flush_attachments(&acc).await.unwrap();
        assert!(!db.blob_exists(slot).await.unwrap());
        assert_eq!(
            fetch_problems(&db).await,
            vec![(format!("notion_attachments:{edge}"), "error".to_string())]
        );
        assert!(db.pages_to_refetch(true).await.unwrap().contains(page));

        let mut acc = CasEdgeAccumulator::new();
        acc.add_fetched(page, slot, b"bytes".to_vec(), None, None);
        db.flush_attachments(&acc).await.unwrap();
        assert!(db.blob_exists(slot).await.unwrap());
        assert!(fetch_problems(&db).await.is_empty());
        assert!(!db.pages_to_refetch(true).await.unwrap().contains(page));
    }

    /// A failed attachment the page no longer links can never fetch, so
    /// it goes rather than keeping the page in the retry set for good.
    #[tokio::test]
    async fn a_failed_attachment_the_body_dropped_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("gone.doltlite_db"))
            .await
            .unwrap();
        let page = "page-defiant";
        let mut acc = CasEdgeAccumulator::new();
        acc.add_failed(page, "https://files.notion.so/a.png", "HTTP 500");
        acc.add_fetched(
            page,
            "https://files.notion.so/b.png",
            b"b".to_vec(),
            None,
            None,
        );
        db.flush_attachments(&acc).await.unwrap();

        let still_linked: HashSet<String> = ["https://files.notion.so/b.png".to_string()].into();
        db.forget_failed_attachments(page, |slot| still_linked.contains(slot))
            .await
            .unwrap();
        assert!(fetch_problems(&db).await.is_empty());
        let left: Vec<String> = sqlx::query_scalar("SELECT ref_id FROM notion_attachments")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(left, vec!["https://files.notion.so/b.png".to_string()]);
    }

    #[tokio::test]
    async fn record_page_error_bumps_attempt_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("y.doltlite_db"))
            .await
            .unwrap();
        db.record_page_error("p1", "boom").await.unwrap();
        db.record_page_error("p1", "boom2").await.unwrap();
        let failed = db.failed_page_ids().await.unwrap();
        assert_eq!(failed, vec!["p1".to_string()]);
    }

    #[tokio::test]
    async fn successful_upsert_clears_last_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("z.doltlite_db"))
            .await
            .unwrap();
        db.record_page_error("p1", "fail").await.unwrap();
        db.upsert_pages(&[PageUpsert {
            id: "p1".into(),
            parent_id: None,
            last_edited_time: Some("2026-01-01T00:00:00Z".into()),
            payload: Some("{}".into()),
            ..Default::default()
        }])
        .await
        .unwrap();
        let failed = db.failed_page_ids().await.unwrap();
        assert!(failed.is_empty());
    }

    #[tokio::test]
    async fn payload_is_stored_as_jsonb_blob() {
        // After upserting via `jsonb(?)`, the stored payload column
        // should be a BLOB (jsonb's binary representation), not TEXT.
        // Guards against silently falling back to plain JSON text when
        // someone unwraps the `jsonb()` call from an INSERT.
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("j.doltlite_db"))
            .await
            .unwrap();
        db.upsert_pages(&[PageUpsert {
            id: "p1".into(),
            parent_id: None,
            last_edited_time: Some("2026-01-01T00:00:00Z".into()),
            payload: Some(serde_json::to_string(&json!({"a": [1, 2, 3], "b": "hi"})).unwrap()),
            ..Default::default()
        }])
        .await
        .unwrap();
        let row = sqlx::query("SELECT typeof(payload) AS t FROM pages WHERE id='p1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let t: String = row.try_get("t").unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }

    #[test]
    fn db_path_for_places_db_inside_directory() {
        let p = std::path::Path::new("/tmp/raw/notion-api");
        assert_eq!(
            db_path_for(p),
            std::path::PathBuf::from("/tmp/raw/notion-api/entities.doltlite_db")
        );
        let p2 = std::path::Path::new("/tmp/raw/notion-api/entities.doltlite_db");
        assert_eq!(db_path_for(p2), p2);
    }
}
