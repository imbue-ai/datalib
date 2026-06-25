# Provider migration recipe: dolt_diff incremental render + per-provider CAS edges

This doc is the migration recipe for the remaining ETL providers
(notion, github, gitlab, beeper, contacts, perseus, yolink). It
captures what we learned doing **whatsapp → email → signal → chatgpt
→ anthropic → slack** and consolidates the steady-state shape that
the recipe lands at.

The recipe matured a lot as we ported providers. The shape is now:

  - One proc-macro derive per row struct (`WirePayloadRow` for
    entity tables, `CasEdgeRow` for attachment-edge tables); SQL DDL,
    `BulkUpsertable` impl, and PK recipe are all derived.
  - One shared `flush_cas_edges` primitive that every per-provider
    attachment flush delegates to.
  - One shared `CasEdgeAccumulator` that every per-provider extract
    pushes (fetched, known, failed) outcomes into; flush at end of
    bucket is a closure call.
  - One shared `scan_buckets` for the dolt_diff incremental-render
    scan; each provider supplies a global-fanout-table list and a
    bucket-key SQL projection.
  - One shared `load_blake3_index` to pre-load the `(ref_id → blake3)`
    map at fetch entry, so the per-file dedupe check is a HashMap hit
    instead of a SQLite round trip.
  - One shared `bulk_upsert_with_tape` for providers that wire the
    JSONL wire-tape mirror (today: slack and beeper).

After this consolidation, each new provider's port is roughly **300
lines of provider-specific glue** (schema_raw + extract walk + render
md generation) plus standard shared-helper plumbing.

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

## The shared primitives

### `frankweiler_etl::render_cursor`

A small JSON file at `<out_dir>/<stanza>/rendered_md/_render_cursor.json`.

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

Each provider owns one of two shapes, picked based on what its
existing schema looks like:

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
no obvious row to bolt a column onto. Add the
[`CasEdgeRow`](#casedgerow-derive) derive to a four-field struct;
the derive emits the table DDL, the two indices, the
`BulkUpsertable` impl, and the synth-PK recipe:

```rust
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "<provider>_attachments")]
pub struct <Provider>AttachmentRow {
    pub id: String,           // synth: "{owning}#{ref}"
    pub <owning>_<id>: String,// FK into the owning entity table
    pub <ref>_id: String,     // upstream id
    pub blake3: Option<String>,// CAS content hash (NULL until bytes land)
}
```

Plug into `full_ddl()`:

```rust
out.extend(<Provider>AttachmentRow::all_ddl());
```

Examples landed: `chatgpt_attachments`, `anthropic_attachments`,
`slack_attachments`, `chat_item_attachments`.

In both shapes the renderer reaches the bytes via the per-bucket
[`BlobBundle`](#blobbundle), which `parse_doltlite_async` loads once
per bucket via a small projection SQL against the per-provider edge
column/table.

### `CasEdgeRow` derive

The four-field shape above is universal: every per-provider CAS edge
table has the same column structure (id PK + owning FK + ref id +
blake3). The `CasEdgeRow` proc-macro derive emits:

- `Self::ddl()` — the `CREATE TABLE IF NOT EXISTS` string.
- `Self::by_owning_index_ddl()` / `Self::by_ref_index_ddl()` — the
  two indices.
- `Self::all_ddl()` — the three above as a `Vec<String>`, ready to
  splice into `full_ddl()`.
- `Self::pk_recipe(owning, ref) -> String` — synth PK
  `"{owning}#{ref}"`. Override only if your provider needs a
  different shape (signal does, since one chat_item can attach the
  same media_name to multiple slots).
- `impl BulkUpsertable for Self` — `TABLE`, `TYPED_COLUMNS`,
  `bind_into`, all derived.

Field-order discipline is enforced at derive-time: `id: String`
must be first, `blake3: Option<String>` must be last. The two
middle fields become `OWNING_COLUMN` / `REF_COLUMN` based on their
identifiers.

### `flush_cas_edges` + `CasEdgeAccumulator`

`flush_cas_edges` is the canonical 3-step end-of-bucket flush:

1. CAS pool: `put_many` so every edge row's `blake3` points at bytes
   already in the CAS.
2. Entity pool, single tx: `bulk_upsert_in_tx` the edge rows, then
   `record_object_error` stamps for every `(synth_pk, err)` pair.
3. Commit.

`CasEdgeAccumulator` is the matching per-bucket walker. The provider
walks upstream and pushes outcomes into the accumulator:

```rust
let mut attach = CasEdgeAccumulator::new();
for file in walk_upstream(bucket) {
    if let Some(blake3) = blake3_by_file.get(&file.id) {
        attach.add_known(&owning_id, &file.id, blake3.clone());
    } else {
        match download(file).await {
            Ok((bytes, ct)) => {
                let b3 = blake3_hex(&bytes);
                blake3_by_file.insert(file.id.clone(), b3);
                attach.add_fetched(&owning_id, &file.id, bytes, ct, Some(file.name));
            }
            Err(e) => attach.add_failed(&owning_id, &file.id, e.to_string()),
        }
    }
}
// End-of-bucket flush: provider supplies a row-builder closure.
attach
    .flush(db.pool(), db.cas(), |owning, ref_id, blake3| <Provider>AttachmentRow {
        id: <Provider>AttachmentRow::pk_recipe(owning, ref_id),
        <owning>_<id>: owning.to_string(),
        <ref>_id: ref_id.to_string(),
        blake3: blake3.map(String::from),
    })
    .await?;
```

The accumulator handles all the bookkeeping: dedup by `(owning,
ref)`, blake3 resolution (fetched → bundle, known → looked up, failed
→ None), and error-stamp expansion. No per-provider flush code.

### `load_blake3_index` + run-scoped cache

At the start of `fetch()`, load the full `(ref_id → blake3)` map
once:

```rust
let mut blake3_by_file = db.load_attachment_blake3s().await?;
```

(Each provider's `RawDb` wraps the shared helper.) Thread `&mut
blake3_by_file` through the fetch chain. The per-file skip check is
a HashMap hit; after a successful download, insert into the map so
subsequent files in the same run hit the cache without re-fetching.

### `scan_buckets`

The dolt_diff scan is consolidated into one shared call:

```rust
let scan = frankweiler_etl::doltlite_raw::scan_buckets(
    pool,
    last_render_hash,
    &DiffScanSpec {
        global_fanout_tables: &["users", "channels"],
        bucket_query: "
            SELECT DISTINCT <bucket_key> FROM (
                SELECT coalesce(to_<col>, from_<col>) AS <bucket_key>
                  FROM dolt_diff_<table_a>
                 WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
                UNION
                <more unions...>
            )
            WHERE <bucket_key> IS NOT NULL
        ",
    },
).await?;
```

Returns `DiffScan { changed_buckets, new_head, scan_elapsed }`. The
provider passes those through to its `ScanResult` (each provider's
ScanResult names its bucket field appropriately:
`changed_conversations`, `changed_threads`, `changed_chats`).

### `bulk_upsert_with_tape`

When the provider mirrors entity-row writes to a JSONL wire-tape
(today: slack, beeper), every entity upsert goes through:

```rust
bulk_upsert_with_tape(pool, tape, rows, payloads).await
```

— tx → `bulk_upsert_in_tx` → commit → post-commit tape append,
with table name derived from `T::TABLE`. Drops in for any
[`BulkUpsertable`] type.

---

## Phase-by-phase recipe (template)

Each provider's port lands as a single commit, ~300–600 lines after
the shared helpers do most of the work. The phases inside that
commit map roughly to:

### Phase 1: schema_raw.rs

1. Every entity row struct uses `#[derive(WirePayloadRow)]` with a
   `#[wire_payload_row(table = "...")]` attribute. The derive emits
   the DDL + `BulkUpsertable` impl. Field shape: `id_and_payload:
   WirePayload` first, then promoted columns
   (`String`/`Option<String>`/`i64`/`Option<i64>`).

2. The per-provider CAS edge table (if Shape B) is a four-field
   struct with `#[derive(CasEdgeRow)]`. See
   [`CasEdgeRow` derive](#casedgerow-derive) above.

3. Any provider-specific non-payload tables (bookkeeping, sync
   cursors) hand-roll `BulkUpsertable` — slack's `RepliesPagesRow`
   is the template.

4. `full_ddl()` composes: each entity `Row::ddl()`, the CAS edge
   `Row::all_ddl()`, any provider-specific index DDLs, and the
   shared bookkeeping DDLs via `dr::bookkeeping_ddl_for(table)`.

### Phase 2: extract — `RawDb` + walk + flush

1. **No pre-seed.** Rows only appear after a successful detail
   fetch. The listing-pass skip-check works by bulk-reading
   `(id → stored.update_time)` for the listed ids; "no row" means
   "fetch it." A crashed fetch leaves no row; the next sync's
   listing re-surfaces it. See `data_architecture_ingestion.md`
   §"No-preseed listing flow".

2. **Run-scoped blake3 cache.** Load it once at the top of `fetch()`:

   ```rust
   let mut blake3_by_file = db.load_attachment_blake3s().await?;
   ```

   Thread `&mut blake3_by_file` through the fetch chain. The
   per-file dedupe check is a HashMap hit, no SQL.

3. **`CasEdgeAccumulator`.** Inside the per-bucket walk, push
   outcomes — `add_known`, `add_fetched`, `add_failed`. End-of-bucket
   flush is one call with a row-builder closure. See
   [`CasEdgeAccumulator`](#flush_cas_edges--casedgeaccumulator).

4. **Entity writes.** Every entity upsert goes through
   `bulk_upsert_in_tx` (or `bulk_upsert_with_tape` if the provider
   wires the JSONL tape). No hand-rolled UPSERT SQL.

5. **`clear_blob_hashes` for `--refetch-blobs`.** Update
   `<provider>_attachments.blake3 = NULL` (or the
   per-column equivalent for Shape A) so the next walk re-decodes
   and re-stores.

### Phase 3: translate — `parse` + `scan_buckets`

1. `parse.rs::parse(path, last_render_hash)` is the doltlite-aware
   entry. It opens a read-only pool, runs `scan_buckets`, filters
   the load to changed buckets, and per-bucket calls
   `BlobBundle::load(refs_pool, cas_pool, projection_sql,
   &ref_ids)` to populate each bucket's attachment bytes.

2. The dolt_diff scan is one `scan_buckets` call. See
   [`scan_buckets`](#scan_buckets) — the provider supplies a
   `global_fanout_tables` list (entities whose changes mean
   "render everything", typically things in every doc's frontmatter)
   and a `bucket_query` projecting bucket keys.

3. The JSON-tree / mbox fallback (where it exists for fixture
   tests) is a separate function that returns the same
   `ParsedX` shape with `BlobBundle::default()` on each bucket.
   No dolt_diff for that path.

### Phase 4: translate — `render`

1. `render_all(parsed, out_dir, source_name, progress,
   on_doc_complete)` is fully sync — `BlobBundle::materialize_to_dir`
   on the bucket's pre-loaded bytes, no `Arc<dyn BlobReader>`.

2. `parsed.threads`/`parsed.conversations`/etc. is already
   filtered down to changed buckets by the parse step. No
   `prior_fingerprints` arg; no fingerprint compare inside render.

3. `source_fingerprint` on the sidecar / `RenderedMarkdown` is the
   bucket UUID itself (stable, distinct).

4. On success, advance the render cursor:

   ```rust
   if let Some(head) = parsed.scan.new_head.as_deref() {
       let cursor_path = render_cursor::cursor_path(out_dir, "<provider>", source_name);
       render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)?;
   }
   ```

### Phase 5: orchestrator integration

In `frankweiler/backend/sync/src/main.rs`'s match arm for the
provider:

```rust
let cursor_path = frankweiler_etl::render_cursor::cursor_path(
    root, "<provider>", name,
);
let cursor = frankweiler_etl::render_cursor::read(&cursor_path)?;
let parsed = parse(
    &fixture,
    cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
)?;
render_all(&parsed, root, name, progress, on_doc_complete)?;
```

The `prior_fingerprints` arg stays in `translate_source`'s signature
for unported providers — just unused inside this arm. Don't refactor
that signature mid-migration.

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
