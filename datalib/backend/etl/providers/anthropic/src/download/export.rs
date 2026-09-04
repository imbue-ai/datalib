//! Ingest an unpacked Claude **bulk export** into this provider's raw
//! store — the download wave of the `claude_export` source type.
//!
//! The export is a small tree of JSON files sitting wherever the user
//! unzipped it:
//!
//! ```text
//! <input_path>/
//!   users.json            # array of accounts (optional)
//!   conversations.json    # array of conversations, in export shape
//!   projects/*.json       # one Claude Project per file, `docs` nested
//! ```
//!
//! Every row lands in the same six tables the live-API downloader
//! writes (`crate::download::db`), so the render step has exactly one
//! input shape to be correct against. Before this existed, the renderer
//! read the export tree in place and had a second parser for it; that
//! branch is gone.
//!
//! ## What the columns mean here
//!
//! `conversations.org_uuid` / `org_name` stay **NULL**: an export
//! carries no organization anywhere, and only the API walk (which
//! learns the org from `/organizations`) can fill them. The renderer
//! reads that NULL as "this payload is already export-shaped, don't run
//! it through `normalize_to_export_shape`" — see
//! [`crate::render::parse::parse_loaded`]. A conversation whose payload
//! happens to carry its own `_source.org_uuid` (because the export was
//! produced from our own API mirror) still gets its org onto the grid
//! row: that field is read from the payload, not from the column.
//!
//! `projects.org_uuid` / `org_name` are the other way round — the
//! render side reads a project's org from the **column** — so we lift
//! `_source.org_uuid` / `_source.org_name` out of the project payload
//! at ingest time when the file has them.
//!
//! ## Snapshot semantics
//!
//! A bulk export is a complete snapshot of the account, so what it does
//! not contain has been deleted. After upserting everything the export
//! holds, [`prune_to`] drops the rows for ids the export no longer
//! mentions — which is the deletion detection reading the tree in place
//! could never give us. Pruning is per-table and only runs when that
//! table's source file was actually present, so pointing the source at
//! a partially-unpacked export can't wipe the store.
//!
//! ## Why there is no blob CAS here
//!
//! A Claude bulk export ships JSON only. `chat_messages[*].files[]`
//! entries name a `preview_url` back on claude.ai, and fetching that
//! needs the credentials this source type deliberately does not have;
//! `chat_messages[*].attachments[]` carry their text inline and have no
//! bytes to fetch at all (see DOWNLOAD.md, "Attachments"). So there is
//! nothing on disk to content-address and `anthropic_attachments` stays
//! empty for an export-backed store. If Anthropic ever starts shipping
//! the binaries inside the export, this is where the CAS walk goes.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::download_run::DownloadRun;
use serde::Serialize;
use serde_json::Value;
use sqlx::{Sqlite, Transaction};
use tracing::{info, instrument, warn};

use super::db::{db_path_for, RawDb};
use super::schema_raw::{
    ConversationRow as ConversationRowSchema, ProjectDocRow, ProjectRow, UserRow,
};

/// Rows per `DELETE … WHERE id IN (…)` statement while pruning. Well
/// under SQLite's parameter ceiling.
const DELETE_CHUNK: usize = 400;

#[derive(Debug, Clone)]
pub struct IngestOptions {
    /// Path to the doltlite database file, or the per-source raw
    /// directory holding it. Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. The step driver pre-opens so the
    /// post-download commit runs on the same connection.
    pub db: Option<RawDb>,
    /// The unpacked export directory (`common.input_path`).
    pub input_path: PathBuf,
    /// Run timestamp, shared by every `<table>_bookkeeping.fetched_at`
    /// stamp this ingest writes.
    pub now: String,
    pub progress: datalib_etl::progress::Progress,
    pub control: datalib_etl::control::DownloadControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct IngestSummary {
    pub users: usize,
    pub conversations: usize,
    pub projects: usize,
    pub project_docs: usize,
    /// Rows dropped because the export no longer mentions them.
    pub pruned: usize,
}

/// Ingest the export at `opts.input_path` into the raw store.
#[instrument(skip_all, fields(export = %opts.input_path.display()))]
pub async fn ingest(opts: IngestOptions) -> Result<IngestSummary> {
    let db_path = db_path_for(&opts.db_path);
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path)
            .await
            .with_context(|| format!("open raw db {}", db_path.display()))?,
    };

    // Each run replaces the snapshot wholesale anyway (upsert + prune),
    // so a reset only changes how the intermediate diff reads. Do it
    // when asked so `--reset-and-redownload` means the same thing here
    // as everywhere else.
    if opts.control.reset_and_redownload {
        info!(event = "claude_export_reset_and_redownload");
        db.reset().await.context("reset raw db before re-ingest")?;
    }

    let run_config = serde_json::json!({ "input_path": opts.input_path });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let mut summary = IngestSummary::default();
    let result = ingest_all(&db, &opts, &mut summary).await;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// The whole snapshot in one transaction: a half-written export is
/// never what render sees.
async fn ingest_all(db: &RawDb, opts: &IngestOptions, summary: &mut IngestSummary) -> Result<()> {
    let dir = &opts.input_path;
    // Read every file before opening the transaction, so a malformed
    // one fails the run without having touched the store.
    let users = read_json_array(&dir.join("users.json"))?;
    let conversations = read_json_array(&dir.join("conversations.json"))?.ok_or_else(|| {
        anyhow::anyhow!(
            "no conversations.json under {} — point `common.input_path` at the \
             directory you unpacked the Claude export into",
            dir.display()
        )
    })?;
    let projects = read_project_files(dir)?;

    let mut tx = db.pool().begin().await.context("begin export ingest tx")?;

    if let Some(users) = users.as_ref() {
        summary.users = upsert_users(&mut tx, users, &opts.now).await?;
        summary.pruned += prune_to(&mut tx, "users", &ids_of(users, "uuid")).await?;
    } else {
        // Not fatal: every export conversation already names its own
        // `account`, so the only thing a missing users.json costs is
        // the account row itself.
        warn!(
            event = "claude_export_no_users_json",
            dir = %dir.display(),
        );
    }

    if let Some(projects) = projects.as_ref() {
        let (n_projects, n_docs) = upsert_projects(&mut tx, projects, &opts.now).await?;
        summary.projects = n_projects;
        summary.project_docs = n_docs;
        let project_ids = ids_of(projects, "uuid");
        summary.pruned += prune_to(&mut tx, "projects", &project_ids).await?;
        summary.pruned += prune_to(&mut tx, "project_docs", &project_doc_ids(projects)).await?;
    }

    summary.conversations = upsert_conversations(&mut tx, &conversations, &opts.now).await?;
    summary.pruned += prune_to(&mut tx, "conversations", &ids_of(&conversations, "uuid")).await?;
    opts.progress.set_message(&format!(
        "{} conversations, {} projects, {} knowledge docs",
        summary.conversations, summary.projects, summary.project_docs,
    ));

    tx.commit().await.context("commit export ingest tx")?;
    if summary.pruned > 0 {
        // Loud on purpose: this is the one path that removes stored
        // rows, and "the export got smaller" is worth seeing.
        info!(
            event = "claude_export_pruned",
            rows = summary.pruned,
            "dropped rows the export no longer contains",
        );
    }
    Ok(())
}

/// Read one top-level JSON array file. `Ok(None)` when the file is
/// absent; an error when it is present but unreadable or not an array
/// (a corrupt export should fail loudly, not ingest half of itself).
fn read_json_array(path: &Path) -> Result<Option<Vec<Value>>> {
    if !path.exists() {
        return Ok(None);
    }
    let txt = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&txt).with_context(|| format!("parse {}", path.display()))?;
    match v {
        Value::Array(a) => Ok(Some(a)),
        _ => bail!("{} must hold a JSON array", path.display()),
    }
}

/// Read `projects/*.json`, one project per file, in sorted filename
/// order. `Ok(None)` when there is no `projects/` directory at all —
/// which is what keeps the prune from deleting a store's projects just
/// because this export didn't include any.
fn read_project_files(dir: &Path) -> Result<Option<Vec<Value>>> {
    let projects_dir = dir.join("projects");
    if !projects_dir.is_dir() {
        return Ok(None);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&projects_dir)
        .with_context(|| format!("read {}", projects_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let txt = std::fs::read_to_string(&f).with_context(|| format!("read {}", f.display()))?;
        let v: Value =
            serde_json::from_str(&txt).with_context(|| format!("parse {}", f.display()))?;
        out.push(v);
    }
    Ok(Some(out))
}

/// The `uuid` of every entry that has one, as a set.
fn ids_of(items: &[Value], key: &str) -> HashSet<String> {
    items
        .iter()
        .filter_map(|v| v.get(key).and_then(Value::as_str))
        .map(String::from)
        .collect()
}

/// Every knowledge-document uuid across every project.
fn project_doc_ids(projects: &[Value]) -> HashSet<String> {
    let mut out = HashSet::new();
    for p in projects {
        for d in docs_of(p) {
            if let Some(id) = d.get("uuid").and_then(Value::as_str) {
                out.insert(id.to_string());
            }
        }
    }
    out
}

/// A project's nested knowledge documents. The bulk export nests them
/// under `docs`; the live API serves them from a separate endpoint.
fn docs_of(project: &Value) -> &[Value] {
    project
        .get("docs")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn str_field(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}

async fn upsert_users(
    tx: &mut Transaction<'_, Sqlite>,
    users: &[Value],
    now: &str,
) -> Result<usize> {
    let mut rows = Vec::with_capacity(users.len());
    for u in users {
        let Some(id) = str_field(u, "uuid") else {
            continue;
        };
        rows.push(UserRow {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(u).context("serialize export user")?,
            },
            email: str_field(u, "email_address"),
            full_name: str_field(u, "full_name"),
        });
    }
    bulk_upsert_in_tx(tx, &rows, now).await?;
    Ok(rows.len())
}

/// Store each conversation exactly as the export wrote it, with the org
/// columns left NULL — see the module header for why both halves of
/// that matter.
async fn upsert_conversations(
    tx: &mut Transaction<'_, Sqlite>,
    convs: &[Value],
    now: &str,
) -> Result<usize> {
    let mut rows = Vec::with_capacity(convs.len());
    for c in convs {
        let Some(id) = str_field(c, "uuid") else {
            warn!(event = "claude_export_conversation_without_uuid");
            continue;
        };
        rows.push(ConversationRowSchema {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(c).context("serialize export conversation")?,
            },
            org_uuid: None,
            org_name: None,
            name: str_field(c, "name"),
            updated_at: str_field(c, "updated_at"),
        });
    }
    bulk_upsert_in_tx(tx, &rows, now).await?;
    Ok(rows.len())
}

/// Split each project into its metadata row and its knowledge-document
/// rows — the same two tables the API walk fills from its two separate
/// endpoints. The nested `docs` array is bookkeeping we exploded out,
/// so it comes off the stored project payload.
async fn upsert_projects(
    tx: &mut Transaction<'_, Sqlite>,
    projects: &[Value],
    now: &str,
) -> Result<(usize, usize)> {
    let mut project_rows = Vec::with_capacity(projects.len());
    let mut doc_rows: Vec<ProjectDocRow> = Vec::new();
    for p in projects {
        let Some(project_uuid) = str_field(p, "uuid") else {
            warn!(event = "claude_export_project_without_uuid");
            continue;
        };
        for d in docs_of(p) {
            let Some(doc_uuid) = str_field(d, "uuid") else {
                continue;
            };
            doc_rows.push(ProjectDocRow {
                id_and_payload: WirePayload {
                    id: doc_uuid,
                    payload: serde_json::to_string(d).context("serialize project doc")?,
                },
                project_uuid: Some(project_uuid.clone()),
                file_name: str_field(d, "file_name"),
                created_at: str_field(d, "created_at"),
            });
        }

        // The export tree carries no org scope of its own; `_source` is
        // present when the file came out of our own API mirror.
        let source = p.get("_source");
        let org_uuid = source.and_then(|s| str_field(s, "org_uuid"));
        let org_name = source.and_then(|s| str_field(s, "org_name"));

        let mut stored = super::canonicalize_project_payload(p);
        if let Some(o) = stored.as_object_mut() {
            o.remove("docs");
        }
        project_rows.push(ProjectRow {
            id_and_payload: WirePayload {
                id: project_uuid,
                payload: serde_json::to_string(&stored).context("serialize export project")?,
            },
            org_uuid,
            org_name,
            name: str_field(p, "name"),
            updated_at: str_field(p, "updated_at"),
        });
    }
    bulk_upsert_in_tx(tx, &project_rows, now).await?;
    bulk_upsert_in_tx(tx, &doc_rows, now).await?;
    Ok((project_rows.len(), doc_rows.len()))
}

/// Delete every row of `table` (and its bookkeeping sidecar row) whose
/// id is not in `keep`. Returns how many rows went.
///
/// Only correct because a bulk export is a complete snapshot: an id the
/// export omits is an entity the user deleted upstream. The API walk
/// deliberately does *not* do this — a listing can omit a conversation
/// for reasons other than deletion (permissions, `since`) — which is
/// why this lives here rather than in the shared store helpers.
async fn prune_to(
    tx: &mut Transaction<'_, Sqlite>,
    table: &'static str,
    keep: &HashSet<String>,
) -> Result<usize> {
    // Audited: `table` is a `&'static str` at every callsite.
    let existing: Vec<String> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT id FROM {table}")))
            .fetch_all(&mut **tx)
            .await
            .with_context(|| format!("list {table} ids for prune"))?;
    let gone: Vec<String> = existing
        .into_iter()
        .filter(|id| !keep.contains(id))
        .collect();
    if gone.is_empty() {
        return Ok(0);
    }
    for chunk in gone.chunks(DELETE_CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        for sql in [
            format!("DELETE FROM {table} WHERE id IN ({placeholders})"),
            format!("DELETE FROM {table}_bookkeeping WHERE id IN ({placeholders})"),
        ] {
            // Audited: `table` is a `&'static str`; `placeholders` is a
            // `?,?,?` run sized from the chunk and every id is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for id in chunk {
                q = q.bind(id.clone());
            }
            q.execute(&mut **tx)
                .await
                .with_context(|| format!("prune {table}"))?;
        }
    }
    Ok(gone.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    /// `(id → payload)` for one table.
    async fn dump(pool: &sqlx::SqlitePool, table: &str) -> HashMap<String, Value> {
        let rows: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id, json(payload) FROM {table}"
        )))
        .fetch_all(pool)
        .await
        .unwrap();
        rows.into_iter()
            .map(|(id, p)| (id, serde_json::from_str(&p).unwrap()))
            .collect()
    }

    const NOW: &str = "2026-09-04T00:00:00-07:00";

    fn write(dir: &Path, rel: &str, v: &Value) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    fn opts(export: &Path, raw: &Path) -> IngestOptions {
        IngestOptions {
            db_path: raw.to_path_buf(),
            db: None,
            input_path: export.to_path_buf(),
            now: NOW.to_string(),
            progress: Default::default(),
            control: Default::default(),
        }
    }

    fn conv(uuid: &str, name: &str) -> Value {
        json!({
            "uuid": uuid,
            "name": name,
            "updated_at": "2026-01-02T03:04:05Z",
            "account": {"uuid": "acct-1"},
            "chat_messages": [
                {"uuid": format!("{uuid}-m1"), "sender": "human", "text": "hi",
                 "content": [{"type": "text", "text": "hi", "flags": null}]}
            ],
        })
    }

    /// The whole point of the ingest: an export directory becomes rows
    /// in the same tables the API downloader writes.
    #[tokio::test]
    async fn ingests_users_conversations_and_projects() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "users.json",
            &json!([{"uuid": "acct-1", "email_address": "picard@enterprise", "full_name": "JLP"}]),
        );
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", "First"), conv("c2", "Second")]),
        );
        write(
            ex.path(),
            "projects/bridge.json",
            &json!({
                "uuid": "p1",
                "name": "Bridge Ops",
                "creator": {"uuid": "acct-1"},
                "docs": [{"uuid": "d1", "file_name": "notes.md", "content": "hello"}],
            }),
        );

        let s = ingest(opts(ex.path(), raw.path())).await.unwrap();
        assert_eq!(
            (
                s.users,
                s.conversations,
                s.projects,
                s.project_docs,
                s.pruned
            ),
            (1, 2, 1, 1, 0)
        );

        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        assert_eq!(dump(db.pool(), "conversations").await.len(), 2);
        assert_eq!(
            db.first_user_uuid().await.unwrap().as_deref(),
            Some("acct-1")
        );

        // `docs` is exploded into its own table, exactly as the API
        // walk's two endpoints land it.
        let projects = dump(db.pool(), "projects").await;
        assert!(
            projects["p1"].get("docs").is_none(),
            "the nested docs array must not stay on the project payload"
        );
        assert_eq!(dump(db.pool(), "project_docs").await.len(), 1);

        // NULL org is what tells render the payload is already
        // export-shaped.
        let convs = db.load_conversations().await.unwrap();
        assert!(convs.iter().all(|c| c.org_uuid.is_none()));
    }

    /// A bulk export is a complete snapshot, so an id it stops
    /// mentioning is a deletion. This is the signal reading the export
    /// tree in place could never produce.
    #[tokio::test]
    async fn a_shrinking_export_deletes_the_rows_it_dropped() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", "First"), conv("c2", "Second")]),
        );
        ingest(opts(ex.path(), raw.path())).await.unwrap();

        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", "First")]),
        );
        let s = ingest(opts(ex.path(), raw.path())).await.unwrap();
        assert_eq!(s.pruned, 1, "c2 vanished from the export");

        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        let ids: Vec<String> = db
            .load_conversations()
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec!["c1".to_string()]);

        // The always-paired lifecycle holds through a delete too: the
        // sidecar row goes with its object row.
        let orphans: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM conversations_bookkeeping WHERE id = 'c2'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(orphans, 0, "bookkeeping sidecar outlived its row");
    }

    /// Pruning is per-table and gated on the file being there, so a
    /// half-unpacked export can't wipe what a complete one ingested.
    #[tokio::test]
    async fn a_missing_projects_dir_leaves_stored_projects_alone() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        write(
            ex.path(),
            "conversations.json",
            &json!([conv("c1", "First")]),
        );
        write(
            ex.path(),
            "projects/bridge.json",
            &json!({"uuid": "p1", "name": "Bridge Ops"}),
        );
        ingest(opts(ex.path(), raw.path())).await.unwrap();

        std::fs::remove_dir_all(ex.path().join("projects")).unwrap();
        let s = ingest(opts(ex.path(), raw.path())).await.unwrap();
        assert_eq!(s.pruned, 0);
        let db = RawDb::open(&db_path_for(raw.path())).await.unwrap();
        assert_eq!(dump(db.pool(), "projects").await.len(), 1);
    }

    /// Pointing the source at the wrong directory says so, rather than
    /// quietly ingesting nothing.
    #[tokio::test]
    async fn a_directory_without_conversations_json_fails_loudly() {
        let ex = tempfile::tempdir().unwrap();
        let raw = tempfile::tempdir().unwrap();
        let err = ingest(opts(ex.path(), raw.path()))
            .await
            .expect_err("no conversations.json");
        let err = format!("{err:#}");
        assert!(err.contains("conversations.json"), "{err}");
        assert!(err.contains("input_path"), "{err}");
    }
}
