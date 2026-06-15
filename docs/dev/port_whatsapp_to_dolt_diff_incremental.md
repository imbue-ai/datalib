# Porting WhatsApp to per-provider CAS + `dolt_diff_<table>`-driven incremental render

This is a focused, self-contained recipe for the WhatsApp provider's
port. Unlike the other targets in `port_provider_to_signal_pattern.md`,
WhatsApp uses a fundamentally different incremental-render primitive:
**`dolt_diff_<table>` instead of bucket fingerprints**.

The reason: WhatsApp's raw store mirrors `msgstore.db` column-by-column
— there's no JSONB `payload` to hash for a per-row content fingerprint,
and no obvious place to bolt one on. But doltlite's
content-addressable storage already knows which rows changed between
two commits — we just consume that machinery via the `dolt_diff_<table>`
virtual tables, and skip the per-row fingerprint apparatus entirely.

This produces a **smaller, simpler, faster** incremental story than
the bucket-fingerprint pattern, at the cost of being tied to dolt's
commit boundaries (which is fine — every sync run already creates a
commit).

The port is **two commits** of work, not the three you'd see on a
wire-payload provider.

---

## What WhatsApp looks like today

Quick mental model before touching any code:

- **`frankweiler/backend/etl/providers/whatsapp/`** is structured
  differently from email/signal: extract is one file
  (`src/extract.rs`, ~970 lines), not split into a `mod/db/schema_raw`
  trio. The schema is `src/schema_raw.rs` at the crate root, not
  under `src/extract/`.
- **8 entity tables**, each a column-by-column mirror of the matching
  `msgstore.db` table:
  `wa_jid`, `wa_chat`, `wa_message`, `wa_message_text`,
  `wa_message_media`, `wa_message_add_on`,
  `wa_message_add_on_reaction`, `wa_media_files`.
- **Composite primary keys** `(chat_jid, key_id, from_me)` across the
  message-shaped tables; `wa_jid.raw_string` and `wa_chat.chat_jid`
  are single-column PKs; `wa_media_files.sha256` is a single-column PK
  too.
- **No JSONB `payload`** anywhere. No `payload_blake3`. The tables
  are typed-column stores, not wire-payload stores.
- **Attachments**: `wa_media_files(sha256 PK, relative_path,
  size_bytes, mtime_unix, mime_type)`. Bytes today live in
  the shared `blob_refs` table + `cas_objects` (via the legacy
  `blob_cas::store_bytes` path). Messages reference attachments by
  `wa_message_media.file_hash = wa_media_files.sha256` etc.
- **Translate already buckets by `(chat_jid, period)`** in
  `src/translate/parse.rs` via the `Period` enum (Month / Day /
  Year / All — same enum signal uses).
- **Render currently goes through `chat-common`** (see
  `frankweiler/backend/etl/chat-common/`) — `render_all` builds an
  `SqliteBlobReader` and forwards into a shared renderer. The shared
  renderer does the actual markdown emission.

---

## Reference commits

The agent **must read these before starting**. They're the canonical
shape — phase 2 here is structurally identical to the email port's
phase 2, just with a single CAS edge table instead of two columns
on two tables.

| commit | what to learn from it |
|---|---|
| `2273227` | Email phase 2: per-provider CAS edge columns, batched `BlobCas::put_many`, `EmailBlobReader`, `clear_blob_hashes`, drop `blob_refs` writes. **Read this carefully** — phase 1 of the WhatsApp port is the same pattern applied to `wa_media_files`. |
| `4de94cc` | Signal's introduction of the per-provider `chat_item_attachments` edge table, retiring `blob_refs`. Closest precedent for a per-provider attachment table with a `blake3` column. |
| `6d1fbba` | Email phase 3: two-phase parse + bucket-fingerprint SQL. **Skim this** to see the shape of the two-phase parse output (`ParsedFoo` + `FooBucket` + `docs_skipped` + `blobs`). You'll mirror the *shape* but use the `dolt_diff_<table>` query in place of the bucket-fingerprint CTE. |
| `docs/dev/doltlite.md` | Inventory of what `dolt_*` SQL functions and vtabs exist. Confirms `dolt_diff_<table>` is a registered vtab, with `from_<col>`/`to_<col>`/`diff_type` columns. |

Also read end-to-end:

- `frankweiler/backend/etl/providers/whatsapp/src/extract.rs`
- `frankweiler/backend/etl/providers/whatsapp/src/schema_raw.rs`
- `frankweiler/backend/etl/providers/whatsapp/src/translate/parse.rs`
- `frankweiler/backend/etl/providers/whatsapp/src/translate/render.rs`
- `frankweiler/backend/etl/providers/email/src/translate/blob_reader.rs`
  (the per-provider `BlobReader` template)
- `frankweiler/backend/etl/providers/email/src/extract/mod.rs` —
  specifically the `flush_blob_batch` function, which is the
  CAS-flush pattern to mirror.

---

## Phase 1 — per-provider CAS edge column, retire `blob_refs`

**Goal:** `wa_media_files` owns its own `ref_id → blake3` mapping.
Bytes still land in the shared `cas_objects`, but the path no longer
runs through `blob_refs`. New `WhatsAppBlobReader` replaces the
`SqliteBlobReader` call in `translate::render::build_blob_reader`.

### Schema

Add a `blake3` column to `wa_media_files`. `sha256` stays as the
upstream identifier; `blake3` is the CAS content hash, populated
via UPDATE after the CAS write succeeds.

```rust
// frankweiler/backend/etl/providers/whatsapp/src/schema_raw.rs
pub const WA_MEDIA_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS wa_media_files (
    sha256 TEXT PRIMARY KEY,
    relative_path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    mtime_unix INTEGER,
    mime_type TEXT,
    blake3 TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
);";
```

Add a composite index for the `BlobReader` skip-check:

```rust
pub const WA_MEDIA_FILES_BY_SHA_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
    wa_media_files_by_sha ON wa_media_files(sha256, blake3);";
```

Wire the new index into `ALL_DDL` alongside the existing entries.

### Extract path

Rewrite `mirror_media_files` in `src/extract.rs:770-829`. Current
shape:

```rust
// One sqlx INSERT per metadata row, commit.
// Then: one `blob_cas::store_bytes(dst, &cas, &stub, &e.content)`
// per file — that helper writes both blob_refs and cas_objects.
```

New shape (mirroring email's `flush_blob_batch` from
`frankweiler/backend/etl/providers/email/src/extract/mod.rs`):

1. Scan media files, accumulate `Vec<MediaEntry>` (already done).
2. Bulk-insert metadata rows into `wa_media_files` with `blake3 = NULL`
   in one transaction (mostly already done — keep the existing
   `INSERT OR IGNORE` block, just don't bind a value for `blake3`).
3. Build `Vec<blob_cas::CasInsert<'_>>` from the accumulator and call
   `db.cas().put_many(&cas_inserts).await?` once. The shared CAS
   already lives on a sibling pool — the `BlobCas` handle is the same
   one email and signal use.
4. In the same entity-pool transaction as step 2 (or a follow-up tx),
   run `UPDATE wa_media_files SET blake3 = ? WHERE sha256 = ?` for
   every file. Per-row UPDATE is fine — these batches are bounded
   (typical WhatsApp media folder is 5–50 files; the scan-the-whole-
   thing pattern caps at a few thousand).

Delete the per-file `blob_cas::store_bytes(dst, &cas, &stub, &e.content)`
loop. The `RefStub`/`blob_refs` path goes away entirely.

### `--refetch-blobs` control

The `ExtractControl::refetch_blobs` flag today calls
`frankweiler_etl::doltlite_raw::truncate_blob_refs(db.pool())`. Replace
with a per-provider `clear_blob_hashes` helper that sets every
`wa_media_files.blake3` back to NULL:

```rust
pub async fn clear_blob_hashes(pool: &SqlitePool) -> Result<()> {
    sqlx::query("UPDATE wa_media_files SET blake3 = NULL")
        .execute(pool)
        .await
        .context("clear wa_media_files.blake3")?;
    Ok(())
}
```

The CAS itself stays — re-walking media re-hashes to the same blake3,
the `put_many` is `INSERT OR IGNORE` on the CAS side.

### `WhatsAppBlobReader`

New file: `frankweiler/backend/etl/providers/whatsapp/src/translate/blob_reader.rs`.
Mirror `EmailBlobReader` (the file the user is looking at right now).
The lookup chain is simpler than email's — only one table to query:

```rust
async fn read_by_ref_id_async(&self, ref_id: &str) -> Result<Option<BlobView>> {
    // ref_id is the sha256 of the original file (per
    // schema_raw.rs:233-235). Resolve to blake3, then bytes.
    let row = sqlx::query(
        "SELECT blake3, mime_type, relative_path FROM wa_media_files
          WHERE sha256 = ? AND blake3 IS NOT NULL LIMIT 1"
    ).bind(ref_id)
     .fetch_optional(&self.refs_pool)
     .await?;
    let Some(row) = row else { return Ok(None); };
    let blake3: String = row.try_get("blake3")?;
    let content_type: Option<String> = row.try_get("mime_type").ok().flatten();
    let upstream_name: Option<String> = row.try_get::<String, _>("relative_path")
        .ok()
        .and_then(|p| p.rsplit('/').next().map(str::to_string));
    // Standard second hop into cas_objects, identical to EmailBlobReader.
    // ...
}
```

`read_by_owner` and `read_by_hash` return `Ok(None)` — same as
`EmailBlobReader`. Render doesn't call them today.

In `src/translate/render.rs:118`, replace:

```rust
Ok(SqliteBlobReader::new(refs_pool, cas_pool).into_handle())
```

with:

```rust
Ok(blob_reader::WhatsAppBlobReader::new(refs_pool, cas_pool).into_handle())
```

Add `pub mod blob_reader;` to `src/translate/mod.rs`.

### Tests

The whatsapp_etl_unittests target covers the extract path; expect to
update any inline test that asserted `db.blob_exists(sha256)` or read
from `blob_refs`. Pattern from email's
`delete_email_cascades_to_joins_and_bookkeeping` test: assert the
`blake3` is set on `wa_media_files` and the bytes exist in
`cas_objects` directly, not via the legacy `blob_refs` path.

### Commit

```
whatsapp: phase 1 — per-provider CAS edge column, retire blob_refs

[body modeled on commit 2273227]
```

Run `bazel test //frankweiler/backend/etl/providers/whatsapp/...` and
iterate to green before committing.

---

## Phase 2 — `dolt_diff_<table>`-driven incremental render

**Goal:** Stop the existing render-everything-and-compare-fingerprints
approach. Replace with: ask doltlite "what changed since the last
successful render commit?" → render only those chats.

This is the interesting part of the port. The approach uses doltlite's
`dolt_diff_<table>` virtual tables to enumerate the set of changed
rows directly, with `chat_jid` as the natural bucket key. No
per-row content hashing, no `bucket_fingerprint_query`-style CTE.

### The changed-chats query

This is phase 1's whole job — one round-trip, set of `chat_jid`s back.
Run it against the read-only sqlite pool the translate side already
opens:

```sql
SELECT DISTINCT chat_jid FROM (
    SELECT coalesce(to_chat_jid, from_chat_jid) AS chat_jid
      FROM dolt_diff_wa_chat
     WHERE from_ref = ?1 AND to_ref = 'HEAD'
       AND diff_type != 'unchanged'
    UNION
    SELECT coalesce(to_chat_jid, from_chat_jid)
      FROM dolt_diff_wa_message
     WHERE from_ref = ?1 AND to_ref = 'HEAD'
       AND diff_type != 'unchanged'
    UNION
    SELECT coalesce(to_chat_jid, from_chat_jid)
      FROM dolt_diff_wa_message_text
     WHERE from_ref = ?1 AND to_ref = 'HEAD'
       AND diff_type != 'unchanged'
    UNION
    SELECT coalesce(to_chat_jid, from_chat_jid)
      FROM dolt_diff_wa_message_media
     WHERE from_ref = ?1 AND to_ref = 'HEAD'
       AND diff_type != 'unchanged'
    UNION
    SELECT coalesce(to_chat_jid, from_chat_jid)
      FROM dolt_diff_wa_message_add_on
     WHERE from_ref = ?1 AND to_ref = 'HEAD'
       AND diff_type != 'unchanged'
    UNION
    SELECT coalesce(to_chat_jid, from_chat_jid)
      FROM dolt_diff_wa_message_add_on_reaction
     WHERE from_ref = ?1 AND to_ref = 'HEAD'
       AND diff_type != 'unchanged'
)
WHERE chat_jid IS NOT NULL
ORDER BY chat_jid;
```

The single positional parameter `?1` binds the last-render commit hash
(repeated by sqlx for each subquery — verify your sqlx flavor handles
positional params or just bind it N times).

Notes on the query:

- **`coalesce(to_chat_jid, from_chat_jid)`** covers the `'removed'`
  case: a row that existed in `from_ref` but not in `to_ref` has
  `to_chat_jid IS NULL`. We still want to re-render that chat
  (which is now smaller — or empty, in which case maybe the
  rendered markdown should be removed; see "edge cases" below).
- **No `wa_jid`** in the union — that table holds participant identity
  for the whole DB, not per-chat content. A change to `wa_jid` doesn't
  by itself imply any rendered chat changed (the participant might be
  referenced from messages whose own rows are tracked separately
  by `dolt_diff_wa_message`).
- **No `wa_media_files`** in the union — attachment changes show up
  via `dolt_diff_wa_message_media` (the row that *references* the
  attachment). If the same `sha256` keeps the same `wa_message_media`
  row but the bytes change underneath (unlikely with a `sha256` PK,
  but possible if the upstream re-export hashes differently), add a
  `dolt_diff_wa_media_files`-via-join branch. **Recommended: skip for
  the first cut**, add later if a real example shows up.

### Where the cursor lives

The "last-render commit" is a per-source piece of state. The orchestrator
already loads a `prior_fingerprints` map per-source via
`load_fingerprints(&index_pool)` in `frankweiler/backend/sync/src/main.rs:680`.

Add a sister surface: a per-source last-render commit hash, stored
in the same index_lib pool the markdown sidecars use. The simplest
shape:

- New table `source_render_cursors(source_name TEXT PRIMARY KEY,
  last_render_commit TEXT NOT NULL, updated_at TEXT NOT NULL)`.
- New helper `load_render_cursor(pool, source_name) -> Result<Option<String>>`
  in `frankweiler/backend/index_lib/src/...`.
- New helper `save_render_cursor(pool, source_name, commit)` called
  by the orchestrator **after** all docs for that source successfully
  flushed via `on_doc_complete`.

The orchestrator's per-source render call already runs to completion
or errors out as a unit — wire the save to the success path of the
WhatsApp arm specifically. **Do not** retrofit this onto the other
providers yet; they still ride on `prior_fingerprints`.

### Two-phase parse, WhatsApp-flavored

Rewrite `src/translate/parse.rs` to mirror the email phase-3 shape but
with the dolt_diff query in place of `bucket_fingerprint_query`:

1. **Phase 1.** Run the union query above. Get back
   `Vec<chat_jid>`. If `last_render_commit` is `None` (first run,
   or post-`--reset-and-redownload`), Phase 1 is "every `chat_jid`
   that has any wa_message" — a simple `SELECT DISTINCT chat_jid
   FROM wa_message`. Same downstream code path.
2. **Phase 2.** For each `chat_jid` in the changed set, load the
   data the current `parse_async` already loads — its
   `NormalizedChat` shape, the per-period bucketing, etc. — but
   filter to those `chat_jid`s only. The existing
   `Period`-based bucketing applies inside each chat once it's
   loaded.
3. **Output.** `ParsedWhatsApp` (or whatever the bag is named —
   mirror `ParsedSignal` / `ParsedEmail`) carries:
   - `docs: Vec<WhatsAppChatBucket>` — one per `(chat_jid, period_key)`
     pair that survived Phase 1 and got loaded in Phase 2.
   - `docs_skipped: usize` — count of chats that `dolt_diff` said
     were unchanged.
   - `blobs: Arc<dyn BlobReader>` — the new `WhatsAppBlobReader`.

`docs_skipped` here counts **chats** (not per-period buckets) because
the dolt_diff query operates at the chat level. The current `Period`
machinery inside each loaded chat doesn't need a separate fingerprint
— the whole chat's set of buckets re-renders together when its rows
changed. If finer-grained skip is wanted later (e.g. "only the May
period of this chat changed"), the dolt_diff results already carry
per-row commit_date info that could be used to project down — explicit
follow-up, not in scope for this port.

### Render becomes a transformer

Update `src/translate/render.rs:render_all`:

- Take `&ParsedWhatsApp` (not `Vec<NormalizedChat>` directly).
- Drop the `prior_fingerprints` parameter from `render_all`'s signature.
- Iterate `parsed.docs` (chat buckets pre-filtered by Phase 1).
- The per-doc fingerprint stored in the sidecar should still be
  *something* (the existing `RenderedMarkdown.source_fingerprint`
  field is read elsewhere). Set it to the bucket's commit-hash-at-
  render. The sidecar still has a stable identifier; the orchestrator's
  next-run skip decision is just driven by the cursor instead of by
  per-doc fingerprint comparison.

### Orchestrator integration

In `frankweiler/backend/sync/src/main.rs`, the WhatsApp arm currently
looks like the signal arm. Update it:

```rust
SourceConfig::Whatsapp { sync, .. } => {
    use frankweiler_etl_whatsapp::translate::{parse, render_all, Period};

    let last_commit = load_render_cursor(&index_pool, name).await?;
    let parsed = parse(&fixture, period, name, last_commit.as_deref())
        .with_context(|| format!("whatsapp parse {fixture:?}"))?;
    render_all(&parsed, root, name, progress, on_doc_complete)
        .context("whatsapp render_all")?;

    // Stamp the new cursor only after on_doc_complete returned Ok
    // for every doc. The render_all path already commits per-doc
    // through on_doc_complete; if any failed it returns Err and
    // we skip this save.
    let head_commit = sqlx::query_scalar::<_, String>("SELECT dolt_hashof('HEAD')")
        .fetch_one(&raw_pool)
        .await?;
    save_render_cursor(&index_pool, name, &head_commit).await?;
}
```

The `parse` signature changes from
`(input, period, source_name)` to
`(input, period, source_name, last_render_commit: Option<&str>)`.

### Tests

The current `whatsapp_etl_unittests` exercises the extract + a
round-trip. Add:

1. **First-run case.** Cursor is `None` → every chat renders.
   `docs_skipped == 0`.
2. **No-op resync.** Run extract once, render, save cursor; run
   render again immediately (no new commits) → `docs_skipped == N`,
   `parsed.docs.is_empty()`.
3. **Targeted change.** Insert a row into `wa_message` directly,
   `dolt_commit` it, then run render → `parsed.docs` has exactly the
   chats touched.

These are the "incremental works" tests — equivalent to signal's
`signal_tng_e2e` second-pass `docs_skipped` assertion.

### Commit

```
whatsapp: phase 2 — dolt_diff_<table>-driven incremental render

[body covering: changed-chats union query; per-source render cursor;
two-phase parse with first-run + reset edge cases; orchestrator
integration; tests]
```

---

## Edge cases worth thinking through before writing code

1. **First run.** No cursor in `source_render_cursors`. Treat as
   "render every chat with any messages." Don't try to use the
   commit-at-init as `from_ref` — easier to special-case the `None`
   path in `parse`.

2. **`--reset-and-redownload`.** The reset wipes the entity tables.
   Two paths:
   - **Cursor stays.** Next render's `dolt_diff` would compute
     "from old commit to HEAD" which now contains the wiped-then-
     refilled state. Dolt collapses this correctly — net diff is
     whatever the difference is between old data and new data, with
     diff_types like `modified` and `removed`/`added` as appropriate.
     Probably fine.
   - **Wipe the cursor on `--reset-and-redownload` too.** Simpler.
     Add `save_render_cursor(name, NULL)` (or a DELETE) to the
     `if opts.control.reset_and_redownload { ... }` block in
     `src/extract.rs::fetch`.
   **Recommended: wipe the cursor.** Cleaner semantics, no
   reasoning about diff collapse.

3. **Removed chats.** A chat fully deleted upstream → the dolt_diff
   shows its rows as `'removed'`, `to_chat_jid` is NULL, the
   `coalesce` picks up `from_chat_jid`. The current `parse` code will
   try to load that chat and find no rows. Two handling options:
   - **Skip in Phase 2** if the loaded chat has zero messages; the
     renderer never sees it. The existing markdown on disk is stale
     but harmless until the indexer GCs it.
   - **Surface as a "shred this markdown" signal** — emit a
     `RenderedMarkdown` with empty content and let the orchestrator's
     `on_doc_complete` notice and remove. More invasive — out of
     scope for this port.
   **Recommended: skip in Phase 2.** Simplest correct behavior. Add a
   TODO for the proper shred story.

4. **`dolt_diff_<table>` schema drift.** If a schema migration adds a
   column to one of the tables between `from_ref` and `to_ref`, the
   diff vtab still reports rows but the new column appears as
   `to_<col>` only (no `from_<col>` projection for non-existent column).
   Today this would only matter if the agent migrates the schema
   mid-port — they shouldn't.

5. **Multi-commit ranges.** If five sync runs landed between two
   renders, `from_ref` points at the older commit and `to_ref='HEAD'`
   covers the cumulative diff. No accumulation logic needed; dolt
   handles arbitrary ranges natively. The set of changed chats is the
   union across all intervening commits.

6. **Concurrent renders.** The cursor is written after a successful
   batch. If two `frankweiler-sync` processes race on the same source,
   the later one overwrites — fine for correctness (worst case: some
   chats render twice, none get skipped wrongly). Don't add locking
   for this port.

---

## What stays the same

Things to **not** touch in this port:

- The existing column-by-column mirror schema for the 8 tables. No
  composite-PK changes, no `BulkUpsertable` impls, no payload
  columns. **Phase 1 of the Signal-pattern doc (wire-payload +
  bulk_upsert_in_tx) does not apply to WhatsApp.**
- `src/extract.rs`'s `bulk_insert_*` helpers for the entity tables —
  they already do chunked multi-row INSERTs. They don't go through
  `bulk_upsert_in_tx`, but the win from forcing them through the
  trait is small compared to the schema churn of synthesizing PKs.
- `chat-common`'s rendering machinery. WhatsApp forwards into it
  today; that contract is unchanged. The only render-side touch is
  the `SqliteBlobReader` → `WhatsAppBlobReader` swap and the
  signature change to consume `&ParsedWhatsApp`.
- The `Period` enum and the per-period bucketing inside each chat.
  Those operate at a different granularity than the dolt_diff
  changed-chat detection and compose cleanly (Phase 1 says "this
  chat changed"; Period inside that chat still slices by month/day/year).

---

## Working principles

These came out of the email port and are non-negotiable here too:

1. **Commit per phase.** Phase 1 (CAS migration) lands as one commit;
   phase 2 (dolt_diff incremental render) as another. Both green at
   `bazel test //frankweiler/backend/etl/providers/whatsapp/...`
   before commit.

2. **Pre-commit hook runs `bazelisk build --config=clippy //...`** —
   that means clippy on the whole workspace. Run
   `cargo fmt -p frankweiler-etl-whatsapp` before committing. If
   clippy fires on `too_many_arguments`, prefer
   `#[allow(clippy::too_many_arguments)]` on the private helper
   over restructuring — same trade we made on the email port.

3. **`blake3` is populated via UPDATE after the CAS write succeeds,
   never via the `wa_media_files` INSERT.** Re-inserting the metadata
   row (idempotent re-scan of `Media/`) would otherwise NULL out a
   previously-computed hash.

4. **Don't proactively unify with `EmailBlobReader`** during this
   port. The N=3 extraction of a shared per-provider BlobReader
   helper is its own follow-up (chatgpt and anthropic would be the
   second and third examples). Premature now.

5. **Don't touch the unported providers' `blob_refs` writes.** Slack,
   github, gitlab, notion, beeper, contacts still ride on the shared
   table. Their phase-out is separate.

---

## When to stop and surface

If during the port any of the following come up, **stop and ask the
user**:

- The `dolt_diff_<table>` query returns results that don't agree with
  ground truth in a manual repro (e.g., a chat known to have changed
  isn't in the changed set). Likely a schema-drift or ref-binding
  bug; surface before working around it.
- The `dolt_hashof('HEAD')` call returns something unexpected (empty
  string, weird length) on the actual `data_root` doltlite — could
  mean the database was never `dolt_commit`'d and we're reading
  uncommitted state.
- The orchestrator's per-source state machinery doesn't have an
  obvious place to wire `source_render_cursors` — surface the
  proposed table addition before committing it.
- The `chat-common` renderer contract turns out to be tighter than
  expected and `&ParsedWhatsApp` doesn't drop in cleanly. Surface
  before redesigning chat-common.

Clippy lints, test failures, and small fmt issues are **not**
stop-and-ask — iterate locally until green.

---

## Expected scope

- **Phase 1** (CAS migration): ~400 lines touched. Mostly
  `mirror_media_files` rewrite, new `WhatsAppBlobReader` (~80
  lines), schema_raw column + index, render.rs one-line swap.
- **Phase 2** (incremental render): ~600 lines touched. New parse
  shape, render signature change, orchestrator integration, the
  `source_render_cursors` table + helpers in `index_lib`, tests.
- **Total**: roughly 1000 lines net change across two commits.
