# Porting a provider to the Signal/email pattern

> **Archived (2026-07).** All three targets (anthropic, chatgpt,
> whatsapp) have been ported. Superseded by
> [`provider_migration_dolt_diff_and_cas_edge.md`](../provider_migration_dolt_diff_and_cas_edge.md),
> which consolidates what was learned across the six ported providers and
> is the live recipe for the remaining ones.

This doc is a per-provider migration recipe for converting an ETL
provider to the same architecture Signal (commits `8e90289`…`d392075`)
and email (commits `72ec85c`…`6d1fbba`) now use:

1. **Wire-payload schema via `#[derive(WirePayloadRow)]`** — row
   structs are the single source of truth; the derive emits the DDL
   and the `BulkUpsertable` impl together.
2. **`bulk_upsert_in_tx` for every entity-table write** — no
   hand-rolled per-row INSERTs on `RawDb`.
3. **Per-provider CAS edge columns / table** — attachment bytes still
   share `cas_objects`, but each provider owns its own
   `<ref_id>→blake3` mapping. The shared `blob_refs` table is no
   longer written to by any ported provider.
4. **Two-phase translate parse with SQL bucket fingerprints** —
   skip-or-load decision happens off a single CTE over the new blake3
   columns; render becomes a transformer.

Three providers are targeted: **anthropic** (claude.ai), **chatgpt**
(chatgpt.com), and **whatsapp** (msgstore.db). The pattern fits
anthropic and chatgpt cleanly (they have wire-payload tables today);
**whatsapp's port is partial** — its tables are column-by-column
mirrors of msgstore.db with no JSONB payload, so phase 1 doesn't apply
literally. See the per-provider sections.

---

## Reference commits

The agent **must read these commits** before starting — they're the
canonical pattern. In order:

| commit | what it shows |
|---|---|
| `727fd34` | `#[derive(WirePayloadRow)]` — the proc-macro in `etl/macros/` + Signal migration. Read first to see the derive shape. |
| `72ec85c` | Email phase 1: schema migration, `BulkUpsertable` for the envelope-shaped `EmailRow` (manual impl, `PAYLOAD_COLUMN = None`). |
| `92c735c` | Code-review follow-up: one `now` per fetch, threaded through to `bulk_upsert_in_tx`. Don't replicate the bug, just thread `now` from the start. |
| `2273227` | Email phase 2: per-provider CAS edge columns, `EmailBlobReader`, drop `blob_refs` writes. |
| `6d1fbba` | Email phase 3: two-phase parse, `bucket_fingerprint_query`, render strip-down. |

For each target provider the agent should:

- **Read those five commits.**
- **Read `frankweiler/backend/etl/providers/signal/src/extract/schema_raw.rs`** as the simplest derive example.
- **Read `frankweiler/backend/etl/providers/email/src/translate/parse.rs`** as the two-phase parse template.
- **Read `frankweiler/backend/etl/providers/signal/src/translate/parse.rs`** as a second two-phase example (slightly different bucket key shape — period-bucketed).

Commit per phase, mirroring the email cadence. Each phase commits with
all tests green at that checkpoint.

---

## What stays shared, what doesn't

Carry these mental models into every port.

**Shared, do not duplicate:**
- `frankweiler_etl::blob_cas::BlobCas` + `cas_objects` table — bytes are
  fully unified. `BlobCas::put_many` is the only CAS-write path.
- `frankweiler_etl::bulk::bulk_upsert_in_tx<T>` — the chunked multi-row
  UPSERT helper. Every entity table goes through it.
- `frankweiler_etl::doltlite_raw::bookkeeping_ddl_for` — the
  `<table>_bookkeeping` sidecars.
- `frankweiler_etl::doltlite_raw::WirePayloadTriad` + the
  `#[derive(WirePayloadRow)]` macro in `frankweiler-etl-macros`.

**Per-provider, must be re-shaped:**
- The schema's per-entity promoted columns and the row-struct field
  list.
- The CAS edge table or columns (which per-provider table owns the
  `ref_id → blake3` mapping).
- The `*BlobReader` impl (per-provider SELECT that resolves
  `ref_id → blake3`; the second `SELECT bytes FROM cas_objects` hop is
  identical across providers and could be lifted later when a fourth
  example lands — explicitly out of scope for these ports).
- The two-phase parse's bucket key (per-thread? per-conversation?
  per-chat+period?) and the bucket-fingerprint SQL.

**Going away (do not write to from any ported provider):**
- The shared `blob_refs` table. It stays in the schema for the
  unported providers (slack, github, gitlab, notion, beeper, contacts)
  but the ported providers must stop writing to it. `truncate_blob_refs`
  in `--refetch-blobs` paths is replaced by a per-provider
  `clear_blob_hashes` helper that sets the per-provider blake3 columns
  back to NULL.

---

## Phase-by-phase recipe (template)

This is the recipe each provider's port follows. Per-provider deltas
are listed in the dedicated sections below.

### Phase 0 — read the reference commits and the provider's current code

The agent should fully read:

- `frankweiler/backend/etl/providers/<provider>/src/extract/schema_raw.rs`
- `frankweiler/backend/etl/providers/<provider>/src/extract/db.rs`
- `frankweiler/backend/etl/providers/<provider>/src/extract/mod.rs`
- `frankweiler/backend/etl/providers/<provider>/src/translate/parse.rs`
- `frankweiler/backend/etl/providers/<provider>/src/translate/render.rs`
- Every test file under `frankweiler/backend/etl/providers/<provider>/tests/`

…and grep for `store_blob`, `pre_seed_blob_stub`, `record_blob_error`,
`loaded_blob_ids`, `SqliteBlobReader`, `truncate_blob_refs` to find
every blob-touch site.

### Phase 1 — wire-payload schema + `BulkUpsertable` row structs

Goal: every entity table is declared by a row struct + derive, and
every entity-write call goes through `bulk_upsert_in_tx`.

Changes:

1. **Add `frankweiler-etl-macros` to the provider's deps** (Cargo.toml
   `frankweiler-etl-macros = { path = "../../macros" }`, BUILD.bazel
   `proc_macro_deps = ["//frankweiler/backend/etl/macros:frankweiler_etl_macros"]`).
   Run `bash tools/repin_cargo.sh`.

2. **Rewrite `extract/schema_raw.rs`:**
   - For each wire-payload-shaped entity table, replace the
     `pub const FOO_DDL: &str` + manual `BulkUpsertable` impl with:
     ```rust
     #[derive(Debug, Clone, WirePayloadRow)]
     #[wire_payload_row(table = "foos")]
     pub struct FooRow {
         pub triad: WirePayloadTriad,    // id + payload + payload_blake3
         pub promoted_a: String,
         pub promoted_b: Option<i64>,
         // ...
     }

     impl FooRow {
         pub fn from_payload(/* upstream-args */, payload: &Value) -> Result<Self> {
             let payload_str = serde_json::to_string(payload)?;
             let payload_blake3 =
                 frankweiler_etl::blob_cas::blake3_hex(payload_str.as_bytes());
             // ... promote columns from payload ...
             Ok(Self {
                 triad: WirePayloadTriad { id, payload: payload_str, payload_blake3 },
                 promoted_a, promoted_b,
             })
         }
     }
     ```
   - `full_ddl()` calls `FooRow::ddl()` per entity, plus the index
     consts, plus `bookkeeping_ddl_for(table)` for each.
   - The derive supports `String`, `Option<String>`, `i64`,
     `Option<i64>`. Anything else (booleans, JSON objects, custom
     types) errors at compile time — convert at the row-struct boundary.

3. **Shrink `extract/db.rs`:**
   - Delete every per-table `upsert_*` method (`upsert_user`,
     `upsert_users`, `upsert_org`, `upsert_orgs`,
     `upsert_conversation_detail`, etc.).
   - Delete the `upsert_*_in` private helpers.
   - Keep: `open`, `reset`, state-token / cursor methods, `load_*`
     for translate, blob plumbing (will shrink further in phase 2).

4. **Refactor `extract/mod.rs`:**
   - Compute `now` exactly once at the top of `fetch()` (or
     `run_sync()`) via
     `frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339()`
     and thread it down. Don't compute it inside helpers.
   - Per-table wrapper functions take `(db, now, &rows)`, build
     `Vec<FooRow>`, and call `bulk_upsert_in_tx`. See
     `frankweiler/backend/etl/providers/email/src/extract/mod.rs`
     functions `upsert_account`, `upsert_mailboxes`, `upsert_threads`,
     `upsert_emails`.
   - Replace every `db.upsert_*(...)` call site with a `bulk_upsert_in_tx`
     wrapper. **Batch at the natural unit** — JMAP gives N rows per
     `*/get` page; bulk-flush per page. Never per-row.
   - Don't call `IsoOffsetTimestamp::now_local()` deep in the call
     stack. (This was a real code smell on email phase 1 — fixed in
     `92c735c`.)

5. **Update inline tests in `db.rs`** (if any) to use the new
   API: build a row via `FooRow::from_payload`, call
   `bulk_upsert_in_tx` directly in the test helper.

6. **Run `bazel test //frankweiler/backend/etl/providers/<provider>/...`**
   and iterate until green. Then commit:

   ```
   <provider>: phase 1 — wire-payload schema + bulk_upsert_in_tx
   ```

   Reference body: see commit `72ec85c`.

### Phase 2 — per-provider CAS edge columns, retire `blob_refs`

Goal: stop writing to the shared `blob_refs` table. Add per-provider
`blake3` columns / a per-provider edge table. New `*BlobReader` reads
from the new columns directly.

Changes:

1. **Schema:**
   - Decide where the `blake3` column lives. Two patterns in play:
     - **Edge column on an existing table** (email's approach for
       `emails.blake3`, `email_attachments.blake3`). Use this when
       the existing table already represents "this entity references
       a blob" — just add a `blake3 TEXT NULL` column + a CHECK
       constraint `CHECK (blake3 IS NULL OR length(blake3) = 64)` +
       an index on `(<existing_ref_id_col>, blake3)`.
     - **New per-provider edge table** (signal's `chat_item_attachments`,
       which carries `id`, `chat_item_id`, `ref_id`, `blake3`). Use
       this when the entity has no obvious column to hold the ref —
       inline-in-payload attachments fall into this case.
   - The blake3 column is **never** in TYPED_COLUMNS. Always
     populated via `UPDATE` after the CAS write succeeds, so that
     re-INSERTing the entity (a resync, an idempotent re-run) can't
     NULL out a previously-computed hash.

2. **Per-provider `*BlobReader`** in `src/translate/blob_reader.rs`:
   - Takes `refs_pool: SqlitePool` (entity-side) + `cas_pool: SqlitePool`
     (CAS-side).
   - `read_by_ref_id` does two queries: first one resolves
     `ref_id → (blake3, upstream_name?, content_type?)` via the new
     per-provider column(s); second hits `cas_objects WHERE blake3 = ?`
     for the bytes.
   - `read_by_owner` and `read_by_hash` return `Ok(None)` unless the
     renderer needs them.
   - See `frankweiler/backend/etl/providers/email/src/translate/blob_reader.rs`
     and `frankweiler/backend/etl/providers/signal/src/translate/parse.rs`
     (the `SignalBlobReader` struct at the bottom of that file).

3. **Replace `db.store_blob` calls** with batched accumulation:
   - Accumulate `Vec<PendingCas>` during the download / decode loop.
   - End-of-walk flush: `BlobCas::put_many` for the CAS pool, then
     one entity-pool tx with `UPDATE <table> SET blake3 = ? WHERE
     <pk> = ?` per row + per-row `record_object_attempt` for any
     errors. See `flush_blob_batch` in
     `frankweiler/backend/etl/providers/email/src/extract/mod.rs`.

4. **`--refetch-blobs` control:** replace
   `frankweiler_etl::doltlite_raw::truncate_blob_refs(db.pool())`
   with `db.clear_blob_hashes()` — a new method that sets every
   per-provider blake3 column back to NULL. See email's
   `clear_blob_hashes` in `extract/db.rs`.

5. **Drop from `RawDb`** (now unused): `store_blob`,
   `pre_seed_blob_stub`, `record_blob_error`, `blob_exists`,
   `loaded_blob_ids` (or rewrite the last to read from the new
   columns).

6. **Wire `*BlobReader`** into `block_on_load_all` (replacing the
   `SqliteBlobReader::new` call). Add `pub mod blob_reader;` to
   `translate/mod.rs`.

7. **Update tests** that asserted `db.blob_exists(...)` — they now
   walk `SELECT blake3 FROM <table> WHERE <pk> = ?` + `SELECT EXISTS
   FROM cas_objects WHERE blake3 = ?`. See email's
   `delete_email_cascades_to_joins_and_bookkeeping` test for the
   structural-guarantee assertion shape.

8. **Run tests, commit:**
   ```
   <provider>: phase 2 — per-provider CAS edge columns, retire blob_refs
   ```

   Reference body: see commit `2273227`.

### Phase 3 — two-phase parse + SQL bucket fingerprints

Goal: drop the Rust-side `fingerprint_for_*` hash walk. The
parse layer runs a single SQL CTE to compute per-bucket fingerprints
off the new blake3 columns, compares to `prior_fingerprints`, and only
loads envelopes for to-render buckets. Render becomes a pure transformer.

Changes:

1. **Rewrite `translate/parse.rs`** to do two-phase load.
   - `ParsedFoo` carries `accounts/users/orgs/etc.` (whatever loads
     unconditionally), `docs: Vec<FooBucket>`, `docs_skipped: usize`,
     `blobs: Arc<dyn BlobReader>`.
   - `FooBucket` carries `bucket_key`, `fingerprint`, the
     fully-loaded data for that bucket (envelopes, joins, whatever
     render needs).
   - `pub fn parse(input, prior_fingerprints) -> Result<ParsedFoo>`.
   - **Phase 1** runs the bucket-fingerprint CTE. The shape is
     provider-specific (see per-provider sections below), but the
     pattern is uniform: one row per bucket, `bucket_concat` column
     is a deterministic concatenation of `payload_blake3` (and/or
     attachment `blake3`s, and/or any other content state the
     renderer reads). Hash each row's `bucket_concat` via
     `frankweiler_etl::blob_cas::blake3_hex` with `RENDER_VERSION`
     mixed in.
   - **Phase 2** runs targeted `SELECT ... WHERE bucket_pk IN (?, ...)`
     for the surviving buckets only. See email's `load_buckets`
     function as a template.

2. **Rewrite `translate/render.rs`'s `render_all`:**
   - Take `&ParsedFoo` (not `LoadedRaw`).
   - Drop the `prior_fingerprints` parameter.
   - Drop the inline `fingerprint_for_conversation` / `source_fingerprint`
     call + skip check.
   - Iterate `parsed.docs`. Each `FooBucket` carries its precomputed
     fingerprint that goes straight into the sidecar via
     `emit_sidecar(..., &bucket.fingerprint, ...)`.
   - Delete the `fingerprint_for_*` Rust functions — the SQL CTE
     subsumed them.

3. **Update the orchestrator** in `frankweiler/backend/sync/src/main.rs`
   (the provider's match arm):
   ```rust
   // Before:
   let parsed = block_on_load_all(&db)?;
   render_all(&parsed, root, name, progress, prior_fingerprints, on_doc_complete)?;
   // After:
   let parsed = parse(&db, prior_fingerprints)?;
   render_all(&parsed, root, name, progress, on_doc_complete)?;
   ```

4. **Update tests** to build `ParsedFoo` (with `FooBucket`s) in-memory
   instead of `LoadedRaw`, or to call `parse()` after extract. See
   email's `tests/jmap_render.rs` and `tests/jmap_mbox.rs`.

5. **Run tests, commit:**
   ```
   <provider>: phase 3 — two-phase parse + SQL bucket fingerprints
   ```

   Reference body: see commit `6d1fbba`.

### Phase 4 (if applicable) — final cleanup

If any helpers / types left over from the old shape can now be
deleted, do so in a final commit. Don't pad earlier phases with
cleanup — phase commits should be "this is the migration step,"
cleanup is its own thing.

---

## Per-provider sections

### Anthropic (claude.ai)

**Fit:** Excellent. Three wire-payload tables (`users`, `orgs`,
`conversations`), each with a JSONB `payload`. Inline-in-payload
attachments downloaded one at a time via `db.store_blob`. Translate
hashes the whole payload + RENDER_VERSION in Rust today.

**Phase 1 specifics:**
- 3 row structs to introduce: `UserRow`, `OrgRow`, `ConversationRow`.
- `UserRow` promoted columns: `email`, `full_name` (both `Option<String>`).
- `OrgRow` promoted columns: `name: Option<String>`.
- `ConversationRow` promoted columns: `org_uuid: String`,
  `org_name: Option<String>`, `name: Option<String>`,
  `updated_at: Option<String>`.
- The `MIGRATION_CONVERSATIONS_ADD_ORG_NAME` const is a leftover from
  an older `ALTER TABLE` migration. The new `CREATE TABLE IF NOT EXISTS`
  emitted by the derive already declares `org_name` — keep the ALTER
  const around (it's idempotent on fresh DBs and load-bearing on
  pre-migration ones), but the comment should be updated.
- Hand-written upserts to delete: `upsert_user`, `upsert_users`,
  `upsert_org`, `upsert_orgs`, `upsert_conversation_detail`, plus the
  `upsert_user_in` / `upsert_org_in` private helpers.

**Phase 2 specifics:**
- Anthropic's attachments are inline in `conversations.payload.chat_messages[*].files[]`
  with `file_uuid` as the upstream identifier. There is **no per-attachment
  metadata table today** — the metadata lives in the payload.
- **Recommended edge table:** add a new
  `conversation_attachments` table with `id TEXT PRIMARY KEY`,
  `conversation_uuid TEXT NOT NULL`, `file_uuid TEXT NOT NULL`,
  `blake3 TEXT NULL CHECK (blake3 IS NULL OR length(blake3) = 64)`.
  PK can be the composite `file_uuid` (it's already a stable
  anthropic-supplied UUID). Index on `(file_uuid, blake3)`.
- The download loop (`download_one_file` in `extract/mod.rs:540`)
  becomes an accumulator + `flush_blob_batch`.
- `AnthropicBlobReader` queries `conversation_attachments` by
  `file_uuid` for the (blake3, content_type) tuple.

**Phase 3 specifics:**
- Bucket = one conversation = one rendered doc. Bucket key:
  `conversation_uuid` (already the markdown_uuid).
- `bucket_concat` should hash: `conversations.payload_blake3`
  || `:` || `group_concat(attachment.blake3 ORDER BY file_uuid)`.
  No mailbox/keyword analog — anthropic conversations don't have
  per-message metadata state outside the payload.
- Existing `fingerprint_for_conversation` in
  `src/translate/grid_rows.rs` is the function to delete.

**Caveat:** the orchestrator currently uses `(account, org)` pair as
the rendered subtree key. Bucket the SQL by `(org_uuid, conversation_uuid)`
or carry both through `FooBucket`.

**Test surface to keep green:**
- `anthropic_playback_roundtrip`, `anthropic_render`,
  `anthropic_reset_and_redownload`, `anthropic_translate_test`,
  `anthropic_unittests`.

---

### ChatGPT

**Fit:** Excellent. Two wire-payload tables (`me`, `conversations`),
each with a JSONB `payload`. Single-account model (one `me` row).
Same inline-payload attachment story as anthropic.

**Phase 1 specifics:**
- 2 row structs: `MeRow`, `ConversationRow`.
- `MeRow` promoted columns: `email: Option<String>`,
  `name: Option<String>`.
- `ConversationRow` promoted columns: `title: Option<String>`,
  `update_time: Option<String>`, `last_listing_update_time: Option<String>`.
  Note `last_listing_update_time` is stored as a JSON-stringified value
  today because the upstream listing returns varied types — keep that
  as `Option<String>` and the call-site does the JSON serialization
  during row construction.
- Hand-written upserts to delete: `upsert_me`, `upsert_conversation_detail`,
  and any inline `upsert_*_in` helpers.

**Phase 2 specifics:**
- ChatGPT attachments come from `conversations.payload.mapping[*].message.content.parts[*]`
  (when they're images) or `conversations.payload.attachments[*]` (when
  they're files). Either way the upstream identifier is `file_id` and
  the owning entity is the conversation.
- **Recommended edge table:** `conversation_attachments` with the
  same shape as the anthropic one (`id`, `conversation_id`, `file_id`,
  `blake3` + CHECK + index).
- Download path: `download_file_for_conversation` (or wherever
  `db.store_blob` is invoked) → accumulator + `flush_blob_batch`.
- `ChatgptBlobReader` queries the new edge table by `file_id`.

**Phase 3 specifics:**
- Bucket = one conversation = one rendered doc. Bucket key:
  `conversation_id`.
- `bucket_concat`: `conversations.payload_blake3` || `:` ||
  `group_concat(attachment.blake3 ORDER BY file_id)`.
- The function to delete:
  `fingerprint_for_conversation` in `src/translate/grid_rows.rs`.

**Test surface:**
- `chatgpt_playback_roundtrip`, `chatgpt_render`,
  `chatgpt_translate_test`, `chatgpt_unittests`.

---

### WhatsApp

**Fit: partial.** WhatsApp is **not** a wire-payload provider —
`schema_raw.rs` declares 8 tables that mirror columns from
`msgstore.db` directly (`wa_jid`, `wa_chat`, `wa_message`,
`wa_message_text`, `wa_message_media`, `wa_message_add_on`,
`wa_message_add_on_reaction`, `wa_media_files`). There is no
`payload` JSONB column anywhere. Composite PKs `(chat_jid, key_id,
from_me)` dominate.

**Phase 1: does not apply literally.** Do not introduce a payload
column. Do not use `#[derive(WirePayloadRow)]`. The migration is
narrower:

- **Hand-rolled `BulkUpsertable` impls** for the row types that map
  to the entity tables. `PAYLOAD_COLUMN = None` for all of them.
  `TYPED_COLUMNS` is just the non-PK columns.
- **Composite PK gotcha.** The `BulkUpsertable` trait assumes a
  single `id` PK via `fn id(&self) -> &str`. For WhatsApp's
  `(chat_jid, key_id, from_me)` triple, options are:
  - Add a synthesized `id` column = `format!("{chat_jid}#{key_id}#{from_me}")`
    (same shape Signal uses for `chat_items`). Touches the schema.
  - Extend `BulkUpsertable` to support composite PKs.
  - Skip the trait entirely for WhatsApp's composite-PK tables and
    keep their bulk-INSERT pathways hand-rolled.
- **Recommendation:** the third option — skip the trait for composite-PK
  tables, keep the existing hand-rolled bulk INSERTs in
  `src/extract.rs`. They already do bulk multi-row INSERTs (look for
  `bulk_insert_*` helpers); they just don't go through the shared
  trait. The win from forcing them through the trait is small
  compared to the schema churn of synthesizing PKs.
- `wa_jid` and `wa_chat` have single-column PKs and **could** use
  `BulkUpsertable` (with `PAYLOAD_COLUMN = None`), but if you skip
  the composite-PK tables you may as well skip these for symmetry.

**Net for phase 1: probably no commit.** The win on whatsapp lives in
phases 2 and 3.

**Phase 2: applies.** This is the highest-value part for whatsapp.
- Add `blake3 TEXT NULL CHECK (blake3 IS NULL OR length(blake3) = 64)`
  to `wa_media_files`. The existing `sha256` column is the upstream
  identifier (the local-file fingerprint); the new `blake3` is the
  CAS content hash. They are NOT the same hash.
- Replace `blob_cas::store_bytes` calls in `mirror_media_files`
  (`src/extract.rs:823`) with batched `BlobCas::put_many` + an
  `UPDATE wa_media_files SET blake3 = ? WHERE sha256 = ?` per file.
- Introduce `WhatsAppBlobReader` in
  `src/translate/blob_reader.rs` that resolves
  `ref_id (= sha256) → blake3 → bytes`. Replaces the
  `SqliteBlobReader::new(...)` call in `src/translate/render.rs:118`.
- `loaded_blob_ids` (or wherever the skip-check map gets built) reads
  from `wa_media_files.blake3 IS NOT NULL` instead of `blob_refs`.
- `--refetch-blobs` clears `wa_media_files.blake3`.

**Phase 3: applies, but with a different bucket-fingerprint shape.**
- Bucket = one chat = one rendered doc. Bucket key: `chat_jid`.
- **No `payload_blake3` to hash** because there's no payload column.
  The bucket fingerprint has to come from the row's content columns
  directly. Two options:
  - **Hash entire row tuples in SQL.** The CTE projects each
    message's content state as a deterministic string concat (sender,
    timestamp, text_data, media file_hash, attachment blake3, etc.)
    and then `group_concat`s across the chat. Cheap, but every column
    rename / new column to fingerprint requires a SQL change.
  - **Promote a `content_blake3` column onto `wa_message`** that the
    extract path populates with `blake3_hex(canonical_message_repr(row))`.
    Then the bucket fingerprint is just
    `group_concat(content_blake3 || ':' || media_blake3)`. This is
    closer to the signal pattern but requires a schema migration and
    a Rust-side serialization decision.
  - **Recommendation:** start with the first option (SQL-only) for
    this port. Promote only if a future change makes the SQL
    unwieldy.
- The existing `parse.rs` already does some incremental work via
  chat-common's pattern; read it carefully before designing the
  bucket-fingerprint CTE so the new layer doesn't fight the old.

**Test surface:**
- `whatsapp_etl_unittests` (under
  `frankweiler/backend/etl/providers/whatsapp/`).

---

## Working principles (do not skip)

These came out of the email port and are non-negotiable for the
upcoming ones:

1. **Commit per phase.** Each phase's commit must pass `bazel test
   //frankweiler/backend/etl/providers/<provider>/...`. Phase 1 done +
   committed before phase 2 starts, etc. Do not stack two phases in
   one commit.

2. **Pre-commit hook runs `bazelisk build --config=clippy //...`** —
   that means clippy on the whole workspace, not just the provider.
   Any clippy violation anywhere blocks the commit. Run
   `cargo fmt -p <crate>` before committing.

3. **One `now` per fetch.** Compute
   `IsoOffsetTimestamp::now_local().to_rfc3339()` exactly once at the
   top of `fetch()` or `run_sync()` and thread it. **Never** call
   `now_local()` inside a wrapper function that gets called per
   batch / per row. Email phase 1 got this wrong; the fix was
   commit `92c735c`.

4. **Bulk-flush per batch, not per row.** Whether the natural batch
   size is "JMAP page" or "claude.ai listing page" or "mbox flush
   window," accumulate `Vec<FooRow>` for the batch and call
   `bulk_upsert_in_tx` once. Don't call it inside a per-row loop.

5. **`blake3` columns are populated via UPDATE after the CAS write,
   never via the entity-row INSERT.** Otherwise re-inserting an
   envelope (a resync, an idempotent re-run) NULLs out a previously-
   computed hash because the bulk-upsert path's ON CONFLICT clause
   does `column = excluded.column` for every typed column.

6. **The render output byte-for-byte stays the same** through phase 1
   and phase 2. Snapshot tests (`*_render`, `*_playback_roundtrip`)
   should pass unchanged. Phase 3 changes the fingerprint pre-image
   formula (SQL CTE vs Rust hash walk), so existing
   `markdowns_loaded.source_fingerprint` values will mismatch the new
   compute and every doc will re-render once on the first sync after
   phase 3 lands. This is a known one-time migration cost (signal
   commit `d392075`, email commit `6d1fbba`).

7. **Don't proactively unify across providers** during a port. If you
   notice a pattern that's now N=2 (e.g., the
   `SELECT bytes FROM cas_objects WHERE blake3 = ?` second hop in
   every `*BlobReader`), make a note for a follow-up but do not
   extract a shared helper as part of the port. The right extraction
   waits for N=3+ examples to confirm the shape.

8. **Do not touch the unported providers' `blob_refs` writes.** Slack,
   github, gitlab, notion, beeper, contacts still ride on the shared
   table. Leave them alone — they get their own ports later.

9. **Use the Explore agent** (subagent_type=Explore) for reading
   unfamiliar code in parallel, but do not delegate any code edits or
   design decisions. The agent that does the port owns the
   architectural calls.

---

## Order of execution

Suggested sequence:

1. **ChatGPT first.** Smallest schema (2 tables), simplest call graph,
   closest in shape to email. Use it to verify the recipe on a clean
   small example.
2. **Anthropic second.** Three tables, slightly more complex (org +
   conversation hierarchy), but still wire-payload-shaped. Should
   reuse most of the chatgpt port's patterns.
3. **WhatsApp last.** Different shape entirely (msgstore mirror, no
   payload). Phase 2 + 3 only. Read the chatgpt + anthropic ports
   first so the agent has two examples of the full pattern before
   tackling the partial fit.

Each provider's full port should be three commits (phase 1, 2, 3) for
chatgpt and anthropic. WhatsApp will be two commits (phase 2, 3).

Total expected diff: roughly **8 commits** across the three providers,
landing over multiple sessions.

---

## When to stop and surface

If during a port any of the following come up, **stop and ask the
user**, do not improvise:

- The provider's existing schema can't be expressed in the
  derive's narrow type universe (`String`, `Option<String>`, `i64`,
  `Option<i64>`) without significant changes.
- The provider has a fundamentally different rendering unit than
  per-conversation / per-thread / per-chat (e.g., a "per-document"
  provider) and the bucket-fingerprint shape isn't obvious.
- The provider's existing tests have non-trivial snapshot fixtures
  that would need regenerating beyond a clean `INSTA_UPDATE=1` pass.
- The user's `RENDER_VERSION` bump would invalidate downstream
  artifacts in a way that isn't already documented.

The pre-commit clippy hook is **not** a stop-and-ask trigger — fix
the lint locally and commit. The bazel test failures are also not
stop-and-ask — iterate on the port until green.
