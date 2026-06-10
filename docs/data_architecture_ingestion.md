# Data architecture: ingestion

This document describes the principles we are striving towards for the
**ingestion (extract) side** of the data layer of this project — how
raw data lands on disk from upstream sources, what shape it has at
rest, and the operational properties (monitorable, stoppable,
resumable, incrementally cheap, verifiable) the extract stage aims
for.

The downstream stages — translate, load, indexing, annotation, and
the UI's consumption of `grid_rows` — are mostly the subject of a
separate document, *Data Architecture: Translation, Indexing,
Annotation, and UI* (not yet written). Where understanding extract
requires referring to a downstream concept (e.g. the sidecar contract
that translate emits, the `GridRow` projection the UI reads), this
document touches on it briefly. The bulk of the downstream-leaning
material that used to live here has been pulled into [Downstream
stages — pointer to the companion doc](#downstream-stages--pointer-to-the-companion-doc)
near the end, pending the split.

It is aspirational as much as descriptive: not every provider or stage
fully honors every principle today, but a new provider, table, or
transformation should be judged against this document, and divergences
should be either justified or fixed.

Pointers to the things that are **not** in this file:

  - The raw store's table-and-blob shape, primary-key rules,
    `sync_runs` bookkeeping: [`backend/etl/DOLTLITE_RAW_PORT_GUIDE.md`](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md).
    (See [Deferred work](#deferred-work) — this doc should move under
    `//docs/` and be reframed away from its porting-guide flavor.)
  - Reading the dolt history of a raw store: [`docs/doltlite.md`](doltlite.md).
    (See [Deferred work](#deferred-work) — this doc should be renamed
    `docs/doltlite_tips.md`.)
  - Per-provider auth, API surface, resume strategy: each provider's
    `EXTRACT.md` (e.g. [`providers/slack/EXTRACT.md`](../frankweiler/backend/etl/providers/slack/EXTRACT.md)).

## Wire-event tape (JSONL)

Doltlite is our primary store, but doltlite is also a binary file you
need a tool to open. So alongside the doltlite raw store, every
extract also writes a **plain-text, append-only JSONL log of what
came off the wire**.

This is the simplest view of the raw data: one event per line, in
the order the extractor saw it. No schema, no migrations — just a
tape you can `tail -f`, `grep`, `jq`, or open in any editor.

The doltlite store is what the stateful, incremental, version-controllable pipeline reads; the JSONL tape is
what a human reads when they want to see what the upstream actually
sent us, with no tooling in the way.

Layout — one directory per source, one file per entity table:

```
<data_root>/raw/<name>/events/
  <table>.jsonl          # one line per upsert
  blob_refs.jsonl
```

Each line is a small JSON object:

```jsonc
{
  "_recorded_at": "2026-06-10T14:22:31.041203-07:00",
  "table": "messages",
  "id": "C0123:1717982351.000200",
  "payload": { ... }     // the wire bytes
}
```

Three rules:

  - **The pipeline never reads it.** Translate, load, resume, retry —
    all of those go through doltlite. The JSONL is a write-only
    mirror. Deleting the `events/` directory does not break anything.
  - **One stream per table, not two.** The Python original had
    `created/` and `updated/` directories; every new row went into
    both. We collapse to a single append: every upsert, every time.
    First-seen is recoverable from it with `awk` if anyone wants it.
  - **Same bytes as the upsert.** We tap right next to the
    `ON CONFLICT(id) DO UPDATE`, so the tape carries the same
    wire-fidelity payload that the doltlite row gets. No second
    parse, no second normalize.

## Shape of the system

We have an ETL-shaped architecture that extracts raw data from many
upstream sources and stores it as **JSON API responses preserved
verbatim** in versioned doltlite tables, with attachment **BLOBs in a
content-addressable store** (also doltlite, but a separate sibling
database per source).

Each entity table has typed columns for the fields we need to query or index (`id`,
`parent_id`, ordering columns, timestamps, foreign keys to other
entities) plus a `payload` column that holds the raw upstream wire
payload. On disk, `payload` is stored as JSONB (SQLite 3.45 binary
JSON, via `jsonb(?)` on write and `json(payload)` on read; see [port
guide §6a](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#6a-jsonb-storage-for-payloads));
in Rust the value round-trips as a text JSON string. JSONB is a storage
encoding. The principle is wire-fidelity (see [Wire-fidelity of the raw store](#wire-fidelity-of-the-raw-store)).

The pipeline has three stages. All three run **in-process inside
`frankweiler-sync`** — one binary, one process, one config-driven
walk over enabled sources. (A standalone-binary-per-stage layout is
something we could go back to if needed; the per-provider library
APIs are still stage-shaped, and there are vestigial
`<provider>_download` rust_binary targets that could be revived.
Today they aren't on the production path.)

| Stage     | Entry point                                                       | Inputs                                  | Outputs                                       |
|-----------|-------------------------------------------------------------------|-----------------------------------------|-----------------------------------------------|
| Extract   | `frankweiler_etl_<provider>::extract::fetch(...)`                 | upstream API                            | `<data_root>/raw/<name>.doltlite_db` + sibling `.blobs.doltlite_db` |
| Translate | `frankweiler_etl_<provider>::translate::...`                      | `<data_root>/raw/<name>.doltlite_db`    | `<data_root>/rendered_md/<provider>/...`      |
| Load      | provider-agnostic `frankweiler_etl::load` (a.k.a. `grid-rows-load`) | `<data_root>/rendered_md/`              | rows in `backend_index.doltlite_db` + `markdowns_loaded` bookkeeping |

  1. **Extract** — pull from upstream, UPSERT into
     `<data_root>/raw/<name>.doltlite_db` (entities) and
     `<data_root>/raw/<name>.blobs.doltlite_db` (a single `cas_objects`
     table keyed by blake3 hash).
  2. **Translate** — derive sidecar `.md` + `.grid_rows.json` under
     `rendered_md/<provider>/...` from the raw store, in-process,
     deterministically.
  3. **Load** — feed the sidecar tree into the canonical `grid_rows`
     table for the UI and for the qmd index.

Each provider is its own crate at
[`frankweiler/backend/etl/providers/<name>/`](../frankweiler/backend/etl/providers/),
named `frankweiler-etl-<name>`. The provider crate owns its Extract +
Translate code, its bins, its integration tests, and the sample
fixtures the tests run against — keeping sample data next to the code
under test serves as documentation of "what this provider's wire
format looks like." Load is provider-agnostic and lives at
[`src/load.rs`](../frankweiler/backend/etl/src/load.rs); a new provider
needs no Load-side changes.

### Per-provider schema layout

Within each provider crate the bytes-at-rest schema is its own
file, deliberately declarations-only:

  - **`providers/<name>/src/extract/schema_raw.rs`** — the
    raw-store schema. DDL constants (one per table / index /
    bookkeeping sidecar), schema-evolution migration constants
    co-located with the table they touch, any synthesized-PK
    recipe functions, and a tiny `full_ddl()` composer that
    splices in `dr::bookkeeping_ddl_for(table)` for each entity.
    **No manipulation code** — `RawDb`, UPSERTs, SELECTs,
    parameter binding stay in `extract/db.rs` and import the
    constants and helpers from `schema_raw`. The convention is
    proto/pydantic-flavored: opening 12 `schema_raw.rs` files at
    the same fixed path is meant to answer "what does the world
    look like at rest?" without opening anything else.
  - **`providers/<name>/src/translate/schema_translate.rs`**
    (aspirational, landing per provider) — the normalized
    representation translate emits. Mostly serde-shaped Rust
    types, not SQL DDL: the in-memory POD form before it's
    shredded into sidecar rows. One provider may have multiple
    `schema_translate_<family>.rs` files when it projects into
    more than one normalized shape. Where a shape is shared
    across providers (chat-human, code-review, time-series, …)
    the canonical type lives in a shared crate and the
    per-provider file re-exports.

The "show me your tables" framing: reading the same two file
paths inside every provider tells you what each one stores and
what it emits, with the wire-fidelity columns, PK recipes, and
denormalized fields visible inline. The plan landing this
convention across the tree is in
[`docs/data_architecture_plan.md`](data_architecture_plan.md)
§P0.1.

### Layering of concerns: extract is downstream-agnostic

The per-stage modules within a provider crate form a strict layer
with a single allowed dependency direction:

```
load   ← translate   ← extract   ← upstream
       (provider-agnostic)
```

  - **`extract`** owns the bytes-at-rest. It knows how to fetch from
    upstream and persist into `<data_root>/raw/<name>.doltlite_db`,
    and nothing else. It must NOT depend on `translate`,
    `render`, `frankweiler_schema::grid_rows::GridRow`, sidecar
    types, or the qmd index. The per-provider `schema_raw.rs`
    rustdoc deliberately avoids describing how translate consumes
    the tables — downstream stages are not extract's concern.
  - **`translate`** depends on `extract` (it reads the raw store
    and projects to the normalized POD + sidecar shape).
    `extract::schema_raw` is part of the contract translate
    consumes.
  - **`load`** is provider-agnostic; it lives at
    [`src/load.rs`](../frankweiler/backend/etl/src/load.rs) in the
    shared `frankweiler_etl` crate and depends on no provider's
    extract or translate. Its input contract is the sidecar tree.

Why the discipline matters:

  - Extract is its own deliverable. A user can run extract, stop,
    inspect the raw store, and have something useful — even if
    translate has bugs or hasn't been written yet.
  - Translate can be re-implemented, replaced, or extended (e.g.
    additional `schema_translate_<family>.rs` projections) without
    touching extract.
  - Disabling a translate path or render version for one provider
    doesn't disturb that provider's extract.

The reverse-direction slip (extract reaching into translate) is the
more common one. We caught a real instance during the
`schema_raw.rs` rollout where slack's `extract/db.rs` was importing
`slack_message_uuid` from `crate::translate` — the UUIDv5 recipe
for the messages table PK had landed on the wrong side of the
layer. Moving it into `extract/schema_raw.rs` reversed the import
direction.

**Aspirationally enforced by bazel target visibility** — extract
targets should declare `visibility = [":__pkg__", "//.../translate:__pkg__"]`
or similar so a translate module accidentally added as an extract
dep fails at the bazel-graph level rather than at link time. Today
this is enforced by code review and the schema_raw convention; we
should land the visibility tightening as a follow-up.

This is not novel. It's the same shape as many Flume / Apache Beam /
Dask / Prefect / Airflow pipelines. What we're optimizing for that
those tools don't, **for a single user on a single laptop**:

* Easy to install
* Easy to configure
* Easy to run
* Easy to monitor

**No cluster, no scheduler service, no DAG server. One config file,
one orchestrator binary, one local data directory.**

## Operational principles

### Monitorable

The first sync from a given source is often very long (hours to days,
many GB). Every stage must surface progress in a way the user can
watch in real time.

  - Every binary flattens [`obs::ObsArgs`](../frankweiler/backend/obs/src/lib.rs)
    into its clap parser, so every stage takes the same logging / OTLP
    / progress-bar flags. On a TTY, pretty log lines on stderr;
    otherwise, NDJSON events on stderr. Log emissions are routed
    through an `IndicatifWriter` that coordinates with the shared
    `MultiProgress` exposed by `frankweiler_obs::shared_multi()` so
    progress bars attached by callers (e.g. sync's per-source bars)
    don't get stomped by log lines.
  - `--otlp-endpoint http://host:4317` exports spans + events via OTLP,
    so a single Tempo/Jaeger collector can ingest every stage. (See
    [the privacy-boundary unresolved question](#observability-and-the-privacy-boundary) for the privacy contract that constrains what may be in those
    spans.)
  - Each stage emits `*_start`, `*_complete`, and per-document
    progress events with a stable provider-prefixed name
    (`slack_download_*`, `grid_rows_load_*`, …). The `*Summary`
    structs are `Serialize`, so a web UI can consume the final stats
    line without knowing which provider produced it.
  - Long-running operations must report something visible at least
    every few seconds; an extract that walks 100k items silently for
    an hour is a bug.

### Stoppable and resumable

A sync that gets interrupted — ^C, OOM, laptop sleep, upstream 5xx —
must be able to make forward progress on the next run. We **do not
require runs to complete to be useful**.

The dedup index *is* the resume cursor:

  - Provider-side dedup keys every UPSERT on the upstream identifier
    (port guide §1), so re-walking already-fetched items is cheap and
    correct.
  - There are no separate checkpoint files. The data we already have
    tells us where to resume.
  - If `doltlite_raw::open` finds a dirty working tree from a prior
    crashed run, it stamps a `rescue:` commit before any DDL (Data
    Definition Language — `CREATE TABLE`, `ALTER TABLE`, etc.; the
    `IF NOT EXISTS` statements every doltlite open runs) — see
    [`docs/doltlite.md`](doltlite.md#rescue-commits-on-every-rust-side-open).

### Efficiently incremental

A second sync run immediately after a successful one should be cheap:
walk what the upstream API forces us to walk, write zero rows, leave
`dolt_log` unchanged.

Two layers do the work:

  - **Provider-side dedup**: every UPSERT uses the upstream identifier
    as PK with `ON CONFLICT(id) DO UPDATE`; unchanged rows are no-op
    writes. `dolt diff` reports an empty changeset and the trailing
    orchestrator commit is skipped.
  - **Translate-side dedup**: the sidecar carries a
    `source_fingerprint`; if the existing `.md` already matches, the
    write is skipped. The Load step honors the same fingerprint in
    `markdowns_loaded`.

Different upstreams expose different surfaces for "what changed since
X", and that drives the cursor pattern (see [Cursor / resume strategy](#cursor--resume-strategy)).

### Wire-fidelity of the raw store

The raw store preserves upstream responses as we received them. Two
load-bearing rules follow:

  - **Normalize at translate time, not extract time.** A lesson learned
    on the anthropic port: we used to pre-normalize the API response
    at fetch time. We don't anymore. The raw `payload` is the upstream
    bytes, so `dolt diff` reflects *actual upstream change* rather than
    churn from our normalizer evolving. Port guide §5 spells this out.
  - **Don't pollute payloads with downloader-synthesized keys.**
    `_fetched_at`, `_listing_update_time` etc. are bookkeeping, not
    upstream data; promote them to real columns on the entity table
    (or its `_bookkeeping` sidecar), not into the JSON.

Corollary: **the raw store is the source of truth; downstream stages
are rebakeable.** Anything we render, project to `grid_rows`, or index
into qmd can be recomputed from raw without re-touching the network.
`RENDER_VERSION` (in each provider's `translate/render.rs`) is the
explicit lever for "force a rebake of all sidecars even when payloads
are unchanged."

### Verifiable via `--reset-and-redownload`

A long chain of incremental syncs can in principle silently drop data
(an upstream that doesn't surface a deletion, a cursor that skipped a
page on a 5xx, a bug in our delta logic). The check is to wipe the
entity tables and the incremental cursors, refetch from scratch, and
**let dolt's diff tell you what was missing**.

  - **`--reset-and-redownload`** wipes every entity table + its
    `_bookkeeping` sidecar. `blob_refs` is preserved so already-fetched
    blob bytes are not re-pulled. Missing-from-the-prior-pass blobs
    are still picked up via the normal entity-walk → blob-fetch path.
  - **`--refetch-blobs`** wipes `blob_refs` + `blob_refs_bookkeeping`,
    forcing every attachment to re-download. The re-fetched bytes hash
    to the same blake3, `INSERT OR IGNORE` into `cas_objects` is a
    no-op, no disk grows.
  - Pass both for a full reset. Pass `--reset-and-redownload` alone for
    the common "check for entity gaps without burning bandwidth on
    blobs" case.

The skip-check is keyed by the **upstream identifier** (known before
fetch), not by content hash (only known after). That makes `blob_refs`
a cache index over the CAS, and `--reset-and-redownload` is the
"invalidate entity data, keep the cache" path.

`cas_objects` has no reset path either way. Bytes are byte-stable;
the only legitimate way to remove them is `blob_cas::gc_orphans()`.

## Object identity: Ship of Theseus on UUIDs

We lean **heavily** on upstream-provided UUIDs to establish permanent
object identity.

  - Every raw-store entity table keys by the upstream provider's
    identifier —
    no surrogate `AUTOINCREMENT`. That's what makes `dolt diff` stable
    across re-fetches, what makes `ON CONFLICT(id) DO UPDATE` work, and
    what makes cross-table references (e.g. `messages.conversation_id`)
    mean something.
  - When an upstream doesn't expose a stable UUID, we **synthesize one
    via UUIDv5** from a per-provider namespace and the most stable
    available fields. The `GridRow` schema documents the exact recipe
    per row type — e.g.
    `slack.message: uuidv5(SLACK_NS, 'slack:{team}:{channel}:{ts}')`,
    `github.pr: uuidv5(GITHUB_NS, 'github:{repo}:pr:{number}')` (see
    `frankweiler/backend/schema/src/generated/grid_rows.rs`).
  - **Backpointers and outlinks are first-class** in the projection
    schema. `GridRow` carries:
      - `uuid` — the Ship-of-Theseus identity, deterministic from
        upstream so re-ingest is idempotent.
      - `external_id` — the provider-native primary id (numeric GH/GL
        id, PR number, …) preserved alongside our UUID so we can
        round-trip back to the provider's API.
      - `source_url` — the canonical URL on the provider's web UI
        (e.g. `pull_request.html_url`, GitLab `note.web_url` with
        `#note_<id>` anchor), populated everywhere we can construct it.
      - `qmd_path` — the path to the rendered markdown sidecar.
      - Provider-specific cross-references (`notion_page_uuid`,
        `notion_block_uuid`, `slack_link`, `git_sha`, …) so the UI can
        link sideways as well as out.
  - We do **not** use row autoincrement or hashes-of-content as
    identity for objects. Both break the Ship-of-Theseus property:
    autoincrement isn't deterministic across re-ingest; content hashes
    change every time the content does.

## Time and ordering discipline

If [Object identity](#object-identity-ship-of-theseus-on-uuids) is "UUIDs give global object identity," this is its temporal
sibling: **timestamps give global temporal ordering** across every
provider that has a time-shape to its data. That global ordering is
what makes the UI's union grid sortable, what makes `before:` /
`after:` queries mean the same thing across Slack and GitHub and
Notion, and what lets a sync delta be "what happened in the last
week" instead of "what happened to be at the top of each provider's
result list."

The principle: **every event-shaped `GridRow` carries an ISO-8601
timestamp with explicit offset.** Concretely, in
`GridRow.when_ts`:

  - **Real upstream timestamp when one exists.** A Slack message's
    `ts`, a GitHub PR's `created_at`, a Notion page's `last_edited_time`.
    Preserved with the explicit offset upstream gave us (typically
    `+00:00` for APIs that hand back UTC).
  - **Microsecond-bump for synthesized timestamps.** Blocks and
    sub-items that lack their own timestamp (chat blocks within a
    message, ChatGPT messages within a conversation that only has a
    create_time) get a synthesized one by bumping microseconds off
    the parent's stamp. This keeps within-parent order stable across
    re-runs and guarantees no collision with real stamps (real
    timestamps don't carry per-row µs precision from upstream).
  - **Strict ISO-8601 with offset, not bare `Z` or naive.** A naive
    timestamp can't be globally sorted alongside a `+02:00` one
    without a hidden timezone assumption.

### Single source of truth: `frankweiler-time`

Every `now()` call and every inbound RFC 3339 parse in the workspace
funnels through the `frankweiler-time` crate
(`frankweiler/backend/time/`). The crate exposes:

  - `IsoOffsetTimestamp::now_local()` — the canonical "now," returning
    the wall clock with the **generating system's local-tz offset**
    (e.g. `2026-06-10T14:23:00-07:00`). An offset-bearing timestamp is
    strictly more information than the same instant in UTC: you can
    recover UTC from `-07:00`, but you can't recover the originating
    offset once it's been normalized away. This is the policy for
    every generated `fetched_at` / `created_at` / run-marker stamp.
  - `parse_strict(s)` — accepts only strings that already carry an
    explicit offset. Most parse callsites should use this.
  - `parse_with_assumed_utc(s)` — **the single function in the whole
    repo** where "the upstream string lacked an offset, assume UTC"
    is allowed. Reach for it only after auditing an upstream feed and
    confirming naive-means-UTC. Any other fallback (local time,
    midnight, run start, epoch) is fabrication.
  - `IsoOffsetTimestamp::bump_micros(n)` / `bump_micros_str(s, n)` —
    the canonical sub-item synthesized-stamp recipe.

### No fabricated timestamps

A logical corollary of the broader "[don't make up data](#wire-fidelity-of-the-raw-store)"
principle, called out here because timestamps are the easiest place
to accidentally violate it:

  - When upstream gives us no timestamp and we can't pick one up
    from a parent (no `bump_micros` source), `when_ts` is **null**.
    Not "epoch," not "now," not "midnight UTC of the row's date."
  - When upstream's timestamp string is naive and we haven't audited
    that feed, parsing returns an error — surfaced as a warning in
    the per-run summary, not silently rescued.
  - Fallback paths that synthesize a value when upstream is silent
    are anti-patterns even when they "look plausible." They mask
    incompleteness in ways the consumer can't tell apart from real
    data.

### Entities without a time-shape

Some upstream object types genuinely don't have a meaningful
timestamp:

  - **Contacts (vCards).** A person doesn't have a creation event in
    the upstream system; they exist. The vCard's `REV` field is
    sometimes set, but most contacts don't have one.
  - **Perseus texts and other immutable corpora.** The corpus is
    upstream-frozen; per-section "timestamps" would be nonsense.
  - **Workspace/account metadata** (Slack `team`, GitHub `org`):
    arguably has a creation date, but it isn't shown in any
    time-ordered view.

For these we accept that `when_ts` is **null**. They don't
participate in temporal views and that's fine — the principle is
"**event-shaped** rows get real timestamps," not "every row
everywhere." Synthesizing a placeholder (the upstream's earliest
reachable date, an epoch fallback, etc.) would be making up data
we don't have; null is the honest answer and the consumer query
filters it out of time-ordered views.

A new provider should decide explicitly which of its row types are
event-shaped and document the source of `when_ts` for each.

## Commit lifecycle (load-bearing rule)

**Providers do not call `dolt_commit` or `commit_run` themselves.** The
orchestrator wraps each source's extract in exactly one commit at the
end. A run that touches N upstream pages / windows / items produces
**one** entry in `dolt_log()`, not N. The commit message is
`extract <name>: <stats>`.

Two consequences:

  - `dolt diff HEAD^1 HEAD` for any raw store is exactly "what this sync
    run pulled" — a clean unit of analysis for incremental delta UI
    surfaces and audits.
  - Provider authors don't have to think about commit boundaries. If
    you find yourself reaching for `commit_run` inside a provider, you
    almost certainly want UPSERT instead.

The only other commits allowed in a raw store are `rescue:` commits.
Anything else is a bug.

## Cursor / resume strategy

Two patterns in the tree, picked by upstream API shape:

  - **Forward-walk + refresh window** (slack, anthropic, chatgpt,
    github, gitlab): resume from `max(ts)` of previously-recorded
    items; also re-query the trailing `refresh_window_days` to catch
    edits / late-arriving items. Dedup collapses the overlap to zero
    writes.
  - **Time-windowed sampling** (yolink): walk `[start, now]` in
    fixed-stride windows. Windows align across runs and devices.
    Per-window UPSERT dedups re-fetched samples.

No checkpoint files. The dedup index is the resume cursor.

## Blobs and the CAS split

Attachment bytes are split out of the entity database into a sibling
content-addressable store. Each source has both
`raw/<name>.doltlite_db` (entities + per-source attachment metadata in
`blob_refs`) and `raw/<name>.blobs.doltlite_db` (`cas_objects` keyed
by blake3). Full schema + helpers in [port guide §7](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#7-blobs).

Two reasons the split matters:

  - `dolt diff` over the entity db stays small and human-grep-able.
    A re-fetch that picks up one new attachment doesn't drown the
    commit in a many-MB BLOB row.
  - The CAS file is byte-addressed: re-fetching identical bytes is a
    no-op via `INSERT OR IGNORE`. Intra-source dedup is automatic;
    cross-source dedup is one config change away (single-writer caveat
    in the port guide).

### Why contacts doesn't participate

Contacts' photo bytes arrive inline in the vCard payload as base64,
decoded once at parse time into `ContactPhoto { bytes, content_type }`,
written straight to `blobs/<uid>.<ext>` at render. They never touch
`blob_refs` or `cas_objects` because there's no separate fetch, no
separate upstream id, and no skip-check semantics needed — the bytes
are a property of the entity, not a separate resource.

If a future provider has the same shape, inline-in-payload is fine;
the shared CAS exists for the fetch-as-separate-resource pattern.

## Auth and credentials

Two patterns:

  - **Most providers**: shell out to `latchkey curl` (see
    [`backend/etl/src/latchkey.rs`](../frankweiler/backend/etl/src/latchkey.rs)).
    Auth lives in the latchkey keyring, indexed by URL host. The
    provider's HTTP transport never sees the bearer token.
  - **Yolink**: latchkey doesn't know about `us.yosmart.com`, and the
    consumer download path isn't bearer-authed — the URL itself is
    signed (`build_signed_url` in
    [`providers/yolink/src/extract.rs`](../frankweiler/backend/etl/providers/yolink/src/extract.rs)).
    Per-device secrets live in config (REDACT before publishing).

If you add a new provider with a new auth shape, prefer extending
latchkey upstream before adding a third pattern.

## Error handling

Two-axis distinction every provider follows:

  - **Per-item failures are tolerated.** A transient failure on one
    window / page / blob — 5xx, network blip, timeout, parse error,
    transient permission denied, rate-limit response — should not
    kill the run. Log a `warn!`, increment an error counter, **leave
    durable evidence in the row** (see [Retry and fetch durability](#retry-and-fetch-durability)
    below), advance the cursor, keep going. The run's `FetchSummary`
    reports `errors=N`.
  - **Auth failures and consecutive-failure budgets are fatal.** A
    workspace-wide 401 / 403 from the auth provider, or N
    back-to-back per-item failures on the same source, should return
    `Err` from `fetch(...)`. Even on auth failure, the orchestrator
    should still `dolt_commit` to record what *did* get pulled before
    the failure plus a note about the problem, then exit non-zero
    once other pipeline pathways finish.

The yolink provider's `CONSECUTIVE_FAILURE_BUDGET = 30` is a template
for the second pattern.

## Retry and fetch durability

The principle: **every failed fetch leaves durable evidence in the
table** so a later run can find it, retry it, and either resolve it
or report it still-failed without re-walking the entire upstream API.

Five sub-rules:

  - **Pre-seed before fetch.** Every entity row should be created in
    the table the moment we *learn the upstream identifier exists*,
    not when the detail fetch returns. A pre-seeded row carries the
    PK, the `parent_id` / context needed to redo the fetch, and a
    `NULL` payload. If the detail fetch then crashes, errors out, or
    gets killed, the row is still there with `payload IS NULL` and
    we know to retry it. This is the
    `dr::ensure_object_row(&mut tx, table, id)` pattern.

    The aspiration is "pre-seed always." Today we do this where the
    upstream API gives us a clean listing-then-detail split (Notion,
    Anthropic). For providers whose upstream forces us to discover
    IDs only inside the same call that fetches their content, we
    can't pre-seed and have to fall back to "row appears at fetch
    success."

  - **Always-paired bookkeeping.** Every object table has a sidecar
    `<table>_bookkeeping` carrying `attempt_count`, `last_attempt_at`,
    `last_error`. A success bumps `attempt_count` and nulls
    `last_error`; a failure bumps `attempt_count` and records
    `last_error`. Either way, the per-row paper trail exists.

  - **Blobs follow the same shape.** `blob_refs` carries a nullable
    `blake3` (NULL = not yet stored in CAS). The
    `blob_refs_bookkeeping` sidecar records attempt count and last
    error. A failed blob fetch leaves a `(ref_id, blake3=NULL,
    last_error=…)` row that a retry walk picks up.

  - **Retry-on-by-default, with opt-out.** The orchestrator takes a
    flag — call it `--retry-failed` (default `true`) — that says
    "before any normal walk, re-fetch every row where
    `last_error IS NOT NULL` or `payload IS NULL AND attempt_count > 0`,
    same for `blob_refs`." Pass `--no-retry-failed` to skip the
    retry pass (useful when the upstream is known-flaky right now and
    you want a fast incremental).

  - **Retry policy is config, not code.** Per-source `sync:` blocks
    in `config.yaml` should support the same retry knobs as the
    global default (max attempts, backoff schedule, "give up after N
    days") using one shared schema. A user who has one source with
    chronically flaky auth shouldn't have to mute retry for the
    whole pipeline.

### Transient vs non-transient

The retry mechanism is for *transient* failures. Some signals deserve
a different mark:

  - **Confirmed-deletion (404 on a known-existed thing).** The
    upstream is telling us "this is gone." A retry will only ever
    return 404 again. The row should carry a distinct
    `deleted_upstream_at` marker so we don't burn API quota retrying
    forever, while still preserving the row (and any history) for
    backpointer / outlink purposes.
  - **Workspace-wide auth failures.** Per
    [Error handling](#error-handling) above, these are fatal and
    bail the run; per-row retry doesn't apply.

### Intra-run backoff

The retry-failed flow above is *between* runs — durable evidence
survives sync exit. **Within** a single run, transient signals can
also drive intra-run backoff: Slack's 429 handling implements
`Retry-After` + exponential backoff before giving up and moving on,
and that pattern generalizes. Providers should prefer intra-run
backoff for fast-recoverable failures (rate limits) and fall through
to the durable-evidence path only for failures that need a fresh run
to clear.

### Bounded backlog

A pure "leave it in the table forever" policy can grow a permanent
backlog of e.g. private-channel 403 rows that will never succeed.
The retry policy's "give up after N days" / "give up after N
attempts" knob is what bounds this. Beyond that, periodic cleanup
of rows whose `last_attempt_at` is older than the retention window
keeps the backlog from growing unboundedly.

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
[`//frankweiler/backend/sync:manual_e2e_live_sync_golden`](../frankweiler/backend/sync/tests/manual_e2e_live_sync_golden.rs)
(tagged `manual` + `external` + `no-sandbox`; runs the full sync
pipeline against `configs/thad_tiny.yaml`, every source, against
live upstreams using host-side latchkey credentials). It snapshots
three things into `frankweiler/backend/sync/tests/snapshots/`:

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
    timestamp-fabrication bugs (see the contacts `when_ts` story
    above), attachment-handling gaps (e.g. attachment slots that
    extract isn't walking), etc.
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
[`frankweiler/backend/etl/providers/`](../frankweiler/backend/etl/providers/),
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
   [`Sidecar`](../frankweiler/backend/etl/src/sidecar.rs).
5. Drop sample wire-format data into `providers/<name>/tests/fixtures/`
   (TNG cast — see [Testing with TNG fixtures](#testing-with-tng-fixtures)) and write integration tests next to it.
6. Add the new source's `type:` discriminator to the `SourceConfig`
   variants in [`backend/core/src/config.rs`](../frankweiler/backend/core/src/config.rs)
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
    `*_bookkeeping` sidecar tables, `blob_refs`. Today's de facto
    answer to "I added a column" is `--reset-and-redownload`. That
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

  - **Upstream schema drift** — Slack adds a field, Notion changes a
    block type, GitHub renames `merged_by`. Because we preserve raw
    payloads verbatim (see [Wire-fidelity of the raw store](#wire-fidelity-of-the-raw-store)), the new bytes are captured for free —
    a translate-side bug is the worst case, never data loss. The
    principle: **upstream change should fail loudly at translate
    time, not silently at extract time.** No automated drift detector
    exists today; see [Detecting upstream shape drift](#detecting-upstream-shape-drift).

## Operating assumptions

A few constraints that aren't really "principles we strive for" but
are load-bearing assumptions the rest of the design rests on:

  - **Single writer per doltlite file.** The raw store has one writer
    (extract) and one reader (translate, after extract has exited).
    Journal mode is `DELETE`, not WAL, specifically so we get a
    single-file byte-stable artifact suitable for golden snapshots
    ([port guide §4](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#4-journal-mode-delete-not-wal)).
    Cross-source CAS sharing is one config change away, but the
    single-writer caveat carries over.
  - **Single-user, single-laptop.** No multi-tenancy, no replication,
    no hosted backend. The data root is one directory on one user's
    machine.
  - **Data stays local.** Slack DMs, Signal messages, email, contacts
    photos, GitHub private repos — the entire reason we built this on
    a laptop instead of a server is that the data doesn't leave the
    laptop. Provider auth tokens live in latchkey (a local keyring);
    no telemetry, no cloud sync, no analytics. A new feature that
    needs to phone home should be flagged explicitly.
  - **Opening doltlite files.** 
    There are several options to read/write doltlite files outside of Rust:
    * The stock doltlite CLI, a drop-in replacement for the sqlite CLI.
    * There are python doltlite bindings: https://libraries.io/pypi/doltlite

## Downstream stages — pointer to the companion doc

The material in this section is here as a placeholder for what will
move into the companion document *Data Architecture: Translation,
Indexing, Annotation, and UI*. It's included so this ingestion-focused
doc can still hand a reader enough of the downstream picture to
understand the contracts extract has to honor.

### Translate and downstream stages

After extract, we run translations for display and indexing — render
to markdown with YAML frontmatter, index the markdown with qmd, derive
`grid_rows` for the UI.

The cross-provider contract is the **sidecar**: for every rendered
document, Translate emits two co-located files —

  - `<id>.md` — human-readable, with YAML frontmatter.
  - `<id>.grid_rows.json` — the
    [`Sidecar`](../frankweiler/backend/etl/src/sidecar.rs):

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

### Shared schemas across similar sources

When several sources are shaped similarly enough (a matter of taste,
but largely driven by schema and UI overlap), they should be massaged
into a **shared canonical schema** so the rest of the pipeline (search,
display, threading, attachments, exports) shares code paths and stays
consistent.

Where unification actually happens **today**: the `GridRow` projection
([`schemas/grid_rows.yaml`](../schemas/grid_rows.yaml), codegen'd into
the Rust struct at `frankweiler/backend/schema/src/generated/grid_rows.rs`).
Every searchable entity from every provider collapses into rows of one
schema with `provider` + `kind` discriminators. The grid backend
reads it with a single query and renders it without knowing which
provider produced any given row.

Unification should **never** happen in the raw store.
For example, Slack, Beeper, Signal, Anthropic, ChatGPT each have their own raw tables and even separate doltlite 
 DBs (`slack_messages`, `beeper_messages`, …)

 However, once we start translating data, we should aspire to share as much as possible.
 For example, we should translate raw data into unified schemas where appropriate, then send
 that unified data through common code paths for interpretation, rendering, and indexing.

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

These are gaps we noticed while writing this doc — places the
principles either aren't yet articulated, aren't yet verified to be
true in code, or genuinely haven't been decided. They're listed here
as desired principles where we know what we want, and as open
questions where we don't. The audit that follows this document will
measure against these alongside the resolved principles above.

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

**Open**: this isn't verified. The `--otlp-endpoint` flag is documented
but the data-stays-local guarantee (see [Operating assumptions](#operating-assumptions)) is not extended to it. We
should audit what `tracing` spans actually carry, redact at the
source, and state the rule explicitly.

### Time and ordering — per-provider consistency

The principle is now in [Time and ordering discipline](#time-and-ordering-discipline). The remaining open question is whether
it's actually honored consistently across the existing providers:

  - Does every event-shaped row type emit ISO-8601 with explicit
    offset, or do some shortcut it with bare `Z` or naive datetimes?
  - Where we synthesize from a parent timestamp, is the
    microsecond-bump applied or did some provider just clone the
    parent stamp (creating ordering ambiguity within a parent)?
  - For entities without a time-shape (contacts, perseus, workspace
    metadata), do we consistently null-out `when_ts` everywhere, or
    do any providers still fabricate a placeholder?

A grep over per-provider `translate/grid_rows.rs` (or equivalent)
plus a spot-check on the produced `GridRow.when_ts` values in the
TNG fixture output should be enough to answer this.

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

**Open**: we don't currently measure this. We should add a mechanism to roughly compute "sync time / size of sync delta" on each sync for each provider, so that we can get a handle on where the slowness it.

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

This is stated in [port guide §3](../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#3-synth-reads-checked-in-fixtures-extract-writes-doltlite),
but it's a project-wide invariant that belongs at the architecture
level too.

### grid_rows itself lives in doltlite

The `grid_rows` table (the projection consumed by the UI) lives in
`<data_root>/backend_index.doltlite_db`, just like raw stores. The
"doltlite is our storage layer" claim should apply to every store
the system writes — raw, blob CAS, and the backend index — not just
to raw. Worth saying explicitly somewhere, probably in [Shape of the system](#shape-of-the-system).

## Deferred work

Edits to this doc and its neighbors that we've agreed to do, but
haven't yet. Each is intentionally not blocking the audit thread —
they're listed here so they don't get lost.

  - **Move `frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md` →
    `docs/doltlite_patterns.md`**, and reframe it from a porting
    guide into "shape of how we use doltlite." The current doc reads
    as one-time migration instructions (which JSONL-tree raw stores
    looked like, the porting checklist, "we tried checking in a
    `.doltlite_db` once and threw it away"); the durable content
    inside it — the design rules, the table-and-blob shape, the
    shared utilities — should be lifted into a stable reference.
  - **Rename `docs/doltlite.md` → `docs/doltlite_tips.md`** to make
    its scope (operational tips and dolt-history reading) explicit
    against the new patterns doc above.
  - Both of the above require updating inbound links across the
    repo: this file, signal's `extract/mod.rs`, each provider's
    `EXTRACT.md` and `DOLTLITE_RAW.md`, the etl crate's module docs,
    and any AGENTS.md / README pointers.

## What this document does not cover

  - The specific table DDL of any provider — see the port guide and
    each provider's source.
  - The UI and how it consumes `grid_rows` — see the frontend docs.
  - The qmd index and how it's built — see [`docs/edges.md`](edges.md)
    and the `qmd_indexer` crate.
  - Anything about hosting, multi-user, or replication — explicitly
    out of scope. This is a single-user, single-laptop system.
