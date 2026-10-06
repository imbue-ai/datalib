//! Doltlite-backed raw store for the Claude provider.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::doltlite_raw::{self as dr};
use datalib_time::IsoOffsetTimestamp;

use super::schema_raw::full_ddl;

pub use datalib_etl::doltlite_raw::db_path_for;

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl());

impl RawDb {
    // ── users ──────────────────────────────────────────────────────

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
        let scope = format!("claude:sweep:{key}");
        let row = sqlx::query("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
            .bind(&scope)
            .fetch_optional(self.pool())
            .await
            .context("select claude sweep marker")?;
        let Some(row) = row else { return Ok(None) };
        let s: String = row
            .try_get("last_seen_at_utc")
            .context("read claude sweep timestamp")?;
        let dt = datalib_time::parse_strict(&s)
            .with_context(|| format!("parse claude sweep timestamp {s:?}"))?
            .inner();
        Ok(Some(now.inner() - dt))
    }

    /// Make the sweep `key` due, whatever its age.
    pub async fn forget_sweep(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM sync_scope_state WHERE scope = ?")
            .bind(format!("claude:sweep:{key}"))
            .execute(self.pool())
            .await
            .context("forget claude sweep marker")?;
        Ok(())
    }

    pub async fn record_sweep(&self, key: &str, now: &IsoOffsetTimestamp) -> Result<()> {
        let scope = format!("claude:sweep:{key}");
        dr::upsert_scope_state(self.pool(), &scope, &now.to_rfc3339())
            .await
            .context("record claude sweep marker")?;
        Ok(())
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

    // ── conversations: listing skip-check ──────────────────────────

    /// Bulk-read `(id → updated_at)` for the listed ids. Returns one
    /// entry per *existing* row (with a non-null `updated_at`). Missing
    /// ids are absent from the map — caller treats them as "we don't
    /// have this conversation yet, fetch it." Used by the listing pass
    /// to decide which conversations need a detail fetch. Rows only
    /// exist post-detail-fetch, so "id in map" ↔ "payload present."
    pub async fn existing_updated_at(&self, ids: &[&str]) -> Result<HashMap<String, String>> {
        self.existing_updated_at_in("conversations", ids).await
    }

    // ── projects ───────────────────────────────────────────────────

    /// Bulk-read `(project id → updated_at)` for the listed ids, same
    /// shape and same purpose as [`Self::existing_updated_at`]: the
    /// caller compares against the live listing to decide which
    /// projects changed. Missing ids are absent from the map.
    pub async fn existing_project_updated_at(
        &self,
        ids: &[&str],
    ) -> Result<HashMap<String, String>> {
        self.existing_updated_at_in("projects", ids).await
    }

    async fn existing_updated_at_in(
        &self,
        table: &str,
        ids: &[&str],
    ) -> Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, updated_at FROM {table} \
              WHERE id IN ({placeholders}) AND updated_at IS NOT NULL"
        );
        // Audited: `table` is a literal at every callsite; `placeholders` is a
        // `?,?,?` run sized from `ids.len()` and each id is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in ids {
            q = q.bind(*id);
        }
        let rows = q
            .fetch_all(self.pool())
            .await
            .with_context(|| format!("existing_updated_at {table}"))?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get("id").unwrap_or_default();
            if let Ok(ut) = r.try_get::<String, _>("updated_at") {
                out.insert(id, ut);
            }
        }
        Ok(out)
    }

    pub async fn load_projects(&self) -> Result<Vec<LoadedProject>> {
        load_projects_from(self.pool()).await
    }

    pub async fn load_project_docs(&self) -> Result<Vec<LoadedProjectDoc>> {
        load_project_docs_from(self.pool()).await
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
        datalib_etl::prune::delete_owned_in_tx(
            &mut tx,
            "claude_attachments",
            "conversation_uuid",
            &gone,
        )
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

    /// Every conversation with an attachment whose last attempt failed.
    /// One that was not there to fetch is a skip, not a failure, and waits
    /// for its conversation to change.
    pub async fn conversations_with_unfetched_attachments(&self) -> Result<Vec<String>> {
        sqlx::query_scalar(
            "SELECT DISTINCT a.conversation_uuid FROM claude_attachments a \
             JOIN problems p ON p.scope_kind = ? \
                AND p.scope_key = 'claude_attachments:' || a.id \
             WHERE p.reason = ? ORDER BY a.conversation_uuid",
        )
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(datalib_problems::Reason::FetchFailed.as_str())
        .fetch_all(self.pool())
        .await
        .context("select conversations with unfetched attachments")
    }

    pub async fn record_conversation_error(&self, id: &str, err: &str) -> Result<()> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .context("begin record_conversation_error tx")?;
        dr::record_object_error(&mut tx, "conversations", id, err).await?;
        tx.commit()
            .await
            .context("commit record_conversation_error tx")?;
        Ok(())
    }

    pub async fn failed_conversation_ids(&self) -> Result<Vec<String>> {
        dr::failed_ids(self.pool(), "conversations").await
    }

    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        load_conversations_from(self.pool()).await
    }

    /// Snapshot `(file_uuid → blake3)` for every attachment whose
    /// bytes have ever landed in the CAS. Loaded once at the start of
    /// a fetch run; updated in-place as new downloads land. Replaces
    /// the per-file SQL `attachment_has_bytes` lookup.
    pub async fn load_attachment_blake3s(&self) -> Result<HashMap<String, String>> {
        datalib_etl::blob_cas::load_blake3_index(self.pool(), "claude_attachments", "file_uuid")
            .await
    }
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
            bulk_upsert_in_tx(&mut tx, &[make_org("org-a", "A Org")], &now())
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        db.record_sweep("orgs", &now()).await.unwrap();

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
    /// cursors without collisions.
    #[tokio::test]
    async fn sweep_keys_are_namespaced() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.record_sweep("orgs", &now()).await.unwrap();
        assert!(db.sweep_age("orgs", &now()).await.unwrap().is_some());
        assert!(
            db.sweep_age("something-else", &now())
                .await
                .unwrap()
                .is_none(),
            "an unrelated key must not see the orgs marker"
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
        for id in ["stub-gone", "stub-listed"] {
            db.record_conversation_error(id, "HTTP 500").await.unwrap();
        }
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
}
