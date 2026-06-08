# blob-cas-split branch — handoff

## What's on this branch (vs `main`)

Two commits:

1. `93767f4` etl: add blob_cas module + blob_refs DDL alongside existing blobs
   - New `frankweiler/backend/etl/src/blob_cas.rs` — universal CAS + ref helpers
   - `SHARED_DDL` in `doltlite_raw.rs` now emits both `blobs` (old) **and**
     `blob_refs`/`blob_refs_bookkeeping` (new), so every entity db gets both
     schemas on next open
   - `blake3 = "1"` added to `etl/Cargo.toml` + `BUILD.bazel`
   - No provider uses any of the new code yet

2. `6b2bbf8` chatgpt: migrate to blob_cas
   - chatgpt is the **only** provider on the new path
   - `RawDb` opens a sibling `<name>.blobs.doltlite_db` (the CAS file)
   - Three wrappers (`upsert_blob_bytes` / `pre_seed_blob_stub` /
     `record_blob_error`) collapsed into one `store_blob(stub, bytes)`
   - Renderer writes attachments under `blobs/<short-b3>.<ext>` and uses
     `BlobView::markdown_link` for the link text
   - Test suite passes (4/4); chatgpt golden snapshot is untouched
     because the fixture has no attachments

## What's left

Each remaining provider needs the same shape of change:

| Provider | extract surface | render surface | snapshot risk |
|----------|------------------|-----------------|----------------|
| `slack` | `extract/db.rs` blob wrappers, `extract/api.rs` calls | `translate/render.rs` (`materialize_blobs`, attachment links) | **HIGH** — `slack_render__tng_md_tree.snap` has attachments |
| `anthropic` | `extract/db.rs` + `extract/mod.rs` | `translate/render.rs` | likely none in fixture |
| `notion` | `extract/db.rs` + `extract/mod.rs` (read-by-owner pattern) | `translate/render.rs` + `tests/blob_render.rs` | **HIGH** — `tests/blob_render.rs` constructs `InMemoryBlobStore` directly |
| `email` | `extract/db.rs` + `extract/mod.rs`, `tests/jmap_render.rs` + `tests/jmap_mbox.rs` | `translate/render.rs` (uses its own `unique_safe_filename`) | medium |
| `beeper` | `extract/index_db.rs` (calls `dr::*` directly), `extract/db.rs` count queries, `translate/render.rs` (raw `SELECT bytes,content_type FROM blobs`), `translate/parse.rs` (raw `SELECT FROM blobs`), `bin/beeper_inspect.rs` | same | medium |
| `yolink` | `extract.rs:164` only does `DELETE FROM blobs` | n/a | n/a |
| `contacts` | grep for any usage | n/a | n/a |

After all providers migrated:

- Remove `BLOBS_DDL`, `BLOBS_BOOKKEEPING_DDL`, `BlobBytes`, `blob_exists`,
  `pre_seed_blob_stub`, `upsert_blob_bytes`, `record_blob_error`,
  `load_blobs_by_id`, `load_blobs_by_owner` from `doltlite_raw.rs`
- Delete `etl/src/blob_store.rs` (188 lines)
- Delete `etl/src/blobs.rs` (50 lines, just `safe_filename`)
- Update `lib.rs` exports
- Update `DOLTLITE_RAW_PORT_GUIDE.md` blob section
- Drop `DELETE FROM blobs` from `truncate_data_tables` in `doltlite_raw.rs`

## The migration recipe per provider

1. In `extract/db.rs`:
   - Replace `use frankweiler_etl::doltlite_raw::{..., BlobBytes}` with
     `use frankweiler_etl::blob_cas::{self, BlobCas, BlobReader,
     InMemoryBlobReader, RefStub, SqliteBlobReader};`
   - Add `cas: BlobCas` field to `RawDb`, open it in `RawDb::open` via
     `BlobCas::open(&blob_cas::cas_path_for(db_path)).await?`
   - Add `pub fn cas(&self) -> &BlobCas`
   - Replace `blob_exists` → `blob_cas::ref_has_hash(&self.pool, ref_id)`
   - Replace the 3-or-4 blob wrappers with one `store_blob(stub, bytes)`
     that calls `blob_cas::store_bytes`
   - `record_blob_error` → `blob_cas::record_ref_error`
   - `LoadedRaw.blobs` field type → `Arc<dyn BlobReader>`
   - `LoadedRaw::Default` uses `InMemoryBlobReader::empty_handle()`
   - `block_on_load_all` opens the CAS and constructs
     `SqliteBlobReader::new(db.pool().clone(), db.cas().pool().clone())`

2. In `extract/mod.rs` (and `extract/api.rs` for slack):
   - Drop `use frankweiler_etl::blobs::safe_filename` if present
   - Replace `db.upsert_blob_bytes(...)` with `db.store_blob(&RefStub { ... }, &bytes)`
   - `pub use db::{...}` — drop `BlobBytes` from the re-export list

3. In `translate/parse.rs`:
   - Field type on whatever `Parsed*` struct holds blobs →
     `Arc<dyn frankweiler_etl::blob_cas::BlobReader>`
   - `Default` impl uses `InMemoryBlobReader::empty_handle()`

4. In `translate/render.rs`:
   - Replace `use frankweiler_etl::blob_store::BlobStore` with
     `use frankweiler_etl::blob_cas::{self, BlobReader}`
   - Drop `use frankweiler_etl::blobs::safe_filename`
   - Materializer becomes: for each unique ref_id seen in attachments,
     call `blob_cas::materialize_to_disk(blobs, ref_id, &blobs_dir)`
   - Attachment-link emitter: look up `blobs.read_by_ref_id(ref_id)`,
     use `BlobView::rendered_filename()` for the filename; preserve
     each provider's link decoration (`![...](...)` for images, etc.)
   - For missing-bytes attachments: emit an HTML-comment placeholder,
     not a fake link

5. For each provider's tests that build `InMemoryBlobStore::from_id_map`
   / `from_owner_map`, replace with `InMemoryBlobReader::new()` +
   `.insert(BlobView { ... })` per blob.

6. Snapshot regeneration:
   - Build will fail on snapshot mismatch.
   - User wants **diff-review each `.snap` change before accepting**.
   - The only legitimate change should be: attachment filenames flip
     from `<sanitized-name>` to `<short-b3>.<ext>` and (when bytes are
     missing) attachment lines become `<!-- attachment ... -->`.

## How to verify a provider migration

```bash
bazel build //frankweiler/backend/etl/providers/<name>/...
bazel test  //frankweiler/backend/etl/providers/<name>/...
# Diff any failing .snap.new vs .snap
bazel build //frankweiler/backend/...   # downstream check
```

## Open design questions still

- **`upstream_name` vs sanitized for link text.** chatgpt currently
  drops `]` and uses the raw name. Slack/notion may have different
  expectations — check each renderer.
- **Multi-blob owners** (chatgpt/anthropic): `read_by_owner` returns
  the lexically-last ref. That semantics is preserved. Notion uses
  `read_by_owner` because blobs are keyed by block id; the `RawDb`
  there has a `load_blobs_by_owner` not `_by_id` — replace with
  `BlobReader::read_by_owner` flow.
- **Yolink reset path.** `DELETE FROM blobs` in `yolink/src/extract.rs`
  becomes `DELETE FROM blob_refs` and `cas.pool() DELETE FROM cas_objects`
  (or call `blob_cas::gc_orphans(&cas, &[])` for the same effect).

## Quick line-count economy check

After commit `6b2bbf8`, branch vs `main` shows:

- `+784` lines in new `blob_cas.rs`
- `-33` lines net in chatgpt (collapsed wrappers)

Provider savings of ~30–100 lines each × 6 = roughly **180–600 lines**
in remaining migrations. Final shared-layer cleanup is `-50` (blobs.rs)
`-188` (blob_store.rs) `-~200` (doltlite_raw blob CRUD) = **~440** lines.

Best case: ~1000 lines deleted vs ~784 lines added — a real but modest
net-negative (~200 lines). The bigger win is mechanism count: one
universal extract write API, one universal reader trait, content-
addressed dedupe for free.
