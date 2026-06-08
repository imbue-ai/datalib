# ETL: the general shape

Cross-cutting conventions every provider's Extract step is expected
to follow. Where to look for the things that **aren't** in this file:

  - The three-stage pipeline (Extract → Translate → Load), crate
    layout, and sidecar contract: [`backend/etl/README.md`](../frankweiler/backend/etl/README.md).
  - The raw store's table-and-blob shape, primary-key rules, and
    `sync_runs` bookkeeping: [`backend/etl/DOLTLITE_RAW_PORT_GUIDE.md`](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md).
  - Reading the dolt history of a raw store: [`docs/doltlite.md`](doltlite.md).
  - Per-provider auth, API surface, resume strategy: each provider's
    `EXTRACT.md` (e.g. [`providers/slack/EXTRACT.md`](../frankweiler/backend/etl/providers/slack/EXTRACT.md)).

This file is the rules-of-the-road every provider obeys.

## What "a data source" is

A `SourceConfig` entry under `sources:` in the user's
`~/.config/frankweiler/config.yaml`. Each entry has:

  - **`name:`** — stable label. It becomes the file basename
    (`raw/<name>.doltlite_db`), the directory under `rendered_md/`,
    and the PK of any per-source bookkeeping. Renaming a source
    orphans its history; treat `name` like a primary key.
  - **`type:`** — discriminator over `SourceConfig` variants (see
    [`backend/core/src/config.rs`](../frankweiler/backend/core/src/config.rs)).
    Picks which provider's Extract code runs.
  - **`sync:`** — optional; provider-specific tunables. Sources
    *without* a `sync:` block are translate-only (the worker ingests
    whatever is already at `input_path`).

## Sync run lifecycle

A "sync run" is one invocation of `frankweiler-sync` — the
orchestrator at [`backend/sync/src/main.rs`](../frankweiler/backend/sync/src/main.rs).
For each enabled source it:

  1. Opens `raw/<name>.doltlite_db`. If the working tree is dirty
     from a prior crashed run, `doltlite_raw::open` stamps a
     `rescue:` commit before any DDL runs — see
     [`docs/doltlite.md`](doltlite.md#rescue-commits-on-every-rust-side-open).
  2. Hands the open pool to the provider's `extract::fetch(...)`.
     The provider walks upstream, UPSERTs into its tables, and
     returns a `FetchSummary` without touching `dolt_commit`.
  3. Calls `commit_run(pool, "extract <name>: <stats>")` exactly
     once. The returned commit hash is appended to the source's
     status line as `; commit=<hash>` and surfaced in the JSON
     summary.
  4. Runs Translate in-process against the same raw store, writing
     sidecars under `rendered_md/<provider>/`.

## Commit lifecycle (load-bearing rule)

**Providers do not call `dolt_commit` or `commit_run` themselves.**
The orchestrator wraps each source's extract in exactly one commit
at the end. A run that touches N upstream pages / windows / items
produces **one** entry in `dolt_log()`, not N. The commit message
is `extract <name>: <stats>`, which is what `dolt_log()` browsers
(`docs/doltlite.md`) and `dolt_diff_summary` consumers index on.

Two consequences:

  - `dolt diff HEAD^1 HEAD` for any source's raw store is exactly
    "what this run pulled" — a clean unit of analysis for the
    UI's incremental delta surface and for after-the-fact audits.
  - Provider authors don't have to think about commit boundaries.
    If you find yourself reaching for `commit_run` inside a
    provider, you almost certainly want UPSERT instead.

The only other commits allowed in a raw store are `rescue:`
commits — see [`docs/doltlite.md`](doltlite.md#rescue-commits-on-every-rust-side-open).
Anything else is a bug.

## Incrementality

Re-running a sync immediately after a successful one is expected
to be cheap. Two layers do the work:

  - **Provider-side dedup**: every UPSERT uses the upstream
    identifier as the PK ([port guide §1](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#1-primary-keys-are-upstream-identifiers))
    with `ON CONFLICT(id) DO UPDATE` so unchanged rows are
    no-op writes — `dolt diff` reports an empty changeset and
    `commit_run` returns `Ok(None)`. The trailing orchestrator
    commit is then skipped (no-op), so `dolt_log` stays clean.
  - **Translate-side dedup**: the sidecar carries a
    `source_fingerprint`; if the existing `.md` already matches,
    the write is skipped ([etl README §Incrementality](../frankweiler/backend/etl/README.md#incrementality)).

A "second run, no upstream changes" should walk every page,
emit zero writes, and leave `dolt_log` unchanged.

## Blobs and the CAS split

Attachment bytes are split out of the entity database into a sibling
content-addressable store: each source has both
`<data_root>/raw/<name>.doltlite_db` (entities + per-source attachment
metadata in `blob_refs`) and `<data_root>/raw/<name>.blobs.doltlite_db`
(a single `cas_objects` table keyed by blake3 hash). Full schema +
helper inventory in [`backend/etl/DOLTLITE_RAW_PORT_GUIDE.md` §7](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#7-blobs).

Two reasons the split matters operationally:

  - `dolt diff` over the entity db stays small and human-grep-able.
    A re-fetch that picks up one new attachment doesn't drown the
    commit in a many-MB BLOB row.
  - The CAS file is byte-addressed, so re-fetching identical bytes
    from upstream is a no-op via `INSERT OR IGNORE`. Intra-source
    dedupe is automatic; cross-source dedupe is one config change
    away (single-writer caveat applies — see the
    [port guide §7](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#7-blobs)).

### Reset semantics

`RawDb::reset()` (driven by `--reset-and-redownload`) currently wipes:

  - every entity table + its `_bookkeeping` sidecar,
  - `blob_refs` + `blob_refs_bookkeeping`.

It does **not** touch the sibling `cas_objects` file. That means a
reset preserves the bytes on disk but loses every per-source ref's
blake3 pointer into them.

The user-facing consequence: on the next extract, every attachment is
re-fetched from upstream. The bytes hash to the same blake3 as before,
`INSERT OR IGNORE` into `cas_objects` is a no-op, and the new
`blob_refs` rows point back at the existing CAS entries. No disk grows;
the cost is purely network IO and time.

This is the right behavior when you genuinely don't trust the prior
fetch (corrupted bytes, deliberately rotated upstream content), and the
wrong behavior when you reset purely to check entity completeness
(messages potentially missed by incremental sync). The fix when that
distinction matters is a second flag — something like
`--reset-blobs` — that wipes `blob_refs` while
`--reset-and-redownload` keeps it. With `blob_refs` intact, the
skip-check `blob_cas::ref_has_hash()` returns true for every
already-fetched blob and the re-extract emits zero network IO for
attachments. Missing-from-the-prior-pass blobs still get picked up
because the extract walks every entity and pre-seeds + fetches any
ref it doesn't already know about. Not implemented yet; documented
here so the next person hitting it knows it's a known choice rather
than a bug.

`cas_objects` itself has no reset path. Bytes are byte-stable; the
only legitimate way to remove them is the
`blob_cas::gc_orphans()` sweep, which deletes hashes no
`blob_refs` row points at across every source.

### Why contacts doesn't participate

Contacts' photo bytes arrive inline in the vCard payload as base64,
decoded once at parse time into `ContactPhoto { bytes, content_type }`.
They're written straight to `blobs/<uid>.<ext>` at render. They never
touch `blob_refs` or `cas_objects` because there's no separate fetch,
no separate upstream id, and no retry/skip-check semantics needed —
the bytes are a property of the entity, not a separate resource.

If a future provider has the same "bytes-arrive-with-entity" shape,
inline-in-payload is fine; the shared CAS exists for the
fetch-as-separate-resource pattern.

## Cursor / resume strategy

Two patterns in the tree, picked by the upstream API's shape:

  - **Forward-walk + refresh window** (slack, anthropic, chatgpt,
    github, gitlab): resume from `max(ts)` of previously-recorded
    items; also re-query the trailing `refresh_window_days` to
    catch edits / late-arriving items. Dedup collapses the overlap
    to zero writes.
  - **Time-windowed sampling** (yolink): walk `[start, now]` in
    fixed-stride windows. The cursor advances by an exact stride
    every iteration, so windows align across runs and devices.
    Per-window UPSERT dedups re-fetched samples.

No checkpoint files. The dedup index *is* the resume cursor.

## Credentials

Two patterns:

  - **Most providers**: shell out to `latchkey curl` (see
    [`backend/etl/src/latchkey.rs`](../frankweiler/backend/etl/src/latchkey.rs)).
    Auth lives in the latchkey keyring, indexed by URL host. The
    provider's HTTP transport never sees the bearer token.
  - **Yolink**: latchkey doesn't know about `us.yosmart.com`, and
    the consumer download path isn't bearer-authed in the first
    place — the URL itself is signed (see
    [`providers/yolink/src/extract.rs`](../frankweiler/backend/etl/providers/yolink/src/extract.rs)
    `build_signed_url`). Per-device secrets live in config
    (REDACT before publishing).

If you add a new provider with a new auth shape, prefer extending
latchkey upstream before adding a third pattern here.

## Error handling

Two-axis distinction the existing providers all follow:

  - **Per-item failures are tolerated.** A 4xx on one window / page
    / blob should not kill the run. Log a `warn!`, increment an
    error counter, advance the cursor, and keep going. The run's
    `FetchSummary` reports `errors=N` and the orchestrator's
    status line picks it up.
  - **Auth failures and consecutive-failure budgets are fatal.**
    A 401 / 403 from the auth provider, or N back-to-back
    per-item failures on the same source, should return `Err`
    from `fetch(...)`. That cancels the trailing commit, leaves
    the working tree to be rescued on the next open, and surfaces
    a non-zero exit through the sync binary.

The yolink provider's `CONSECUTIVE_FAILURE_BUDGET = 30` is a
template for the second pattern.
