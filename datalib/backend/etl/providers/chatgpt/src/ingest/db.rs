//! Doltlite-backed raw store for the ChatGPT provider: what the store
//! lists for the attachment loop, and how a fetched conversation or a
//! blob is written in the transaction the loop hands over. The loop
//! (`datalib_etl_web::owed`) records what is held; nothing here stamps a
//! record done on its own.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl_web::owed::Listed;

use super::schema_raw::{
    full_ddl, ConversationAttachmentRow, ConversationRow, ATTACHMENTS, LADDER,
};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

/// One conversation as the detail endpoint answered it, ready to store:
/// the payload canonicalized, and the files its messages name.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub id: String,
    pub title: Option<String>,
    /// The detail's `update_time`, JSON-encoded as the row keeps it.
    pub update_time: Option<String>,
    pub payload: String,
    pub files: Vec<FileRef>,
}

/// One attachment a conversation names, each file once: identical
/// assets often appear under several parts (asset_pointer + attachments
/// mirror). The name and MIME type are what the message says of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    pub id: String,
    pub name: Option<String>,
    pub mime: Option<String>,
}

impl RawDb {
    // ── `me` ────────────────────────────────────────────────────────

    pub async fn load_me(&self) -> Result<Option<Value>> {
        let row = sqlx::query("SELECT json(payload) AS payload FROM me ORDER BY id LIMIT 1")
            .fetch_optional(self.pool())
            .await
            .context("select me")?;
        let Some(row) = row else { return Ok(None) };
        let payload: Option<String> = row.try_get("payload").context("me payload")?;
        Ok(payload.and_then(|s: String| serde_json::from_str(&s).ok()))
    }

    // ── what the store lists ───────────────────────────────────────

    /// Every attachment a stored conversation names, at the version its
    /// conversation is held at: the bytes are owed once per fetch of the
    /// conversation, and a file chatgpt.com no longer serves is asked for
    /// again only when the conversation changes.
    pub async fn attachments_listed(&self) -> Result<Vec<Listed>> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT a.id, b.held_version FROM chatgpt_attachments a \
             LEFT JOIN conversations_bookkeeping b ON b.id = a.conversation_id \
             ORDER BY a.conversation_id, a.file_id",
        )
        .fetch_all(self.pool())
        .await
        .context("list the attachments")?;
        Ok(rows
            .into_iter()
            .map(|(id, version)| Listed::new(id, version))
            .collect())
    }

    /// The bytes the CAS holds for a file under any conversation, by
    /// their hash: signed URLs rotate, bytes do not, so a file landed
    /// under one conversation is not fetched for another.
    pub async fn blake3_of_file(&self, file_id: &str) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT blake3 FROM chatgpt_attachments \
             WHERE file_id = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(file_id)
        .fetch_optional(self.pool())
        .await
        .context("look a file up in the attachment edges")
    }

    /// Delete every conversation not in `keep`, its attachment edges, and
    /// the fetch problems of both.
    ///
    /// Only for a caller holding a **complete** listing — see the gate at
    /// the callsite. That gate is the whole safety story: nothing here
    /// second-guesses how much it deletes, because the rows stay in
    /// doltlite history either way.
    pub async fn prune_conversations(&self, keep: &HashSet<String>) -> Result<usize> {
        let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
            .fetch_one(self.pool())
            .await
            .context("count conversations for prune")?;
        let mut tx = self.pool().begin().await.context("begin prune tx")?;
        let gone =
            datalib_etl::prune::prune_scope_in_tx(&mut tx, "conversations", &[], keep).await?;
        datalib_etl::prune::delete_owned_in_tx(&mut tx, ATTACHMENTS, "conversation_id", &gone)
            .await?;
        tx.commit().await.context("commit prune tx")?;
        datalib_etl::prune::record("chatgpt conversations", held as usize, gone.len());
        Ok(gone.len())
    }

    pub async fn has_any_conversation(&self) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM conversations LIMIT 1")
            .fetch_optional(self.pool())
            .await
            .context("has_any_conversation")?;
        Ok(row.is_some())
    }

    /// The conversation as stored, `None` for one never fetched.
    pub async fn load_conversation_payload(&self, id: &str) -> Result<Option<Value>> {
        let payload: Option<Option<String>> =
            sqlx::query_scalar("SELECT json(payload) FROM conversations WHERE id = ?")
                .bind(id)
                .fetch_optional(self.pool())
                .await
                .context("select one conversation")?;
        payload
            .flatten()
            .map(|s| serde_json::from_str(&s).context("parse a stored conversation"))
            .transpose()
    }

    // ── writes, in the transaction the caller holds ────────────────────

    /// A conversation, and the attachment edges for the files it names:
    /// an edge is listed by the conversation, and its bytes are what the
    /// attachment loop owes. The conversation was fetched whole, so its
    /// edges are the files it names and no other.
    pub async fn store_conversation(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        c: &Conversation,
    ) -> Result<()> {
        let row = ConversationRow {
            id_and_payload: WirePayload {
                id: c.id.clone(),
                payload: c.payload.clone(),
            },
            title: c.title.clone(),
            update_time: c.update_time.clone(),
        };
        bulk_upsert_entity_in_tx(tx, &[row]).await?;
        let mut keep: HashSet<String> = HashSet::new();
        for f in &c.files {
            let id = ConversationAttachmentRow::pk_recipe(&c.id, &f.id);
            sqlx::query(
                "INSERT INTO chatgpt_attachments (id, conversation_id, file_id, blake3) \
                 VALUES (?, ?, ?, NULL) ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .bind(&c.id)
            .bind(&f.id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list attachment {id}"))?;
            sqlx::query(
                "INSERT INTO chatgpt_attachments_bookkeeping (id, attempt_count) VALUES (?, 0) \
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list attachment {id} in its sidecar"))?;
            keep.insert(id);
        }
        datalib_etl::prune::prune_scope_in_tx(
            tx,
            ATTACHMENTS,
            &[("conversation_id", &c.id)],
            &keep,
        )
        .await?;
        Ok(())
    }

    /// chatgpt.com no longer has the conversation: its row and its
    /// edges go. The loop takes the sidecar and the problem rows.
    pub async fn forget_conversation(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
    ) -> Result<()> {
        datalib_etl::prune::delete_owned_in_tx(
            tx,
            ATTACHMENTS,
            "conversation_id",
            &[id.to_string()],
        )
        .await?;
        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("forget conversation {id}"))?;
        Ok(())
    }

    /// An edge's bytes landed in the CAS under `blake3`.
    pub async fn store_blob(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        edge_id: &str,
        blake3: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE chatgpt_attachments SET blake3 = ? WHERE id = ?")
            .bind(blake3)
            .bind(edge_id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("record the bytes of {edge_id}"))?;
        Ok(())
    }

    // ── loads ───────────────────────────────────────────────────────

    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        let rows = sqlx::query(
            "SELECT c.id, json(c.payload) AS payload, b.fetched_at_utc
             FROM conversations c
             LEFT JOIN conversations_bookkeeping b ON b.id = c.id
             WHERE c.payload IS NOT NULL
             ORDER BY c.id",
        )
        .fetch_all(self.pool())
        .await
        .context("select conversations")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let fetched_at_utc: Option<String> = r
                .try_get("fetched_at_utc")
                .context("conversations fetched_at_utc")?;
            let Ok(payload_v) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            out.push(LoadedConversation {
                id,
                payload: payload_v,
                fetched_at_utc,
            });
        }
        Ok(out)
    }
}

/// One row's worth of loaded conversation data — payload plus the
/// fetch timestamp. Rows only exist post-detail-fetch.
#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    pub payload: Value,
    pub fetched_at_utc: Option<String>,
}

/// Bag returned to the synchronous render / synthesize path.
/// Attachment bytes are no longer carried alongside — render's
/// `parse` loads a per-doc [`BlobBundle`] for each conversation,
/// keeping render fully sync.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub me: Option<Value>,
    pub conversations: Vec<LoadedConversation>,
}

/// Synchronous helper for tests that want a snapshot of every entity
/// table at a fixed point in time. Production render uses
/// `datalib_etl_chatgpt_render::render::parse::parse(..., last_render_hash)` instead;
/// this one ignores the cursor and loads everything. Attachment bytes
/// are NOT loaded here — tests that need them load a [`BlobBundle`]
/// via `BlobBundle::load_many(...)` directly.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                me: db.load_me().await?,
                conversations: db.load_conversations().await?,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_web::owed;
    use serde_json::json;

    #[tokio::test]
    async fn me_round_trips() {
        use crate::ingest::schema_raw::MeRow;
        use datalib_etl::bulk::bulk_upsert_in_tx;
        use datalib_etl::doltlite_raw::WirePayload;
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let me = json!({"id": "u1", "email": "x@y", "name": "X Y"});
        let mut tx = db.pool().begin().await.unwrap();
        bulk_upsert_in_tx(
            &mut tx,
            &[MeRow {
                id_and_payload: WirePayload {
                    id: "u1".into(),
                    payload: serde_json::to_string(&me).unwrap(),
                },
                email: Some("x@y".into()),
                name: Some("X Y".into()),
            }],
            &datalib_time::parse_strict("2026-06-11T00:00:00-07:00").unwrap(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let loaded = db.load_me().await.unwrap().expect("me present");
        assert_eq!(loaded["id"], "u1");
        assert_eq!(loaded["email"], "x@y");
    }

    fn conversation(id: &str, files: &[&str]) -> Conversation {
        Conversation {
            id: id.into(),
            title: Some(id.into()),
            update_time: Some("1.0".into()),
            payload: json!({"id": id, "mapping": {}}).to_string(),
            files: files
                .iter()
                .map(|f| FileRef {
                    id: (*f).into(),
                    name: None,
                    mime: None,
                })
                .collect(),
        }
    }

    /// A conversation's edges are the files it names: a refetch that
    /// drops one drops its edge, an edge whose bytes landed keeps them,
    /// and every edge is listed at the conversation's held version.
    #[tokio::test]
    async fn a_conversations_edges_follow_the_files_it_names() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        db.store_conversation(&mut tx, &conversation("c1", &["f1", "f2"]))
            .await
            .unwrap();
        owed::hold(&mut tx, "conversations", "c1", Some("1"))
            .await
            .unwrap();
        db.store_blob(&mut tx, "c1#f1", &"ab".repeat(32))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let listed = db.attachments_listed().await.unwrap();
        assert_eq!(
            listed,
            [
                Listed::new("c1#f1", Some("1")),
                Listed::new("c1#f2", Some("1"))
            ]
        );
        assert_eq!(
            db.blake3_of_file("f1").await.unwrap().as_deref(),
            Some("ab".repeat(32).as_str())
        );

        let mut tx = db.pool().begin().await.unwrap();
        db.store_conversation(&mut tx, &conversation("c1", &["f1", "f3"]))
            .await
            .unwrap();
        owed::hold(&mut tx, "conversations", "c1", Some("2"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let listed = db.attachments_listed().await.unwrap();
        assert_eq!(
            listed,
            [
                Listed::new("c1#f1", Some("2")),
                Listed::new("c1#f3", Some("2"))
            ],
            "the dropped file's edge went; the kept one is owed again at the new version"
        );
        assert!(db.blake3_of_file("f1").await.unwrap().is_some());
        db.close().await;
    }
}
