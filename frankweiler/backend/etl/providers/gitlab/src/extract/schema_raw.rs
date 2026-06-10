//! Raw-store schema for the GitLab provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! and [`docs/data_architecture_plan.md`](../../../../../docs/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! GitLab-specific notes:
//!
//! - **Event-shaped MRs / discussions / notes.** Merge requests carry
//!   `updated_at` and `merged_at` upstream; discussions carry per-note
//!   `updated_at` (we promote `max_note_updated_at` for cheap
//!   freshness queries). The translate side sources `GridRow.when_ts`
//!   from these — typically `notes[].updated_at` for the note family,
//!   `payload.updated_at` for the MR family.
//!
//! - **String PKs, no UUIDv5 in extract.** Raw-store PKs are
//!   constructed from upstream-stable parts —
//!   `"<project_full_path>!<mr_iid>"` for MRs and
//!   `"<project_full_path>!<mr_iid>#<discussion_id>"` for discussions.
//!   See [`mr_pk_recipe`] and [`discussion_pk_recipe`]. The
//!   Ship-of-Theseus UUIDv5 recipes from
//!   [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//!   §"Object identity" (e.g. `uuidv5(GITLAB_NS, "gitlab:{proj}:mr:{iid}")`,
//!   `uuidv5(GITLAB_NS, "gitlab:{proj}:note:{id}")` keyed off
//!   `note.web_url`-style ids) live in `crate::translate::parse`
//!   alongside the `GITLAB_UUID_NS` constant, since UUIDv5 minting is
//!   a translate-side concern — extract keeps the raw upstream
//!   identifiers.
//!
//! - **Refresh-window cursor.** Sync is incremental: per-scope
//!   `last_seen_at` lives in the shared `sync_scope_state` table (see
//!   [`frankweiler_etl::doltlite_raw`]) and is narrowed by
//!   `refresh_window_days` so the listing pass keeps re-checking the
//!   recent past even after we've advanced the cursor. The per-MR
//!   skip-check then compares listing `updated_at` against the locally
//!   stored value to avoid the detail + discussions fetch for MRs
//!   that haven't changed.
//!
//! - **Cross-references for the code-review-thread family.** GitLab
//!   shares its code-review-thread shape with GitHub: an MR + its
//!   notes line up with a PR + its review-comments. We promote
//!   `head_sha` / `base_sha` / `start_sha` from `payload.diff_refs`
//!   into top-level columns so a downstream join from
//!   `GridRow.git_sha` (translate-side) and the MR's numeric `iid`
//!   (preserved as `GridRow.external_id`) back to this row is one
//!   indexed lookup.

use frankweiler_etl::doltlite_raw as dr;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
pub const DATA_TABLES: &[&str] = &["self_identity", "merge_requests", "discussions"];

/// `self_identity` — exactly one row carrying the authenticated
/// GitLab user (the result of `GET /user`).
///
/// Columns:
/// - `id` — the upstream GitLab user id (numeric, stringified).
///   Primary key. One row per file in practice — the data root is
///   1:1 with a single GitLab account.
/// - `username` — denormalized `payload.username` for cheap lookups
///   without cracking the JSON.
/// - `web_url` — denormalized `payload.web_url`, same rationale.
/// - `payload` — the raw `/user` JSON object (JSONB-encoded on disk).
pub const SELF_IDENTITY_DDL: &str = "CREATE TABLE IF NOT EXISTS self_identity (
    id TEXT PRIMARY KEY,
    username TEXT NULL,
    web_url TEXT NULL,
    payload TEXT NULL
)";

/// `merge_requests` — one row per discovered merge request.
///
/// Upstream provenance: each row is the result of
/// `GET /projects/{pid}/merge_requests/{iid}`. The listing pass
/// (search across `created_by_me` / `assigned_to_me` / `reviewer`
/// scopes) surfaces `(proj, iid, updated_at)`; the detail fetch
/// populates everything else.
///
/// Event-shape: `payload.updated_at` is the closest event-shaped
/// timestamp — translate sources it into `GridRow.when_ts` for the
/// MR family.
///
/// Columns:
/// - `id` — `"<project_full_path>!<mr_iid>"`. Primary key. Both parts
///   are upstream-stable and known before the detail fetch; see
///   [`mr_pk_recipe`].
/// - `project_full_path` — `namespace/project` slug, the half of the
///   PK that identifies the project. Promoted so translate / indexer
///   joins don't have to parse the PK.
/// - `mr_iid` — per-project MR `iid` (the small integer in the URL),
///   the other half of the PK.
/// - `state` — `opened` / `merged` / `closed` etc., promoted from
///   `payload.state` for cheap filtering.
/// - `web_url` — `payload.web_url`, the canonical gitlab.com URL.
/// - `head_sha`, `base_sha`, `start_sha` — promoted from
///   `payload.diff_refs`. Drives the GitLab side of the
///   code-review-thread join with the local git checkout: a row in
///   `GridRow` with a matching `git_sha` can resolve back to this
///   MR via an indexed lookup.
/// - `source_branch`, `target_branch` — promoted refs, same
///   provenance.
/// - `updated_at` — upstream `payload.updated_at`. Used both as the
///   per-MR skip-check (compared against the listing's
///   `updated_at`) and as `GridRow.when_ts` for the MR family.
/// - `merged_at` — upstream `payload.merged_at`, nullable until
///   the MR merges. Preserved for translate so a `merged_at`-shaped
///   `when_ts` is available when wanted.
/// - `payload` — the raw MR detail JSON (JSONB-encoded on disk).
pub const MERGE_REQUESTS_DDL: &str = "CREATE TABLE IF NOT EXISTS merge_requests (
    id TEXT PRIMARY KEY,
    project_full_path TEXT NOT NULL,
    mr_iid INTEGER NOT NULL,
    state TEXT NULL,
    web_url TEXT NULL,
    head_sha TEXT NULL,
    base_sha TEXT NULL,
    start_sha TEXT NULL,
    source_branch TEXT NULL,
    target_branch TEXT NULL,
    updated_at TEXT NULL,
    merged_at TEXT NULL,
    payload TEXT NULL
)";

/// Index on `merge_requests(project_full_path, mr_iid)` — supports
/// the project-scoped lookups translate uses to walk all MRs under a
/// given project without scanning.
pub const MERGE_REQUESTS_BY_PROJ_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS merge_requests_by_proj ON merge_requests(project_full_path, mr_iid)";

/// `discussions` — one row per discussion thread on an MR.
///
/// Upstream provenance: each row comes from
/// `GET /projects/{pid}/merge_requests/{iid}/discussions`. A
/// discussion bundles 1..N notes; we store the whole bundle as one
/// row.
///
/// Event-shape: discussions don't have their own `updated_at`. We
/// promote `max_note_updated_at` = `max(notes[].updated_at)` so
/// freshness queries (and the translate side's `GridRow.when_ts`
/// for the note family) don't have to crack the payload open.
///
/// Columns:
/// - `id` — `"<project_full_path>!<mr_iid>#<discussion_id>"`. Primary
///   key. GitLab's `discussion_id` is a hex string scoped to the
///   project — we include the MR scope to keep the PK construction
///   trivial and avoid surprises around bare discussion id
///   collisions across projects. See [`discussion_pk_recipe`].
/// - `project_full_path`, `mr_iid` — promoted FK halves into
///   [`MERGE_REQUESTS_DDL`]'s key.
/// - `discussion_id` — the upstream `payload.id` (hex string), kept
///   separately so callers can recover the PK recipe without
///   parsing.
/// - `individual_note` — `payload.individual_note` boolean (stored
///   as 0/1 INTEGER), promoted because it changes the rendering
///   shape (single-note vs. threaded reply chain).
/// - `max_note_updated_at` — `max(payload.notes[].updated_at)`,
///   promoted for cheap freshness queries. The closest event-shaped
///   value for the discussion as a whole.
/// - `payload` — the raw discussion JSON with its `notes` array
///   (JSONB-encoded on disk).
pub const DISCUSSIONS_DDL: &str = "CREATE TABLE IF NOT EXISTS discussions (
    id TEXT PRIMARY KEY,
    project_full_path TEXT NOT NULL,
    mr_iid INTEGER NOT NULL,
    discussion_id TEXT NOT NULL,
    individual_note INTEGER NULL,
    max_note_updated_at TEXT NULL,
    payload TEXT NULL
)";

/// Index on `discussions(project_full_path, mr_iid)` — supports the
/// "all discussions for one MR" scan that translate uses to assemble
/// the per-MR rendered thread.
pub const DISCUSSIONS_BY_MR_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS discussions_by_mr ON discussions(project_full_path, mr_iid)";

/// Recipe for the [`MERGE_REQUESTS_DDL`] primary key.
///
/// Format: `"{project_full_path}!{mr_iid}"`. Both parts come straight
/// from upstream (the project slug from `web_url`, the iid from the
/// listing payload), are stable across re-fetches, and are known
/// before the detail fetch so the PK is constructible at discovery
/// time.
///
/// Lives here next to the schema so writers (extract upsert) and
/// readers (translate's fixture-replay synthesizer in
/// `crate::synthesize`) agree on the exact format — same pattern as
/// signal's [`chat_item_id_recipe`].
///
/// [`chat_item_id_recipe`]: ../../../signal/src/extract/schema_raw.rs
pub fn mr_pk_recipe(project_full_path: &str, mr_iid: u32) -> String {
    format!("{project_full_path}!{mr_iid}")
}

/// Recipe for the [`DISCUSSIONS_DDL`] primary key.
///
/// Format:
/// `"{project_full_path}!{mr_iid}#{discussion_id}"`. The MR-scoped
/// prefix is what avoids bare discussion-id collisions across
/// projects — GitLab's discussion id is only guaranteed unique within
/// its parent MR.
///
/// Same writers-and-readers-agree rationale as [`mr_pk_recipe`].
pub fn discussion_pk_recipe(project_full_path: &str, mr_iid: u32, discussion_id: &str) -> String {
    format!("{project_full_path}!{mr_iid}#{discussion_id}")
}

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
///
/// Schema-local glue, kept here so the "what tables exist?" answer
/// is one function call from this file. Heavier composition (e.g. a
/// repo-wide bookkeeping macro) is deferred to P1.1.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        SELF_IDENTITY_DDL.to_string(),
        MERGE_REQUESTS_DDL.to_string(),
        MERGE_REQUESTS_BY_PROJ_INDEX_DDL.to_string(),
        DISCUSSIONS_DDL.to_string(),
        DISCUSSIONS_BY_MR_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
