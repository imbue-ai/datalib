# Provider migration recipe: dolt_diff incremental render + per-provider CAS edges

This doc is the migration recipe for the remaining ETL providers
(anthropic, chatgpt, slack, github, gitlab, notion, beeper, contacts,
perseus). It captures what we learned doing **whatsapp → email →
signal**, distilled into a sequence of moves that should apply to any
provider whose raw store lives in doltlite.

The migration combines **two separate changes** that turned out to be
load-bearing and naturally land together:

1. **Attachment references** — every provider used to write into a
   shared `blob_refs(ref_id, blake3, owning_id, slot)` table. That
   table was vestigial: the (owning_id, slot) columns were never
   queried by any render path, and the (ref_id → blake3) mapping
   collided across providers that happened to use the same upstream
   identifier. Each ported provider now owns a small per-provider
   table or column that maps its native attachment id to the CAS
   `blake3` directly. Bytes still live in the shared `cas_objects`.

2. **Incremental render** — every provider used to compute a per-row
   `payload_blake3` at extract time, hand-maintained on every entity
   row, aggregated by a SQL CTE in translate into a per-bucket
   fingerprint, compared against the load step's
   `prior_fingerprints` map to decide skip-or-render. That whole
   apparatus is replaced by **`dolt_diff_<table>` virtual tables +
   a per-source render cursor JSON**. Doltlite's prolly-tree diff is
   already in its hot path; we let it answer "what changed since
   last render?" directly.

Both changes are necessary on a port. (1) makes attachment lookup
work without crossing providers; (2) is the win that justifies the
whole migration.

---

## What we deleted, what we kept

**Deleted per-provider:**
- `payload_blake3` field on `WirePayloadTriad` (the type also got
  renamed → `WirePayload`, since it's a pair now, and field name on
  every row struct went `triad` → `id_and_payload`).
- Every `let payload_blake3 = blake3_hex(payload.as_bytes())` site
  in extract.
- The `bucket_fingerprint_query` CTE in translate's parse.
- Per-bucket `fingerprint: String` field on the parsed-bucket type.
- The `prior_fingerprints: &HashMap<String, String>` arg threaded
  through parse + render + orchestrator.
- Direct writes to the shared `blob_refs` table.

**Also dropped during the port (where the provider had it):**
- Listing-pass pre-seeding. The chatgpt and anthropic providers used
  to write a stub row `(id, name, updated_at, payload=NULL)` for
  every conversation surfaced by the listing endpoint, before the
  detail fetch ran. The stub row didn't fit `WirePayloadRow` and
  forced a parallel hand-rolled UPSERT path. We've reversed that
  decision globally: rows only appear *after* a successful detail
  fetch. The listing-pass skip-check works by bulk-reading
  `(id → stored.update_time)` for the listed ids and comparing to
  the listing's update_time; "no row" means "fetch it." A crashed
  detail-fetch leaves no row; the next sync's listing surfaces it
  as missing and re-fetches. See `data_architecture_ingestion.md`
  §"No-preseed listing flow" for the rationale.

**Kept (and load-bearing):**
- Per-doc `source_fingerprint` field on the sidecar / `RenderedMarkdown`
  — the load step still reads it. It's now set to the markdown_uuid
  (or thread_uuid). Stable across re-renders of the same bucket,
  distinct across buckets; the skip decision happens elsewhere.
- The bookkeeping sidecars (`<table>_bookkeeping`) and the rest of
  the `bulk_upsert_in_tx` machinery — unaffected.
- `WirePayloadRow` derive macro — emits one less column.
- `blob_refs` table itself — unported providers still write into it.
  We are NOT dropping the table during this migration.

---

## The two new primitives

### `frankweiler_etl::render_cursor`

A small JSON file at `<out_dir>/rendered_md/<provider>/<source_name>/_render_cursor.json`.

```json
{
  "last_rendered_hash": "k7v9...",
  "last_scan_ms": 12,
  "last_render_at": "2026-06-11T17:44:32-07:00"
}
```

- `last_rendered_hash`: the doltlite HEAD that the previous run
  successfully completed against. Used as `from_ref` for the next
  run's `dolt_diff_<table>` query.
- `last_scan_ms`: how long the previous run's dolt_diff union query
  took. Omitted (`None`) on cold start (no diff was issued). Logged
  on every render so users can watch how the prolly-tree diff scales.
- `last_render_at`: RFC 3339 stamp of when the cursor was written.

Single-writer assumption. No locking, no atomic-rename dance.
Missing file → cold start → render everything.

API (see `frankweiler/backend/etl/src/render_cursor.rs`):

```rust
pub fn cursor_path(out_dir: &Path, provider: &str, source_name: &str) -> PathBuf;
pub fn read(path: &Path) -> Result<Option<RenderCursor>>;
pub fn write(path: &Path, hash: &str, scan_elapsed: Option<Duration>) -> Result<()>;
```

### Per-provider CAS edge

Each provider owns one of two shapes, picked based on what its existing
schema looks like:

**Shape A — edge column on an existing table.** Use this when the
table that "owns" the attachment already has a natural row per
attachment slot. Add a `blake3 TEXT NULL` column with a
`CHECK (blake3 IS NULL OR length(blake3) = 64)` constraint, plus a
composite index on `(<ref_id_col>, blake3)`. The blake3 column is
populated via `UPDATE` after the CAS write succeeds — never via the
entity-row INSERT.

Examples landed:
- `wa_media_files.blake3` (whatsapp; `sha256` is the upstream id).
- `emails.blake3` + `email_attachments.blake3` (email; `blob_id` is
  the upstream id on both).

**Shape B — new per-provider edge table.** Use this when the
attachment metadata lives only inside a JSONB `payload` and there's
no obvious row to bolt a column onto. Schema:

```sql
CREATE TABLE IF NOT EXISTS <provider>_attachments (
    id TEXT PRIMARY KEY,              -- synthesized PK
    <owner_id> TEXT NOT NULL,         -- FK into the entity table
    ref_id TEXT NOT NULL,             -- upstream id (chat-side)
    blake3 TEXT NULL,                 -- CAS content hash (NULL until bytes land)
    CHECK (blake3 IS NULL OR length(blake3) = 64)
);
CREATE INDEX <provider>_attachments_by_<owner>
    ON <provider>_attachments(<owner_id>);
CREATE INDEX <provider>_attachments_by_ref
    ON <provider>_attachments(ref_id, blake3);
```

Example landed: `chat_item_attachments` (signal).

In both shapes the renderer reaches the bytes via a per-provider
`*BlobReader` impl that resolves `ref_id → blake3` from this table
and `blake3 → bytes` from `cas_objects`. The second hop is identical
across providers; we haven't lifted it because the first hop's SQL
is provider-specific and the helper would be a one-line shim.

---

## Phase-by-phase recipe (template)

Each provider's port lands as a single commit. The phases inside
that commit map roughly to:

### Phase 1: per-provider CAS edge

1. Pick shape A or B based on the provider's schema.
2. Add the column/table to `extract/schema_raw.rs`. Wire into
   `full_ddl()` and any index list.
3. Rewrite the attachment-download path:
   - Accumulate `Vec<PendingCas>` during the listing/decrypt loop.
   - End-of-walk flush: `BlobCas::put_many` for the bytes, then one
     entity-pool tx that UPDATEs each row's `blake3` column.
   - Delete the per-attachment `blob_cas::store_bytes` /
     `db.store_blob` loop — the `RefStub`/`blob_refs` path goes
     away entirely for this provider.
4. Add a `clear_blob_hashes` helper that sets every per-provider
   `blake3` column back to NULL. Wire it into the
   `ExtractControl::refetch_blobs` arm in place of
   `truncate_blob_refs(db.pool())`.
5. Write a per-provider `*BlobReader` in `src/translate/blob_reader.rs`
   (or inline at the bottom of `src/translate/parse.rs` — signal's
   pattern). Two queries: ref_id → blake3 from the new column/table,
   then blake3 → (bytes, content_type) from `cas_objects`.
6. Replace `SqliteBlobReader::new(...)` with the new reader.

### Phase 2: render cursor + dolt_diff

1. Refactor `translate::parse` to take a `last_render_hash:
   Option<&str>` arg (or wrap it in a `ScanResult` struct that
   parse fills and render consumes).
2. The dolt_diff scan is a single union query. Identify the
   provider's natural bucket key (what becomes one rendered .md
   per row) and union over every per-table `dolt_diff_<table>` that
   carries — directly or via join — that bucket key. Always do
   `coalesce(to_<col>, from_<col>)` to cover the removed-row case.
3. Issue `SELECT commit_hash FROM dolt_log() ORDER BY date DESC
   LIMIT 1` to read the current HEAD. Wrap in `.fetch_optional`
   so non-doltlite sqlite returns `None` and the cursor stays
   unwritten.
4. Time the union query. Record both elapsed and result count so
   render can log and persist them.
5. Phase 2 of parse loads envelopes/joins only for the surviving
   bucket keys.
6. `render_all` reads the cursor before parse, advances it after a
   successful loop (when HEAD was readable). `tracing::info!`s the
   scan_elapsed_ms + changed-bucket-count + cold_start flag on
   every run.
7. Drop the per-bucket `fingerprint` field; set `source_fingerprint`
   on the sidecar to the stable bucket UUID.

### Phase 3: orchestrator integration

In `frankweiler/backend/sync/src/main.rs`'s match arm for the
provider:

```rust
let cursor_path = frankweiler_etl::render_cursor::cursor_path(
    root, "<provider>", name,
);
let cursor = frankweiler_etl::render_cursor::read(&cursor_path)
    .with_context(...)?;
let parsed = parse(
    &fixture,
    cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
)?;
render_all(&parsed, root, name, progress, on_doc_complete)?;
```

The `prior_fingerprints` arg stays in the function signature
(`translate_source` still threads it for unported providers) — just
unused inside this arm. Don't try to refactor that signature
mid-migration.

### Phase 4: payload_blake3 cleanup

If the provider was on the `WirePayloadRow` derive pattern (anthropic,
chatgpt, …):
- The derive emits no `payload_blake3` column anymore (already done
  in the framework crate). Remove the `let payload_blake3 = ...;`
  lines and the `payload_blake3:` field from every `WirePayload {
  ... }` construction at extract sites.

Otherwise (envelope-only `BulkUpsertable` impls like email's
`EmailRow`): nothing to do — those impls already had
`PAYLOAD_COLUMN = None` and no `payload_blake3`.

---

## Per-provider notes

### Anthropic (claude.ai)

- Schema: 3 wire-payload tables (`users`, `orgs`, `conversations`).
- Attachments: inline in `conversations.payload.chat_messages[*].files[]`,
  keyed by `file_uuid`. No metadata table today.
- **Recommended edge: Shape B** — new
  `conversation_attachments(id, conversation_uuid, file_uuid,
  blake3)` table. PK can be `file_uuid` (already a stable
  anthropic-supplied UUID).
- Bucket: one rendered .md per conversation. Bucket key:
  `conversation_uuid`.
- dolt_diff union: `dolt_diff_conversations`,
  `dolt_diff_conversation_attachments` (joined to `conversations`
  for org_uuid), `dolt_diff_orgs` (any change → "render every
  conversation in this org"), `dolt_diff_users` (any change →
  render everything; this is a small fan-out).

### ChatGPT (chatgpt.com)

- Schema: 2 wire-payload tables (`me`, `conversations`).
- Attachments: same shape as anthropic — inline in
  `conversations.payload.mapping[*]...` keyed by `file_id`.
- **Recommended edge: Shape B** — `conversation_attachments(id,
  conversation_id, file_id, blake3)`.
- Bucket: one rendered .md per conversation. Bucket key:
  `conversation_id`.
- dolt_diff union: `dolt_diff_conversations`,
  `dolt_diff_conversation_attachments`, `dolt_diff_me` (any change
  → render everything).

### Slack

- Schema: per-team / per-channel / per-thread layout (see
  `frankweiler_etl_slack::extract`). Already has a cheap per-thread
  cursor (`block_on_probe_thread_cursors`) probed in the
  orchestrator's slack arm.
- **Migration is partial.** Slack's cheap-probe already does
  per-thread skip without needing dolt_diff. Two options:
  - **Keep the cheap probe, drop only `payload_blake3` and
    `blob_refs`.** Less risk; the slack arm stays roughly as-is.
  - **Replace the cheap probe with dolt_diff for symmetry.** More
    consistent across providers; loses the "no payload load
    required for the skip" property the probe currently has.
- **Recommend the first.** Slack is structurally different enough
  that forcing it through the dolt_diff template is churn for no
  visible win.

### GitHub / GitLab

- Schema: PRs / issues / comments, all wire-payload-shaped.
- Attachments: github carries attachment URLs in body markdown
  (no separate metadata column). gitlab similar.
- **Recommended edge: Shape B** — small per-repo
  `<provider>_attachments` table if a port needs blob CAS; today
  these providers do little binary attachment work and the
  `blob_refs` migration may not be load-bearing for them.
  Phase 1 may be a no-op; phase 2 still applies.
- Bucket: one rendered .md per PR/issue. Bucket key:
  `(repo_id, pr_or_issue_number)` or its UUID.
- dolt_diff union: `dolt_diff_prs`, `dolt_diff_issues`,
  `dolt_diff_pr_comments`, `dolt_diff_issue_comments` (joined back
  to find the owning PR/issue), `dolt_diff_users` (likely fan-out
  to "render everything").

### Notion

- Schema: pages + blocks, both wire-payload.
- Attachments: blocks with image/file content carry external URLs;
  bytes are downloaded into the shared blob CAS today via
  `blob_refs`.
- **Recommended edge: Shape A** on `blocks` if blocks carry the
  attachment ref, otherwise Shape B with a `notion_attachments`
  table keyed by `block_uuid`.
- Bucket: one rendered .md per page. Bucket key: `page_uuid`.
- dolt_diff union: `dolt_diff_pages`, `dolt_diff_blocks` (joined
  to `pages` to project page_uuid via `pages.id` /
  `blocks.parent_page_id`).

### Beeper

- Schema: per-network / per-room / per-period buckets, similar to
  signal+whatsapp. Already has its own period-bucketing.
- **Recommended edge: Shape A** on whatever beeper's
  attachment-metadata table is (or B if it's payload-inline).
- Bucket: one rendered .md per `(network, room, period_key)`.
  dolt_diff at the room granularity (like whatsapp at chat granularity).

### Contacts (carddav)

- Schema: single-row per contact, very small.
- Attachments: photos inline.
- **Recommended edge: Shape A** if `contacts` table has a photo
  column, else Shape B `contacts_attachments` keyed by contact uuid.
- Bucket: small (~few hundred contacts at most) — could even skip
  dolt_diff and just render every contact every time. Up to
  judgment.

### Perseus

- Special: file-tree backed, no doltlite raw db. Translate runs
  HF-Hub-backed BERT alignment. The migration **does not apply** —
  no per-row payload_blake3, no blob_refs, no dolt_diff vtab.

---

## Edge cases worth knowing about

1. **First run / no cursor.** Cold start: parse loads every bucket
   (whatever the provider's "all" query is), `docs_skipped == 0`.
   After render: cursor is written. Next run starts incremental.

2. **`--reset-and-redownload`.** Tables get truncated. Two options:
   - Cursor stays. The next dolt_diff goes from old_hash to HEAD
     (where HEAD now has the re-populated tables); dolt collapses
     this correctly as added/modified/removed.
   - Wipe the cursor. Simpler — next run is a cold start.
   - **Recommend wipe the cursor.** Cleaner semantics, no reasoning
     about diff collapse. Add the wipe to the reset_and_redownload
     branch in extract.

3. **dolt_diff_<table> says "no such table".** Happens on a brand-
   new working set where the table exists but has no dolt history
   yet (extract ran but no `dolt_commit` happened). In production
   the orchestrator commits after every extract — this only bites
   tests that bypass the orchestrator. **Don't paper over with a
   blanket try-catch around the query**; that turns "render
   nothing changed" into "render everything", which masks real
   bugs. Have the test do a `commit_run` after extract.

4. **Removed rows.** A row that existed in `from_ref` but not in
   `to_ref` has `to_<col> IS NULL` in dolt_diff. The
   `coalesce(to_<col>, from_<col>)` projection still picks up the
   old key, so we re-render the bucket (which is now smaller or
   empty). The on-disk stale markdown sits until something GCs it
   — fine for now; add a "shred this markdown" signal as a
   follow-up.

5. **Multi-commit ranges.** If five sync runs happened between two
   renders, `from_ref` points at the older commit and `to_ref =
   'HEAD'` covers the cumulative diff. Dolt handles arbitrary
   ranges natively; no per-commit accumulation needed.

6. **Concurrent renders.** Cursor write isn't locked. Two
   `frankweiler-sync` processes racing on the same source: the
   later overwrites. Worst case: some buckets render twice, none
   get skipped wrongly. Don't add locking.

7. **`dolt_log()` unavailable.** Non-doltlite libsqlite3. The
   `fetch_optional` returns `None`, scan_diff returns
   `ScanResult { new_head: None, .. }`, render skips the cursor
   write. Next run is another cold start. Same outcome as if the
   db didn't exist; reasonable.

---

## Working principles (do not skip)

These came out of the signal + email + whatsapp ports and are
non-negotiable for the upcoming ones:

1. **One commit per provider port.** Each provider's port lands as
   a single commit that turns its sync arm green end-to-end. Pre-
   commit hook runs `bazelisk build --config=clippy //...` so
   clippy must be green workspace-wide.

2. **Run `cargo fmt -p <crate>`** before committing. Bazel's
   rustfmt aspect catches drift in CI; you'd rather catch it
   locally.

3. **`blake3` is populated via UPDATE after the CAS write
   succeeds, never via the entity-row INSERT.** Otherwise a
   re-insert (resync, idempotent re-run) NULLs out a previously-
   computed hash because the bulk-upsert path's ON CONFLICT clause
   does `column = excluded.column` for every typed column.

4. **Don't close the entity-side `SqlitePool` at the end of
   `parse_async`.** The `*BlobReader` cloned it; the renderer reads
   through that clone after parse returns. Closing the original
   pool breaks the clone. The pool closes naturally on the last
   Arc<BlobReader> drop.

5. **Don't proactively unify the `*BlobReader` impls.** At N=3
   we have whatsapp, email, signal each with a tiny per-provider
   reader. The `SELECT bytes FROM cas_objects WHERE blake3 = ?`
   second hop is identical across all three, but the first hop's
   SQL is per-provider. Wait until something forces the
   extraction.

6. **Don't touch unported providers' `blob_refs` writes.** Each
   provider's port is its own commit; mixing two providers'
   migrations is asking for an awkward bisect later.

7. **`source_fingerprint` on the sidecar is now the bucket UUID,
   not a content hash.** The load step still consumes the field
   for its `(qmd_path, source_fingerprint)` skip key; we're
   trading "fingerprint changes when content changes" for
   "fingerprint is stable per bucket". The load step's skip
   becomes "have we ever loaded this exact UUID before?" instead
   of "is the content the same?" — fine because the renderer no
   longer writes if the content hasn't changed.

---

## Order of execution (recommendation)

1. **ChatGPT first.** Smallest schema (2 tables), simplest call
   graph. Use it to confirm the recipe transfers cleanly to a
   never-touched-before provider.
2. **Anthropic second.** Three tables, slightly more complex org +
   conversation hierarchy. Should reuse most of the chatgpt port's
   patterns.
3. **Notion third.** Page/block hierarchy is the largest schema
   that fits the recipe cleanly.
4. **GitHub + GitLab.** Similar shape (PRs / issues / comments).
   Phase 1 may be a no-op for both; phase 2 still applies.
5. **Beeper.** Has its own period-bucketing — closest to
   whatsapp/signal in shape.
6. **Contacts.** Trivial; possibly skip the dolt_diff phase
   entirely.
7. **Slack last.** Already has its own cheap-probe story; the
   minimal port (drop `payload_blake3` + `blob_refs`, keep the
   probe) is likely the right call.

Total expected diff: roughly **8 commits**, similar in size to the
signal+email port (~500–900 lines each, mostly mechanical).

---

## When to stop and surface

If during a port any of the following come up, **stop and ask the
user**:

- The provider's existing schema makes the dolt_diff union
  awkward (no clear bucket key, joins that don't project the key
  cleanly, etc).
- A test against a checked-in fixture fails in a way that's not
  obviously a snapshot needing `cargo insta` review or a
  fingerprint snapshot needing one-line update.
- The orchestrator's per-source state machinery doesn't have an
  obvious place to thread the cursor (most arms do; if one
  doesn't, surface the proposed restructure before doing it).
- The provider needs a new `BulkUpsertable` shape (composite PK,
  non-text non-int column type) that the derive macro doesn't
  support yet. The macro's narrow type universe is intentional;
  extend it deliberately.

Clippy lints, fmt drift, and test snapshot one-line updates are
**not** stop-and-ask — fix locally and proceed.

---

## What's deliberately NOT in this migration

- **Dropping the `blob_refs` table itself.** It stays in the
  schema for the unported providers. Drop it in a final cleanup
  pass once every provider has migrated off it.
- **A `--reset-cursor` CLI flag.** If a user wants to force-rerender
  everything, they can `rm _render_cursor.json`. Add a flag only
  when someone actually needs it.
- **Lifting the second-hop `SELECT bytes FROM cas_objects WHERE
  blake3 = ?` SQL into a shared helper.** It's three lines, and
  unifying it now would obscure the per-provider first hop.
- **A more sophisticated dolt_diff that projects column-level
  change kinds.** Bucket-grained is sufficient and the cheapest
  thing to maintain. If render-bytes mtime churn ever becomes a
  real problem, reintroduce a per-bucket compare *inside* the
  render path; don't push it back into the diff query.
