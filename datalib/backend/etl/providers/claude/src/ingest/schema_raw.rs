//! Raw-store schema for the Claude provider.
//!
//! What a table holds, and at what version, is `held_version` in its
//! `_bookkeeping` sidecar (`datalib_etl_web::owed`): a conversation and a
//! project at the `updated_at` the listing named, a project's docs
//! listing at the `updated_at` it was read for, an attachment edge at
//! the version its conversation is held at. No table carries a stamp of
//! its own.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};

pub const USERS: &str = "users";
pub const ORGS: &str = "orgs";
pub const PROJECTS: &str = "projects";
pub const PROJECT_DOCS: &str = "project_docs";
pub const PROJECT_DOCS_LISTINGS: &str = "project_docs_listings";
pub const CONVERSATIONS: &str = "conversations";
pub const ATTACHMENTS: &str = "claude_attachments";

pub const DATA_TABLES: &[&str] = &[
    USERS,
    ORGS,
    PROJECTS,
    PROJECT_DOCS,
    PROJECT_DOCS_LISTINGS,
    CONVERSATIONS,
    ATTACHMENTS,
];

/// `users` — one row per Anthropic user UUID.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "users")]
pub struct UserRow {
    pub id_and_payload: WirePayload,
    pub email: Option<String>,
    pub full_name: Option<String>,
}

/// `orgs` — one row per Anthropic organization UUID.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "orgs")]
pub struct OrgRow {
    pub id_and_payload: WirePayload,
    pub name: Option<String>,
}

/// `conversations` — one row per Anthropic conversation UUID.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "conversations")]
pub struct ConversationRow {
    pub id_and_payload: WirePayload,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub name: Option<String>,
    pub updated_at: Option<String>,
}

/// `projects` — one row per Claude Project UUID: the listing entry,
/// which is the whole of a project's metadata.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "projects")]
pub struct ProjectRow {
    pub id_and_payload: WirePayload,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub name: Option<String>,
    pub updated_at: Option<String>,
}

/// `project_docs` — one row per knowledge document attached to a
/// project.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "project_docs")]
pub struct ProjectDocRow {
    pub id_and_payload: WirePayload,
    pub project_uuid: Option<String>,
    pub file_name: Option<String>,
    pub created_at: Option<String>,
}

/// `project_docs_listings` — one row per project whose knowledge docs
/// have been listed, keyed by the project. The row is only its id: the
/// project `updated_at` the listing was read for is `held_version` in
/// its sidecar, and a listing that failed is an attempt there. Its own
/// row rather than the project's, so that the project being written
/// again does not clear a listing that failed, and so that the two
/// requests a project takes can each fail on their own.
pub const PROJECT_DOCS_LISTINGS_DDL: &str =
    "CREATE TABLE IF NOT EXISTS project_docs_listings (id TEXT PRIMARY KEY)";

pub const PROJECTS_ORG_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS projects_org ON projects(org_uuid)";

pub const PROJECT_DOCS_PROJECT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS project_docs_project ON project_docs(project_uuid)";

pub const CONVERSATIONS_ORG_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_org ON conversations(org_uuid)";

pub const CONVERSATIONS_UPDATED_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_updated ON conversations(updated_at)";

/// `claude_attachments` — N:M edge between one conversation's
/// attachment slot and a `cas_objects` blob, written with the
/// conversation that names the file. `blake3` is null until the CAS
/// write lands; an edge without bytes is what the attachment loop owes.
/// Universal CAS-edge shape; see [`datalib_etl::blob_cas::CasEdgeRow`].
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "claude_attachments")]
pub struct ConversationAttachmentRow {
    pub id: String,
    pub conversation_uuid: String,
    pub file_uuid: String,
    pub blake3: Option<String>,
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "what each table holds is its sidecar's held_version",
    apply: |conn| Box::pin(held_into_the_sidecar(conn)),
}];

/// Rung 1. A conversation and a project were held by their row's
/// `updated_at` matching the listing's, a project's docs listing by a
/// sweep marker alone, and an attachment by its edge having a `blake3`
/// or a `not_found` warning. Each becomes a `held_version` in its
/// sidecar, the docs listing in a row of its own, so the next run
/// fetches only what moved. The sweep markers stay: they still say when
/// a listing is due again.
async fn held_into_the_sidecar(conn: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    for ddl in [
        PROJECT_DOCS_LISTINGS_DDL.to_string(),
        dr::bookkeeping_ddl_for(PROJECT_DOCS_LISTINGS),
    ] {
        // Audited: this module's own DDL.
        sqlx::query(sqlx::AssertSqlSafe(ddl))
            .execute(&mut *conn)
            .await?;
    }
    for table in [CONVERSATIONS, PROJECTS, ATTACHMENTS] {
        let has_column: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?) WHERE name = 'held_version')",
        )
        .bind(format!("{table}_bookkeeping"))
        .fetch_one(&mut *conn)
        .await?;
        if !has_column {
            // Audited: `table` is one of this module's constants.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE {table}_bookkeeping ADD COLUMN held_version TEXT NULL"
            )))
            .execute(&mut *conn)
            .await?;
        }
    }
    for table in [CONVERSATIONS, PROJECTS] {
        // Audited: `table` is one of this module's constants.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {table}_bookkeeping SET held_version = \
                (SELECT t.updated_at FROM {table} t WHERE t.id = {table}_bookkeeping.id) \
             WHERE fetched_at_utc IS NOT NULL"
        )))
        .execute(&mut *conn)
        .await?;
    }
    let (now, _) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    sqlx::query(
        "INSERT OR IGNORE INTO project_docs_listings (id) \
         SELECT p.id FROM projects p \
         WHERE 'claude:sweep:project_docs:' || p.id IN (SELECT scope FROM sync_scope_state)",
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO project_docs_listings_bookkeeping (id, attempt_count, fetched_at_utc, held_version) \
         SELECT l.id, 0, ?, p.updated_at FROM project_docs_listings l JOIN projects p ON p.id = l.id \
         WHERE true \
         ON CONFLICT(id) DO UPDATE SET held_version = excluded.held_version, \
            fetched_at_utc = COALESCE(project_docs_listings_bookkeeping.fetched_at_utc, \
                                      excluded.fetched_at_utc)",
    )
    .bind(&now)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO claude_attachments_bookkeeping (id, attempt_count, fetched_at_utc, held_version) \
         SELECT a.id, 0, ?, b.held_version FROM claude_attachments a \
         JOIN conversations_bookkeeping b ON b.id = a.conversation_uuid \
         WHERE a.blake3 IS NOT NULL \
            OR a.id IN (SELECT substr(scope_key, length('claude_attachments:') + 1) FROM problems \
                        WHERE scope_kind = ? AND reason = ? AND scope_key LIKE 'claude_attachments:%') \
         ON CONFLICT(id) DO UPDATE SET held_version = excluded.held_version, \
            fetched_at_utc = COALESCE(claude_attachments_bookkeeping.fetched_at_utc, \
                                      excluded.fetched_at_utc)",
    )
    .bind(&now)
    .bind(datalib_problems::ScopeKind::Entity.as_str())
    .bind(datalib_problems::Reason::NotFound.as_str())
    .execute(&mut *conn)
    .await?;
    Ok(())
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        UserRow::ddl(),
        OrgRow::ddl(),
        ProjectRow::ddl(),
        ProjectDocRow::ddl(),
        PROJECT_DOCS_LISTINGS_DDL.to_string(),
        PROJECTS_ORG_INDEX_DDL.to_string(),
        PROJECT_DOCS_PROJECT_INDEX_DDL.to_string(),
        ConversationRow::ddl(),
        CONVERSATIONS_ORG_INDEX_DDL.to_string(),
        CONVERSATIONS_UPDATED_INDEX_DDL.to_string(),
    ];
    out.extend(ConversationAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
