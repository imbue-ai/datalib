# Data architecture: ingestion — practices and open questions

Companion to
[`data_architecture_ingestion.md`](data_architecture_ingestion.md), which
covers the load-bearing principles and at-rest shape of the download stage.
This document collects the practitioner-facing material: how we test, how to
add a provider, how the schema is allowed to evolve, the downstream contract
download has to honor, and the open questions we haven't resolved yet.

For the stage *after* download, see
[`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md).

## Testing with TNG fixtures

We try to have test coverage for as much of the ETL code as possible
using **checked-in, fictional Star Trek: TNG data sets** as fixtures.
The fixtures supply data with the same wire-format shape as real
upstream APIs, but no real user data, so they can live in the repo and
be the source-of-truth for "what does this provider's payload look
like."

Each provider crate owns its own `tests/fixtures/` tree. **Build and
test through bazel:**

```bash
bazelisk test //...                                        # everything
bazelisk test //datalib/backend/etl/providers/<name>/...  # one provider
```

Bazel is the only supported build and test driver. It gets caching,
sandboxing, and remote-execution right; raw `cargo build` /
`cargo test` invocations bypass that and risk producing artifacts
that disagree with what CI sees. If your inner loop feels slow,
*fix the bazel target*, don't shell out to cargo.

`bazelisk test //...` runs both the unit tests and the fixture-backed
integration tests — no `manual` tag, no special invocation. The only
tests tagged `manual` are the per-provider `*_live` tests, which hit
real upstream APIs and require latchkey credentials from the host
machine.

### The live-golden e2e test

The TNG fixtures catch code-level regressions; the **live-golden e2e**
catches what happens against the actual world. The
`//datalib/backend/dag:manual_e2e_live_sync_golden` target runs
the full pipeline, every source, against live upstreams using
host-side latchkey credentials, snapshotting a file-tree manifest of
each group's `ingest/` + `render_markdown/` and per-file content
snapshots into a private dir named by `$DATALIB_MANUAL_E2E_DIR`
(kept outside the repo so the slightly sensitive source data isn't
shared when the repo is open-sourced). It is the only test that
catches **render-side drift against real payloads** — upstream shape
changes, schema-projection bugs, timestamp-fabrication bugs,
attachment-handling gaps — with a human-reviewable diff, triaged
per cluster as deliberate / accidental / noise.

It was retired along with the `datalib-sync` crate when the pipeline
moved to the DAG runner, and ported back onto `datalib-dag` afterward.
One thing changed shape in that port, and it is the interesting bit: the
old aggregate `sync_summary_<now>.json` carried per-source counts, but
`datalib-dag`'s `run_summary` NDJSON event deliberately does not —
the orchestrator is storage-agnostic and doesn't know sources persist
to doltlite. The counts live at the correct grain instead, in each
source's own `sync_runs.summary` (`deltas` from `dolt_diff_<table>`,
plus the `sync_scope_state` cursors that moved), which is where the
test now reads them from. See [`/docs/dev/testing.md`](/docs/dev/testing.md).

## Adding new sources is meant to be easy

A new provider is a sibling crate under
[`datalib/backend/etl/providers/`](../../datalib/backend/etl/providers/),
named `datalib-etl-<name>`.

### Pick a template to copy from

Reach for the simplest existing provider that's shaped like yours,
*not* the most feature-complete one. In rough order of "simple first":

  1. **`signal`** — Backup-file
     ingestion shape (no auth, no live API, no token refresh, no rate-limit
     dance), so the auth and resume machinery you'd need to understand
     for live providers stays out of the way while you learn the
     download / render / store shape.
  2. **`claude`** (Claude) — first choice if your provider *is* a
     live API. Single-account, simple bearer auth via latchkey, clean
     forward-walk cursor. Most of the "what does download / render /
     blob-CAS look like for an API-backed provider" is here without
     the multi-workspace / multi-channel complexity of chat.
  3. **`slack`** — The most elaborate provider: multiple
     entity tables (channels, users, messages, replies, files), JSONL
     event streams in synth, workspace-wide redaction in live-golden,
     thread-aware `source_fingerprint`. Copy from here only if you
     genuinely need its shape; otherwise it'll drag in complexity you
     don't want.

### The recipe

1. Copy your chosen template into `providers/<name>/`, then strip out
   the provider-specific code.
2. Rename the package in its `Cargo.toml` to `datalib-etl-<name>`,
   lib name `datalib_etl_<name>`.
3. Add `etl/providers/<name>` to the workspace `members =` list in
   `datalib/backend/Cargo.toml` and to the `crate.from_cargo`
   manifest list in `MODULE.bazel`.
4. Implement `ingest::fetch(...)` and `<name>::render::...`. The
   render side hands each finished document to `ctx.emit_doc` as a
   [`RenderedMarkdown`](../../datalib/backend/etl/render/src/grid_index.rs);
   the render step writes it into that source's store.
5. Drop sample wire-format data into `providers/<name>/tests/fixtures/`
   (TNG cast — see [Testing with TNG fixtures](#testing-with-tng-fixtures)) and write integration tests next to it.
6. Wire the provider's `processor.rs` (`plan_ingest` / `plan_render`)
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

7. Write `providers/<name>/INGEST.md`, and
   `providers/<name>_render/TRANSLATE.md` too if the provider has a
   render side. See
   [Every provider documents itself, in the same place](#every-provider-documents-itself-in-the-same-place).

Grid index needs no per-provider changes — the `grid_index` step
(the `grid_index` step, `build_grid_index` in
`etl/render/src/grid_index.rs`) picks up the new source's store on its next
run.

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

This is the rule for new providers, not yet a description of all of
them: several of the earlier ones ship no `INGEST.md`, and the render
side is documented more thinly than the download side across the board.
`ls datalib/backend/etl/providers/*/*.md` is the current state. Adding
one to a provider you are already working in is a welcome thing to do.

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

The principle we aspire to: **our schema is allowed to evolve, and an
evolution should never strand existing user data.** A new column on a
raw entity table, a new entity table, a new `GridRow` field, a new
fingerprint input, a new `RENDER_VERSION` — all of these should be
deployable to a user who has months of accumulated data, without
asking them to refetch from upstream.

Two halves to this:

  - **Our internal schema** — the typed columns on raw entity tables,
    the `GridRow` struct, the render store's tables, the
    `*_bookkeeping` sidecar tables, the per-provider CAS edge
    tables. Today's de facto answer to "I added a column" is
    `--reset-and-redownload`. That
    works for *rebakeable* sources (anything we can refetch from a
    live API) but breaks down for:
      - one-shot imports (Signal backup, archive ingestion) where
        the upstream is no longer reachable;
      - sources whose first sync is expensive enough in time / API
        quota / bandwidth that a refetch is genuinely costly;
      - changes to the projection layer (`grid_rows`) where the
        source-of-truth (raw) is fine but the projection is stale —
        these *shouldn't* require an upstream refetch, just a
        re-render.

    The principle we want: **additive schema changes (new columns,
    new tables, new fields) are no-downtime, no-refetch.**
    Subtractive changes (renames, removals, type changes) get an
    explicit, named migration step. We aren't there yet.

    The pattern that gets us closest, today: when the new "column"
    is derivable from the payload (which is most of them — see
    [Events vs bookkeeping](data_architecture_ingestion.md#events-vs-bookkeeping-where-each-column-lives)),
    add it as a `VIRTUAL` generated column over `payload->>'$.path'`
    plus an index, or as a bare expression index. Both work in
    DoltLite v0.11.9, both produce COVERING index plans, and
    `ALTER TABLE ADD COLUMN … VIRTUAL` applies to existing rows
    with no refetch and no payload rewrite. Reserve real stored
    columns for the small set of writer-supplied fields that
    genuinely aren't in the payload (synthesized PKs, FKs, namespace
    discriminators).

  - **Upstream schema drift** — Slack adds a field, Notion changes a
    block type, GitHub renames `merged_by`. Because we preserve raw
    payloads verbatim (see [Wire-fidelity of the raw store](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store)), the new bytes are captured for free —
    a render-side bug is the worst case, never data loss. The
    principle: **upstream change should fail loudly at render
    time, not silently at download time.** No automated drift detector
    exists today; see [Detecting upstream shape drift](#detecting-upstream-shape-drift).

## Render and downstream stages, and shared schemas

Both moved to
[`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md)
— the render-store contract and the aspired-to properties of the render
stage in its §2 and §5, the `GridRow` family taxonomy in its §3.

## Unresolved questions

These are gaps we noticed while writing the architecture doc — places
the principles either aren't yet articulated, aren't yet verified to be
true in code, or genuinely haven't been decided. They're listed here
as desired principles where we know what we want, and as open
questions where we don't.

### Backup, restore, and portability

**Desired principle**: the data root is a self-contained, portable
artifact. `cp -r <data_root>` (or `rsync`) on one machine and dropping
it on another should reconstitute the system byte-for-byte, with no
re-fetch, re-render, or re-index step needed.

### Removing a source

Note: This is not yet handled in a meaningful way.  We haven't decided yet what it should mean.

**Desired principle**: removing a `sources:` entry should leave the
system clean. A single GC pass should reclaim the source's raw store,
its blob CAS contribution, its `<name>/render_markdown/` tree, and its
`grid_rows` rows — without disturbing other sources that share the CAS.

**Open**: there is no GC at all today — not for the blob side either.
`blob_cas::gc_orphans()` was removed uncalled in `7f588ba1` and this
paragraph kept citing it as if it shipped. So the question is wider
than it looked: if a user removes Slack from their config, what is the
expected sequence of operations, and what reclaims the CAS bytes no
edge table points at any more?


### Multi-account / multi-instance within a provider type

**Desired principle**: the framework supports N instances of the same
provider type (two Slack workspaces, three GitHub orgs, two ChatGPT
accounts) by virtue of each having its own `sources:` entry with a
distinct `name:`. `GridRow.account` and the per-account segments in
`<stanza>/render_markdown/<account>/...` exist to keep them disjoint.

**Open**: this should be documented as a first-class case, not an
incidental side effect of "each `name:` gets its own raw store." Are
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

**Open**: not implemented today, and we don't know yet what we want.
A previous attempt (`endpoint_shapes`) was deleted; see commit history.

### Quantitative bound on "fast incremental"

**Desired principle**: a second sync run immediately after a
successful one, with no upstream changes, completes in time bounded
by *upstream API walk time*, not by local work. Concretely: tens of
seconds for a small source, low single-digit minutes for a large one
— never tens of minutes, never re-doing the first-sync cost.

**Open**: we don't currently measure this. We should add a mechanism to roughly compute "sync time / size of sync delta" on each sync for each provider, so that we can get a handle on where the slowness is.

### Fixture hygiene

**Desired principle**: no real user data, ever, in any checked-in
fixture or any insta snapshot. TNG is the cover story — Picard,
Riker, Worf, Enterprise stardates, etc. Live-golden snapshots that
capture real workspace data must be redacted before they land in git.

**Open**: how is this enforced? There's a `SKIP_PATH_SEGMENTS`
convention for the Slack live golden but no project-wide pre-commit
check for "looks like real data." A regex over names / emails /
domains / known channel patterns is the obvious low-cost mitigation.

### Render-side partial-progress visibility

Moved to
[`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md#5-incrementality-and-progress).

### The fixtures → playback → doltlite chain

**Desired principle**: the artifact a human edits and reviews in PRs
is always JSON/JSONL — diffable, language-agnostic, no doltlite
version skew. The doltlite db is always a *produced* artifact, never
a checked-in input. The flow is: synth reads JSONL → emits HTTP
playback responses → download reads playback → writes the runtime
`.doltlite_db`.

It is a project-wide invariant and this is now its only statement of
record: it used to be duplicated in `DOLTLITE_RAW_PORT_GUIDE.md`,
deleted 2026-09-03 (see [Deferred work](#deferred-work)).

### grid_rows itself lives in doltlite

The `grid_rows` table (the projection consumed by the UI) lives in
`<data_root>/unified_index/grid_index/db.doltlite_db`, just like raw stores. The "doltlite
is our storage layer" claim should apply to every store the system writes —
raw, blob CAS, and the backend index — not just to raw. Worth saying
explicitly in
[Introduction and Context](data_architecture_ingestion.md#introduction-and-context).

## Deferred work

Edits to these docs and their neighbors that we've agreed to do, but
haven't yet. Each is intentionally not blocking the audit thread —
they're listed here so they don't get lost.

  - ~~**Move `DOLTLITE_RAW_PORT_GUIDE.md` → `docs/dev/doltlite_patterns.md`**
    and reframe it as "shape of how we use doltlite."~~ **Done
    differently: deleted, 2026-09-03.** By the time anyone got to it,
    the durable content had been written down elsewhere and what
    remained was wrong — a checklist naming the retired
    `datalib/backend/sync` crate, `src/extract/` module paths that
    became `src/ingest/`, the retired `RefStub` / `pre_seed_ref`
    blob API in its utilities table and code templates, and a
    `journal_mode=DELETE` snippet that contradicts
    `doltlite_raw::open()`, which deliberately does not set the pragma
    because doltlite rejects it. Only §6a survived, inlined into
    [the JSONB paragraph](data_architecture_ingestion.md#schema_rawrs-per-provider-schema-layout).
  - **Rename `docs/dev/doltlite.md` → `docs/dev/doltlite_tips.md`** —
    still open, but the motivation was to disambiguate it against the
    patterns doc that no longer exists, so it is now optional. Its
    scope (operational tips, reading dolt history) is already clear
    from its own opening.

  - **VIRTUAL column projection from JSONB payload.** Each
    `WirePayloadRow`-derived row currently stores a small set of
    denormalized columns alongside the payload for cheap predicate
    queries (`name`, `update_time`, `is_member`, etc.). On DoltLite
    v0.11.9+ these are candidates for `VIRTUAL` generated columns
    over `payload->>'$.x'` expressions, paired with expression
    indexes. The denormalization stays queryable; the write cost
    drops to zero and drift-vs-payload becomes impossible by
    construction. The `WirePayloadRow` macro would need a per-field
    attribute like `#[wire_payload_row(virtual = "$.profile.real_name")]`.
    Several FIXMEs in `slack/src/ingest/schema_raw.rs` (UserRow,
    ChannelRow, MessageRow) flag the specific columns that would
    convert cleanly.

  - **`BulkUpsertable` derive for non-payload tables.** Several
    provider tables (bookkeeping tables like slack's
    `RepliesPagesRow`) hand-roll the `BulkUpsertable` impl because
    they have no wire payload. The shape is mechanical — a
    `#[derive(BulkUpsertable)]` macro with a per-field column-name
    attribute would collapse each impl to the struct definition.
