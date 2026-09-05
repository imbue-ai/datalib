//! Raw-store schema for the GitLab provider.

use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_macros::WirePayloadRow;
use serde_json::Value;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &["self_identity", "merge_requests", "discussions"];

/// `self_identity` — exactly one row carrying the authenticated
/// GitLab user (the result of `GET /user`).
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "self_identity")]
pub struct SelfIdentityRow {
    pub id_and_payload: WirePayload,
    pub username: Option<String>,
    pub web_url: Option<String>,
}

impl SelfIdentityRow {
    pub fn from_payload(payload: &Value) -> anyhow::Result<Self> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("/user response missing id"))?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            username: payload
                .get("username")
                .and_then(|v| v.as_str())
                .map(String::from),
            web_url: payload
                .get("web_url")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

/// `merge_requests` — one row per discovered merge request.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "merge_requests")]
pub struct MergeRequestRow {
    pub id_and_payload: WirePayload,
    pub project_full_path: String,
    pub mr_iid: i64,
    pub state: Option<String>,
    pub web_url: Option<String>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub start_sha: Option<String>,
    pub source_branch: Option<String>,
    pub target_branch: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
}

impl MergeRequestRow {
    pub fn from_payload(
        project_full_path: &str,
        mr_iid: u32,
        payload: &Value,
    ) -> anyhow::Result<Self> {
        let diff_refs = payload.get("diff_refs");
        Ok(Self {
            id_and_payload: WirePayload {
                id: mr_pk_recipe(project_full_path, mr_iid),
                payload: serde_json::to_string(payload)?,
            },
            project_full_path: project_full_path.to_string(),
            mr_iid: mr_iid as i64,
            state: payload
                .get("state")
                .and_then(|v| v.as_str())
                .map(String::from),
            web_url: payload
                .get("web_url")
                .and_then(|v| v.as_str())
                .map(String::from),
            head_sha: diff_refs
                .and_then(|d| d.get("head_sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            base_sha: diff_refs
                .and_then(|d| d.get("base_sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            start_sha: diff_refs
                .and_then(|d| d.get("start_sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            source_branch: payload
                .get("source_branch")
                .and_then(|v| v.as_str())
                .map(String::from),
            target_branch: payload
                .get("target_branch")
                .and_then(|v| v.as_str())
                .map(String::from),
            updated_at: payload
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            merged_at: payload
                .get("merged_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

/// Index on `merge_requests(project_full_path, mr_iid)` — supports
/// the project-scoped lookups render uses to walk all MRs under a
/// given project without scanning.
pub const MERGE_REQUESTS_BY_PROJ_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS merge_requests_by_proj ON merge_requests(project_full_path, mr_iid)";

/// `discussions` — one row per discussion thread on an MR.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "discussions")]
pub struct DiscussionRow {
    pub id_and_payload: WirePayload,
    pub project_full_path: String,
    pub mr_iid: i64,
    pub discussion_id: String,
    pub individual_note: Option<i64>,
    pub max_note_updated_at: Option<String>,
}

impl DiscussionRow {
    pub fn from_payload(
        project_full_path: &str,
        mr_iid: u32,
        payload: &Value,
    ) -> anyhow::Result<Self> {
        let discussion_id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("discussion missing id"))?
            .to_string();
        let max_note_updated_at = payload
            .get("notes")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .filter_map(|n| n.get("updated_at").and_then(|v| v.as_str()))
                    .max()
                    .map(String::from)
            });
        Ok(Self {
            id_and_payload: WirePayload {
                id: discussion_pk_recipe(project_full_path, mr_iid, &discussion_id),
                payload: serde_json::to_string(payload)?,
            },
            project_full_path: project_full_path.to_string(),
            mr_iid: mr_iid as i64,
            discussion_id,
            individual_note: payload
                .get("individual_note")
                .and_then(|v| v.as_bool())
                .map(|b| b as i64),
            max_note_updated_at,
        })
    }
}

/// Index on `discussions(project_full_path, mr_iid)` — supports the
/// "all discussions for one MR" scan that render uses to assemble
/// the per-MR rendered thread.
pub const DISCUSSIONS_BY_MR_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS discussions_by_mr ON discussions(project_full_path, mr_iid)";

/// Recipe for the [`MERGE_REQUESTS_DDL`] primary key.
pub fn mr_pk_recipe(project_full_path: &str, mr_iid: u32) -> String {
    format!("{project_full_path}!{mr_iid}")
}

/// Recipe for the [`DISCUSSIONS_DDL`] primary key.
pub fn discussion_pk_recipe(project_full_path: &str, mr_iid: u32, discussion_id: &str) -> String {
    format!("{project_full_path}!{mr_iid}#{discussion_id}")
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        SelfIdentityRow::ddl(),
        MergeRequestRow::ddl(),
        MERGE_REQUESTS_BY_PROJ_INDEX_DDL.to_string(),
        DiscussionRow::ddl(),
        DISCUSSIONS_BY_MR_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
