# Data architecture: ingestion — practices and open questions

Companion to
[`data_architecture_ingestion.md`](data_architecture_ingestion.md), which
covers the load-bearing principles and at-rest shape of the extract stage.
This document collects the practitioner-facing material: how we test, how to
add a provider, how the schema is allowed to evolve, the downstream contract
extract has to honor, and the open questions we haven't resolved yet.

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
bazelisk test //frankweiler/backend/etl/providers/<name>/...  # one provider
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

The TNG fixtures catch code-level regressions; the **live-golden
e2e** catches what happens against the actual world. The target is
[`//frankweiler/backend/sync:manual_e2e_live_sync_golden`](../../frankweiler/backend/sync/tests/manual_e2e_live_sync_golden.rs)
(tagged `manual` + `external` + `no-sandbox`; runs the full sync
pipeline, every source, against live upstreams using host-side
latchkey credentials). Its config, file-based source data, and golden
snapshots all live OUTSIDE this repo — in the private dir named by
`$FRANKWEILER_MANUAL_E2E_DIR` (so the slightly sensitive source data
isn't shared when the repo is open-sourced); the `run.sh` there sets
the env var and invokes the test. It snapshots three things into
`$FRANKWEILER_MANUAL_E2E_DIR/snapshots/`:

  - `sync_summary.snap` — the per-source `FetchSummary` JSON the
    orchestrator emits at end of run.
  - `manifest.snap` — the file-tree manifest of `raw/` +
    `rendered_md/`. Catches additions or removals of entire files
    without having to diff every per-file snapshot.
  - Per-file snapshots of every `.doltlite_db`, every rendered `.md`,
    every `.grid_rows.json` sidecar, and every blob materialized
    under `blobs/`. The doltlite-db snaps are byte-count summaries
    (always change on re-fetch); the rest are content snapshots.

Why this is uniquely useful:

  - It's the only test that catches **render-side drift against real
    payloads** — upstream shape changes, schema-projection bugs,
    timestamp-fabrication bugs, attachment-handling gaps (e.g.
    attachment slots that extract isn't walking), etc.
  - The diff is human-readable. `git diff
    frankweiler/backend/sync/tests/snapshots/` after an update is the
    same shape as a code review.
  - After any change to extract schema, translate render, or the
    sidecar contract, refresh the goldens with:
    ```sh
    bazel run //frankweiler/backend/sync:manual_e2e_live_sync_golden.update
    ```
    and review the diff before committing. Treat each cluster of
    changes as a finding: deliberate (commit), accidental
    (investigate), or noise (block on a fix).

The trade-off is that the goldens necessarily capture **real user
data** — they live inside private workspaces. Acceptable here
because the data root is single-user / single-laptop and the
snapshots stay in a private repo; see the
[fixture-hygiene unresolved question](#fixture-hygiene) for the
public-facing version of this problem.

## Adding new sources is meant to be easy

A new provider is a sibling crate under
[`frankweiler/backend/etl/providers/`](../../frankweiler/backend/etl/providers/),
named `frankweiler-etl-<name>`.

### Pick a template to copy from

Reach for the simplest existing provider that's shaped like yours,
*not* the most feature-complete one. In rough order of "simple first":

  1. **`signal`** — Backup-file
     ingestion shape (no auth, no live API, no token refresh, no rate-limit
     dance), so the auth and resume machinery you'd need to understand
     for live providers stays out of the way while you learn the
     extract / translate / sidecar shape.
  2. **`anthropic`** (Claude) — first choice if your provider *is* a
     live API. Single-account, simple bearer auth via latchkey, clean
     forward-walk cursor. Most of the "what does extract / translate /
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
2. Rename the package in its `Cargo.toml` to `frankweiler-etl-<name>`,
   lib name `frankweiler_etl_<name>`.
3. Add `etl/providers/<name>` to the workspace `members =` list in
   `frankweiler/backend/Cargo.toml` and to the `crate.from_cargo`
   manifest list in `MODULE.bazel`.
4. Implement `extract::fetch(...)` (the in-process entry point sync
   calls) and `<name>::translate::...`. The translate side must emit
   `*.grid_rows.json` sidecars matching
   [`Sidecar`](../../frankweiler/backend/etl/src/sidecar.rs).
5. Drop sample wire-format data into `providers/<name>/tests/fixtures/`
   (TNG cast — see [Testing with TNG fixtures](#testing-with-tng-fixtures)) and write integration tests next to it.
6. Add the new source's `type:` discriminator to the `SourceConfig`
   variants in [`backend/core/src/config.rs`](../../frankweiler/backend/core/src/config.rs)
   and wire `extract::fetch(...)` into `sync/src/main.rs`'s per-type
   dispatch.

Load needs no per-provider changes — `grid_rows_load` (in-process)
picks up the new sidecars on its next run.

### Worked examples beyond the chat shape

The framework has stretched in a few directions; these are useful
references when your provider doesn't look like chat:

  - **yolink** — time-windowed sampling, signed-URL auth, time-series
    data shape.
  - **perseus** — the corpus (Perseus Digital Library TEI editions) is
    *immutable upstream*, so perseus deliberately doesn't use the
    incremental-fetch / cursor / refresh-window machinery. It uses the
    framework for the typed `GridRow` schema coupling, the unified
    `bazel run //...:sync` UX, the obs/progress contract, and the
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
    `grid_rows.yaml`, the sidecar `Sidecar` struct, the
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
        re-translate.

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
    a translate-side bug is the worst case, never data loss. The
    principle: **upstream change should fail loudly at translate
    time, not silently at extract time.** No automated drift detector
    exists today; see [Detecting upstream shape drift](#detecting-upstream-shape-drift).

## Translate and downstream stages

After extract, we run translations for display and indexing — render
to markdown with YAML frontmatter, index the markdown with qmd, derive
`grid_rows` for the UI.

The cross-provider contract is the **sidecar**: for every rendered
document, Translate emits two co-located files —

  - `<id>.md` — human-readable, with YAML frontmatter.
  - `<id>.grid_rows.json` — the
    [`Sidecar`](../../frankweiler/backend/etl/src/sidecar.rs):

    ```jsonc
    {
      "header": {
        "document_uuid": "…",       // primary key for the document
        "source_fingerprint": "…",  // hash of upstream payload
        "render_version": 1         // renderer-side schema stamp
      },
      "rows": [GridRow, …]
    }
    ```

Load reads the sidecar tree — **it never re-parses markdown**. The
markdown is for humans; the JSON sidecar is the machine-readable
projection.

This part of the pipeline aspires to the same properties as extract:

  - **Monitorable**: same `obs` flags, same progress-bar contract.
  - **Incremental**: the sidecar `source_fingerprint` short-circuits
    re-render. Load reads `(qmd_path, source_fingerprint)` from
    `markdowns_loaded` and skips unchanged sidecars.
  - **Resumable in the steady state**: a translate pass that gets
    re-run after producing N of M sidecars will skip those N via the
    fingerprint check and continue from where it stopped. We do not,
    however, guarantee crash-mid-write atomicity per file; a partial
    `.md` left by a SIGKILL during a write may have a fingerprint that
    no longer matches the file body and will be regenerated next run.
    That's good enough for our use case but is not a separately
    engineered property.

Less attention has been paid to translate-side observability and to
making partial-progress visible to the user than to the same on
extract; this is an area where the implementation trails the
principle.

## Shared schemas across similar sources

When several sources are shaped similarly enough (a matter of taste,
but largely driven by schema and UI overlap), they should be massaged
into a **shared canonical schema** so the rest of the pipeline (search,
display, threading, attachments, exports) shares code paths and stays
consistent.

Where unification actually happens **today**: the `GridRow` projection
([`schemas/grid_rows.yaml`](../../schemas/grid_rows.yaml), codegen'd into
the Rust struct at `frankweiler/backend/schema/src/generated/grid_rows.rs`).
Every searchable entity from every provider collapses into rows of one
schema with `provider` + `kind` discriminators. The grid backend
reads it with a single query and renders it without knowing which
provider produced any given row.

Unification should **never** happen in the raw store: Slack, Beeper,
Signal, Anthropic, and ChatGPT each have their own raw tables, in their
own doltlite DBs (`slack_messages`, `beeper_messages`, …). Once we
*translate*, though, we aspire to share as much as possible — projecting
raw data into unified schemas where appropriate, then sending that
unified data through common code paths for interpretation, rendering,
and indexing.

Examples where schema and data handling should be unified:

  1. **Chat (human)** — Slack, Beeper, Signal. "Messages in
     channels/DMs between humans with attachments and threading."
     Unified at `GridRow`; per-provider raw + render.
  2. **Chat (LLM)** — Claude, ChatGPT, Gemini (planned). Same chat
     shape but with assistant turns, thinking, and tool-use surfaced.
     Unified at `GridRow` via `kind = 'User Input' | 'LLM Response' |
     'LLM Thinking' | 'Tool Call'`.
  3. **Code review threads** — GitHub PR discussions, GitLab MR
     discussions. Threaded inline comments on diffs. Unified at
     `GridRow`; `git_sha` and `external_id` columns are specifically
     there to serve this family.
  4. **Document-comment threads** — Notion. Very similar in shape to
     (3); may eventually share more than just `GridRow` projection.
  5. **Time-series sensor data** — yolink today; Garmin fitness and
     IQ Air air quality planned. Per-device samples over time with a
     small fixed set of value channels. Not yet projected to
     `GridRow`; this family hasn't picked its shared schema yet.

A new provider that fits a family should at minimum project to the
family's `GridRow` shape rather than inventing a new `kind` taxonomy.
A provider that doesn't fit may motivate a new family; opening one
should be deliberate.

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
its blob CAS contribution, its `rendered_md/<name>/` tree, and its
`grid_rows` rows — without disturbing other sources that share the CAS.

**Open**: today we have `blob_cas::gc_orphans()` for the blob side, but
no top-level "uninstall this source" path. If a user removes Slack
from their config, what is the expected sequence of operations?


### Multi-account / multi-instance within a provider type

**Desired principle**: the framework supports N instances of the same
provider type (two Slack workspaces, three GitHub orgs, two ChatGPT
accounts) by virtue of each having its own `sources:` entry with a
distinct `name:`. `GridRow.account` and the per-account segments in
`rendered_md/<provider>/<account>/...` exist to keep them disjoint.

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

### Translate-side partial-progress visibility

**Desired principle**: a long-running translate pass — first run after
a big initial extract, or a `RENDER_VERSION` bump that invalidates
every sidecar — must be as monitorable and as stoppable-resumable as
extract is. The user sees "rendered 12,347 / 89,201" with an ETA;
^C-then-rerun resumes from 12,347 not 0.

**Open**: the fingerprint-skip *does* give resumability in the steady
state (see [Translate and downstream stages](#translate-and-downstream-stages)), but translate-side progress reporting is less developed
than extract-side. Worth measuring.

### The fixtures → playback → doltlite chain

**Desired principle**: the artifact a human edits and reviews in PRs
is always JSON/JSONL — diffable, language-agnostic, no doltlite
version skew. The doltlite db is always a *produced* artifact, never
a checked-in input. The flow is: synth reads JSONL → emits HTTP
playback responses → extract reads playback → writes the runtime
`.doltlite_db`.

This is stated in [port guide §3](../../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#3-synth-reads-checked-in-fixtures-extract-writes-doltlite),
but it's a project-wide invariant that belongs at the architecture
level too.

### grid_rows itself lives in doltlite

The `grid_rows` table (the projection consumed by the UI) lives in
`<data_root>/backend_index.doltlite_db`, just like raw stores. The "doltlite
is our storage layer" claim should apply to every store the system writes —
raw, blob CAS, and the backend index — not just to raw. Worth saying
explicitly in
[Introduction and Context](data_architecture_ingestion.md#introduction-and-context).

## Deferred work

Edits to these docs and their neighbors that we've agreed to do, but
haven't yet. Each is intentionally not blocking the audit thread —
they're listed here so they don't get lost.

  - **Move `frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md` →
    `docs/dev/doltlite_patterns.md`**, and reframe it from a porting
    guide into "shape of how we use doltlite." The current doc reads
    as one-time migration instructions (which JSONL-tree raw stores
    looked like, the porting checklist, "we tried checking in a
    `.doltlite_db` once and threw it away"); the durable content
    inside it — the design rules, the table-and-blob shape, the
    shared utilities — should be lifted into a stable reference.
  - **Rename `docs/dev/doltlite.md` → `docs/dev/doltlite_tips.md`** to make
    its scope (operational tips and dolt-history reading) explicit
    against the new patterns doc above.
  - Both of the above require updating inbound links across the
    repo: this file, signal's `extract/mod.rs`, each provider's
    `EXTRACT.md` and `DOLTLITE_RAW.md`, the etl crate's module docs,
    and any AGENTS.md / README pointers.

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
    Several FIXMEs in `slack/src/extract/schema_raw.rs` (UserRow,
    ChannelRow, MessageRow) flag the specific columns that would
    convert cleanly.

  - **`BulkUpsertable` derive for non-payload tables.** Several
    provider tables (bookkeeping tables like slack's
    `RepliesPagesRow`) hand-roll the `BulkUpsertable` impl because
    they have no wire payload. The shape is mechanical — a
    `#[derive(BulkUpsertable)]` macro with a per-field column-name
    attribute would collapse each impl to the struct definition.
