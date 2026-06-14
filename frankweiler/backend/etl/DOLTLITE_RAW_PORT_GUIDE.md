# Porting a provider's raw store to doltlite

Reference for the pattern every provider now follows: a single
`.doltlite_db` file per source as the raw store. Distilled from the
ports: commits `5d5676d` (notion), `d0a07af` (chatgpt + anthropic +
shared utils), `815f290` (slack/bazel cleanup), `79c3b4a` (live
golden refresh), `24f1769` (bazel `.update` flow), and the
github/gitlab ports that closed out the file-tree backed providers.

All in-tree providers (anthropic, chatgpt, notion, slack, github,
gitlab, beeper) use this pattern — no more JSONL-tree raw stores.
Kept as a reference for new providers added later.

---

## End state

For a provider `<name>`, the raw store goes from this:

```
<data_root>/raw/<name>/
  some.json
  some_other.jsonl
  blobs/<id>/<file>
  ...
```

to this:

```
<data_root>/raw/<name>.doltlite_db
```

**That's it.** A single sqlite file is the entire output of the
download. No `raw/<name>/` dir, no `raw/<name>/blobs/` dir. Object
payloads, sync-run logs, AND blob bytes all live in
tables inside that one `.doltlite_db`.

Blob bytes get materialized to disk **next to the rendered markdown**
at translate time, following Notion's page-dir layout:

```
rendered_md/<provider>/<acct>/<scope>/<entity_id>/
  index.md
  index.grid_rows.json
  blobs/<filename>          # written by translate, byte-equal to the
                            # `bytes` column in the doltlite blobs table
```

The markdown link is the relative `blobs/<filename>`. A single
`<entity_id>/` directory is sharable in isolation — drop it on a USB
stick and the markdown + every attachment travels with it.

---

## Design rules (load-bearing — read before deviating)

### 1. Primary keys are upstream identifiers

Every object table keys by the **upstream provider's identifier**,
stored as `TEXT`. No surrogate `AUTOINCREMENT INTEGER`s on object
tables. The reasons (also spelled out in
`frankweiler/backend/etl/src/doltlite_raw.rs` module docs):

- `dolt diff` **stability**. Re-fetching the same upstream row must
  land at the same PK so dolt sees content changes, not row-id churn.
- **Idempotent upserts**. `ON CONFLICT(id)` works only if `id` is the
  upstream id.
- **Pre-seeding**. `(id, NULL payload)` rows must collapse into the
  same row when the detail fetch lands later. Both writers must know
  the PK up front.
- **Cross-table references** (e.g. `messages.conversation_id`) only
  mean something if they point at upstream ids.

Exception: log tables (`sync_runs`) use `AUTOINCREMENT INTEGER`
because a sync invocation has no upstream identity.

### 2. Don't use surrogate IDs for ordering either

If you need within-parent ordering (e.g. messages within a channel,
blocks within a page), add an explicit `<scope>_order INTEGER NULL`
column and `ORDER BY <scope>, <scope>_order, id`. We do NOT borrow the
PK for ordering. We do NOT use sqlite's `rowid` — doltlite hides it
(`WITHOUT ROWID`-flavored).

### 3. Synth reads checked-in fixtures; extract writes doltlite

Critical separation that prevents binary blobs in git:

- **Checked-in fixtures stay as JSON/JSONL.** Diffable,
  human-editable, no sqlite-version skew.
- The provider's `Synth` reads those fixtures and emits HTTP playback
  fixtures.
- `extract` writes the doltlite db when it runs against playback. The
  bazel fixture pipeline produces the `.doltlite_db` as an output of
  extract — never as a checked-in input.

We tried checking in a built `.doltlite_db` once. Python's stock
sqlite couldn't open the doltlite-flavored file. Don't go that route.

### 4. Journal mode DELETE, not WAL

Handled inside `frankweiler_etl::doltlite_raw::open()`:

```rust
.journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
.synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
```

The raw store has a single writer (extract) and a single reader
(translate, after extract has exited). WAL leaves `<file>-wal` /
`<file>-shm` sidecars that wreck golden snapshots. `DELETE` is a
single file, byte-stable across runs.

### 5. Store payloads as raw as possible

Lesson learned on the anthropic port: we used to pre-normalize the
API response (`normalize_to_export_shape`) at fetch time. With the
doltlite port we store the **raw** response and run normalize at read
time in `translate`. That way `dolt diff` reflects actual upstream
change instead of churn from our normalizer evolving.

Corollary: stop polluting JSON payloads with downloader-synthesized
keys (`_fetched_at`, `_listing_update_time` etc.). Promote them to
real columns. This was a deliberate change made during the chatgpt
port.

### 6. Object table shape

Data columns live on the object table; bookkeeping
(`fetched_at`, `attempt_count`, `last_attempt_at`, `last_error`)
lives on a sidecar `<entity>_bookkeeping` table built by
`dr::bookkeeping_ddl_for("<entity>")`. The split lets
`dolt diff` over the data tables reflect only upstream content
change — bookkeeping churn from re-fetches is invisible to
`dolt diff`.

```sql
-- data table — provider-owned
CREATE TABLE IF NOT EXISTS <entity> (
    id TEXT PRIMARY KEY,                -- upstream id
    parent_id TEXT NULL,                -- if relevant
    -- ... provider-specific columns ...
    payload TEXT NULL                   -- raw JSON wire payload (stored as JSONB; see below)
)
-- sidecar — added via `dr::bookkeeping_ddl_for("<entity>")`
CREATE TABLE IF NOT EXISTS <entity>_bookkeeping (
    id TEXT PRIMARY KEY,
    fetched_at TEXT NULL,
    attempt_count INTEGER NOT NULL DEFAULT 0,
    last_attempt_at TEXT NULL,
    last_error TEXT NULL
)
```

**Always-paired**: every object row has a matching sidecar row in
the same transaction (use `dr::record_object_attempt(&mut tx,
table, id, None)` on success, `dr::record_object_error(&mut tx,
table, id, err)` on failure, or `dr::ensure_object_row(&mut tx,
table, id)` to pre-seed both with `attempt_count=0`).

`payload IS NULL` means "exists upstream, not yet fetched."
`--retry-failed` re-fetches rows with
`last_error IS NOT NULL OR (payload IS NULL AND attempt_count > 0)`,
which `dr::failed_ids(table)` computes via a JOIN against the
sidecar.

### 6a. JSONB storage for payloads

Doltlite v0.11.2+ inherits SQLite 3.45's binary JSON. Every
`payload` column stores JSONB (a BLOB at the SQLite level), produced
by wrapping the bind in `jsonb(?)` on INSERT and unwrapping with
`json(payload) AS payload` on SELECT. The Rust side still binds a
text JSON string and still reads a text JSON string — only the
on-disk representation changed.

```sql
-- INSERT
INSERT INTO conversations (id, ..., payload, ...) VALUES (?, ..., jsonb(?), ...)
ON CONFLICT(id) DO UPDATE SET ..., payload = excluded.payload, ...
--                                                ^ excluded.payload carries the JSONB
--                                                  through the upsert; don't re-wrap

-- SELECT
SELECT id, json(payload) AS payload, ... FROM conversations WHERE payload IS NOT NULL
```

Add a typeof() probe test in your provider's `db.rs::tests`:

```rust
let row = sqlx::query("SELECT typeof(payload) AS t FROM <table> WHERE id='...'")
    .fetch_one(db.pool()).await.unwrap();
let t: String = row.try_get("t").unwrap();
assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
```

This guards against an accidental `jsonb()` removal that would
silently fall back to text storage with no other visible difference.

**Don't wrap** `sync_runs.config|summary` — those are tiny single-row
bookkeeping where ad-hoc `sqlite3 ... SELECT ...` ergonomics matter
more than
binary-format parse perf.

Reads via `dr::load_payloads()` already unwrap with `json(...)`; if
you write a custom SELECT that returns `payload`, wrap it manually.

### 7. Blobs

Bytes live in a **sibling CAS file** — `<name>.blobs.doltlite_db` —
not in the entity db. The CAS holds a single `cas_objects` table keyed
by blake3 hash. The entity db carries `blob_refs` (PK = upstream-stable
id, fallback `{owning_id}:{slot}`) with a nullable `blake3` column
pointing into the CAS. See `frankweiler_etl::blob_cas` for the full
shape; full DDL in the `BLOB_REFS_DDL` / `CAS_OBJECTS_DDL` constants.

Open the CAS alongside the entity db in `RawDb::open`:

```rust
let pool = dr::open(db_path, DDL).await?;
let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
```

Trust-our-copy refetch policy: skip if `blake3 IS NOT NULL`. Handled
by `blob_cas::ref_has_hash()` (exposed as `db.blob_exists()` in your
provider's RawDb).

Write path: one call covers both pre-seed-then-fetch and
fetch-and-store. Build a `RefStub` with the upstream metadata and
either call `db.pre_seed_blob_stub` (no bytes yet) or `db.store_blob`
(post-download). `store_blob` does both the CAS `put` and the
`blob_refs` `attach_hash` in two steps; cross-file atomicity isn't
guaranteed, but the `gc_orphans` sweep reconciles any orphan CAS
rows. Failed fetches go through `db.record_blob_error`.

Read path: parse builds per-bucket
[`BlobBundle`](../etl/src/blob_cas.rs)s — one bag of `(ref_id → bytes
+ blake3 + content_type)` per rendered doc — by issuing one
[`BlobBundle::load`](../etl/src/blob_cas.rs) per bucket against the
provider's CAS-edge table joined to the sibling `cas_objects` pool.
The shape of every per-provider edge table is the same
[`CasEdgeRow`](../etl/src/blob_cas.rs) derive (id PK, owning FK, ref
column, nullable blake3). Render is then a sync transformer: `bundle.
materialize_to_dir(<page_dir>/blobs)` writes bytes, `bundle.get
(ref_id)` returns one entry, `bundle.markdown_link(ref_id, …)` emits
the `![…](blobs/…)` link. For unit tests, build the bundle directly
with `BlobBundle::add(ref_id, bytes, content_type, upstream_name)`.

### 8. sync_runs (log table)

Full DDL in `doltlite_raw::SYNC_RUNS_DDL`. Append-only. One row per
sync invocation. Stamp via `start_run()` / `finish_run()` so a crash
mid-sync still leaves a row with status='running'.

---

## Shared utilities — use them

`frankweiler_etl::doltlite_raw` owns everything provider-agnostic.
Don't re-implement these in your provider:

| Need | Use |
|------|-----|
| Open DB + apply DDL | `dr::open(db_path, provider_specific_ddl)` |
| `<data_root>/raw/<name>` ↔ `<...>.doltlite_db` | `dr::db_path_for()` |
| Sync run logging | `dr::start_run()` / `dr::finish_run()` |
| Sidecar bookkeeping DDL | `dr::bookkeeping_ddl_for(table)` |
| Pre-seed (id) row + sidecar | `dr::ensure_object_row(&mut tx, table, id)` |
| Record fetch attempt | `dr::record_object_attempt(&mut tx, table, id, None\|Some(err))` |
| Record fetch error (alias) | `dr::record_object_error(&mut tx, table, id, err)` |
| Retry list | `dr::failed_ids(table)` |
| Read JSON payloads | `dr::load_payloads(table)` |
| Blob write (pre-seed) | `blob_cas::pre_seed_ref(&mut tx, &RefStub { ... })` |
| Blob write (fetched bytes) | `cas.put(bytes, ct)` + `blob_cas::attach_hash(&mut tx, stub, &hash)`, wrapped as `blob_cas::store_bytes(entity_pool, cas, stub, bytes)` |
| Blob error | `blob_cas::record_ref_error(&mut tx, ref_id, owning_id, slot, err)` |
| Per-provider CAS-edge table DDL | `#[derive(CasEdgeRow)] struct <X>AttachmentRow { id, owning, ref, blake3 }` + `<X>AttachmentRow::all_ddl()` |
| Per-bucket blob bag | `blob_cas::BlobBundle::load(refs_pool, cas_pool, projection_sql, &ref_ids)` |
| Per-bucket file write | `bundle.materialize_to_dir(&page_dir.join("blobs"))` |
| Universal markdown link | `bundle.markdown_link(ref_id, display, is_image)` |

The shared module ships shared DDL constants too — `BLOB_REFS_DDL`
(+ its indexes), `SYNC_RUNS_DDL` — appended to your provider's DDL
inside `open()`. The CAS file gets `CAS_OBJECTS_DDL`
applied by `BlobCas::open()`.

Your `extract/db.rs` should be a thin provider-specific layer (see
`notion/src/extract/db.rs` as the canonical template — ~440 lines
covering pages/blocks/comments/databases/users with shared blob/sync
plumbing delegated).

---

## Implementation checklist

### Crate code (`frankweiler/backend/etl/providers/<name>/`)

1. **`src/extract/db.rs`** — provider-specific tables only. Shape:

   ```rust
   use frankweiler_etl::blob_cas::{self, BlobBundle, BlobCas, RefStub};
   use frankweiler_etl::doltlite_raw::{self as dr};
   pub use frankweiler_etl::doltlite_raw::db_path_for;

   const DDL: &[&str] = &[
       "CREATE TABLE IF NOT EXISTS <entity> ( ... )",
       "CREATE INDEX IF NOT EXISTS ...",
       // ... your provider-specific tables ...
   ];

   pub struct RawDb { pool: SqlitePool, cas: BlobCas }

   impl RawDb {
       pub async fn open(p: &Path) -> Result<Self> {
           let pool = dr::open(p, DDL).await?;
           let cas = BlobCas::open(&blob_cas::cas_path_for(p)).await?;
           Ok(Self { pool, cas })
       }
       pub fn cas(&self) -> &BlobCas { &self.cas }
       pub async fn store_blob(&self, stub: &RefStub<'_>, bytes: &[u8]) -> Result<String> {
           blob_cas::store_bytes(&self.pool, &self.cas, stub, bytes).await
       }
       // Provider-specific upserts + state-check methods.
       // - Wrap payload binds in `jsonb(?)` (see §6a).
       // - Wrap payload SELECTs in `json(payload) AS payload`.
   }

   pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
       // tokio::task::block_in_place + Handle::current().block_on()
       // so the sync translate path can read from the async DB.
   }
   ```

2. **`src/extract/mod.rs`** — swap on-disk writes for DB upserts.
   Standard flow:
   - `FetchOptions { db_path: PathBuf, ... }` (NOT `out_dir`)
   - `let db_path = db_path_for(&opts.db_path)` (handles legacy dir)
   - `let db = RawDb::open(&db_path).await?`
   - `let run_id = db.start_run(&run_config).await?`
   - Async work block: list / pre-seed / prioritize missing→stale /
     fetch detail / upsert payload / download blobs into DB
   - `db.finish_run(run_id, status, &summary_json).await`

3. **`src/extract/api.rs`** — transport-only. Move any
   `download_one_file` / `download_attachments_for_conversation`
   helpers out: they now write into the CAS via
   `db.store_blob(&RefStub { ... }, &bytes)` rather than the
   filesystem. Pattern: tempfile + `latchkey curl -fSL -o tmpfile
   signed_url`, then `fs::read(tmp.path())` and `store_blob`. Errors
   call `db.record_blob_error()`.

4. **`src/translate/parse.rs`** — dispatch on path:

   ```rust
   pub fn parse_export(path: &Path) -> Result<Parsed> {
       let db_path = db_path_for(path);
       if db_path.exists() {
           return Ok(parse_loaded(block_on_load_all(&db_path)?));
       }
       if path.is_dir() {
           return parse_export_json_dir(path);  // legacy fallback
       }
       bail!("source not found at {}", path.display())
   }
   ```

   Keep the legacy JSON-tree reader as `parse_*_json_dir` so the
   in-crate render fixture test (which uses checked-in JSON) still
   works. Factor shared row-walking logic so both readers produce
   byte-identical output.

   Build a per-bucket
   `HashMap<bucket_key, frankweiler_etl::blob_cas::BlobBundle>` (or
   attach `pub blobs: BlobBundle` to each bucket struct, as slack
   does) by issuing one `BlobBundle::load(...)` per bucket against
   the provider's CAS-edge table joined to the sibling CAS pool. The
   default is an empty bundle / empty map.

5. **`src/translate/render.rs`** — restructure to **page-dir layout**:

   - `Rendered::relative_path()` returns `<entity_id>/index.md` (NOT
     `<entity_id>.md`). One directory per renderable entity.
   - Materializer is a single call to
     `bundle.materialize_to_dir(&page_dir.join("blobs"))`. Filenames
     come from `Blob::rendered_filename` (`<short-b3>.<ext>`) so
     collisions and "what extension?" go away.
   - Attachment-link emitter uses `bundle.markdown_link(ref_id,
     display, is_image)` for the image-or-file shape. Providers with
     a non-standard decoration around the link build it themselves
     and call `blob.rendered_filename()` directly.
   - Inside `render_all()`, the per-entity loop becomes:
     `mkdir(page_dir)` → `materialize_conv_blobs(...)` → write
     `index.md` → write `index.grid_rows.json` (use
     `abs.with_extension("grid_rows.json")` on `<page_dir>/index.md`).

   The rendered markdown's blob links change shape vs. the
   pre-doltlite era (no more `../../../raw/<src>/blobs/...`); that's
   intentional — it's part of "the only output of download is the
   doltlite db". Re-record the rendered_md snapshots.

   See `chatgpt/src/translate/render.rs` and `anthropic/src/translate/render.rs`
   for the canonical pattern.

6. **`src/bin/<name>_translate.rs`** — switch to
   `#[tokio::main(flavor = "multi_thread", worker_threads = 2)]`
   because `block_on_load_all()` uses `block_in_place`.

7. **`synthesize.rs`** — **no changes**. Synth keeps reading
   checked-in JSON/JSONL fixtures. Synth's job is fixtures → HTTP
   playback responses; the runtime doltlite db is produced by extract
   running against the playback synth writes.

8. **`Cargo.toml`** — add `sqlx = { workspace = true }` to
   `[dependencies]`.

### Bazel (`BUILD.bazel`)

9. Add `"@frankweiler_crates//:sqlx"` to the lib's `deps`.
10. Add `"@frankweiler_crates//:tokio"` to the `<name>_translate` bin
    `deps` (it's now async).
11. Add `tempfile` to the deps of any insta-using tests that
    `tempfile::TempDir::with_prefix` (the live tests typically do —
    they only ever built under cargo before).

### Sync orchestrator (`frankweiler/backend/sync/src/main.rs`)

12. Change `out_dir: ...` to `db_path: ...` in the `FetchOptions`
    construction for this provider.

### Tests

13. **Provider unit tests** in `db.rs`: open a fresh `RawDb` in a
    tempdir, exercise upsert / load / record-error / retry-failed.
14. **`playback_roundtrip` integration test**: rewrite to assert
    against the DB rather than the on-disk JSON files. Existing
    pattern: synth JSON → playback → extract → `block_on_load_all` →
    diff payloads against the input. Has a `rust_test` rule in
    BUILD.bazel.
15. **`*_render` test**: continues using the checked-in JSON tree
    fixture via the `parse_*_json_dir` fallback. Tested by cargo and
    bazel both.
16. **`*_live` test**: must read via `block_on_load_all(db_path_for(tmp))`
    after `fetch()` rather than slurping files. Has a `rust_test` rule
    (with `manual` tag since it hits real APIs).
17. **`fixture_db_snapshot__fixture_backend_index.snap`** (in
    `frankweiler-core`): the per-conversation/page `source_fingerprint`
    will drift when you stop polluting payloads with synthetic keys.
    Re-record once via
    `bazel run //frankweiler/backend/core:fixture_db_snapshot_test.update`.
18. **`manual_e2e_live_sync_golden.snap`** + per-file snaps under
    `frankweiler/backend/sync/tests/snapshots/raw/`: re-record once via
    `bazel run //frankweiler/backend/sync:manual_e2e_live_sync_golden.update`
    (needs `LATCHKEY_CURL` set on the host — see AGENTS.md for the
    full incantation). Delete the now-orphan per-file `.snap`s that
    the new manifest doesn't reference.

---

## Snapshot-update flow (post-port)

Every insta-using test has a `.update` sibling in BUILD.bazel via
`//tools:insta.bzl::insta_update`. Run with `bazel run`, not
`bazel test`. See the "Updating insta snapshots" section in AGENTS.md.
When you add a new snapshot test in your port, add a sibling
`.update`:

```python
load("//tools:insta.bzl", "insta_update")

rust_test(
    name = "foo_render",
    data = [":tng_fixture"],
    env = {"FOO_FIXTURE_DIR": "frankweiler/.../fixtures/foo_api"},
    ...
)

insta_update(
    name = "foo_render.update",
    test = ":foo_render",
    test_args = ["--ignored"],  # if the test is #[ignore]'d
    # `data` and `env` on the rust_test DO NOT propagate through the
    # sibling sh_binary wrapper. Mirror every fixture / env-var dep
    # here or the .update run will panic with "fixture not found".
    extra_data = [":tng_fixture"],
    extra_env = {"FOO_FIXTURE_DIR": "frankweiler/.../fixtures/foo_api"},
)
```

---

## Gotchas (we hit these and you will too)

1. **doltlite hides `rowid`**. Don't `ORDER BY rowid`. Don't use
   `INTEGER PRIMARY KEY AUTOINCREMENT` as a fake rowid for ordering.

2. **Python stock sqlite can't open doltlite files**. Anywhere a
   non-Rust tool wants to read the db (the bazel pipeline's
   `run_sync_pipeline.py`, etc.), have it read the JSON/JSONL
   fixture directly instead. The doltlite db only flows through Rust
   code.

3. **WAL/SHM sidecars wreck golden tests**. The shared `dr::open()`
   already sets `journal_mode=DELETE`. Don't override.

4. **`cargo fmt --check` runs in precommit**. `cargo fmt` your new
   files before committing.

5. **`MODULE.bazel.lock`** changes when you add new workspace deps in
   `Cargo.toml`. Include it in the commit.

6. **Don't add a separate `<name>_jsonl_to_doltlite` converter
   binary**. We tried this once and threw it away. The correct
   architecture: synth reads JSONL → produces playback → extract
   reads playback → writes doltlite. The doltlite db is naturally
   generated by running the real pipeline.

7. **The `INSTA_WORKSPACE_ROOT` trap**. For new `.update` targets,
   the macro sets it to `$BUILD_WORKSPACE_DIRECTORY` (the workspace
   root), NOT a crate subdir. Insta combines that root with the
   crate-relative path it derives from the source file. Setting
   workspace_subdir would double-path it.

8. **Bazel test target name = cargo binary name for snapshot tests.**
   Insta names snapshot files `<binary>__<snap>.snap`, where
   `<binary>` is the cargo binary name (= test source filename).
   When the bazel `rust_test` name differs (`foo_test` vs `foo`),
   the bazel run looks for `foo_test__*.snap` while cargo writes
   `foo__*.snap`. Fix: name the bazel target to match the cargo
   binary. We had to rename a few `*_test` rules to drop the suffix.
   For Slack specifically the slack_translate rust_binary was
   renamed to `slack_translate_bin` to free the label.

9. **`live` tests need `tempfile` in bazel deps.** They only ever
   built under cargo before, so the bazel `rust_test` rules were
   missing the dep. Adding the `.update` target builds the test —
   surfacing the missing dep. Add `tempfile` if you see
   `cannot find module or crate \`tempfile\``.

10. **Don't checked-in your fixture's `.doltlite_db`.** Run
    `bazel run //frankweiler/backend/sync:manual_e2e_live_sync_golden.update`
    after your port to refresh the binary-blob marker snapshots.
    Those are `<binary N bytes>` markers, not the actual db
    contents — the golden test deliberately skips byte-identity on
    binary files.

---

## Quick test loop

```bash
# 1. Inner-loop while writing your port:
cargo test -p frankweiler-etl-<name>

# 2. Round-trip via playback:
cargo test -p frankweiler-etl-<name> --test playback_roundtrip

# 3. Live golden (needs LATCHKEY_CURL set):
bazelisk build //frankweiler/backend/etl:latchkey_curl_shim
export LATCHKEY_CURL="$(pwd)/bazel-bin/frankweiler/backend/etl/latchkey_curl_shim"
bazelisk run //frankweiler/backend/sync:manual_e2e_live_sync_golden.update

# 4. Full bazel verify — CANONICAL invocation, matches AGENTS.md:
bazelisk test //...
```

Do NOT substitute `bazelisk test //... --test_tag_filters=-manual,-external`
here. That filter exists in some legacy notes but it silently excludes
`//:precommit_test` (cargo fmt / clippy / ruff / pyright / vue-tsc) and
`//frankweiler/ui:e2e_test` (Playwright UI suite), letting fmt drift and
UI regressions through. The canonical answer to "are tests green?" is
the bare `bazelisk test //...` line — see the "Running tests" section
of `AGENTS.md` at the repo root.

A successful port produces:

- All cargo tests green
- `bazelisk test //...` green (no filter)
- `manifest.snap` collapses `raw/<name>/<files...>` to one
  `raw/<name>.doltlite_db` row, plus blob rows shift from
  `raw/<name>/blobs/...` to `rendered_md/.../<entity>/blobs/<file>`.
- `rendered_md/` paths shift from `<entity>.md` to `<entity>/index.md`
  (page-dir layout). The blob link target inside the .md changes
  from `../../../raw/<src>/blobs/...` to `blobs/<filename>`. Re-record.
- `fixture_db_snapshot__fixture_backend_index.snap`:
  `qmd_path` columns shift to the page-dir form
  (`<entity>/index.md`); `source_fingerprint` may drift too if you
  dropped synthetic keys from the payload.
- `raw/<name>.doltlite_db.snap`: just a `<binary N bytes>` marker;
  the byte count will change as you populate the new tables. Expect
  small drifts on subsequent edits (JSONB encoding, schema changes,
  page-alignment quirks) — review the size delta as a sanity check
  but don't try to make it byte-stable across encoding changes.

---

## Slack specifically

Looking ahead to the Slack port: Slack's current raw store is more
elaborate than the others. Things to plan for:

- **Multiple entity tables**: channels, users (workspace listings),
  messages (per-channel), replies (thread-children), file media.
  Current layout is JSONL events under `raw_api/<entity>/run-*.jsonl`,
  not single JSON files.
- **Conversations.list / users.list redaction**: the live-golden test
  (`SKIP_PATH_SEGMENTS`) deliberately omits these from snapshots
  because they're workspace-wide and churn on every join/leave.
  Make sure your DB-backed equivalent keeps that semantics — either
  by skipping them at upsert time or by filtering at read time.
- **`source_fingerprint` per thread**: slack's render is the most
  fingerprint-sensitive in the tree. Any normalization shift will
  cascade to all `.grid_rows.json` sidecars.
- **Synth + JSONL**: slack's synth reads JSONL (one event per line),
  not flat JSON files. Keep that input shape. The output (playback)
  is what extract reads.
- **Slack has a `slack_translate_bin` (rust_binary) AND a
  `slack_translate` (rust_test)**. Don't accidentally undo that
  split — the bazel target / snapshot file naming depends on it.

Good luck.
