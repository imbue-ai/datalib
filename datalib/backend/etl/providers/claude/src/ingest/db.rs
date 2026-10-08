//! Doltlite-backed raw store for the Claude provider: what the store
//! lists for the attachment loop, the sweep markers that say when a
//! listing is due, and how a fetched record is written in the
//! transaction the loop hands over. The loop (`datalib_etl_web::owed`)
//! records what is held; nothing here stamps a record done on its own.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::{bulk_upsert_entity_in_tx, bulk_upsert_in_tx};
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl_web::owed::{self, Listed};
use datalib_time::IsoOffsetTimestamp;

use super::schema_raw::{
    full_ddl, ConversationAttachmentRow, ConversationRow, ProjectDocRow, ProjectRow, ATTACHMENTS,
    LADDER, PROJECTS,
};

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl(), LADDER);

/// One conversation as the detail endpoint answered it, ready to store:
/// the payload canonicalized, and the file objects its messages name.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub uuid: String,
    pub org_uuid: String,
    pub org_name: String,
    pub name: Option<String>,
    pub updated_at: Option<String>,
    pub payload: String,
    /// `chat_messages[].files[]`, each `file_uuid` once.
    pub files: Vec<Value>,
}

/// One project as the listing names it, ready to store.
#[derive(Debug, Clone)]
pub struct ProjectUpsert {
    pub uuid: String,
    pub org_uuid: String,
    pub org_name: String,
    pub name: Option<String>,
    pub updated_at: Option<String>,
    pub payload: String,
}

fn sweep_scope(key: &str) -> String {
    format!("claude:sweep:{key}")
}

impl RawDb {
    // ── users and orgs ───────────────────────────────────────────────

    pub async fn has_any_user(&self) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM users LIMIT 1")
            .fetch_optional(self.pool())
            .await
            .context("has_any_user")?;
        Ok(row.is_some())
    }

    /// How long before `now` the sweep `key` last completed. `now` is the
    /// run's pinned now: the answer decides whether a listing is
    /// requested at all.
    pub async fn sweep_age(
        &self,
        key: &str,
        now: &IsoOffsetTimestamp,
    ) -> Result<Option<chrono::Duration>> {
        let row = sqlx::query("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
            .bind(sweep_scope(key))
            .fetch_optional(self.pool())
            .await
            .context("select claude sweep marker")?;
        let Some(row) = row else { return Ok(None) };
        let s: String = row
            .try_get("last_seen_at_utc")
            .context("read claude sweep timestamp")?;
        Ok(Some(sweep_age_of(&s, now)?))
    }

    /// How long before `now` each project's docs were last listed, by
    /// project, for every project with a marker.
    pub async fn project_docs_sweep_ages(
        &self,
        now: &IsoOffsetTimestamp,
    ) -> Result<HashMap<String, chrono::Duration>> {
        let prefix = sweep_scope("project_docs:");
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT scope, last_seen_at_utc FROM sync_scope_state WHERE scope LIKE ? || '%'",
        )
        .bind(&prefix)
        .fetch_all(self.pool())
        .await
        .context("select the project docs sweep markers")?;
        let mut out = HashMap::with_capacity(rows.len());
        for (scope, stamp) in rows {
            out.insert(
                scope[prefix.len()..].to_string(),
                sweep_age_of(&stamp, now)?,
            );
        }
        Ok(out)
    }

    pub async fn record_sweep(&self, key: &str, now: &IsoOffsetTimestamp) -> Result<()> {
        let mut tx = self.pool().begin().await.context("begin sweep tx")?;
        record_sweep_in_tx(&mut tx, key, now).await?;
        tx.commit().await.context("commit sweep tx")
    }

    /// The orgs listed, and the marker that says when to list them
    /// again, in one transaction: a marker never stands over a listing
    /// that did not land.
    pub async fn store_orgs(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        rows: &[super::schema_raw::OrgRow],
        now: &IsoOffsetTimestamp,
        run_now: &IsoOffsetTimestamp,
    ) -> Result<()> {
        bulk_upsert_in_tx(tx, rows, now).await?;
        record_sweep_in_tx(tx, super::ORGS_SWEEP_KEY, run_now).await
    }

    /// The `orgs` rows we already have, as raw payloads — what a warm
    /// [`Self::sweep_age`] hit serves instead of re-listing upstream.
    pub async fn load_orgs(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "orgs").await
    }

    pub async fn load_users(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "users").await
    }

    pub async fn first_user_uuid(&self) -> Result<Option<String>> {
        first_user_uuid_from(self.pool()).await
    }

    // ── projects ───────────────────────────────────────────────────

    /// Each project's metadata, held at its `updated_at`. A project held
    /// at the version listed is not written: the listing is the content.
    pub async fn store_projects(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        rows: &[ProjectUpsert],
    ) -> Result<()> {
        let built: Vec<ProjectRow> = rows
            .iter()
            .map(|p| ProjectRow {
                id_and_payload: WirePayload {
                    id: p.uuid.clone(),
                    payload: p.payload.clone(),
                },
                org_uuid: Some(p.org_uuid.clone()),
                org_name: Some(p.org_name.clone()),
                name: p.name.clone(),
                updated_at: p.updated_at.clone(),
            })
            .collect();
        bulk_upsert_entity_in_tx(tx, &built).await?;
        for p in rows {
            owed::hold(tx, PROJECTS, &p.uuid, p.updated_at.as_deref()).await?;
        }
        Ok(())
    }

    /// One project's knowledge docs, listed whole: the docs, the
    /// project's `project_docs_listings` row, and the marker that says
    /// when to list them again. A doc the listing no longer names keeps
    /// its row: project deletions are not mirrored.
    pub async fn store_project_docs(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        project_uuid: &str,
        docs: &[Value],
        now: &IsoOffsetTimestamp,
        run_now: &IsoOffsetTimestamp,
    ) -> Result<usize> {
        let mut rows: Vec<ProjectDocRow> = Vec::with_capacity(docs.len());
        for doc in docs {
            let Some(id) = doc.get("uuid").and_then(Value::as_str) else {
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
                    .and_then(Value::as_str)
                    .map(String::from),
                created_at: doc
                    .get("created_at")
                    .and_then(Value::as_str)
                    .map(String::from),
            });
        }
        bulk_upsert_in_tx(tx, &rows, now).await?;
        sqlx::query("INSERT OR IGNORE INTO project_docs_listings (id) VALUES (?)")
            .bind(project_uuid)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list the docs of {project_uuid}"))?;
        record_sweep_in_tx(tx, &super::project_docs_sweep_key(project_uuid), run_now).await?;
        Ok(rows.len())
    }

    pub async fn load_projects(&self) -> Result<Vec<LoadedProject>> {
        load_projects_from(self.pool()).await
    }

    pub async fn load_project_docs(&self) -> Result<Vec<LoadedProjectDoc>> {
        load_project_docs_from(self.pool()).await
    }

    // ── conversations ──────────────────────────────────────────────

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
                id: c.uuid.clone(),
                payload: c.payload.clone(),
            },
            org_uuid: Some(c.org_uuid.clone()),
            org_name: Some(c.org_name.clone()),
            name: c.name.clone(),
            updated_at: c.updated_at.clone(),
        };
        bulk_upsert_entity_in_tx(tx, &[row]).await?;
        let mut keep: HashSet<String> = HashSet::new();
        for file_uuid in c.files.iter().filter_map(file_uuid_of) {
            let id = ConversationAttachmentRow::pk_recipe(&c.uuid, file_uuid);
            sqlx::query(
                "INSERT INTO claude_attachments (id, conversation_uuid, file_uuid, blake3) \
                 VALUES (?, ?, ?, NULL) ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .bind(&c.uuid)
            .bind(file_uuid)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list attachment {id}"))?;
            sqlx::query(
                "INSERT INTO claude_attachments_bookkeeping (id, attempt_count) VALUES (?, 0) \
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
            &[("conversation_uuid", &c.uuid)],
            &keep,
        )
        .await?;
        Ok(())
    }

    /// claude.ai no longer has the conversation: its row and its edges
    /// go. The loop takes the sidecar and the problem rows.
    pub async fn forget_conversation(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        uuid: &str,
    ) -> Result<()> {
        datalib_etl::prune::delete_owned_in_tx(
            tx,
            ATTACHMENTS,
            "conversation_uuid",
            &[uuid.to_string()],
        )
        .await?;
        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(uuid)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("forget conversation {uuid}"))?;
        Ok(())
    }

    /// Every attachment a stored conversation names, at the version its
    /// conversation is held at: the bytes are owed once per fetch of the
    /// conversation, and a file claude.ai no longer serves is asked for
    /// again only when the conversation changes.
    pub async fn attachments_listed(&self) -> Result<Vec<Listed>> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT a.id, b.held_version FROM claude_attachments a \
             LEFT JOIN conversations_bookkeeping b ON b.id = a.conversation_uuid \
             ORDER BY a.conversation_uuid, a.file_uuid",
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
    /// their hash: a file's bytes never change, so one landed under one
    /// conversation is not fetched for another.
    pub async fn blake3_of_file(&self, file_uuid: &str) -> Result<Option<String>> {
        sqlx::query_scalar(
            "SELECT blake3 FROM claude_attachments \
             WHERE file_uuid = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(file_uuid)
        .fetch_optional(self.pool())
        .await
        .context("look a file up in the attachment edges")
    }

    /// An edge's bytes landed in the CAS under `blake3`.
    pub async fn store_blob(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        edge_id: &str,
        blake3: &str,
    ) -> Result<()> {
        sqlx::query("UPDATE claude_attachments SET blake3 = ? WHERE id = ?")
            .bind(blake3)
            .bind(edge_id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("record the bytes of {edge_id}"))?;
        Ok(())
    }

    /// Delete this org's conversations that a **complete** listing of that
    /// org did not name, with their attachment edges and the fetch
    /// problems of both.
    ///
    /// Scoped to one `org_uuid`, and that scope is load-bearing twice over.
    /// An org whose listing 403'd was never enumerated, so it must not be
    /// passed here at all. And rows with a NULL `org_uuid` are the ones
    /// `claude_export` ingested — the two source types share this store, and
    /// an API sync has no standing to say an export's conversations are
    /// gone.
    ///
    /// `/chat_conversations` is a single unpaginated GET, so if claude.ai
    /// ever starts capping it the symptom is a large prune rather than an
    /// error. That is what `prune::record`'s WARN is for — and why the rows
    /// go to history rather than away.
    pub async fn prune_org_conversations(
        &self,
        org_uuid: &str,
        keep: &HashSet<String>,
    ) -> Result<usize> {
        let held: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM conversations WHERE org_uuid = ?")
            .bind(org_uuid)
            .fetch_one(self.pool())
            .await
            .context("count org conversations for prune")?;
        let gone = self
            .prune_conversations_in(&[("org_uuid", org_uuid)], keep)
            .await?;
        datalib_etl::prune::record(
            &format!("claude org {org_uuid} conversations"),
            held as usize,
            gone,
        );
        Ok(gone)
    }

    /// Delete the stubs of conversations whose every fetch failed — no
    /// payload, so no org — that no listing names any more. Only for a
    /// caller every org of which listed completely: a stub belongs to
    /// whichever org listed it, and nothing records which.
    pub async fn prune_unlisted_stubs(&self, listed: &HashSet<String>) -> Result<usize> {
        let stubs: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM conversations WHERE org_uuid IS NULL AND payload IS NULL",
        )
        .fetch_all(self.pool())
        .await
        .context("list conversation stubs")?;
        let gone: HashSet<&String> = stubs.iter().filter(|id| !listed.contains(*id)).collect();
        if gone.is_empty() {
            return Ok(0);
        }
        let keep: HashSet<String> = sqlx::query_scalar::<_, String>("SELECT id FROM conversations")
            .fetch_all(self.pool())
            .await
            .context("list conversation ids")?
            .into_iter()
            .filter(|id| !gone.contains(id))
            .collect();
        let n = self.prune_conversations_in(&[], &keep).await?;
        datalib_etl::prune::record("claude conversation stubs", stubs.len(), n);
        Ok(n)
    }

    async fn prune_conversations_in(
        &self,
        scope: &[(&str, &str)],
        keep: &HashSet<String>,
    ) -> Result<usize> {
        let mut tx = self.pool().begin().await.context("begin prune tx")?;
        let gone =
            datalib_etl::prune::prune_scope_in_tx(&mut tx, "conversations", scope, keep).await?;
        datalib_etl::prune::delete_owned_in_tx(&mut tx, ATTACHMENTS, "conversation_uuid", &gone)
            .await?;
        tx.commit().await.context("commit prune tx")?;
        Ok(gone.len())
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

    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        load_conversations_from(self.pool()).await
    }
}

/// The marker's stamp is the run's pinned now, kept as UTC with the
/// offset beside it, as `doltlite_raw::upsert_scope_state` keeps one.
pub(crate) async fn record_sweep_in_tx(
    tx: &mut Transaction<'_, Sqlite>,
    key: &str,
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    let stamp = datalib_time::split_stamp(&now.to_rfc3339());
    sqlx::query(
        "INSERT INTO sync_scope_state (scope, last_seen_at_utc, tz_offset) VALUES (?, ?, ?)
         ON CONFLICT(scope) DO UPDATE SET last_seen_at_utc = excluded.last_seen_at_utc,
            tz_offset = excluded.tz_offset",
    )
    .bind(sweep_scope(key))
    .bind(&stamp.utc)
    .bind(&stamp.tz_offset)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("record claude sweep marker {key}"))?;
    Ok(())
}

fn sweep_age_of(stamp: &str, now: &IsoOffsetTimestamp) -> Result<chrono::Duration> {
    let dt = datalib_time::parse_strict(stamp)
        .with_context(|| format!("parse claude sweep timestamp {stamp:?}"))?
        .inner();
    Ok(now.inner() - dt)
}

/// The `file_uuid` of one `chat_messages[].files[]` object.
pub fn file_uuid_of(file: &Value) -> Option<&str> {
    file.get("file_uuid").and_then(Value::as_str)
}

/// Every `chat_messages[].files[]` object the conversation names, each
/// `file_uuid` once.
pub fn files_of(conv: &Value) -> Vec<Value> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    let Some(messages) = conv.get("chat_messages").and_then(Value::as_array) else {
        return out;
    };
    for msg in messages {
        let Some(files) = msg.get("files").and_then(Value::as_array) else {
            continue;
        };
        for f in files {
            if let Some(id) = file_uuid_of(f) {
                if seen.insert(id.to_string()) {
                    out.push(f.clone());
                }
            }
        }
    }
    out
}

/// One project as it sits between download and render.
#[derive(Debug, Clone)]
pub struct LoadedProject {
    pub id: String,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub payload: Value,
}

/// One knowledge document, with its owning project surfaced out of the
/// payload so render can bucket docs without re-parsing every one.
#[derive(Debug, Clone)]
pub struct LoadedProjectDoc {
    pub id: String,
    pub project_uuid: String,
    pub payload: Value,
}

pub async fn load_conversations_from(pool: &SqlitePool) -> Result<Vec<LoadedConversation>> {
    let rows = sqlx::query(
        "SELECT id, org_uuid, org_name, json(payload) AS payload FROM conversations \
          WHERE payload IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load_conversations")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(payload) = row_payload(r) else {
            continue;
        };
        out.push(LoadedConversation {
            id: r.try_get("id").unwrap_or_default(),
            org_uuid: r
                .try_get::<Option<String>, _>("org_uuid")
                .unwrap_or_default(),
            org_name: r.try_get("org_name").ok(),
            payload,
        });
    }
    Ok(out)
}

pub async fn first_user_uuid_from(pool: &SqlitePool) -> Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM users ORDER BY id LIMIT 1")
        .fetch_optional(pool)
        .await
        .context("first_user_uuid")?;
    Ok(row.and_then(|r| r.try_get::<String, _>("id").ok()))
}

pub async fn load_projects_from(pool: &SqlitePool) -> Result<Vec<LoadedProject>> {
    let rows = sqlx::query(
        "SELECT id, org_uuid, org_name, json(payload) AS payload FROM projects \
          WHERE payload IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load_projects")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(payload) = row_payload(r) else {
            continue;
        };
        out.push(LoadedProject {
            id: r.try_get("id").unwrap_or_default(),
            org_uuid: r.try_get("org_uuid").ok(),
            org_name: r.try_get("org_name").ok(),
            payload,
        });
    }
    Ok(out)
}

pub async fn load_project_docs_from(pool: &SqlitePool) -> Result<Vec<LoadedProjectDoc>> {
    let rows = sqlx::query(
        "SELECT id, project_uuid, json(payload) AS payload FROM project_docs \
          WHERE payload IS NOT NULL AND project_uuid IS NOT NULL ORDER BY project_uuid, id",
    )
    .fetch_all(pool)
    .await
    .context("load_project_docs")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(payload) = row_payload(r) else {
            continue;
        };
        let Ok(project_uuid) = r.try_get::<String, _>("project_uuid") else {
            continue;
        };
        out.push(LoadedProjectDoc {
            id: r.try_get("id").unwrap_or_default(),
            project_uuid,
            payload,
        });
    }
    Ok(out)
}

fn row_payload(r: &sqlx::sqlite::SqliteRow) -> Option<Value> {
    let s: String = r.try_get("payload").ok()?;
    serde_json::from_str(&s).ok()
}

#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    /// Owning Anthropic organization, or `None`.
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub payload: Value,
}

#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub users: Vec<Value>,
    pub first_user_uuid: Option<String>,
    pub conversations: Vec<LoadedConversation>,
}

/// Synchronous helper for tests that want a snapshot of every entity
/// table at a fixed point in time. Production render uses
/// `datalib_etl_claude_render::render::parse::parse(..., last_render_hash)` instead;
/// this one ignores the cursor and loads everything. Attachment bytes
/// are NOT loaded here — tests that need them load a `BlobBundle`
/// via `BlobBundle::load_many(...)` directly.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let loaded = async {
                Ok::<_, anyhow::Error>(LoadedRaw {
                    users: db.load_users().await?,
                    first_user_uuid: db.first_user_uuid().await?,
                    conversations: db.load_conversations().await?,
                })
            }
            .await;
            // Closed, not dropped: the caller opens this store again next,
            // and a connection still closing is a writer still holding it.
            db.close().await;
            loaded
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::schema_raw::{OrgRow, UserRow};
    use datalib_etl::bulk::bulk_upsert_in_tx;
    use datalib_etl::doltlite_raw::WirePayload;
    use serde_json::json;

    fn now() -> datalib_time::IsoOffsetTimestamp {
        datalib_time::parse_strict("2026-06-11T00:00:00-07:00").unwrap()
    }

    fn later(minutes: i64) -> datalib_time::IsoOffsetTimestamp {
        datalib_time::IsoOffsetTimestamp::from(now().inner() + chrono::Duration::minutes(minutes))
    }

    fn make_user(id: &str, email: &str, name: &str) -> UserRow {
        UserRow {
            id_and_payload: WirePayload {
                id: id.into(),
                payload: serde_json::to_string(
                    &json!({"uuid": id, "email_address": email, "full_name": name}),
                )
                .unwrap(),
            },
            email: Some(email.into()),
            full_name: Some(name.into()),
        }
    }

    fn make_org(id: &str, name: &str) -> OrgRow {
        OrgRow {
            id_and_payload: WirePayload {
                id: id.into(),
                payload: serde_json::to_string(&json!({"uuid": id, "name": name})).unwrap(),
            },
            name: Some(name.into()),
        }
    }

    #[tokio::test]
    async fn user_and_org_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        {
            let mut tx = db.pool().begin().await.unwrap();
            bulk_upsert_in_tx(&mut tx, &[make_user("u1", "x@y", "X")], &now())
                .await
                .unwrap();
            bulk_upsert_in_tx(&mut tx, &[make_org("org-a", "A Org")], &now())
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        assert_eq!(db.first_user_uuid().await.unwrap(), Some("u1".into()));
    }

    /// A cold store has no marker, so `fetch` still makes the live
    /// `/organizations` call — which is what keeps that call working as a
    /// credential preflight on the run where a bad credential is likely.
    #[tokio::test]
    async fn sweep_age_is_none_before_any_sweep() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        assert!(
            db.sweep_age("orgs", &now()).await.unwrap().is_none(),
            "a store that never completed a sweep must report no marker"
        );
    }

    /// After recording, the marker is fresh — so a warm store serves the
    /// stored rows instead of re-listing.
    #[tokio::test]
    async fn recorded_sweep_is_fresh_and_serves_stored_orgs() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        {
            let mut tx = db.pool().begin().await.unwrap();
            db.store_orgs(&mut tx, &[make_org("org-a", "A Org")], &now(), &now())
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }

        let age = db
            .sweep_age("orgs", &later(5))
            .await
            .unwrap()
            .expect("marker recorded");
        assert_eq!(age, chrono::Duration::minutes(5));
        assert!(
            age < super::super::ORGS_TTL,
            "a just-recorded sweep must be inside the TTL"
        );
        assert_eq!(
            db.load_orgs().await.unwrap().len(),
            1,
            "the warm path must be able to serve the stored orgs"
        );
    }

    /// Re-recording moves the marker rather than inserting a second row —
    /// the `ON CONFLICT` upsert. A duplicate would make `sweep_age`'s
    /// single-row read arbitrary.
    #[tokio::test]
    async fn record_sweep_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.record_sweep("orgs", &now()).await.unwrap();
        db.record_sweep("orgs", &now()).await.unwrap();
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sync_scope_state WHERE scope = 'claude:sweep:orgs'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(n, 1, "expected exactly one marker row, got {n}");
    }

    /// The marker is namespaced per provider and per key, so it can share
    /// `sync_scope_state` with slack's markers and with the real resume
    /// cursors without collisions; the per-project markers are read as a
    /// set by project.
    #[tokio::test]
    async fn sweep_keys_are_namespaced() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.record_sweep("orgs", &now()).await.unwrap();
        db.record_sweep("project_docs:p-1", &now()).await.unwrap();
        assert!(db.sweep_age("orgs", &now()).await.unwrap().is_some());
        assert!(
            db.sweep_age("something-else", &now())
                .await
                .unwrap()
                .is_none(),
            "an unrelated key must not see the orgs marker"
        );
        let ages = db.project_docs_sweep_ages(&later(5)).await.unwrap();
        assert_eq!(
            ages.into_iter().collect::<Vec<_>>(),
            [("p-1".to_string(), chrono::Duration::minutes(5))]
        );
    }

    /// A sweep's age is measured from the run's now, not the wall clock:
    /// whether `/organizations` is asked for again must be the same on
    /// every replay of a run.
    #[tokio::test]
    async fn sweep_age_is_measured_from_the_runs_now() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.record_sweep("orgs", &now()).await.unwrap();
        let age = db.sweep_age("orgs", &later(7 * 60)).await.unwrap().unwrap();
        assert!(age > super::super::ORGS_TTL, "{age}");
    }

    /// A conversation whose every fetch failed is an id-only stub with a
    /// problem row. One no listing names any more goes, problem and all;
    /// one still listed, and anything with a payload, stays.
    #[tokio::test]
    async fn an_unlisted_stub_goes_with_its_problem() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        for id in ["stub-gone", "stub-listed"] {
            dr::record_object_error(&mut tx, "conversations", id, "HTTP 500")
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        sqlx::query("INSERT INTO conversations (id, payload) VALUES ('exported', jsonb('{}'))")
            .execute(db.pool())
            .await
            .unwrap();
        let listed: HashSet<String> = ["stub-listed".to_string()].into_iter().collect();
        assert_eq!(db.prune_unlisted_stubs(&listed).await.unwrap(), 1);
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM conversations ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(ids, ["exported", "stub-listed"]);
        let problems: Vec<String> =
            sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
                .fetch_all(db.pool())
                .await
                .unwrap();
        assert_eq!(problems, ["conversations:stub-listed"]);
    }

    fn conversation(uuid: &str, files: &[&str]) -> Conversation {
        Conversation {
            uuid: uuid.into(),
            org_uuid: "org-a".into(),
            org_name: "A".into(),
            name: Some(uuid.into()),
            updated_at: Some("2026-01-01T00:00:00Z".into()),
            payload: json!({"uuid": uuid}).to_string(),
            files: files
                .iter()
                .map(|f| json!({"file_uuid": f, "preview_url": format!("/api/files/{f}/preview")}))
                .collect(),
        }
    }

    /// A conversation's edges are the files it names: a refetch that
    /// drops one drops its edge, an edge whose bytes landed keeps them,
    /// and every edge is listed at the conversation's held version.
    #[tokio::test]
    async fn a_conversations_edges_follow_the_files_it_names() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        db.store_conversation(&mut tx, &conversation("c1", &["f1", "f2"]))
            .await
            .unwrap();
        owed::hold(&mut tx, "conversations", "c1", Some("v1"))
            .await
            .unwrap();
        db.store_blob(&mut tx, "c1#f1", &"ab".repeat(32))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            db.attachments_listed().await.unwrap(),
            [
                Listed::new("c1#f1", Some("v1")),
                Listed::new("c1#f2", Some("v1"))
            ]
        );

        let mut tx = db.pool().begin().await.unwrap();
        db.store_conversation(&mut tx, &conversation("c1", &["f1", "f3"]))
            .await
            .unwrap();
        owed::hold(&mut tx, "conversations", "c1", Some("v2"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            db.attachments_listed().await.unwrap(),
            [
                Listed::new("c1#f1", Some("v2")),
                Listed::new("c1#f3", Some("v2"))
            ],
            "the dropped file's edge went; the kept one is owed again at the new version"
        );
        assert!(db.blake3_of_file("f1").await.unwrap().is_some());
        db.close().await;
    }
}
