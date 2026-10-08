//! Raw-store schema for the ChatGPT provider.
//!
//! What a table holds, and at what version, is `held_version` in its
//! `_bookkeeping` sidecar (`datalib_etl_web::owed`): a conversation at the
//! `update_time` the listing named, in whole seconds; an attachment
//! edge at the version its conversation is held at. No table carries a
//! stamp of its own.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};

pub const ME: &str = "me";
pub const CONVERSATIONS: &str = "conversations";
pub const ATTACHMENTS: &str = "chatgpt_attachments";

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &[ME, CONVERSATIONS, ATTACHMENTS];

/// `me` — the upstream `/backend-api/me` response.
///
/// One row per ChatGPT account. We keep `email` and `name` denormalized
/// for cheap predicate queries; the full response stays in `payload`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "me")]
pub struct MeRow {
    pub id_and_payload: WirePayload,
    pub email: Option<String>,
    pub name: Option<String>,
}

/// `conversations` — one row per ChatGPT conversation id. `update_time`
/// is the detail endpoint's, JSON-encoded; what the listing named is
/// the sidecar's `held_version`.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "conversations")]
pub struct ConversationRow {
    pub id_and_payload: WirePayload,
    pub title: Option<String>,
    pub update_time: Option<String>,
}

pub const CONVERSATIONS_UPDATE_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS conversations_update ON conversations(update_time)";

/// `chatgpt_attachments` — N:M edge between one conversation's
/// attachment slot and a `cas_objects` blob, written with the
/// conversation that names the file. `blake3` is null until the CAS
/// write lands; an edge without bytes is what the attachment loop owes.
/// The per-attachment metadata render needs (file name, mime type)
/// already lives in `conversations.payload.mapping[*]...`; only the
/// (file_id → blake3) mapping is here. Universal CAS-edge shape; see
/// [`datalib_etl::blob_cas::CasEdgeRow`].
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "chatgpt_attachments")]
pub struct ConversationAttachmentRow {
    pub id: String,
    pub conversation_id: String,
    pub file_id: String,
    pub blake3: Option<String>,
}

/// The raw store's migration ladder (etl/README.md §"The migration
/// ladder").
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "what each table holds is its sidecar's held_version",
    apply: |conn| Box::pin(held_into_the_sidecar(conn)),
}];

/// Rung 1. A conversation was held by its row's `update_time` matching
/// the listing's at whole seconds, and an attachment by its edge having
/// a `blake3` or a `not_found` warning. Each becomes a `held_version`
/// in its sidecar, so the next run fetches only what moved.
async fn held_into_the_sidecar(conn: &mut sqlx::SqliteConnection) -> anyhow::Result<()> {
    for table in [CONVERSATIONS, ATTACHMENTS] {
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
    // The listing's `update_time` reduces to whole seconds; the detail's
    // is the same instant as an epoch float, which the row kept.
    sqlx::query(
        "UPDATE conversations_bookkeeping SET held_version = \
            (SELECT CASE WHEN c.update_time GLOB '[0-9]*' \
                    THEN CAST(CAST(c.update_time AS INTEGER) AS TEXT) END \
             FROM conversations c WHERE c.id = conversations_bookkeeping.id) \
         WHERE fetched_at_utc IS NOT NULL",
    )
    .execute(&mut *conn)
    .await?;
    let (now, _) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    sqlx::query(
        "INSERT INTO chatgpt_attachments_bookkeeping (id, attempt_count, fetched_at_utc, held_version) \
         SELECT a.id, 0, ?, b.held_version FROM chatgpt_attachments a \
         JOIN conversations_bookkeeping b ON b.id = a.conversation_id \
         WHERE a.blake3 IS NOT NULL \
            OR a.id IN (SELECT substr(scope_key, length('chatgpt_attachments:') + 1) FROM problems \
                        WHERE scope_kind = ? AND reason = ? AND scope_key LIKE 'chatgpt_attachments:%') \
         ON CONFLICT(id) DO UPDATE SET held_version = excluded.held_version, \
            fetched_at_utc = COALESCE(chatgpt_attachments_bookkeeping.fetched_at_utc, \
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
        MeRow::ddl(),
        ConversationRow::ddl(),
        CONVERSATIONS_UPDATE_INDEX_DDL.to_string(),
    ];
    out.extend(ConversationAttachmentRow::all_ddl());
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
