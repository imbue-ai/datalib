# Data architecture: ingestion

## Introduction and Context

We have an incremental, resumable ETL-shaped architecture that extracts raw
data from many upstream sources and stores it as **JSON API responses
preserved verbatim** in versioned doltlite tables, with attachment **BLOBs in
a content-addressable store** (also doltlite, but a separate sibling database
per source).

This is not novel — it's the same shape as many Flume / Apache Beam / Dask /
Prefect / Airflow pipelines. What we optimize for that those tools don't,
**for a single user on a single laptop**:

* Easy to install, configure, run, and monitor.
* **No cluster, no scheduler service, no DAG server. One config file, one
  orchestrator binary, one local data directory.**

This document describes the principles we strive towards for the **ingestion
(extract) side**: how raw data lands on disk, what shape it has at rest, and
the operational properties (monitorable, stoppable, resumable, incrementally
cheap, verifiable) the extract stage aims for. It is aspirational as much as
descriptive: a new provider, table, or transformation should be judged
against it, and divergences should be either justified or fixed.

### Related documents

The downstream stages — translate, load, indexing, view, annotation — are
mostly the subject of
[`docs/dev/post_ingestion_architecture.md`](post_ingestion_architecture.md).
Where understanding extract requires a downstream concept (the sidecar
contract translate emits, the `GridRow` projection the UI reads), this
document touches on it briefly.

Practitioner-facing material — how we test, how to add a provider, how the
schema evolves, and the open questions — lives in the companion
[`data_architecture_ingestion_practices.md`](data_architecture_ingestion_practices.md).

## Schema first — the tables _are_ the design

> _"Show me your flowchart and conceal your tables, and I shall continue to be mystified. Show me your tables, and I won't usually need your flowchart; it'll be obvious."_ -- Fred Brooks, The Mythical Man Month (1975)

The single most load-bearing principle of this whole document is
that **the schema is the design**. When we add a new data source, 
or sketch a new feature, the **first** artifact is the
table — its columns, its primary key, its uniqueness constraints, its
foreign-key relationships, and a paragraph or two of prose explaining
what each row _means_ and why it is there.

Concretely, when starting any non-trivial piece of work in this
codebase:

1. **Write the DDL first** 
2. **Document each table _in the same file as the DDL_**. Per-provider
   `schema_raw.rs` files (`etl/providers/<p>/src/extract/schema_raw.rs`)
   are the canonical home for both the `CREATE TABLE` text and the
   prose commentary on it. Tables without their prose are
   half-finished.

## Extract schemas should be simple

The extract portion of our system captures raw data from sources in its
native format with as little translation as possible: typically JSON payloads
as they arrived off the wire, with enough indexing that related payloads can
be updated and grouped together. So extract schemas should often be extremely
simple — just a stable primary key and a JSONB payload column.

Two cases justify more schema:

* When the JSON payload is missing contextual data the requester knew, store
  it alongside the payload as extra columns on the table.
* For attachments (stored in a separate blob CAS), we need to know which
  attachments belong with which payloads. We link event payloads and blobs in
  a many-to-many relationship via a dedicated edge table.

## Wire-event tape (JSONL)

Doltlite is our primary store, but doltlite is also a binary file you
need a tool to open. So alongside the doltlite raw store,
extracts also write a **plain-text, append-only JSONL log of what
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
  <table>.jsonl                       # one line per upsert
  <provider>_<attachments>.jsonl      # the per-provider CAS edge table
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

Rules:

  - **The pipeline never reads it.** Translate, load, resume, retry —
    all of those go through doltlite. The JSONL is a write-only
    mirror. Deleting the `events/` directory does not break anything.
  - **Same bytes as the upsert.** We tap right next to the
    `ON CONFLICT(id) DO UPDATE`, so the tape carries the same
    wire-fidelity payload that the doltlite row gets. No second
    parse, no second normalize.

## Schema details

Each entity table has a `payload` column holding the raw upstream wire
payload, plus a small number of typed columns the writer must populate at
insert time (synthesized-PK components, FKs into parent tables that aren't in
the payload, namespace discriminators). On disk `payload` is stored as JSONB
(SQLite 3.45 binary JSON, via `jsonb(?)` on write and `json(payload)` on
read; see [port
guide §6a](../../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#6a-jsonb-storage-for-payloads)) —
purely a storage encoding; the principle is wire-fidelity (see [Wire-fidelity
of the raw store](#wire-fidelity-of-the-raw-store)).

**Fields derivable from the payload** (`updated_at`, `state`, `name`,
`html_url`, `display_name`) — even when we want to query or index them —
should **not** be duplicated as stored columns. Use either a
`CREATE INDEX … ON t(payload->>'$.path')` expression index or a `VIRTUAL`
generated column plus an index over it. Both produce COVERING index plans in
DoltLite v0.11.9; the VIRTUAL+index variant additionally restores
`SELECT col FROM t` ergonomics. Either way, `ALTER TABLE ADD COLUMN … VIRTUAL`
(or a new expression index) is a no-refetch additive change against existing
user data. See [Schema evolution](data_architecture_ingestion_practices.md#schema-evolution).

The pipeline has three stages, all running **in-process inside
`frankweiler-sync`** — one binary, one process, one config-driven walk over
enabled sources:

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
[`frankweiler/backend/etl/providers/<name>/`](../../frankweiler/backend/etl/providers/),
named `frankweiler-etl-<name>`. The provider crate owns its Extract +
Translate code, its bins, its integration tests, and the sample
fixtures the tests run against — keeping sample data next to the code
under test serves as documentation of "what this provider's wire
format looks like." Load is provider-agnostic and lives at
[`src/load.rs`](../../frankweiler/backend/etl/src/load.rs); a new provider
needs no Load-side changes.

### Per-provider schema layout

Within each provider crate the bytes-at-rest schema is its own file,
deliberately declarations-only:

  - **`providers/<name>/src/extract/schema_raw.rs`** — the raw-store schema:
    DDL constants (one per table / index / bookkeeping sidecar),
    schema-evolution migration constants co-located with the table they
    touch, any synthesized-PK recipe functions, and a tiny `full_ddl()`
    composer that splices in `dr::bookkeeping_ddl_for(table)` for each
    entity. **No manipulation code** — `RawDb`, UPSERTs, SELECTs, and
    parameter binding stay in `extract/db.rs` and import from `schema_raw`.
    The convention is proto/pydantic-flavored: opening the `schema_raw.rs`
    files at the same fixed path answers "what does the world look like at
    rest?" without opening anything else.
  - **`providers/<name>/src/translate/schema_translate.rs`** (aspirational,
    landing per provider) — the normalized representation translate emits:
    mostly serde-shaped Rust types, not SQL DDL, the in-memory POD form
    before it's shredded into sidecar rows. A provider may have multiple
    `schema_translate_<family>.rs` files; where a shape is shared across
    providers (chat-human, code-review, time-series, …) the canonical type
    lives in a shared crate and the per-provider file re-exports.

#### Events vs bookkeeping: where each column lives

Every entity table `<t>` is paired with a sidecar `<t>_bookkeeping`.
The split is load-bearing — three buckets to think about when adding
a column:

  1. **Upstream payload data** (a Slack `text`, a GitHub `state`, a
     Notion `last_edited_time`) → lives inside `payload`. If we need
     to query or index it, use a VIRTUAL generated column + index or
     an expression index over `payload->>'$.path'`. Do **not** copy it
     into a stored column.
  2. **Writer-supplied identity / joins** (synthesized-PK
     components, FK references to parent entities the walker knows
     but the payload doesn't, namespace discriminators like beeper's
     `source`/`network`) → stored typed columns on `<t>`. These are
     the only typed columns the entity table should grow.
  3. **Writer-supplied per-row state** (`fetched_at`, `attempt_count`,
     `last_attempt_at`, `last_error`, per-row cursors like CardDAV
     `etag`, ChatGPT `last_listing_update_time`, YoLink
     `last_ts_ms`, server-supplied freshness markers like
     `ctag`/`sync_token`) → `<t>_bookkeeping` sidecar.
  4. **Upstream-supplied per-fetch bookkeeping embedded _inside_ the
     payload** (Slack's channel/user `updated` epoch, which the server
     bumps spuriously; user-specific view state like a message's
     `last_read`/`subscribed`) → split OUT of `payload` and stored in
     `<t>_bookkeeping.volatile_payload`. This is bucket 1's evil twin:
     it looks like payload data because it rides in the wire object, but
     it describes the fetch, not the object's state, and it churns on
     every re-fetch.

The split matters because bookkeeping changes on every attempt regardless of
upstream change. Storing it on the entity table makes every `dolt diff` noisy,
defeats the wire-fidelity of `payload`, and forces re-renders of unchanged
content. Keeping it on the sidecar means `<t>` mutates only when upstream
actually changed, and the sidecar churn stays out of any cross-stage
fingerprint.

##### Volatile-field split (bucket 4) — the mechanism

Bucket 4 is the only one where bookkeeping arrives *interleaved* with
content in a single JSON object, so it needs an explicit split rather
than just "write it to a different column." The standard mechanism lives
in [`frankweiler_etl::doltlite_raw`](../../frankweiler/backend/etl/src/doltlite_raw.rs)
and is the same for every provider:

  - Each provider declares its volatile field paths next to the row's
    table definition in `schema_raw.rs` — a `&[VolatilePath]`, where a
    `VolatilePath` is a key path from the payload root (`&["updated"]`
    for a top-level field, `&["topic", "last_set"]` for a nested one).
    **Every provider participates; the set is just empty for providers
    with no embedded bookkeeping**, and an empty set is a zero-cost
    no-op (see below).
  - At the upsert site, [`split_volatile(payload, paths)`] partitions the
    wire object into `(base, volatile)`: `base` is the content payload
    (what lands in `<t>.payload` and drives `dolt_diff_<t>`), `volatile`
    is an object holding only the split-out fields.
  - [`bulk_upsert_with_tape_split`] writes `base` to the entity table and
    `volatile` to `<t>_bookkeeping.volatile_payload` in one transaction,
    while the **JSONL wire-tape still records the full original payload**
    — the tape stays a byte-faithful record of what came off the wire.
  - [`overlay(base, volatile)`] reconstructs the exact wire object when a
    reader needs it. It's a plain recursive object merge, **not** RFC 7386
    JSON Merge-Patch: a `null` in `volatile` is a value, not a delete
    (upstream payloads legitimately carry nulls). `overlay(split(p)) == p`
    is a tested invariant.

The empty-set case is free by construction: `bulk_upsert_with_tape(…)`
is exactly `bulk_upsert_with_tape_split(…, &[])`, and the
`volatile_payload` column is `NULL`-able and present on every sidecar
regardless. So a provider with no volatile fields needs no code and no
schema change; when one is discovered, it adds a `*_VOLATILE_PATHS`
const and switches its upsert site to the `_split` chokepoint.

How volatile fields are discovered: the live golden sync test
(`manual_e2e_live_sync_golden`) runs a third `--reset-and-redownload`
pass and asserts the data-table `dolt diff` is empty. Any field that
drifts across an identical re-fetch shows up there and must be moved to
bucket 4 (or explained). See [§"Verifiable via
`--reset-and-redownload`"](#verifiable-via---reset-and-redownload).

### Layering of concerns: extract is downstream-agnostic

The per-stage modules within a provider crate form a strict layer
with a single allowed dependency direction:

```
load   ← translate   ← extract   ← upstream
       (provider-agnostic)
```

  - **`extract`** owns the bytes-at-rest. It fetches from upstream and
    persists into `<data_root>/raw/<name>.doltlite_db`, and nothing else. It
    must NOT depend on `translate`, `render`,
    `frankweiler_schema::grid_rows::GridRow`, sidecar types, or the qmd
    index. The per-provider `schema_raw.rs` rustdoc deliberately avoids
    describing how translate consumes the tables.
  - **`translate`** depends on `extract` (it reads the raw store and projects
    to the normalized POD + sidecar shape). `extract::schema_raw` is part of
    the contract translate consumes.
  - **`load`** is provider-agnostic; it lives at
    [`src/load.rs`](../../frankweiler/backend/etl/src/load.rs) and depends on no
    provider's extract or translate. Its input contract is the sidecar tree.

Why the discipline matters: extract is its own deliverable — a user can run
it, stop, inspect the raw store, and have something useful even if translate
has bugs or hasn't been written yet. Translate can then be re-implemented or
extended without touching extract, and disabling a translate path for one
provider doesn't disturb that provider's extract.

## Operational principles

### Monitorable

The first sync from a given source is often very long (hours to days, many
GB). Every stage must surface progress the user can watch in real time.

  - Every binary flattens [`obs::ObsArgs`](../../frankweiler/backend/obs/src/lib.rs)
    into its clap parser, so every stage takes the same logging / OTLP /
    progress-bar flags. On a TTY, pretty log lines on stderr; otherwise NDJSON
    events. Log emissions route through an `IndicatifWriter` coordinating with
    the shared `MultiProgress` (`frankweiler_obs::shared_multi()`) so caller
    progress bars don't get stomped by log lines.
  - `--otlp-endpoint http://host:4317` exports spans + events via OTLP, so a
    single Tempo/Jaeger collector can ingest every stage. (See [the
    privacy-boundary unresolved question](data_architecture_ingestion_practices.md#observability-and-the-privacy-boundary)
    for the contract that constrains what may be in those spans.)
  - Each stage emits `*_start`, `*_complete`, and per-document progress events
    with a stable provider-prefixed name (`slack_download_*`,
    `grid_rows_load_*`, …). The `*Summary` structs are `Serialize`, so a web
    UI can consume the final stats line provider-agnostically.
  - Long-running operations must report something visible every few seconds;
    an extract that walks 100k items silently for an hour is a bug.

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
    [`docs/dev/doltlite.md`](doltlite.md#rescue-commits-on-every-rust-side-open).

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

The raw store preserves the **semantic content** of upstream responses
verbatim — every field, every value, with no loss and no
pre-shaping into our internal model. The on-disk *encoding* of that
content is a separate question; we pick whichever encoding is
human-readable and inspectable. Concretely:

  - **JSON-shaped sources** (HTTP API responses from Slack,
    Anthropic, Notion, GitHub, etc.) store the response JSON
    verbatim, as JSONB.
  - **Binary-protocol sources** (Signal's encrypted protobuf
    backup, future binary feeds) are **decoded** at extract time
    into JSON of equal semantic content. Encryption layers, compression,
    binary wire encodings, and other transport-level packaging are
    artifacts of how the data got to us, not part of the wire data
    itself. Storing them raw on disk would be "too raw" — the point
    of the raw store is that a human can `tail`/`grep`/`jq` it
    without a decoder in the loop.
  - **File-imported sources** (mbox `.eml` bytes, vCard `.vcf` files,
    WhatsApp `msgstore.db`, Beeper `index.db`) promote the *semantic* content (typed columns,
    JSONB payloads) into the entity tables. **File-tree imports go
    through extract** just like API-backed sources: a directory of
    `.vcf` files lands in the same raw-store row shape CardDAV
    produces, an mbox lands in the same shape JMAP produces.
    Translate has exactly one input contract per provider regardless
    of whether the data came over the wire or off disk.

The rationale: **if all we wanted was a copy of the upstream bytes, we'd just
use `cp`.** The raw store earns its keep by being *queryable and
human-inspectable* in a way the original bytes aren't — JSONB rows, typed
columns, predictable structure across providers. That's the criterion for "is
this decoding step OK at extract time?" If the alternative to decoding is
asking the user to install a special tool to see their own data, the decoding
belongs in extract.

Two load-bearing rules follow:

  - **Normalize at translate time, not extract time.** A lesson
    learned on the anthropic port: we used to pre-normalize the API
    response (renaming fields, collapsing shapes, dropping subtrees)
    at fetch time. We don't anymore. Decoding a binary wire encoding
    to JSON of the **same** semantic content is **not normalization**
    — every field upstream sent us is still present, with the same
    values. Normalization means pre-shaping into our internal model
    (renaming, collapsing, projecting), which we defer to translate.
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
page on a 5xx, a bug in our delta logic). One check is to wipe the
entity tables and the incremental cursors, refetch from scratch, and
**let dolt's diff tell you what was missing**.

  - **`--reset-and-redownload`** wipes every entity table + its
    `_bookkeeping` sidecar. Per-provider CAS edge tables
    (`<provider>_attachments`) are preserved so already-fetched blob
    bytes are not re-pulled. Missing-from-the-prior-pass blobs are
    still picked up via the normal entity-walk → blob-fetch path.
  - **`--refetch-blobs`** clears the `blake3` column on the per-provider
    edge tables, forcing every attachment to re-download. The re-fetched
    bytes hash to the same blake3, `INSERT OR IGNORE` into `cas_objects`
    is a no-op, no disk grows.
  - Pass both for a full reset. Pass `--reset-and-redownload` alone for
    the common "check for entity gaps without burning bandwidth on
    blobs" case.

The skip-check is keyed by the **upstream identifier** (known before
fetch), not by content hash (only known after). The per-provider edge
table is the cache index over the CAS, and `--reset-and-redownload` is
the "invalidate entity data, keep the cache" path.

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
    available fields.  This is done in the data source's schema_raw.rs DDL.
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
what makes the UI's union grid time-sortable, what makes `before:` /
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

Some upstream object types genuinely don't have a meaningful timestamp:

  - **Contacts (vCards).** A person doesn't have a creation event; they
    exist. The vCard's `REV` field is sometimes set, but most contacts lack
    one.
  - **Perseus texts and other immutable corpora.** The corpus is
    upstream-frozen; per-section "timestamps" would be nonsense.
  - **Workspace/account metadata** (Slack `team`, GitHub `org`): arguably has
    a creation date, but it isn't shown in any time-ordered view.

For these `when_ts` is **null** and the consumer query filters them out of
time-ordered views — the principle is "**event-shaped** rows get real
timestamps," not "every row everywhere." A new provider should decide
explicitly which of its row types are event-shaped and document the source of
`when_ts` for each.

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

## One writer per row (load-bearing rule)

**Each write to a raw entity row is complete as of that write.** The
writer's job is to assemble everything it knows about the row —
`payload` plus all writer-supplied identity columns — and emit it in
one UPSERT. We do not have a notion of "partial" writes that leave
NULL columns the writer chose not to populate, and we do not have
multi-pass enrichment where writer A populates some columns and
writer B fills in the rest. Both are anti-patterns.

Consequences:

  - **One `ON CONFLICT(id) DO UPDATE` shape, everywhere.** Every
    column on the entity table (other than `id`) is updated with
    `excluded.<col>`. No `COALESCE(excluded.<col>, <table>.<col>)` —
    that pattern only exists to protect a stale-but-known value from
    being clobbered by a fresh-but-incomplete write, and we don't
    allow incomplete writes. The uniform shape is what lets the
    generalized bulk-upsert helper (see [Bulk-upsert as the standard
    write path](#bulk-upsert-as-the-standard-write-path)) cover every
    table.
  - **One writer per row, normally.** Typically each raw entity
    table has a single producing extractor. If two producers can in
    principle write the same id (e.g. a JMAP API extractor and an
    mbox file-import both targeting the `emails` table), it is a
    configuration error to enable both for the same destination, and
    the semantics if you did are **last-writer-wins, not merged**.
    The system is not built to maintain a hodgepodge of two writers'
    partial knowledge of the same row.

Why the discipline matters: the alternative is per-column conflict
policies (COALESCE on some columns, replace on others), which makes
the UPSERT shape diverge per table, makes the chunked-multi-row
helper proliferate variants, makes `dolt diff` harder to read, and
makes "which writer last touched this row?" an ambiguous question.
None of that is value we want to maintain.

## Bulk-upsert as the standard write path

Every extract is shaped the same at the bottom: for some entity
table `<t>`, upsert N rows of `(id, payload, …extras)`, paired with
N rows on `<t>_bookkeeping` for `(id, fetched_at, attempt_count,
last_error)`, and (if the source produced blobs) M rows on the CAS
of `(blake3, byte_len, content_type, bytes)`. Doltlite charges a
prolly-tree manifest mutation per `BEGIN … COMMIT`, so the right
shape is **one entity-pool tx + one CAS-pool tx per batch**, each
containing chunked multi-row `INSERT … ON CONFLICT(id) DO UPDATE`
statements. Email's mbox extractor proved this in practice: 25k
emails dropped from many minutes to ~75 seconds at FLUSH_BATCH=2000.

The principle: **every provider's extract uses shared
chunked-multi-row helpers for the entity-table UPSERT, the
`<t>_bookkeeping` upsert, and the CAS write.** Per-row UPSERTs
are an anti-pattern outside ad-hoc maintenance code.

The shared pieces, all in `frankweiler_etl`:

  - **`bulk::SQL_CHUNK` + `bulk::push_placeholders` /
    `bulk::push_placeholder_list`** — chunking utilities the provider's
    per-table multi-row `INSERT` builders use.
  - **`bulk::bulk_upsert_bookkeeping(tx, table, ids, now)`** — the generic
    `<t>_bookkeeping` UPSERT. Call directly inside the provider's tx after
    the entity UPSERT.
  - **`bulk::EventBatch<'a>`** — the per-table `(table, &[(id, &payload)])`
    shape the batch primitives share.
  - **`blob_cas::BlobCas::put_many`** — chunked multi-row `INSERT OR IGNORE`
    over `cas_objects`, one tx per call. The per-doc `blob_cas::BlobBundle`
    accumulates a document's attachments during extract (`add` / `add_error`)
    and exports its `cas_inserts()` and edge rows for these writes; the same
    bundle is reloaded at parse and consumed at render.
  - **`doltlite_raw::bulk_upsert_events(tx, tape, &[EventBatch], now)`** — the
    **wire-event** chokepoint. The caller has already issued its multi-row
    entity UPSERTs inside `tx`; this stamps `<t>_bookkeeping` for every batch
    in the same tx, commits, and (if a tape is attached) appends one JSONL
    line per row via `EventTape::append_batch`. Tape errors log but don't
    fail the upsert — doltlite is the source of truth.
  - **`doltlite_raw::bulk_upsert_with_tape(pool, tape, rows, payloads)`** —
    all-in-one variant (open tx → `bulk_upsert_in_tx` → commit → tape
    append). Use when the caller has a `&[T: BulkUpsertable]` vec in hand
    (every `WirePayloadRow`-derived provider does). Same "doltlite is truth,
    tape is best-effort mirror" semantics.

The chokepoint is the right tool **only for tables whose rows came
off a wire**. For everything else — CAS edge tables, sidecars,
file-imported entities like mbox or vcf where there is no upstream
"event" — call `bulk_upsert_bookkeeping` directly inside the same
tx and skip the tape. Synthesizing a fake wire payload just to feed
the chokepoint would be making up data we don't have.

The `ON CONFLICT` clause is **the same shape on every table**: every
non-PK column is set to `excluded.<col>`. The column list itself
still varies per table (different writer-supplied extras — see
[Events vs bookkeeping](#events-vs-bookkeeping-where-each-column-lives)
for which extras belong on the entity table vs the bookkeeping
sidecar vs as VIRTUAL+index over payload), but the conflict policy
does not vary — see [One writer per row](#one-writer-per-row-load-bearing-rule).
That uniformity is what makes a single generic
`bulk_upsert<T>(rows: &[T])` helper feasible (in flight): the only
per-table input is the column list, which we can derive from the
row type at compile time. What these shared helpers own today is the
cross-provider boilerplate — chunked SQL, bookkeeping, commit, and
(for the wire-event subset) the tape mirror.

## Incremental update via content fingerprints (load-bearing rule)

**Every derived artifact in the system carries a fingerprint of the
inputs that produced it, and every re-derivation step compares its
*current* inputs against the stored fingerprint before doing any
work.** Structurally this is the same content-addressable cache
pattern that Bazel and Nix use for build artifacts — derived
outputs are keyed by a hash of their inputs; on re-run, if the
inputs hash the same, the cached output is still valid. The
difference is that Bazel and Nix make the user write build files;
we hide the cache machinery inside each stage so the user just
runs `frankweiler-sync`.

The recipe at every stage:

  1. **Each persisted artifact carries a `*_fingerprint` field**
     that hashes the inputs that produced it.
  2. **The inputs hash covers both content and dependencies** — the
     payload bytes themselves plus the content hashes of every
     entity this artifact reads from upstream stages (so a change to
     an attachment's content invalidates the rendered doc that
     references it; a change to a chat_item's payload invalidates
     the bucket-fingerprint for its bucket; etc.).
  3. **Before re-deriving the artifact, recompute the inputs hash
     and compare.** Skip if equal. This compare-and-skip is the only
     thing the re-run loop should be doing for the common steady
     state.
  4. **A `*_VERSION` constant is the explicit "invalidate everything
     downstream" lever.** Bumping `RENDER_VERSION` is how we say
     "the renderer schema changed; treat every prior fingerprint as
     mismatched."

Where the pattern lives today:

| Stage                       | Inputs hash                                                                | Stored as                                | Skip-check it powers                                                       |
|-----------------------------|----------------------------------------------------------------------------|------------------------------------------|----------------------------------------------------------------------------|
| Extract (snapshot-level)    | `(metadata, main, files)` triple of `(mtime, byte_size)` (Signal)          | `ingested_backups.fingerprint`           | "Have we already ingested this snapshot?" — before any decrypt/walk        |
| Extract (per-attachment)    | blake3 of the decrypted attachment bytes                                   | `<provider>_attachments.blake3`          | Edge-row dedupe; also the input to translate's bucket diff                 |
| Translate (per source)      | doltlite HEAD commit hash at last successful render                        | `_render_cursor.json`                    | `dolt_diff_<table>` answers "which buckets changed?" — before loading rows |
| Load (per sidecar)          | the sidecar's own `source_fingerprint` (a stable bucket UUID today)        | `markdowns_loaded.source_fingerprint`    | "Have we already loaded this sidecar's rows?" — before re-applying rows    |

Where it still needs to land (aspirational; some not yet built):

  - **Per-table extract commit hash.** Today the orchestrator
    wraps each source in one commit per run; with finer-grained
    table-level commit hashes we could skip downstream stages whose
    inputs are unchanged at the table level even when other tables
    changed.
  - **Attachment-GC dependency tracking.** `gc_orphans` walks
    references today; making it incremental requires a fingerprint
    over "the union of referenced blake3s across providers."
  - **qmd-index rebuild.** Currently rebuilt full; should derive an
    inputs fingerprint per index shard.

Why this pattern matters:

  - **Re-runs are cheap.** A second sync against an unchanged source touches
    as little disk as possible at every stage; a re-render against an
    unchanged bucket loads zero payloads (translate's two-phase parse
    short-circuits before reading any chat_items in it).
  - **Bazel/Nix complexity, hidden.** No build files, no DAG declaration, no
    rule registration. Each stage's fingerprint recipe is internal to it, and
    the orchestrator wires the "previous fingerprints" map through
    automatically.
  - **Correct-by-construction.** Runs that should differ hash differently;
    runs that should match produce matching fingerprints and reuse the cached
    output, byte-for-byte.

### dolt_diff supersedes per-bucket fingerprints

The per-bucket fingerprint pattern has been **replaced with
`dolt_diff_<table>` virtual tables driven by a per-source render cursor**.
The translate-side fingerprint CTE is gone; doltlite's prolly-tree diff
answers "what changed since last render?" directly.

Mechanism: on render success, the per-source render cursor stamps the
doltlite HEAD into `_render_cursor.json`. On the next render, `parse` reads
that hash and runs `doltlite_raw::scan_buckets(pool, last_hash, &DiffScanSpec
{ global_fanout_tables, bucket_query })`, which cold-starts if any
`dolt_diff_<global_fanout_table>` row is non-`unchanged` (those fan out to
"render everything"), otherwise runs the per-bucket `bucket_query` across the
relevant `dolt_diff_*` vtabs. Parse then loads payloads only for the
surviving bucket keys.

Sidecar `source_fingerprint` and the load-step compare stay — they still gate
the load step ("have we already loaded this sidecar's rows?").
`source_fingerprint` is now just the stable bucket UUID. This swap moves the
"what's different?" decision from Rust-computed hash trees to doltlite's
native diff, which it maintains anyway for `dolt diff`. The per-row
`payload_blake3` columns are gone — the `WirePayloadRow` derive no longer
emits them.

**Rule for new stages.** Any new derivation added to the pipeline
(future Annotate step, future index shard, future projection)
follows the same recipe: declare what the inputs are (content +
dependency hashes), compute a deterministic hash over them, store
it alongside the output, compare on re-run. The compare-and-skip
loop is what makes the system feel responsive on a laptop with
months of accumulated data.

## Cursor / resume strategy

Cursor / resume is the **extract-side specialization** of the
[Incremental update](#incremental-update-via-content-fingerprints-load-bearing-rule)
pattern: "what was the last upstream identifier we successfully
recorded?" answers "what's the inputs hash for the next walk?"
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
`raw/<name>.doltlite_db` (entities + a per-provider
`<provider>_attachments` edge table mapping `(owning, ref) → blake3`)
and `raw/<name>.blobs.doltlite_db` (`cas_objects` keyed by blake3).
Full schema + helpers in [port guide §7](../../frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#7-blobs).

Two reasons the split matters:

  - `dolt diff` over the entity db stays small and human-grep-able.
    A re-fetch that picks up one new attachment doesn't drown the
    commit in a many-MB BLOB row.
  - The CAS file is byte-addressed: re-fetching identical bytes is a
    no-op via `INSERT OR IGNORE`. Intra-source dedup is automatic;
    cross-source dedup is one config change away (single-writer caveat
    in the port guide).

### Per-provider CAS edge tables

Every provider with attachments owns a small four-column edge table
that maps `(owning_id, ref_id) → blake3`. Bytes still live in the
shared `cas_objects`; the edge table is provider-specific so
providers that happen to use the same upstream id format don't
collide, and so per-provider semantics (refetch policies, dolt_diff
fanout) don't bleed across sources. The legacy shared `blob_refs`
table has been retired entirely.

The four-column shape is universal — `id` (synth PK
`{owning}#{ref}`) + owning FK + ref id + nullable blake3 — so the
declaration in `schema_raw.rs` is a single `#[derive(CasEdgeRow)]`
struct. The derive emits the DDL, the two indices, the synth-PK
recipe, and the `BulkUpsertable` impl. See
[`provider_migration_dolt_diff_and_cas_edge.md`](provider_migration_dolt_diff_and_cas_edge.md)
for the full recipe.

### Shared attachment-flush primitives

Per-bucket attachment-fetch flow is consolidated into three shared
pieces in `frankweiler_etl::blob_cas`:

  - **`load_blake3_index(pool, table, ref_id_column)`** — one SQL
    scan at fetch entry produces the run-scoped `(ref_id → blake3)`
    map. The per-file dedupe check is a HashMap hit, not a SQL
    round trip queued behind preceding multi-MB CAS commits on the
    single-connection doltlite pool.
  - **`CasEdgeAccumulator`** — per-bucket walker. Three add paths:
    `add_fetched`, `add_known`, `add_failed`. Tracks the
    `BlobBundle`, the `(owning, ref)` edge list, and per-`ref_id`
    error strings. Dedupes by `(owning, ref)`.
  - **`flush_cas_edges(pool, cas, cas_inserts, rows, errors)`** — the
    canonical 3-step end-of-bucket flush: CAS `put_many` →
    `bulk_upsert_in_tx` the edge rows → stamp `last_error` on the
    bookkeeping sidecar for every failed `(synth_pk, err)` pair →
    commit. `CasEdgeAccumulator::flush` delegates to it via a
    provider-supplied row-builder closure.

The blake3 forward-stamp invariant: every edge row carries the
actual content hash. When a file's bytes were already in CAS from a
prior sync, the new `(this_bucket, file_id)` edge row gets the
looked-up hash too — not NULL with the bytes only reachable through
a sibling row keyed by a different bucket. This keeps the
`<provider>_attachments` table self-describing: an edge row tells
you what you need to fetch the bytes, without joining through any
other row.

### Why contacts doesn't participate

Contacts' photo bytes arrive inline in the vCard payload as base64,
decoded once at parse time into `ContactPhoto { bytes, content_type }`,
written straight to `blobs/<uid>.<ext>` at render. They never touch
a CAS edge table or `cas_objects` because there's no separate fetch,
no separate upstream id, and no skip-check semantics needed — the
bytes are a property of the entity, not a separate resource.

If a future provider has the same shape, inline-in-payload is fine;
the shared CAS exists for the fetch-as-separate-resource pattern.

## Auth and credentials

Two patterns:

  - **Most providers**: shell out to `latchkey curl` (see
    [`backend/etl/src/latchkey.rs`](../../frankweiler/backend/etl/src/latchkey.rs)).
    Auth lives in the latchkey keyring, indexed by URL host. The
    provider's HTTP transport never sees the bearer token.
  - **Yolink**: latchkey doesn't know about `us.yosmart.com`, and the
    consumer download path isn't bearer-authed — the URL itself is
    signed (`build_signed_url` in
    [`providers/yolink/src/extract.rs`](../../frankweiler/backend/etl/providers/yolink/src/extract.rs)).
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

  - **No-preseed listing flow.** Earlier versions pre-seeded entity rows from
    the listing pass with `payload IS NULL` so a crashed detail fetch left a
    row visible to the retry walk. We reversed that: rows appear only *after a
    successful detail fetch*. A pre-seeded row is a tri-state shape (doesn't
    exist / pre-seeded / fully fetched) that doesn't fit the `WirePayloadRow`
    derive and forces a hand-rolled UPSERT path diverging from
    `bulk_upsert_in_tx`, and the skip-check works just as well without it: the
    listing pass bulk-reads `(id → stored.update_time)`, compares to
    upstream's `update_time`, and routes each id to `missing` / `stale` /
    `up_to_date`. "missing" already covers crashed fetches — the next sync's
    listing re-surfaces them. Failed detail fetches still record `last_error`
    in `<table>_bookkeeping` via `record_object_error`; a silently dropped id
    (process killed mid-fetch) leaves no row, which is fine — the next listing
    surfaces it as missing.

  - **Always-paired bookkeeping.** Every object table has a sidecar
    `<table>_bookkeeping` carrying `attempt_count`, `last_attempt_at`,
    `last_error`. A success bumps `attempt_count` and nulls
    `last_error`; a failure bumps `attempt_count` and records
    `last_error`. Either way, the per-row paper trail exists.

  - **Blobs follow the same shape.** The per-provider
    `<provider>_attachments` edge table carries a nullable `blake3`
    (NULL = not yet stored in CAS); its `_bookkeeping` sidecar
    records attempt count and last error. A failed blob fetch leaves
    a `(ref_id, blake3=NULL, last_error=…)` edge row that a retry
    walk picks up.

  - **Retry-on-by-default, with opt-out.** The orchestrator takes a
    flag — call it `--retry-failed` (default `true`) — that says
    "before any normal walk, re-fetch every row where
    `last_error IS NOT NULL` or `payload IS NULL AND attempt_count > 0`,
    same for the per-provider CAS edge tables." Pass
    `--no-retry-failed` to skip the retry pass (useful when the
    upstream is known-flaky right now and you want a fast incremental).

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

The retry-failed flow above is *between* runs — durable evidence survives sync
exit. **Within** a single run, transient signals also drive intra-run
backoff: Slack's 429 handling (in `extract/api.rs`) implements `Retry-After` +
exponential backoff before giving up and moving on, and that pattern
generalizes. This is an HTTP-transport concern, independent of the
no-preseed change. Providers should prefer intra-run backoff for
fast-recoverable failures (rate limits) and fall through to the
durable-evidence path only for failures that need a fresh run to clear.

### Bounded backlog

A pure "leave it in the table forever" policy can grow a permanent
backlog of e.g. private-channel 403 rows that will never succeed.
The retry policy's "give up after N days" / "give up after N
attempts" knob is what bounds this. Beyond that, periodic cleanup
of rows whose `last_attempt_at` is older than the retention window
keeps the backlog from growing unboundedly.

## What this document does not cover

  - Testing, adding a new provider, schema evolution, the downstream
    translate/load contract, and the open questions — see the companion
    [`data_architecture_ingestion_practices.md`](data_architecture_ingestion_practices.md).
  - The specific table DDL of any provider — see the port guide and
    each provider's source.
  - The UI and how it consumes `grid_rows` — see the frontend docs.
  - The qmd index and how it's built — see [`docs/dev/edges.md`](edges.md)
    and the `qmd_indexer` crate.
  - Anything about hosting, multi-user, or replication — explicitly
    out of scope. This is a single-user, single-laptop system.
