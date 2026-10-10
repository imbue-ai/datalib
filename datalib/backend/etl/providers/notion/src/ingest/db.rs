//! Doltlite-backed raw store for the Notion provider: what each table
//! lists for the loop that fills the next, and how a flush of fetched
//! records is written. Every write a fetcher makes is in the
//! transaction `datalib_etl_web::owed` hands it, which then records what is
//! held; nothing here stamps a record done on its own.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr};
use datalib_etl_web::owed::{self, Listed};

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::{
    full_ddl, NotionAttachmentRow, ATTACHMENTS, COMMENTS, LADDER, PAGES, PAGE_MARKDOWN,
};

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

/// One page object as `pages` stores it.
#[derive(Debug, Clone, Default)]
pub struct PageUpsert {
    pub id: String,
    pub parent_type: Option<String>,
    pub parent_id: Option<String>,
    pub in_trash: bool,
    pub created_time: Option<String>,
    pub last_edited_time: Option<String>,
    pub url: Option<String>,
    pub payload: String,
}

impl PageUpsert {
    /// `None` for an object with no id.
    pub fn from_object(page: &Value) -> Option<Self> {
        let text = |key: &str| page.get(key).and_then(Value::as_str).map(String::from);
        let id = text("id")?;
        let parent = page.get("parent");
        Some(Self {
            id,
            parent_type: parent
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                .map(String::from),
            parent_id: parent.and_then(parent_id_of),
            in_trash: page
                .get("in_trash")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            created_time: text("created_time"),
            last_edited_time: text("last_edited_time"),
            url: text("url"),
            payload: page.to_string(),
        })
    }

    pub fn listed(&self) -> Listed {
        Listed::new(self.id.clone(), self.last_edited_time.clone())
    }
}

fn parent_id_of(p: &Value) -> Option<String> {
    p.get("page_id")
        .and_then(Value::as_str)
        .or_else(|| p.get("block_id").and_then(Value::as_str))
        .or_else(|| p.get("database_id").and_then(Value::as_str))
        .or_else(|| p.get("workspace").map(|_| "workspace"))
        .map(String::from)
}

/// One page's body for [`RawDb::store_body`].
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
}

/// One anchor for [`RawDb::store_anchor`]: the block a comment hangs
/// off, and the text it hangs off of. Both `None` for a block Notion
/// no longer has.
#[derive(Debug, Clone, Default)]
pub struct CommentAnchorUpsert {
    pub id: String,
    pub page_id: Option<String>,
    pub block_type: Option<String>,
    pub plain_text: Option<String>,
}

/// One comment for [`RawDb::store_comments`].
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

impl CommentUpsert {
    /// `None` for a comment with no id. `page_id` is the page the
    /// listing was asked for; the parent is the block or page it hangs
    /// off.
    pub fn from_object(c: &Value, page_id: &str) -> Option<Self> {
        let text = |key: &str| c.get(key).and_then(Value::as_str).map(String::from);
        let id = text("id")?;
        let parent = c.get("parent");
        let parent_id = parent.and_then(|p| {
            p.get("block_id")
                .and_then(Value::as_str)
                .or_else(|| p.get("page_id").and_then(Value::as_str))
                .map(String::from)
        });
        Some(Self {
            id,
            discussion_id: text("discussion_id"),
            parent_type: parent
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str)
                .map(String::from),
            parent_id: parent_id.or_else(|| Some(page_id.to_string())),
            page_id: Some(page_id.to_string()),
            created_time: text("created_time"),
            last_edited_time: text("last_edited_time"),
            payload: c.to_string(),
        })
    }
}

/// A commented block the store names, and the page its comment is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorListed {
    pub block_id: String,
    pub page_id: Option<String>,
}

/// One `notion_attachments` row as render reads it.
#[derive(Debug, Clone)]
pub struct AttachmentRow {
    pub id: String,
    pub page_id: String,
    pub ref_id: String,
    pub blake3: Option<String>,
}

impl RawDb {
    // ── what each table lists for the next ─────────────────────────────

    /// Every page with an object, at its `last_edited_time`, newest
    /// first: what a body and a comments listing are owed against.
    pub async fn pages_listed(&self) -> Result<Vec<Listed>> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT id, last_edited_time FROM pages WHERE payload IS NOT NULL \
             ORDER BY last_edited_time DESC, id",
        )
        .fetch_all(self.pool())
        .await
        .context("list the pages")?;
        Ok(rows
            .into_iter()
            .map(|(id, edited)| Listed::new(id, edited))
            .collect())
    }

    /// Every attachment a stored body names, at no version: its bytes
    /// only have to have landed once.
    pub async fn attachments_listed(&self) -> Result<Vec<Listed>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM notion_attachments ORDER BY id")
            .fetch_all(self.pool())
            .await
            .context("list the attachments")?;
        Ok(ids
            .into_iter()
            .map(|id| Listed::new(id, None::<String>))
            .collect())
    }

    /// Every block a stored comment hangs off, at no version.
    pub async fn anchors_listed(&self) -> Result<Vec<AnchorListed>> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT parent_id, MIN(page_id) FROM comments \
             WHERE parent_type = 'block_id' AND parent_id IS NOT NULL \
             GROUP BY parent_id ORDER BY parent_id",
        )
        .fetch_all(self.pool())
        .await
        .context("list the commented blocks")?;
        Ok(rows
            .into_iter()
            .map(|(block_id, page_id)| AnchorListed { block_id, page_id })
            .collect())
    }

    /// Every user a page or a comment names, at no version: a page's
    /// `created_by` and `last_edited_by`, the people in its properties,
    /// and a comment's author. `json_tree` spells a key with an
    /// underscore quoted, so both spellings are asked for.
    pub async fn users_listed(&self) -> Result<Vec<Listed>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT id FROM (\
                SELECT j.value AS id FROM pages p, json_tree(p.payload) j \
                WHERE p.payload IS NOT NULL AND j.key = 'id' AND j.type = 'text' \
                  AND (j.path IN ('$.created_by', '$.\"created_by\"', \
                                  '$.last_edited_by', '$.\"last_edited_by\"') \
                       OR j.path LIKE '$.properties.%people[%]') \
                UNION ALL \
                SELECT json_extract(c.payload, '$.created_by.id') AS id FROM comments c \
                WHERE c.payload IS NOT NULL) \
             WHERE id IS NOT NULL AND id <> '' ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .context("list the users the store names")?;
        Ok(ids
            .into_iter()
            .map(|id| Listed::new(id, None::<String>))
            .collect())
    }

    /// The child pages the stored bodies of `page_ids` link, in the
    /// order the bodies name them, each once.
    pub async fn child_pages_of(&self, page_ids: &[String]) -> Result<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        for chunk in page_ids.chunks(datalib_etl::bulk::SQL_CHUNK) {
            let mut placeholders = String::new();
            datalib_etl::bulk::push_placeholder_list(&mut placeholders, chunk.len());
            // Audited: the IN-list is a `?,?,?` run sized from the chunk and
            // every id is bound.
            let sql = format!(
                "SELECT id, markdown FROM page_markdown \
                 WHERE markdown IS NOT NULL AND id IN ({placeholders})"
            );
            let mut q = sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind(id);
            }
            let mut bodies: HashMap<String, String> = q
                .fetch_all(self.pool())
                .await
                .context("read the stored bodies for their children")?
                .into_iter()
                .collect();
            for id in chunk {
                let Some(md) = bodies.remove(id) else {
                    continue;
                };
                let body = super::markdown::parse(&serde_json::json!({ "markdown": md }));
                for child in body.child_pages {
                    if !out.contains(&child) {
                        out.push(child);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The bytes the CAS holds for a slot under any page, by their
    /// hash: a file's bytes never change, so one landed under one page
    /// is not fetched for another.
    pub async fn blake3_of_slot(&self, slot: &str) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT blake3 FROM notion_attachments \
             WHERE ref_id = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(slot)
        .fetch_optional(self.pool())
        .await
        .context("look a slot up in the attachment edges")
    }

    /// Whether the CAS holds bytes for `ref_id` under any page.
    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        Ok(self.blake3_of_slot(ref_id).await?.is_some())
    }

    // ── writes, in the transaction the caller holds ────────────────────

    /// Each page object, held at its `last_edited_time`.
    pub async fn store_pages(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        rows: &[PageUpsert],
    ) -> Result<()> {
        for r in rows {
            sqlx::query(
                "INSERT INTO pages (id, parent_type, parent_id, in_trash, created_time, last_edited_time, url, payload)
                 VALUES (?, ?, ?, ?, ?, ?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    parent_type = excluded.parent_type,
                    parent_id = excluded.parent_id,
                    in_trash = excluded.in_trash,
                    created_time = excluded.created_time,
                    last_edited_time = excluded.last_edited_time,
                    url = excluded.url,
                    payload = excluded.payload",
            )
            .bind(&r.id)
            .bind(&r.parent_type)
            .bind(&r.parent_id)
            .bind(r.in_trash as i64)
            .bind(&r.created_time)
            .bind(&r.last_edited_time)
            .bind(&r.url)
            .bind(&r.payload)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("upsert page {}", r.id))?;
            owed::hold(tx, PAGES, &r.id, r.last_edited_time.as_deref()).await?;
        }
        Ok(())
    }

    /// A body, and the attachment edges for the slots it names: an edge
    /// is listed by the body, and its bytes are what the attachment loop
    /// owes. A `complete` body's edges are the slots it names and no
    /// other; one with a subtree missing may be missing a link too, so
    /// its old edges stand.
    pub async fn store_body(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        body: &PageMarkdownUpsert,
        slots: &[String],
        complete: bool,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO page_markdown (id, markdown, truncated, unresolved_block_ids)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                markdown = excluded.markdown,
                truncated = excluded.truncated,
                unresolved_block_ids = excluded.unresolved_block_ids",
        )
        .bind(&body.id)
        .bind(&body.markdown)
        .bind(body.truncated as i64)
        .bind(&body.unresolved_block_ids)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("upsert page_markdown {}", body.id))?;
        let mut keep: HashSet<String> = HashSet::new();
        for slot in slots {
            let id = NotionAttachmentRow::pk_recipe(&body.id, slot);
            sqlx::query(
                "INSERT INTO notion_attachments (id, page_id, ref_id, blake3) VALUES (?, ?, ?, NULL)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .bind(&body.id)
            .bind(slot)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list attachment {id}"))?;
            sqlx::query(
                "INSERT INTO notion_attachments_bookkeeping (id, attempt_count) VALUES (?, 0)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list attachment {id} in its sidecar"))?;
            keep.insert(id);
        }
        if complete {
            datalib_etl::prune::prune_scope_in_tx(tx, ATTACHMENTS, &[("page_id", &body.id)], &keep)
                .await?;
        }
        Ok(())
    }

    /// An edge's bytes landed in the CAS under `blake3`.
    pub async fn store_blob(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        edge_id: &str,
        blake3: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE notion_attachments SET blake3 = ? WHERE id = ?")
            .bind(blake3)
            .bind(edge_id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("record the bytes of {edge_id}"))?;
        Ok(())
    }

    /// Upstream no longer serves the file: the edge goes.
    pub async fn forget_attachment(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        edge_id: &str,
    ) -> Result<()> {
        sqlx::query("DELETE FROM notion_attachments WHERE id = ?")
            .bind(edge_id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("forget attachment {edge_id}"))?;
        Ok(())
    }

    /// A page's comments, listed whole: the page's `page_comments` row,
    /// each comment, and the deletion of the page's stored comments the
    /// listing did not return.
    pub async fn store_comments(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        page_id: &str,
        rows: &[CommentUpsert],
    ) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO page_comments (id) VALUES (?)")
            .bind(page_id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list the comments of {page_id}"))?;
        upsert_comments_in_tx(tx, rows).await?;
        let keep: HashSet<String> = rows.iter().map(|r| r.id.clone()).collect();
        let gone =
            datalib_etl::prune::prune_scope_in_tx(tx, COMMENTS, &[("page_id", page_id)], &keep)
                .await?;
        if !gone.is_empty() {
            tracing::info!(
                event = "notion_comments_pruned",
                page = %page_id,
                removed = gone.len(),
                "these comments are gone from the page Notion just listed whole",
            );
        }
        Ok(())
    }

    pub async fn store_anchor(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        row: &CommentAnchorUpsert,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO comment_anchors (id, page_id, block_type, plain_text)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                page_id = excluded.page_id,
                block_type = excluded.block_type,
                plain_text = excluded.plain_text",
        )
        .bind(&row.id)
        .bind(&row.page_id)
        .bind(&row.block_type)
        .bind(&row.plain_text)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("upsert comment anchor {}", row.id))?;
        Ok(())
    }

    /// A user as Notion describes it, or an id-only row for one Notion
    /// no longer has.
    pub async fn store_user(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        name: Option<&str>,
        payload: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO users (id, name, payload) VALUES (?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                payload = excluded.payload",
        )
        .bind(id)
        .bind(name)
        .bind(payload)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("upsert user {id}"))?;
        Ok(())
    }

    // ── one transaction each: for tests and fixtures ───────────────────

    pub async fn upsert_pages(&self, rows: &[PageUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await.context("begin pages tx")?;
        self.store_pages(&mut tx, rows).await?;
        tx.commit().await.context("commit pages tx")
    }

    /// Each body held at its page's `last_edited_time`, as a fetch of
    /// it would be.
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
            let (_, slots) = super::slots::rewrite(&r.markdown);
            self.store_body(&mut tx, r, &slots, true).await?;
            let version: Option<String> =
                sqlx::query_scalar("SELECT last_edited_time FROM pages WHERE id = ?")
                    .bind(&r.id)
                    .fetch_optional(&mut *tx)
                    .await
                    .context("read the page a body is of")?
                    .flatten();
            owed::hold(&mut tx, PAGE_MARKDOWN, &r.id, version.as_deref()).await?;
        }
        tx.commit().await.context("commit page_markdown tx")
    }

    /// The comment rows alone: no listing is held, nothing is pruned.
    pub async fn upsert_comments(&self, rows: &[CommentUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool().begin().await.context("begin comments tx")?;
        upsert_comments_in_tx(&mut tx, rows).await?;
        tx.commit().await.context("commit comments tx")
    }

    // ── what render reads ──────────────────────────────────────────────

    pub async fn load_pages(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), PAGES).await
    }

    /// Every stored page body, as `(page_id, markdown)`.
    pub async fn load_page_markdown(&self) -> Result<Vec<(String, String)>> {
        sqlx::query_as(
            "SELECT id, markdown FROM page_markdown WHERE markdown IS NOT NULL ORDER BY id",
        )
        .fetch_all(self.pool())
        .await
        .context("select page_markdown")
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
            let page_id: Option<String> = r.try_get("page_id").context("comments page_id")?;
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.push((v, page_id));
            }
        }
        Ok(out)
    }

    /// `(user_id, display name)` for every user we resolved.
    pub async fn load_user_names(&self) -> Result<HashMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, name FROM users WHERE name IS NOT NULL")
                .fetch_all(self.pool())
                .await
                .context("select user names")?;
        Ok(rows.into_iter().collect())
    }

    /// `(block_id, anchor text)` for every block a comment hangs off.
    pub async fn load_comment_anchors(&self) -> Result<HashMap<String, String>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, plain_text FROM comment_anchors \
             WHERE plain_text IS NOT NULL AND plain_text <> ''",
        )
        .fetch_all(self.pool())
        .await
        .context("select comment anchors")?;
        Ok(rows.into_iter().collect())
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
}

async fn upsert_comments_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[CommentUpsert],
) -> Result<()> {
    for r in rows {
        sqlx::query(
            "INSERT INTO comments (id, discussion_id, parent_type, parent_id, page_id, created_time, last_edited_time, payload)
             VALUES (?, ?, ?, ?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                discussion_id = excluded.discussion_id,
                parent_type = excluded.parent_type,
                parent_id = excluded.parent_id,
                page_id = excluded.page_id,
                created_time = excluded.created_time,
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
        .execute(&mut **tx)
        .await
        .with_context(|| format!("upsert comment {}", r.id))?;
        dr::record_object_attempt(tx, COMMENTS, &r.id, None).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::PAGE_COMMENTS;
    use serde_json::json;

    fn page(id: &str, edited: &str, payload: Value) -> PageUpsert {
        PageUpsert {
            id: id.into(),
            last_edited_time: Some(edited.into()),
            payload: payload.to_string(),
            ..Default::default()
        }
    }

    async fn open(dir: &tempfile::TempDir) -> RawDb {
        RawDb::open(&dir.path().join("x.doltlite_db"))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn open_creates_file_and_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        assert!(db_file.exists());
        assert!(db.load_pages().await.unwrap().is_empty());
    }

    /// A page is listed at its `last_edited_time`, and its body and its
    /// comments listing are owed until held at that same stamp.
    #[tokio::test]
    async fn a_stored_page_lists_its_body_and_its_comments() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        db.upsert_pages(&[page("p1", "2026-05-21T19:37:00.000Z", json!({"id": "p1"}))])
            .await
            .unwrap();
        let listed = db.pages_listed().await.unwrap();
        assert_eq!(
            listed,
            [Listed::new("p1", Some("2026-05-21T19:37:00.000Z"))]
        );
        assert_eq!(
            owed::owed(db.pool(), PAGE_MARKDOWN, listed.clone())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            owed::owed(db.pool(), PAGE_COMMENTS, listed.clone())
                .await
                .unwrap()
                .len(),
            1
        );
        db.upsert_page_markdown(&[PageMarkdownUpsert {
            id: "p1".into(),
            markdown: "# hi\n".into(),
            ..Default::default()
        }])
        .await
        .unwrap();
        assert!(owed::owed(db.pool(), PAGE_MARKDOWN, listed)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(db.load_pages().await.unwrap()[0]["id"], "p1");
    }

    /// The users a store names: a page's author and last editor, the
    /// people in its properties, and a comment's author, each once.
    #[tokio::test]
    async fn the_users_listed_are_those_pages_and_comments_name() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        db.upsert_pages(&[page(
            "p1",
            "2026-05-21T19:37:00.000Z",
            json!({
                "id": "p1",
                "created_by": {"object": "user", "id": "picard"},
                "last_edited_by": {"object": "user", "id": "riker"},
                "properties": {
                    "Crew": {"type": "people", "people": [
                        {"object": "user", "id": "data"}, {"object": "user", "id": "picard"}]},
                    "Ship": {"type": "rich_text", "rich_text": [{"plain_text": "id"}]},
                    "Relation": {"type": "relation", "relation": [{"id": "not-a-user"}]},
                },
            }),
        )])
        .await
        .unwrap();
        db.upsert_comments(&[CommentUpsert {
            id: "c1".into(),
            page_id: Some("p1".into()),
            payload: json!({"id": "c1", "created_by": {"object": "user", "id": "worf"}})
                .to_string(),
            ..Default::default()
        }])
        .await
        .unwrap();
        let users: Vec<String> = db
            .users_listed()
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.key)
            .collect();
        assert_eq!(users, ["data", "picard", "riker", "worf"]);
    }

    /// A body lists the attachments it names and prunes the edges of
    /// slots it no longer links; one with a subtree missing prunes
    /// nothing.
    #[tokio::test]
    async fn a_body_lists_its_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        let a = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/a.png";
        let b = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/b.png";
        let body = |md: &str| PageMarkdownUpsert {
            id: "p1".into(),
            markdown: md.into(),
            ..Default::default()
        };
        let slots = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let edges = |db: &RawDb| {
            let pool = db.pool().clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT ref_id FROM notion_attachments ORDER BY ref_id",
                )
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        let mut tx = db.pool().begin().await.unwrap();
        db.store_body(&mut tx, &body("![a](a) ![b](b)"), &slots(&[a, b]), true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(edges(&db).await, [a, b]);
        assert_eq!(
            owed::owed(
                db.pool(),
                ATTACHMENTS,
                db.attachments_listed().await.unwrap()
            )
            .await
            .unwrap()
            .len(),
            2,
            "listed, not held"
        );

        let mut tx = db.pool().begin().await.unwrap();
        db.store_body(&mut tx, &body("![a](a)"), &slots(&[a]), false)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            edges(&db).await,
            [a, b],
            "an incomplete body prunes nothing"
        );

        let mut tx = db.pool().begin().await.unwrap();
        db.store_body(&mut tx, &body("![a](a)"), &slots(&[a]), true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(edges(&db).await, [a]);
    }

    /// A page's comments listed whole replace what the store had for
    /// that page, and the listing is one row the loop holds.
    #[tokio::test]
    async fn comments_listed_whole_replace_the_pages_stored_ones() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        let c = |id: &str| CommentUpsert {
            id: id.into(),
            page_id: Some("p1".into()),
            payload: json!({"id": id}).to_string(),
            ..Default::default()
        };
        let mut tx = db.pool().begin().await.unwrap();
        db.store_comments(&mut tx, "p1", &[c("c1"), c("c2")])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(db.load_comments().await.unwrap().len(), 2);
        let mut tx = db.pool().begin().await.unwrap();
        db.store_comments(&mut tx, "p1", &[c("c2")]).await.unwrap();
        tx.commit().await.unwrap();
        let left = db.load_comments().await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0["id"], "c2");
        let listed: Vec<String> = sqlx::query_scalar("SELECT id FROM page_comments")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(listed, ["p1"]);
    }

    #[tokio::test]
    async fn payload_is_stored_as_jsonb_blob() {
        // After upserting via `jsonb(?)`, the stored payload column
        // should be a BLOB (jsonb's binary representation), not TEXT.
        // Guards against silently falling back to plain JSON text when
        // someone unwraps the `jsonb()` call from an INSERT.
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir).await;
        db.upsert_pages(&[page(
            "p1",
            "2026-01-01T00:00:00Z",
            json!({"a": [1, 2, 3], "b": "hi"}),
        )])
        .await
        .unwrap();
        let t: String = sqlx::query_scalar("SELECT typeof(payload) FROM pages WHERE id='p1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
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
