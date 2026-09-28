# Data architecture: ingestion — practices and open questions

Companion to
[`data_architecture_ingestion.md`](data_architecture_ingestion.md), which
covers the principles and at-rest shape of the download stage. This one
is the practitioner's side: how we test, how to add a provider, how the
schema evolves, and the open questions.

For the stage *after* download, see
[`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md).

## Testing with TNG fixtures

We try to have test coverage for as much of the ETL code as possible
using **checked-in, fictional Star Trek: TNG data sets** as fixtures.
The fixtures supply data with the same wire-format shape as real
upstream APIs, but no real user data, so they can live in the repo and
be the source-of-truth for "what does this provider's payload look
like."

Each provider crate owns its own `tests/fixtures/` tree. Build and test
through bazel:

```bash
bazelisk test //...                                        # everything
bazelisk test //datalib/backend/etl/providers/<name>/...  # one provider
```

That runs the unit tests and the fixture-backed integration tests. A
provider's live tests, which talk to the real service through latchkey,
are the `live` module of the same test binary, skipped there and run by
hand with `bazel run //…/<name>:<name>_live`. The **live-golden e2e**
(`//datalib/backend/dag:manual_e2e_live_sync_golden`) runs the whole
pipeline against live upstreams and is the only test that catches
render-side drift against real payloads. Both are in
[`testing.md`](testing.md) (§"The `live` module", §"Manual e2e live-sync
golden").

## Adding new sources is meant to be easy

A new provider is three sibling crates under
[`datalib/backend/etl/providers/`](../../datalib/backend/etl/providers/):
`<name>` (download), `<name>_render` and `<name>_config`.

### Pick a template to copy from

Reach for the simplest existing provider that's shaped like yours,
*not* the most feature-complete one. In rough order of "simple first":

  1. **`signal`** — Backup-file
     ingestion shape (no auth, no live API, no token refresh, no rate-limit
     dance), so the auth and resume machinery you'd need to understand
     for live providers stays out of the way while you learn the
     download / render / store shape.
  2. **`claude`** (Claude) — first choice if your provider *is* a
     live API. Single-account, simple bearer auth via latchkey, a
     listing-diff walk. Most of the "what does download / render /
     blob-CAS look like for an API-backed provider" is here without
     the multi-workspace / multi-channel complexity of chat.
  3. **`slack`** — The most elaborate provider: multiple
     entity tables (channels, users, messages, replies, files), JSONL
     event streams in synth, workspace-wide redaction in live-golden,
     thread-level render buckets. Copy from here only if you
     genuinely need its shape; otherwise it'll drag in complexity you
     don't want.

### The recipe

1. Copy your chosen template into `providers/<name>/` (and its
   `_render` and `_config` siblings), then strip out the
   provider-specific code.
2. Rename the targets in each `BUILD.bazel`: `datalib_etl_<name>`,
   `datalib_etl_<name>_render`, `datalib_etl_<name>_config`. A crate
   needs a `Cargo.toml` (and a line in `datalib/backend/Cargo.toml`'s
   `members`) only if something outside bazel has to see it —
   AGENTS.md §"Git: prefer merges over rebases" says why;
   `calendar` and `calendar_render` have none.
3. Implement `ingest::fetch(...)` and the render side. The render side
   hands each finished document to `ctx.emit_doc` as a
   [`RenderedMarkdown`](../../datalib/backend/etl/render/src/grid_index.rs);
   the render step writes it into that source's store. In `fetch`,
   read `opts.control.stop` before starting each unit of work, and
   write every claim of completeness — cursor, state token, scope
   config — under one end-of-run predicate, never at the point the
   value became available:
   [A claim of completeness is written only by a walk that completed](data_architecture_ingestion.md#a-claim-of-completeness-is-written-only-by-a-walk-that-completed).
4. Drop sample wire-format data into `providers/<name>/tests/fixtures/`
   (TNG cast — see [Testing with TNG fixtures](#testing-with-tng-fixtures)) and write integration tests next to it.
5. Wire the provider's `processor.rs` (`plan_ingest` / `plan_render`)
   into the per-type dispatch in
   [`datalib_step/src/dispatch.rs`](../../datalib/backend/datalib_step/src/dispatch.rs),
   which is what the running pipeline reads — and what
   `config_examples_test.rs` beside it plans every documented example
   through, so a source added to `all_sources.toml` is checked against
   its real schema with no further registration.
   The same wiring asks the config crate which of its params tables
   are ways in, and whether each reaches a live service or reads files
   on disk: `impl IngestMethods for <Name>Config` in
   `providers/<name>_config`. An `ingest` step holding none of them is
   refused, the Manage row reads "Download" or "Import" from it, and
   `bazel run //datalib/backend/datalib_step:ingest_methods.update`
   regenerates the UI's copy.
6. Write `providers/<name>/INGEST.md`, and
   `providers/<name>_render/TRANSLATE.md` too if the provider has a
   render side. See
   [Every provider documents itself, in the same place](#every-provider-documents-itself-in-the-same-place).

The grid index needs no per-provider change: the `grid_index` step
(`build_grid_index` in `etl/render/src/grid_index.rs`) picks up the new
source's render store on its next run.

### Key a table for what one run writes together

A doltlite table is a tree sorted by primary key, and a write rewrites
every leaf page its keys fall in — the changed rows and their unchanged
neighbours together. So the cost of a run is not how many rows it
writes but **how many leaves those rows are spread across**, and that
is decided by the key. Rows that arrive together and sort together
touch a leaf or two; rows that arrive together and sort randomly touch
one leaf each. The measurement is
[`etl/README.md` § "What a write costs"](../../datalib/backend/etl/README.md#what-a-write-costs-the-transaction-is-the-unit-and-the-key-decides-the-size).

The raw-store rule stands — the PK is the upstream id, whatever shape
it has — so this is about the tables whose key is ours. Where you get
to choose, put the coordinate that grows first:

- **Time series**: `(device_id, ts_ms)` as a composite PK
  (`airvisual_samples`), or the same as text, `"{device}#{ts_ms}#{metric}"`
  (`yolink_readings`), `"{metric}#{calendar_date}"` (`garmin_daily`).
  A sync's new samples for a device are then the largest keys in that
  device's run and land at its edge. A 13-digit millisecond stamp sorts
  lexically as a number until 2286, and `#` sorts below every digit and
  letter, so the text form needs no padding.
- **Minted uuids**: `datalib_id::entity_id` puts the record's own
  `created_at` in the leading 48 bits, so a message's row sorts by
  when it was sent and a sync's new rows are adjacent. Pass the stamp
  wherever the record has one of its own; what carries none, and why,
  is in [`entity_ids.md`](entity_ids.md#the-layout-the-stamp-first-then-the-hash).
  Raw stores keep their upstream keys; where that key leads with a
  time (slack's `{team}#{channel}#{ts}`) they have the property too.

Two things this does *not* ask for. Don't sort rows before inserting:
within one transaction the tree is built once, whichever order the
statements ran in, and every store here batches a run into a few
transactions. And don't touch the SQLite mirrors: a truncate-and-refill
inside one transaction produces the same tree as before when the
source has not changed, and the commit stores nothing new.

### Every provider documents itself, in the same place

A provider's documentation lives beside its code, under a name that is
the same for every provider: **`INGEST.md`** in the download crate,
and **`TRANSLATE.md`** in the `_render` crate beside the code it
describes. That consistency is the whole
point — it is what lets a reader (or an agent) find a provider's docs by
convention instead of by searching, so nothing has to maintain an index
of them and no provider gets forgotten by one.

Cover at least these, because they are the questions people actually
arrive with:

- **Where the data comes from**, and what auth it needs.
- **What one run does**, and what a *second* run costs — the
  incrementality story, including what makes a record look changed.
- **The store's shape**: the tables written, and **what keys each one**.
  Say it even when the answer is "the source's own key", and say it
  especially when some tables end up keyless — an unexplained keyless
  table reads as a bug to the next person, and has.
- **What the provider deliberately does not do**, and the known gaps.
- **How to inspect the result** — real `datalib-doltlite` queries against
  the store, not a description of them.

Prefer measurements over adjectives, and say which input you measured on
so the next person can reproduce the number rather than wonder whether
it went stale.

Not every provider meets this yet: `contacts`, `linkedin`, `perseus`,
`signal` and `sms_backup_restore` have no `INGEST.md`, and only a few
render crates have a `TRANSLATE.md`. `ls
datalib/backend/etl/providers/*/*.md` is the current state. Adding one
to a provider you are already working in is a welcome thing to do.

### Worked examples beyond the chat shape

The framework has stretched in a few directions; these are useful
references when your provider doesn't look like chat:

  - **yolink** — time-windowed sampling, signed-URL auth, time-series
    data shape.
  - **perseus** — the corpus (Perseus Digital Library TEI editions) is
    *immutable upstream*, so perseus deliberately doesn't use the
    incremental-fetch / cursor / refresh-window machinery. It uses the
    framework for the typed `GridRow` schema coupling, the unified
    `datalib-dag` pipeline UX, the obs/progress contract, and the
    bazel test rig. A useful reminder that the framework is valuable
    for more than just incremental delta-fetching.

## Schema evolution

The principle: **our schema is allowed to evolve, and an evolution
should never strand existing user data.** A new column on a raw entity
table, a new entity table, a new `GridRow` field, a new
`RENDER_VERSION` — all of these should be deployable to a user who has
months of accumulated data, without asking them to refetch from
upstream.

Two halves to this:

  - **Our internal schema** — the typed columns on raw entity tables,
    the `*_bookkeeping` sidecars, the per-provider CAS edge tables,
    the render store's tables and `GridRow`. How each kind of change
    lands is [`etl/README.md`](../../datalib/backend/etl/README.md)
    §"Schema self-healing" and §"The migration ladder": an additive
    change to a raw store lands by `ADD COLUMN` on the next open, with
    rows and cursors kept; anything else refuses the open until the
    provider declares a rung on its migration ladder, or the user
    resets the store with `datalib-dag --reset` (fine for a live API,
    costly or impossible for a one-shot import whose upstream is gone,
    which is why the ladder exists). A derived store — a render store,
    the grid index — is rebuilt from its input, since every row in it
    is a function of another store; a projection change is a
    `RENDER_VERSION` bump and a re-render, never a refetch.

    When the new "column" is derivable from the payload, which is most
    of them, it is a `VIRTUAL` generated column or an expression index
    rather than a stored column, and lands on existing rows with no
    refetch: [Events vs bookkeeping](data_architecture_ingestion.md#events-vs-bookkeeping-where-each-column-lives)
    and the paragraph before it.

  - **Upstream schema drift** — Slack adds a field, Notion changes a
    block type, GitHub renames `merged_by`. Because we preserve raw
    payloads verbatim (see [Wire-fidelity of the raw store](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)), the new bytes are captured for free —
    a render-side bug is the worst case, never data loss. The
    principle: **upstream change should fail loudly at render
    time, not silently at download time.** No automated drift detector
    exists; see [Detecting upstream shape drift](#detecting-upstream-shape-drift).

## Render and downstream stages, and shared schemas

Both are in
[`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md)
— the render-store contract in its §2, the `GridRow` families in its
§3, incrementality in its §5.

## Unresolved questions

Gaps in the principles: places they aren't yet articulated, aren't yet
verified to be true in code, or haven't been decided. Each is listed as
a desired principle where we know what we want, and as an open question
where we don't.

### Backup, restore, and portability

**Desired principle**: the data root is a self-contained, portable
artifact. `cp -r <data_root>` (or `rsync`) on one machine and dropping
it on another should reconstitute the system byte-for-byte, with no
re-fetch, re-render, or re-index step needed.

### Removing a source

**Desired principle**: removing a source's group from the config should
leave the system clean. A single GC pass should reclaim the source's
raw store, its blob CAS, its `<group>/render_markdown/` tree, and its
`grid_rows` rows — without disturbing other sources.

**Open**: nothing does this, and we haven't decided what it should
mean. There is no GC at all — not for the blob side either. If a user
removes Slack from their config, what is the expected sequence of
operations, and what reclaims the CAS bytes no edge table points at any
more?

### Multi-account / multi-instance within a provider type

**Desired principle**: the framework supports N instances of the same
provider type (two Slack workspaces, three GitHub orgs, two ChatGPT
accounts) by virtue of each being its own group with its own id, and so
its own `<group>/` tree. `grid_rows.source_id` and `GridRow.account`
keep their rows apart in the index.

**Open**: this should be documented as a first-class case, not an
incidental side effect of "each group gets its own raw store." Are
there shared-secret or shared-state pitfalls that bite when you have
two instances of one provider type? Latchkey is keyed by URL host,
which collapses two GitHub orgs to one credential slot — is that the
right shape?

### Observability and the privacy boundary

**Desired principle**: observability (logs, NDJSON events, OTLP
spans) carries timing, counters, stable IDs, and error metadata only.
**No item *contents***. A user shipping spans to a Tempo/Jaeger
collector outside their laptop must not thereby leak Slack DM text,
Signal message bodies, or email contents.

**Open**: this isn't verified. The `--otlp-endpoint` flag is documented but
the data-stays-local guarantee is not extended to it. We should audit what
`tracing` spans actually carry, redact at the source, and state the rule
explicitly.

### Detecting upstream shape drift

**Desired principle**: when an upstream changes the shape of its
responses (new field, removed field, renamed field, type change), we
detect it as part of a sync run and surface it to the user with
enough context to decide whether to ignore, file a bug, or block
further syncs.

**Open**: not implemented, and we don't know yet what we want.

### Quantitative bound on "fast incremental"

**Desired principle**: a second sync run immediately after a
successful one, with no upstream changes, completes in time bounded
by *upstream API walk time*, not by local work. Concretely: tens of
seconds for a small source, low single-digit minutes for a large one
— never tens of minutes, never re-doing the first-sync cost.

**Open**: we don't measure this. We should add a mechanism to roughly compute "sync time / size of sync delta" on each sync for each provider, so that we can get a handle on where the slowness is.

### Fixture hygiene

**Desired principle**: AGENTS.md §"Real data stays out of the repo".
TNG is the cover story — Picard, Riker, Worf, Enterprise stardates. A
shape learned from a real root is rebuilt in TNG data; the capture
itself stays out of git.

**Open**: how is this enforced? The live golden keeps Slack's
workspace-wide listings out of its snapshots (`SKIP_PATH_SEGMENTS`), but there is no
project-wide pre-commit check for "looks like real data." A regex over
names / emails / domains / known channel patterns is the obvious
low-cost mitigation.

### The fixtures → playback → doltlite chain

**Desired principle**: the artifact a human edits and reviews in PRs
is always JSON/JSONL — diffable, language-agnostic, no doltlite
version skew. The doltlite db is always a *produced* artifact, never
a checked-in input. The flow is: synth reads JSONL → emits HTTP
playback responses (`DATALIB_HTTP_PLAYBACK`) → download reads playback
→ writes the runtime `.doltlite_db`. This is the invariant's only
statement.

## Deferred work

  - **VIRTUAL column projection from JSONB payload.** Each
    `WirePayloadRow`-derived row stores a small set of denormalized
    columns alongside the payload for cheap predicate queries (`name`,
    `update_time`, `is_member`, etc.). These are candidates for
    `VIRTUAL` generated columns over `payload->>'$.x'` expressions,
    paired with expression indexes. The denormalization stays
    queryable; the write cost drops to zero and drift-vs-payload
    becomes impossible by construction. The `WirePayloadRow` macro
    would need a per-field attribute like
    `#[wire_payload_row(virtual = "$.profile.real_name")]`. The FIXMEs
    in `slack/src/ingest/schema_raw.rs` flag the columns that would
    convert cleanly.

  - **Hand-rolled `BulkUpsertable` impls.** The `RawTable` derive
    covers payload-less tables ([`etl/macros/README.md`](../../datalib/backend/etl/macros/README.md)),
    but 22 provider tables still hand-roll the impl (fsindex's, media's,
    yolink's devices, slack's `RepliesPagesRow`, …). Moving each to the
    derive collapses it to the struct definition.
