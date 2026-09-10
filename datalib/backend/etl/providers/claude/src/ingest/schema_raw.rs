//! Raw-store schema for the Claude provider.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};

pub const DATA_TABLES: &[&str] = &[
    "users",
    "orgs",
    "projects",
    "project_docs",
    "conversations",
    "claude_attachments",
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

/// `projects` — one row per Claude Project UUID.
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

pub const PROJECTS_ORG_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS projects_org ON projects(org_uuid)";

pub const PROJECT_DOCS_PROJECT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS project_docs_project ON project_docs(project_uuid)";

pub const CONVERSATIONS_ORG_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_org ON conversations(org_uuid)";

pub const CONVERSATIONS_UPDATED_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_updated ON conversations(updated_at)";

/// Idempotent migration adding `conversations.org_name`. The
/// `CREATE TABLE IF NOT EXISTS` already declares this column, so on
/// fresh DBs the `ALTER` is a no-op. Kept around for older databases
/// created before `org_name` existed.
pub const MIGRATION_CONVERSATIONS_ADD_ORG_NAME: &str =
    "ALTER TABLE conversations ADD COLUMN org_name TEXT";

/// `claude_attachments` — N:M edge between one conversation's
/// attachment slot and a `cas_objects` blob. Replaces this provider's
/// use of the shared `blob_refs` table. Universal CAS-edge shape;
/// see [`datalib_etl::blob_cas::CasEdgeRow`].
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "claude_attachments")]
pub struct ConversationAttachmentRow {
    pub id: String,
    pub conversation_uuid: String,
    pub file_uuid: String,
    pub blake3: Option<String>,
}

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        UserRow::ddl(),
        OrgRow::ddl(),
        ProjectRow::ddl(),
        ProjectDocRow::ddl(),
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
