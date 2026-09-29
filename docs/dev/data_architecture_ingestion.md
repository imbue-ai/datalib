# Data architecture: ingestion

# Introduction and Context
We have an incremental, resumable, layered ETL-shaped architecture that downloads raw data from many upstream sources and stores it as **JSON API responses preserved** in versioned doltlite tables, with attachment **BLOBs in a content-addressable store** (CAS, a plain SQLite sibling database per source), then applies transformations (rendering, indexing) and presents the rendered data in a UI. Every other store the pipeline writes is doltlite: the raw stores, each source's render store and the grid index. The CAS is not, because a content-addressed table is its own history.

Parts of this are not novel — the data pipeline aspect shares shape with Flume / Apache Beam / Dask / Prefect / Airflow ETL pipelines. What we optimize for that those tools don't:

- Single user, single laptop
    - No cluster, no scheduler service, no DAG server
- Easy to install, configure, run, and monitor
    - One config file, one orchestrator binary, one local data directory
- User can "own" their data
    - It exists in files they can see and inspect themselves in non-proprietary formats.

This document describes the principles for the **ingestion (download) side**: how raw data lands on disk, what shape it has at rest, and the operational properties (monitorable, stoppable, resumable, incrementally cheap, verifiable) the download stage aims for. A new provider, table, or transformation should be judged against it, and divergences should be either justified or fixed.

## Related documents
The **parse and render stage** — deserializing a stored payload, projecting it to `GridRow` + markdown, its data-quality rules, its incrementality, and the `GridRow.created_at` policy — is [`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md). The tables render writes into are covered by [`grid_rows.md`](grid_rows.md) and [`edges.md`](edges.md). The practitioner's companion — testing, adding a provider, schema evolution, open questions — is [`data_architecture_ingestion_practices.md`](/docs/dev/data_architecture_ingestion_practices.md). The shared code and its rules — keys, sidecars, the doltlite pool rules, schema changes — are in [`etl/README.md`](/datalib/backend/etl/README.md).

# General pipeline structure

The pipeline has three stages, each running as a **subprocess step under the `datalib-dag` DAG runner** ([`datalib/backend/dag`](/datalib/backend/dag)) — one process per step, each step an invocation of the `datalib-step` binary ([`datalib/backend/datalib_step`](/datalib/backend/datalib_step)); see [`datalib/backend/dag/README.md`](/datalib/backend/dag/README.md) for the runner's rules and [`step_protocol.md`](step_protocol.md) for the step contract:

1. **Download** — pull from upstream, UPSERT into `<data_root>/<group>/ingest/entities.doltlite_db` (entities) and `<data_root>/<group>/ingest/blobs.sqlite` (a single `cas_objects` table keyed by blake3 hash).
2. **Render** — derive `.md` files under `<group>/render_markdown/` plus that source's render store (`render_markdown/indexed_markdown.doltlite_db`) from the raw store, deterministically. Indexing with qmd is the source's separate `keyword_index` and `embed` steps.
3. **Grid index** — read every source's render store into the one `grid_rows` table the UI's grid reads.

Each provider (data source) is **three** crates at [`datalib/backend/etl/providers/`](/datalib/backend/etl/providers): `datalib_etl_<name>_config` is the config schema (serde structs, nothing else), `datalib_etl_<name>` downloads, and `datalib_etl_<name>_render` renders. The download crate owns its bins, its integration tests, and the sample fixtures the tests run against — keeping sample data next to the code under test serves as documentation of "what this provider's wire format looks like." The split is what keeps the render schema off the download side; see AGENTS.md §"Ingest and render are separate crates".

## Layering of concerns: download is downstream-agnostic
The per-stage modules within a provider crate form a strict layer with a single allowed dependency direction:

```
upstream → download → render → grid_index
```

- **`download`** owns the bytes-at-rest. It fetches from upstream and persists into `<data_root>/<group>/ingest/entities.doltlite_db`, and nothing else. It must NOT depend on `render`, `datalib_schema::grid_rows::GridRow`, the render store, or the qmd index. The per-provider `schema_raw.rs` rustdoc deliberately avoids describing how render consumes the tables.
- **`render`** depends on `download` (it reads the raw store and projects to the normalized POD + `GridRow` shape). `ingest::schema_raw` is part of the contract render consumes. **Render reads only the raw store, never the original source.** Its sole input is `<data_root>/<group>/ingest/` (`RenderCommon::raw_path`); it must never reach back into upstream (the API) or into a file-backed method's `path` (the `.mbox`, the Takeout export, …). Render shows us **what we have captured and internalized**, not what is currently live at the source.
- **`grid_index`** is provider-agnostic; it lives at [`render/src/grid_index.rs`](/datalib/backend/etl/render/src/grid_index.rs) (`build_grid_index`) and depends on no provider's download or render. Its input contract is the per-source render store, so a new provider needs no grid_index-side change.

Why the discipline matters: download is its own deliverable — a user can run it, stop, inspect the raw store, and have something useful (a backup, or mirror, at the very least) even if render has bugs or hasn't been written yet. Render can then be re-implemented or extended without touching download, and disabling a render path for one provider doesn't disturb that provider's download.

Why render's read-only-from-the-raw-store rule matters: the raw store is the boundary between "the outside world" and "our copy." Once download has captured the bytes, everything downstream is reproducible offline and stable — re-rendering yields the same result whether or not the upstream still exists, has changed, or is reachable. The original export can be deleted, the API token can expire, the phone backup can be wiped: render still produces exactly what we hold. The store's location is fixed by the ingest step's id (`<group>/ingest`), and a symlink there is how it moves to a bigger disk; the renderer reads the directory its input names and nothing else, so it follows without caring.

# Schemas first, but also simple
> *"Show me your flowchart and conceal your tables, and I shall continue to be mystified. Show me your tables, and I won't usually need your flowchart; it'll be obvious."* -- Fred Brooks, The Mythical Man Month (1975)

The single most load-bearing principle of this whole document is that **the schema is the design**. When we add a new data source, or sketch a new feature, the **first** artifact is the table — its columns, its primary key, its uniqueness constraints, its foreign-key relationships, and inline comments explaining what each row and column *means* and why it is there.

Concretely, when starting any non-trivial piece of work in this codebase:

1. **Write the DDL first**
2. **Document each table *in the same file as the DDL***. Per-provider `schema_raw.rs` files (`etl/providers/<p>/src/ingest/schema_raw.rs`) are the canonical home for both the `CREATE TABLE` text and the prose commentary on it. Tables without their prose are half-finished.

## Download schemas must be simple: mostly PK + payload as JSONB
We don't want our DB schema tightly coupled to upstream schemas, so we don't try to translate upstream data into a complete set of SQL columns.

Rule: download schemas should often be extremely simple — often just a stable primary key and a JSONB payload column.

The download portion of our system captures raw data from sources in its native format with as little translation as possible: typically JSON payloads as they arrived off the wire, with enough indexing that related payloads can be updated and grouped together.

A few cases justify more schema:

- When the JSON payload is missing contextual data the requester knew (account_id, say), it can be stored alongside the payload as extra columns in the table.
- If the payload returned by the data source includes fields that always change regardless of the object's state, like a fetch time, then we move those fields into the bookkeeping sidecar so that they don't churn the main payload table ([`etl/README.md` §"Volatile fields"](/datalib/backend/etl/README.md)).
- For attachments (which are stored separately in a BLOB CAS), we need to know which attachments belong with which payloads. A dedicated edge table links payloads and BLOBs many-to-many.

## Object identity: Ship of Theseus on UUIDs
We lean **heavily** on upstream-provided UUIDs to establish permanent object identity.

- Every raw-store entity table keys by the upstream provider's identifier — no surrogate `AUTOINCREMENT`. That's what makes `dolt diff` stable across re-fetches, what makes `ON CONFLICT(id) DO UPDATE` work, and what makes cross-table references (e.g. `messages.conversation_id`) mean something ([`etl/README.md` §"Raw stores: the primary key is the upstream id"](/datalib/backend/etl/README.md)).
- When an upstream doesn't expose a stable UUID, we **synthesize one via UUIDv5** from a per-provider namespace and the most stable available fields, in a recipe function in the data source's `schema_raw.rs`. Such a key is the raw store's own; the rendered id is minted from it by `datalib_id`, and it becomes that row's `upstream_id`.
- We do **not** use row autoincrement or hashes-of-content as identity for objects. Both break the Ship-of-Theseus property: autoincrement isn't deterministic across re-ingest; content hashes change every time the content does.
- **Sanctioned divergence: a content-hash id when the export carries no stable field at all.** Facebook's export gives most records — posts, comments, the profile — no `fbid` or any other id, so `facebook` mints `uuidv5(table, canonical JSON)` for those. The cost is named and accepted: an edit to such a record is a delete plus an add in `dolt_diff`, never a modification. Reach for this only after confirming there is no stable field; a synthesized UUIDv5 over stable fields is always preferred.
- **A raw key is a function of the download, never of the config.** Nothing under `<group>/ingest/` may embed the source's group id, its display name, or anything else from `config.toml` that says *which configured source* fetched the row: two roots that download the same account under different group ids must produce byte-identical raw stores. The raw store is the backup; the config is how we asked for it. What a raw key may embed is what the upstream gave — its own ids (a Matrix event id, a profile URL), a join of them (`{team}#{channel}#{ts}`), the bytes' hash — and nothing in a raw store mints an entity id; the render mints one from the raw key. The one place a configured source belongs in an id is the *rendered* side, where it is a component of every id precisely so two sources can never overlap — [`entity_ids.md`](entity_ids.md) § "The rule". (yolink's `yolink_devices.id` is the device's config *name*, because the ids YoLink issues are secrets; that is config in a raw key, though not the source's identity, and its config doc says renaming re-keys history.)
- The **projection** side of identity — `GridRow.uuid`, the `upstream_*` backpointers, `source_url`, and the per-provider cross-references the UI links sideways through — is in [`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md#identity-and-backpointers-are-first-class-in-the-projection).

### `schema_raw.rs`: Per-provider schema layout
Within each provider crate the bytes-at-rest schema is its own file, deliberately declarations-only: **`providers/<name>/src/ingest/schema_raw.rs`** holds the raw-store schema — the row structs and their table derives ([`etl/macros/README.md`](/datalib/backend/etl/macros/README.md)) or DDL constants, one per table / index / bookkeeping sidecar; any migration ladder; any synthesized-PK recipe functions; and a small `full_ddl()` composer that splices in `dr::bookkeeping_ddl_for(table)` for each entity. **No manipulation code** — `RawDb`, UPSERTs, SELECTs, and parameter binding stay in `ingest/db.rs` and import from `schema_raw`. Opening the `schema_raw.rs` files at the same fixed path answers "what does the world look like at rest?" without opening anything else.

Each entity table has a JSONB `payload` column holding the raw upstream wire payload, plus a small number of typed columns the writer must populate at insert time (synthesized-PK components, FKs into parent tables that aren't in the payload, namespace discriminators). On disk `payload` is stored as JSONB — purely a storage encoding; the principle is wire-fidelity (see [Wire-fidelity of the raw store](#wire-fidelity-of-the-raw-store)).

**The JSONB convention, concretely.** Doltlite carries SQLite's binary JSON. A `payload` column holds JSONB (a BLOB at the SQLite level): wrap the bind in `jsonb(?)` on INSERT, unwrap with `json(payload) AS payload` on SELECT. The Rust side still binds and reads a text JSON string; only the on-disk representation changes. Three things that are easy to get wrong:

- `ON CONFLICT … DO UPDATE SET payload = excluded.payload` carries the JSONB through the upsert already — **don't re-wrap it**.
- `dr::load_payloads()` unwraps for you; a hand-written SELECT that returns `payload` must call `json()` itself.
- **Don't** wrap `sync_runs.config` / `summary`. Those are tiny single-row bookkeeping where plain-text `SELECT` ergonomics beat binary-parse speed.

Because dropping the `jsonb()` wrapper silently falls back to text storage with no other visible difference, providers assert the encoding directly — `SELECT typeof(payload)` must be `blob`. Five provider test modules do this (email, github, gitlab, notion, slack); a new one should too.

### More details

**Fields derivable from the payload** (`updated_at`, `state`, `name`, `html_url`, `display_name`) — even when we want to query or index them — should **not** be duplicated as stored columns. Use either a `CREATE INDEX … ON t(payload->>'$.path')` expression index or a `VIRTUAL` generated column plus an index over it. Neither scans the table ([query plans](/docs/dev/doltlite.md#query-plans-and-indexes)); the VIRTUAL+index variant also restores `SELECT col FROM t`. Either way, `ALTER TABLE ADD COLUMN … VIRTUAL` (or a new expression index) is a no-refetch additive change against existing user data. See [Schema evolution](/docs/dev/data_architecture_ingestion_practices.md#schema-evolution).

### Events vs bookkeeping: where each column lives
Every entity table `<t>` is paired with a sidecar `<t>_bookkeeping`. The split is load-bearing — three buckets to think about when adding a column:

1. **Upstream payload data** (a Slack `text`, a GitHub `state`, a Notion `last_edited_time`) → lives inside `payload`. If we need to query or index it, use a VIRTUAL generated column + index or an expression index over `payload->>'$.path'`. Do **not** copy it into a stored column.
2. **Writer-supplied identity / joins** (synthesized-PK components, FK references to parent entities the walker knows but the payload doesn't, namespace discriminators like beeper's `source`/`network`) → stored typed columns on `<t>`.
3. **Writer-supplied per-fetch state** (`fetched_at_utc`, `attempt_count`, `last_attempt_at_utc`, `last_error`, `volatile_payload`) → the `<t>_bookkeeping` sidecar.

A per-row resume cursor is the one kind of writer state that lives on the entity table: YoLink's and AirVisual's `<device>.last_ts_ms`, an address book's `ctag` / `sync_token`, a contact's `etag`. Each is a typed column advanced by its own `UPDATE` once the work it vouches for is stored, and left out of the upsert's column list so a re-fetch of the row cannot clobber it.

The split matters because bookkeeping changes on every attempt regardless of upstream change. Storing it on the entity table makes every `dolt diff` noisy, defeats the wire-fidelity of `payload`, and forces re-renders of unchanged content. Keeping it on the sidecar means `<t>` mutates only when upstream actually changed.

**Sanctioned divergence: no sidecar for a snapshot input read whole every run.** `facebook`, `claude_code`, `codex` and `airvisual` read a local export or tree from the start every run; there is no per-row fetch to record, so a sidecar would only churn `last_attempt_at_utc` on every row for nothing. Those tables have no `<t>_bookkeeping`. A provider that fetches records one at a time from a service keeps the sidecar — it is what makes a partial run resumable and a failed row visible.

### Blobs and the CAS split
Attachment bytes are split out of the entity database into a sibling content-addressable store. We do this because:

- `dolt diff` over the entity db stays small and human-grep-able. A re-fetch that picks up one new attachment doesn't drown the commit in a many-MB BLOB row.
- The CAS by nature is append-only.
- Attachments can be big, and a doltlite store is (purposefully) difficult to erase from: a deleted row stays reachable from the commits before it, so even `dolt_gc()` keeps it ([disk space](/docs/dev/doltlite.md#disk-space-and-dolt_gc)).
- Someday we might want to share a BLOB store across multiple data sources (Perkeep-style).

A source with attachments has both `<group>/ingest/entities.doltlite_db` (entities + a per-provider edge table mapping `(owning, ref) → blake3`, e.g. `slack_attachments`) and `<group>/ingest/blobs.sqlite` (`cas_objects` keyed by blake3). The code is [`blob_cas.rs`](/datalib/backend/etl/src/blob_cas.rs); why the bytes always commit before the rows naming them is in [`etl/README.md` §"Blob CAS and per-provider edge tables"](/datalib/backend/etl/README.md).

**Per-provider CAS edge tables**

Every provider with attachments owns a small four-column edge table that maps `(owning_id, ref_id) → blake3`. Bytes live in the shared `cas_objects`; the edge table is provider-specific so providers that happen to use the same upstream id format don't collide, and so per-provider semantics (refetch policies, dolt_diff fanout) don't bleed across sources.

The four-column shape is universal — `id` (synth PK `{owning}#{ref}`) + owning FK + ref id + nullable blake3 — so the declaration in `schema_raw.rs` is a single `#[derive(CasEdgeRow)]` struct ([`etl/macros/README.md`](/datalib/backend/etl/macros/README.md)). The render side of the same edge — how a document's attachments are loaded and materialized — is in [`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md).

The skip-check ("do we already have these bytes?") is keyed by the **upstream identifier** (known before fetch), not by content hash (only known after). The per-provider edge table is the cache index over the CAS.

### Shared attachment-flush primitives
Per-bucket attachment-fetch flow is consolidated into three shared pieces in `datalib_etl::blob_cas`:

- **`load_blake3_index(pool, table, ref_id_column)`** — one SQL scan at fetch entry produces the run-scoped `(ref_id → blake3)` map. The per-file dedupe check is a HashMap hit, not a SQL round trip per file.
- **`CasEdgeAccumulator`** — per-bucket walker. Three add paths: `add_fetched`, `add_known`, `add_failed`. Tracks the `BlobBundle`, the `(owning, ref)` edge list, and per-`ref_id` errors. Dedupes by `(owning, ref)`.
- **`flush_cas_edges(pool, cas, cas_inserts, rows, errors)`** — the canonical end-of-bucket flush: CAS `put_many` → one transaction that `bulk_upsert_in_tx`s the edge rows and records each failed ref through `record_object_attempt` (or `record_object_skipped`, for a ref deliberately not fetched), which writes the sidecar and the `problems` row → commit. `CasEdgeAccumulator::flush` delegates to it via a provider-supplied row-builder closure.

## [Doltlite](https://github.com/dolthub/doltlite) is our primary raw store

For raw ingestion, each data source owns a directory `<data_root>/<group>/ingest/` holding up to two DBs:

- entities.doltlite_db: Event payloads and metadata, attachment edges
- blobs.sqlite: a CAS of BLOB data specific to that source.

That directory is the ingest step's tree, `<data_root>/<group>/ingest`,
identically for every source: the step writes only the tree its id
names. A config that still carries the retired `common.raw_path` key is
refused, and the refusal says to move the store with a symlink instead.
One resolver serves both sides (`SourceCommon::resolve_paths`,
`RenderCommon::resolve_paths`): the downloader writes there and the
renderer reads there, the latter through its `inputs`. The filenames
inside it (`entities.doltlite_db`, `blobs.sqlite`, `events/`) are
the constants in `datalib_etl::raw_layout`, the one place the layout is
defined. This is distinct from a file-backed method's `path`
(`[steps.params.mbox] path = …`), which says where the data is read
*from* (a `.mbox`, a Takeout export, …).

We use doltlite because:

- At the API level, it effectively "is-a" sqlite, supporting all sqlite behavior (JSONB, etc.)
    - Except it has its own binary format and thus needs a differently compiled sqlite binary ([`doltlite.md`](doltlite.md), which is also where everything the engine does is written down).
- It supports data versioning (commit, branch, merge, tag, etc.)
    - Different versions of the data are stored space-efficiently.
    - SQL operations (even DROP TABLE) do not actually delete anything.
    - It can enumerate deltas between any two versions of the data (including "DROP TABLE" and recreate with new schema), enabling incremental processing ("what changed since commit X (the last I saw)?")
    - The same deltas answer a question a plain mirror can't: **what did the upstream quietly change or delete since we last looked?** See [Noticing when the *upstream* loses data](#noticing-when-the-upstream-loses-data).
- Thad believes it is the future for local-first software: "skate to where the puck is going to be."

We acknowledge these risks:

- It is very young technology and changing quickly.
- There's a small space and time penalty for the versioning.

If we had to, we could return to plain-old-sqlite, with these options:

- Drop support for data versioning and incremental data processing (always rerender)
- Implement "what changed since moment X" ourselves

## Wire-event tape (JSONL)
But doltlite is also a binary file you need a tool to open. So alongside the doltlite raw store, Slack's download also writes a **plain-text, append-only JSONL log of what came off the wire, for debugging. It can be safely deleted.** It is the only provider that does; the `common.event_tape.enabled` config key (default on) turns it off.

This is the simplest view of the raw data: one event per line, in the order the downloader saw it. No schema, no migrations — just a tape you can `tail -f`, `grep`, `jq`, or open in any editor.

Layout — one file per entity table:

```
<data_root>/<group>/ingest/events/
  <table>.jsonl                       # one line per upserted row
```

Each line is a small JSON object:

```
{
  "_recorded_at": "2026-06-10T14:22:31.041203-07:00",
  "table": "messages",
  "id": "C0123:1717982351.000200",
  "payload": { ... }     // the wire bytes
}
```

Rules:

- **The pipeline never reads it.** Render, grid_index, resume, retry — all of those go through doltlite. Deleting the `events/` directory does not break anything.
- **Same bytes as the upsert.** The tape is written next to the `ON CONFLICT(id) DO UPDATE`, after its commit, so it carries the same wire-fidelity payload that the doltlite row gets. No second parse, no second normalize. A tape write that fails is logged and does not fail the upsert.

# Operational principles
## Monitorable
The first sync from a given source is often very long (hours to days, many GB, subject to rate limits). Every stage must surface progress the user can watch in real time.

- Every binary flattens [`obs::ObsArgs`](/datalib/backend/obs/src/lib.rs) into its clap parser, so every stage takes the same logging / OTLP flags. On a TTY, pretty log lines on stderr; otherwise JSON. Log emissions route through an `IndicatifWriter` coordinating with the shared `MultiProgress` (`datalib_obs::shared_multi()`) so progress bars don't get stomped by log lines. Where each log line goes and how to read it: [`logging.md`](logging.md).
- `--otlp-endpoint http://host:4317` exports spans + events via OTLP, so a single Tempo/Jaeger collector can ingest every stage. (See [the privacy-boundary unresolved question](/docs/dev/data_architecture_ingestion_practices.md#observability-and-the-privacy-boundary) for the contract that constrains what may be in those spans.)
- A provider's standalone download binary ends with a `*_download_complete` event (`slack_download_complete`, `jmap_download_complete`, …) carrying the counts from its `FetchSummary`.
- Long-running operations must report something visible every few seconds; a download that walks 100k items silently for an hour is a bug.

## Stoppable and resumable
A sync that gets interrupted — ^C, OOM, laptop sleep, upstream 5xx — must be able to make forward progress on the next run. We **do not require runs to complete to be useful**.

The dedup index *is* the resume cursor:

- Provider-side dedup keys every UPSERT on the upstream identifier, so re-walking already-fetched items is cheap and correct.
- There are no separate checkpoint files. The data we already have tells us where to resume. (One narrow exception, for config changes rather than for resumption: see [Cursor / resume strategy](#cursor--resume-strategy).)

## Efficiently incremental
Any subsequent sync should pick up as close as possible to where the last one left off: walk what the upstream API forces us to walk, although it should also be safe to fetch with a bit of overlap, too.

Two layers do the work:

- **Provider-side dedup**: every UPSERT uses the upstream identifier as PK with `ON CONFLICT(id) DO UPDATE`; unchanged rows are no-op writes, and the run's closing commit finds nothing to commit.
- **Render-side dedup**: render diffs the raw store from the commit its `render_cursor` row names and renders only the buckets that moved; every document it renders is written, and an unchanged one writes an identical row, which doltlite's content-addressed tables store as no change. The grid_index step reaches a document at all only when that store's `dolt_diff` surfaced it (see `source_cursors`).

Different upstreams expose different surfaces for "what changed since X", and that drives the cursor pattern (see [Cursor / resume strategy](#cursor--resume-strategy)).

**Both layers rest on writes being surgical, and that is easy to break by accident.** The diff is cheap because it is proportional to what actually changed; a write that touches every row produces a diff the size of the table, and every consumer downstream re-does all its work for nothing. So a record that is *unchanged* upstream must serialize *identically* to itself, byte for byte, on every fetch.

That is what makes [`AGENTS.md`'s "give a bag an order before storing it"](/AGENTS.md) an architectural rule rather than a tidiness one: an API that returns a set in a different order each time will, left alone, manufacture a diff out of nothing — and the symptom is not an error but a pipeline that quietly stops being incremental. The same reasoning covers volatile paths (`VolatilePath`, which drops a per-fetch stamp that carries no information into the sidecar) and any future rule of that shape: canonical field order, stable number formatting, excluded bookkeeping. When you add one, say which of the two it is — **sorting keeps the signal and removes the noise; declaring a field volatile throws the signal away too** — because the two look interchangeable and are not.

The property this protects is what a consumer actually buys with a cursor; [`toolchain_for_agents.md`](plans/data_lib_as_a_library/toolchain_for_agents.md#the-incremental-contract-is-a-capability-not-a-protocol) makes the case for it as the thing datalib offers anyone building on it.

## Wire-fidelity of the raw store
The raw store preserves the **semantic content** of upstream responses verbatim — every field, every value, with no loss and no pre-shaping into our internal model. The on-disk *encoding* of that content is a separate question; we pick whichever encoding is human-readable and inspectable. Concretely:

- **JSON-shaped sources** (HTTP API responses from Slack, Claude, Notion, GitHub, etc.) store the response JSON verbatim, as JSONB.
- **Binary-protocol sources** (Signal's encrypted protobuf backup, future binary feeds) are **decoded** at download time into JSON of equal semantic content. Encryption layers, compression, binary wire encodings, and other transport-level packaging are artifacts of how the data got to us, not part of the wire data itself. Storing them raw on disk would be "too raw" — the point of the raw store is that a human can `grep`/`jq` it without a decoder in the loop.
- **File-imported sources** (mbox `.eml` bytes, vCard `.vcf` files, WhatsApp `msgstore.db`, Beeper `index.db`) promote the *semantic* content (typed columns, JSONB payloads) into the entity tables. **File-tree imports go through download** just like API-backed sources: a directory of `.vcf` files lands in the same raw-store row shape CardDAV produces, an mbox lands in the same shape JMAP produces. Render has exactly one input contract per provider regardless of whether the data came over the wire or off disk.

The rationale: **if all we wanted was a copy of the upstream bytes, we'd just use `cp`.** The raw store earns its keep by being *queryable and human-inspectable* in a way the original bytes aren't — JSONB rows, typed columns, predictable structure across providers. That's the criterion for "is this decoding step OK at download time?" If the alternative to decoding is asking the user to install a special tool to see their own data, the decoding belongs in download.

Two rules follow:

- **Normalize at render time, not download time.** Decoding a binary wire encoding to JSON of the **same** semantic content is **not normalization** — every field upstream sent us is still present, with the same values. Normalization means pre-shaping into our internal model (renaming fields, collapsing shapes, dropping subtrees, projecting), which we defer to render.
- **Don't pollute payloads with downloader-synthesized keys.** `_fetched_at`, `_listing_update_time` etc. are bookkeeping, not upstream data; promote them to real columns on the entity table (or its `_bookkeeping` sidecar), not into the JSON.

Corollary: **the raw store is the source of truth; downstream stages are rebakeable.** Anything we render, project to `grid_rows`, or index into qmd can be recomputed from raw without re-touching the network. A provider's `RENDER_VERSION` is the explicit lever for "force a rebake of every document even when payloads are unchanged."

## Verifiable via a reset
A long chain of incremental syncs can in principle silently drop data (an upstream that doesn't surface a deletion, a cursor that skipped a page on a 5xx, a bug in our delta logic). One check is to empty the store, refetch from scratch, and **let dolt's diff tell you what was missing**.

Reset and sync are two operations, and no provider knows about the first: `datalib-dag --reset <source>/ingest` empties every table of the raw store and commits, so the rows (the run log included) stay in history; the runner forgets the step ever ran; the next sync finds empty tables with no cursor and walks from the start, exactly as a new source does. The CAS edge table goes with the rest, so every attachment is fetched over the wire again; the re-fetched bytes hash to the same blake3, `INSERT OR IGNORE` into `cas_objects` is a no-op, no disk grows. Nothing resets the CAS. To get its space back, delete `blobs.sqlite` by hand **and** reset the ingest step: deleting only the file leaves edge rows naming bytes that are gone, and the download will not fetch what its edge rows say it has. `--reset` alone stops there; `--reset X --sync X` does both in one invocation. `datalib_etl::doltlite_raw::reset_store` is the whole of it.

Nothing garbage-collects `cas_objects`: bytes are byte-stable and nothing in the tree deletes them. See [Removing a source](/docs/dev/data_architecture_ingestion_practices.md#removing-a-source) for the open design.

### A 200 can be wrong, and nothing in the store says so

The quieter way to hold bad data is not a dropped row but a **degraded
read**: the upstream answers `200` with a well-formed record that is
missing something. A manual e2e bake caught one — ChatGPT's
`/backend-api/me` returned `first_name: null` on one fetch and the
account's real first name five minutes later, with every other field
byte-identical. Not a profile change; a bad read on their side. To the
pipeline the first response was a success: the row was written, the
commit recorded it, and no error, no `problems` row, no sidecar field
marks it as suspect, because there is nothing to detect it on. A
partial record is indistinguishable from a record that really looks
like that. It is not a one-off: another bake had claude.ai return a
conversation with `files[].size_bytes` null on one fetch and populated
on the fetches either side of it.

Incrementality then preserves the mistake. A [listing-diff
provider](#cursor--resume-strategy) re-fetches a record only when its
listing stamp moves, and a degraded detail fetch does not move the
stamp, so the stale field sits until the upstream edits that record for
some other reason — possibly never. The `me` row healed on the next run
only because it is one of the few fetched unconditionally every time.

There is no signal to gate on, so the only check is to fetch again and
let the diff say what changed: **a reset and a resync, run now and
then rather than only when something looks wrong**. A field that
"changes" across a reset on a record the upstream did not touch is the
signature; the run-3 stability check in the manual e2e bake
([`testing.md`](testing.md) §"Manual e2e live-sync golden") is the same
test applied deliberately.

## Noticing when the *upstream* loses data

The versioned raw store is usually argued for on incrementality — "what
changed since commit X" is how render and the grid index avoid redoing
work. The same property answers a question a plain mirror cannot answer
at all: **what did the provider quietly change or delete since we last
looked?**

A "download the latest state" mirror overwrites itself, so an upstream
deletion is indistinguishable from a row that was never there. Here
every sync is a commit, so a row that vanished upstream is a `removed`
in `dolt_diff` with its last value still on disk. That is close to the
point of the project: the reason to keep your own copy is that the
provider's copy is not under your control.

It is also the good side of a property we criticize elsewhere.
That a commit keeps every row it ever reached
([disk space](/docs/dev/doltlite.md#disk-space-and-dolt_gc)) is a real cost for
[derived intermediates](plans/data_lib_as_a_library/toolchain_for_agents.md),
which we could always rebuild. On the raw store it is the feature.

### What is on disk

The commits are the durable record: `dolt_diff` between any two refs
answers this for any window, for every provider.

There is also a precomputed per-run summary, and it is narrower than it
looks. `DownloadRun::finish`
([`download_run.rs`](/datalib/backend/etl/src/download_run.rs)) counts
`dolt_diff_<table>` by `diff_type` for every table in the store and
writes `{table: {added, modified, removed}}` to
`sync_runs.summary.deltas`.

The diff runs from the HEAD the run *started* at, which `DownloadRun`
captures before the provider writes anything. That has to be the
anchor, and it is the one thing to preserve if you touch this code:
a streaming provider commits part-way through so the render step can
start on what has landed, which leaves those earlier batches clean and
gone from `dolt_status`. Measured from the store's last commit instead,
the summary reports only the final batch — silently, since a smaller
number looks like a smaller run. `deltas_span_a_mid_run_commit` in
`download_run.rs` is the guard.

**Only 9 of the 28 providers with a download side use `DownloadRun`**
(beeper, calendar, chatgpt, claude, email, github and gitlab through
`forge-ingest-common`, notion, slack). The other nineteen write no
`sync_runs` row and no deltas — their history is still in the commits,
but nothing precomputes it.

One thing `removed` does *not* mean: it counts rows **our downloader
deleted**, not rows the provider stopped serving. Those coincide only
for a provider that deletes on absence.

### Snapshot inputs

A source whose input is a *complete* snapshot — an export, a phone
backup, a folder that is the whole collection — learns what was deleted
by comparing: what the input holds now is the enumeration, and a stored
record it does not hold goes. Every such source does this on an ordinary
sync, and the old rows stay in history, so `dolt_diff` still says what
went. How a file-backed source knows what changed:
[`etl/README.md`](/datalib/backend/etl/README.md#answering-did-it-change-for-a-file-backed-source).

The condition is the whole rule: **absence in the input has to mean
deletion.** Three things break it, and each source guards against the
ones it can meet:

- **An input that evicts.** For a cache, absence means "not cached
  here", which is why [`beeper`](/datalib/backend/etl/providers/beeper/INGEST.md)
  prunes nothing.
- **A read that failed.** A file that did not parse, a backup frame that
  did not decode or a walk that reported an error says nothing about the
  records it would have held, so the source deletes nothing it could
  have come from, and says so.
- **A part the person left out.** An export's request form lets a person
  pick categories (Takeout's products, LinkedIn's and Facebook's data
  categories), and one requested without a category looks the same as
  one whose category was emptied. So a category missing from the export
  entirely deletes nothing; inside a category that is there, what is
  missing was deleted.

`always_clear_before_ingest`, a config key that emptied a source's store
before every ingest, is retired: it fell into the third trap for any
partial export, and every source it served prunes on its own.
`datalib-http` takes it out of an old config by itself
(`datalib_migrate_config::upgrade`). `datalib-dag --reset` is the same
wipe, asked for once.

A Takeout feed read from one file (Maps reviews and saved places,
YouTube, Gemini) treats that file as its whole table: re-read, it
replaces the table and deletes what it no longer lists
(`file_checkpoint::ingest_snapshot`).

Every Takeout feed draws the line at what the request form lets a person
leave out. A product missing from the export entirely (no `Google Chat/`,
no `Voice/`, no `Maps/Photos and videos/`, or a single-file feed's file)
deletes nothing and keeps its cursor, because an export requested without
it looks the same as one whose product was emptied. Inside a product that
is there, what is missing was deleted. A single-file feed's product is
its file, not its folder: YouTube's `history/` can hold search history
without watch history when only one was ticked. What this cannot see is
a product split across the zips of a large Takeout and unpacked from
only some of them.

### What limits it

**Detection needs a re-enumeration.** What each source re-enumerates, and
therefore what it can see:

| source | re-enumeration | prunes |
| --- | --- | --- |
| `email` (JMAP) | `Email/changes` / `Mailbox/changes` tombstones | emails, mailboxes, and the label joins |
| `email` (Gmail) | `history.list` deletions; a full walk when the cursor ages out | emails, via the same cascade |
| `contacts` (CardDAV) | RFC 6578 sync-collection `404`/`410` | contacts |
| `contacts` (`.vcf` folder), `calendar` (`.ics` folder) | the folder's scan, and each re-read file | a gone file's address book or calendar; cards or events a re-read file dropped. Nothing is deleted when the walk reported an error |
| `google_takeout` Chat, Maps photos | the export's scan, and each re-read `messages.json` | a gone file's user, group, messages or photo; messages a re-read file dropped. A missing `Google Chat/` or photos folder deletes nothing |
| `google_takeout` Maps reviews and saved places, YouTube, Gemini | each re-read file, which is the feed's whole table | records the file no longer lists, and a Gemini activity's attachment edges. A missing file deletes nothing |
| `email` (mbox), `sms_backup_restore`, `google_takeout` Voice | a read of every file, whenever one was removed or rewritten | records no file holds any more. Nothing is deleted when the walk or any file's read failed, or when Takeout's `Voice/` is missing |
| `slack` | the trailing `refresh_window_days` re-walk, and each `conversations.replies` thread | messages inside the walked range; replies on a re-fetched thread |
| `github` / `gitlab` | every PR's / MR's whole child list, per fetch | deleted comments, reviews, discussions |
| `claude` (`api`) | `/chat_conversations`, one org at a time | that org's conversations |
| `chatgpt` | `/conversations`, when the walk reached `total` | conversations |
| `fsindex`, `pdf` | truncate-and-refill | structurally |
| `media` | the scan | the rows of every path the scan did not see |
| `claude` (`export`) | the export is the enumeration | users, projects, docs and conversations it no longer holds; a missing `users.json` or `projects/` deletes nothing |
| `signal` | each newly read backup | every record the backup no longer holds, with its attachment edges. Nothing is deleted when a frame did not decode |
| `linkedin`, `facebook` | each CSV or JSON file the export holds | the rows a present file no longer holds; LinkedIn's photo edges of gone connections, Facebook's media edges of gone records. A missing file deletes nothing, and Facebook prunes no table one of whose files did not parse |
| `apple_messages`, `apple_photos`, `lightroom`, `whatsapp` | the mirrored SQLite file | structurally: each table is refilled from it |
| `yolink` | — | nothing; append-only telemetry |
| `notion`, `beeper` | — | not wired (rework; poorly supported) |

**The hard part is not the deletion, it is establishing the
enumeration was complete.** Every gate above exists because some
ordinary condition makes absence meaningless: a `since` cutoff or page
cap that stopped the walk early, a label filter narrowing it
server-side, an org whose listing `403`'d, a request that simply
failed. That last one is the sharpest — a failed list request yields an
empty list, which is byte-identical to "everything was deleted", so
`unwrap_or_default` on a listing is a correctness bug, not a
convenience.

**The gate is the whole safety story; nothing second-guesses how much a
prune deletes.** A prune is a commit, so the previous commit still has
the rows: `dolt_diff_<table>` names them and
`dolt_at_<table>('HEAD^1')` reads them back
([`doltlite.md`](doltlite.md)). Nothing is lost, so a veto on a large
prune would protect nothing — and it would leave the store holding rows
upstream no longer has, with nothing recording the divergence and a full
re-download as the only remedy. This is the concrete payoff of a
version-controlled raw store: we can act on our best reading of an
ambiguous signal and let the history be the safety net, where a plain
mirror would have to choose between guessing and freezing.
`prune::record` WARNs on an unusually large prune — a signal to
investigate, not a veto.

**A deletion the download notices reaches the grid** — see
[parse and render § "The sweep"](data_architecture_parse_and_render.md#the-sweep).
Keep the two apart when reading a bug report: "we never noticed" (this
section) and "we noticed and the grid still shows it" (that one) look
identical from the UI.

Every provider that records deltas can also detect a deletion, and
`media` / `fsindex` / `pdf` detect structurally while recording none;
the one mismatch is that the structural detectors write no `sync_runs`
row.

**False positives track canonicalization.** An unchanged record that
serializes differently from itself manufactures a `modified` (see
[Efficiently incremental](#efficiently-incremental)); claude.ai
returning a project's `permissions` in a different order on different
fetches is the worked example. A spurious re-render wastes CPU; a
spurious "your provider changed this" wastes trust.

### The gap

Nothing reads `summary.deltas` back — nothing in `datalib/backend/http`
or `datalib/ui` does. The only place any of it reaches a human is
`fsindex`'s standalone CLI printing `vs last scan: N added, M modified,
K removed, U unchanged`, which is one provider's local convenience
rather than a product surface. `sync_runs` also records no commit
hashes (its columns are `run_id`, `started_at_utc`, `finished_at_utc`,
`tz_offset`, `config`, `status`, `summary`), so recovering the exact
commit range for a past run means reading `dolt_log` by hand.

Detection is available; delivery is partial. The part that is
delivered is the delta itself, as a thing a person can read — see the
next section. The rest — `summary.deltas` read back, a run's commit
range recorded in `sync_runs` — is #513.

### The delta as a source: diff groups

The commits answer "what changed?" at the level of rows; a person asks
it at the level of the things the rows make up. A **diff group**
(`type = "diff"`, `source = <group>`, two raw commits under
`params.diff`) is the source's own render step run at both commits and
subtracted, written as an ordinary render tree — documents with the
changes marked, `grid_rows` with `diff_status` set — so everything that
serves a source serves the difference. "Compare two versions…" on a
Manage row opens its commit history, where one is written; [`config_model.md`](config_model.md) has the
shape and [`plans/completed/diff_renderer.md`](plans/completed/diff_renderer.md) the design.

What a diff can show is bounded by what the ingest carried into the
store. A provider that syncs forward from a cursor — an API
`conversations.history` bounded by `oldest` — brings in what is newer
and never sees a deletion, so a diff over such a source shows adds and
edits but not removals until something re-walks the range. A provider
that reads a whole export or file each time (the `.vcf` address books,
a Takeout tree) sees deletions on every sync. When you build a
provider, this is one more reason to prefer the full re-read where it
is cheap, and to record in `scope_config` what a cursor was taken
under when it is not.

**`deleted_upstream_at` is specified but not built.** [Transient vs
non-transient](#transient-vs-non-transient) below says a confirmed 404
should carry that marker; no such column exists anywhere in the tree. A
provider that hard-deletes the row instead keeps the fact only in
history, not in current state.

## Timestamps: one clock, no fabrication

What goes in `GridRow.created_at` — the global-ordering policy, the
microsecond-bump recipe for sub-items, no-fabricated-timestamps, and
which entity kinds legitimately have none — is a projection concern and
lives in [`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md#6-timestamps).
How a stamp is stored (`<x>_at_utc` plus `tz_offset`) is AGENTS.md
§"Timestamp convention". What stays here is the crate every stage
shares, including download for its own `fetched_at_utc` stamps.

### Single source of truth: `datalib-time`
Mint every `now` and parse every inbound RFC 3339 string through the `datalib-time` crate (`datalib/backend/time/`); a handful of call sites still reach `chrono` directly, and new code should not. The crate exposes:

- `IsoOffsetTimestamp::now_local()` — the canonical "now," returning the wall clock with the **generating system's local-tz offset** (e.g. `2026-06-10T14:23:00-07:00`). An offset-bearing timestamp is strictly more information than the same instant in UTC: you can recover UTC from `-07:00`, but you can't recover the originating offset once it's been normalized away. `to_utc_and_offset()` splits a value into the stored pair, `split_stamp` does the same for one that arrived as a string, and `bulk_upsert_bookkeeping` does it for the sidecar.
- `parse_strict(s)` — accepts only strings that already carry an explicit offset. Most parse callsites should use this.
- `parse_with_assumed_utc(s)` — **the single function in the whole repo** where "the upstream string lacked an offset, assume UTC" is allowed. Reach for it only after auditing an upstream feed and confirming naive-means-UTC. Any other fallback (local time, midnight, run start, epoch) is fabrication.
- `IsoOffsetTimestamp::bump_micros(n)` — the canonical sub-item synthesized-stamp recipe.

## Commit lifecycle
**Providers do not call `dolt_commit` or `commit_run` themselves.** `RawStoreSession` ([`raw_store.rs`](/datalib/backend/etl/src/raw_store.rs)) commits for them: a `checkpoint <name>: entities` seal on the `Checkpointer`'s cadence, and `finish` at the end (`download <name>: <stats>`). The blob CAS is plain SQLite and has no seal: each `put_many` commits itself before the edge rows naming its bytes are written. A run that touches N upstream pages / windows / items produces one `download` entry in `dolt_log()`, plus its checkpoints, not N. A `finish` whose commit fails fails the step: the next `open` discards what was never committed, so "logged and returned Ok" would have been work done for nothing.

Two consequences:

- The commits since the previous `download` entry are exactly "what this sync run pulled" — a clean unit of analysis for incremental delta UI surfaces and audits.
- Provider authors don't have to think about commit boundaries. If you find yourself reaching for `commit_run` inside a provider, you almost certainly want UPSERT instead.

The other commits a raw store carries are the store's own: `open`'s schema commit (`schema: apply DDL …`), a migration rung's (`migrate v<n>: …`) and a reset's. Nothing ever commits what a crashed or interrupted writer left behind; the next `open` discards it.

## One writer per row
**Each write to a raw entity row is complete as of that write.** The writer's job is to assemble everything it knows about the row — `payload` plus all writer-supplied identity columns — and emit it in one UPSERT. We do not have a notion of "partial" writes that leave NULL columns the writer chose not to populate, and we do not have multi-pass enrichment where writer A populates some columns and writer B fills in the rest. Both are anti-patterns.

Consequences:

- **One `ON CONFLICT(id) DO UPDATE` shape, everywhere.** Every column in the upsert's column list (other than `id`) is updated with `excluded.<col>`. No `COALESCE(excluded.<col>, <table>.<col>)` — that pattern only exists to protect a stale-but-known value from being clobbered by a fresh-but-incomplete write, and we don't allow incomplete writes. The uniform shape is what lets one generic bulk-upsert helper (see [Bulk-upsert as the standard write path](#bulk-upsert-as-the-standard-write-path)) cover every table.
- **One writer per row, normally.** Typically each raw entity table has a single producing downloader. If two producers can in principle write the same id (e.g. a JMAP API downloader and an mbox file-import both targeting the `emails` table), it is a configuration error to enable both for the same destination, and the semantics if you did are **last-writer-wins, not merged**. The system is not built to maintain a hodgepodge of two writers' partial knowledge of the same row.

Why the discipline matters: the alternative is per-column conflict policies (COALESCE on some columns, replace on others), which makes the UPSERT shape diverge per table, makes the chunked-multi-row helper proliferate variants, makes `dolt diff` harder to read, and makes "which writer last touched this row?" an ambiguous question.

## Bulk-upsert as the standard write path
Every download is shaped the same at the bottom: for some entity table `<t>`, upsert N rows of `(id, payload, …extras)`, paired with N rows on `<t>_bookkeeping`, and (if the source produced blobs) M rows on the CAS of `(blake3, byte_len, content_type, bytes)`. A SQL transaction rewrites each prolly-tree page it touched once, at `COMMIT` ([`etl/README.md` §"What a write costs"](/datalib/backend/etl/README.md#what-a-write-costs-the-transaction-is-the-unit-and-the-key-decides-the-size)), so the right shape is **one entity-pool tx + one CAS-pool tx per batch** (the CAS is plain SQLite, where a transaction per batch is what keeps the fsyncs down), each containing chunked multi-row `INSERT … ON CONFLICT(id) DO UPDATE` statements. Email's mbox downloader measured it: 25k emails dropped from many minutes to ~75 seconds at `FLUSH_BATCH = 2000`.

The principle: **every provider's download uses the shared chunked-multi-row helpers for the entity-table UPSERT, the `<t>_bookkeeping` upsert, and the CAS write.** Per-row UPSERTs are an anti-pattern outside ad-hoc maintenance code.

Because those statements are built at runtime, they go through `sqlx::AssertSqlSafe` — see [`AGENTS.md`](/AGENTS.md) §"Dynamic SQL needs `AssertSqlSafe` and a reason".

### The shared pieces, all in `datalib_etl`:

- **`bulk::bulk_upsert_in_tx(tx, rows, now)`** — the generic write, for any `T: BulkUpsertable` (which the table derives emit); [`etl/README.md` §"Writes: one UPSERT shape, everywhere"](/datalib/backend/etl/README.md).
- **`bulk::SQL_CHUNK` + `bulk::push_placeholders` / `bulk::push_placeholder_list`** — chunking utilities for a provider's own multi-row `INSERT` builders.
- **`bulk::bulk_upsert_bookkeeping(tx, table, ids, now)`** — the `<t>_bookkeeping` UPSERT alone, for a hand-built entity write.
- **`bulk::EventBatch<'a>`** — the per-table `(table, &[(id, &payload)])` shape the tape primitives share.
- **`blob_cas::BlobCas::put_many`** — chunked multi-row `INSERT OR IGNORE` over `cas_objects`, one tx per call. The per-doc `blob_cas::BlobBundle` accumulates a document's attachments during download (`add` / `add_error`) and exports its `cas_inserts()` and edge rows for these writes; the same bundle is reloaded at parse and consumed at render.
- **`doltlite_raw::bulk_upsert_events(tx, tape, &[EventBatch], now)`** and **`doltlite_raw::bulk_upsert_with_tape(pool, tape, rows, payloads)`** — the same writes plus the [wire-event tape](#wire-event-tape-jsonl): the caller's entity UPSERTs (or `bulk_upsert_in_tx`), the sidecar stamp, the commit, then one JSONL line per row via `EventTape::append_batch` when a tape is attached. Tape errors log but don't fail the upsert — doltlite is the source of truth.

The tape variants are the right tool **only for tables whose rows came off a wire**. For everything else — CAS edge tables, sidecars, file-imported entities like mbox or vcf where there is no upstream "event" — use `bulk_upsert_in_tx` and skip the tape. Synthesizing a fake wire payload just to feed the tape would be making up data we don't have.

The `ON CONFLICT` clause is **the same shape on every table**; only the column list varies (see [Events vs bookkeeping](#events-vs-bookkeeping-where-each-column-lives) for which extras belong on the entity table vs the sidecar vs as VIRTUAL+index over payload), and the derive supplies it from the row type.

## dolt_diff supersedes per-bucket fingerprints
Render decides what changed with **`dolt_diff_<table>` virtual tables driven by a per-source render cursor**, not with fingerprints of its own: doltlite's prolly-tree diff answers "what changed since last render?" directly.

Mechanism: on render success, the render step records the doltlite HEAD the provider pinned in the render store's `render_cursor` row, in the same transaction as the run's last document. On the next render, the provider is handed that hash (`RenderCtx::raw_cursor`) and `parse` runs `doltlite_raw::scan_buckets(pool, last_hash, &DiffScanSpec { global_fanout_tables, bucket_query })`, which cold-starts if any `dolt_diff_<global_fanout_table>` row is non-`unchanged` (those fan out to "render everything"), otherwise runs the per-bucket `bucket_query` across the relevant `dolt_diff_*` vtabs. Parse then loads payloads only for the surviving bucket keys.

There is no fingerprint compare beside it: rewriting an identical row to a content-addressed table is no change, so nothing downstream ever sees it.

**Rule for new stages.** Any new derivation added to the pipeline follows the same recipe: read its input pinned at a commit, record that commit beside its output in the same transaction, and on the next run diff the input from there. The grid index does the same over every render store (`source_cursors`). The compare-and-skip loop is what makes the system feel responsive on a laptop with months of accumulated data.

## Cursor / resume strategy
Cursor / resume is the **download-side specialization** of the [Incremental update](#efficiently-incremental) pattern: "what was the last upstream identifier we successfully recorded?" answers "where does the next walk start?" Three patterns in the tree, picked by upstream API shape:

- **Forward-walk + refresh window** (slack, github, gitlab): resume from `max(ts)` of previously-recorded items; also re-query the trailing `refresh_window_days` to catch edits / late-arriving items. Dedup collapses the overlap to zero writes.
- **Listing diff** (claude, chatgpt): re-list everything each run and compare each item's listing `updated_at`/`update_time` against the stored copy; only new/changed items get a detail fetch. An optional `since` bounds the diff — items updated before it are never detail-fetched, and chatgpt's newest-first paginated listing additionally stops walking once it pages past the cutoff.
- **Time-windowed sampling** (yolink): walk `[start, now]` in fixed-stride windows. Windows align across runs and devices. Per-window UPSERT dedups re-fetched samples.

No checkpoint files. The dedup index is the resume cursor.

### A claim of completeness is written only by a walk that completed

Three things a download writes are not data but **claims about how far
it got**: a resume cursor or state token ("everything up to here is
mirrored"), the `scope_config` record ("the config as it stands has
been satisfied"), and the authority a prune needs ("this enumeration
was complete, so absence means deletion"). Each is read by the *next*
run as permission to skip work. A claim written by a walk that did
not finish is therefore a silent data loss: the next run believes it,
does less, and nothing anywhere reports the gap.

The trap is that a walk ends early far more often than it fails.
Every one of these is an `Ok` return with the marker's write still
ahead of it, or already behind it:

- an error on one unit that the loop tolerated and moved past;
- a budget (`message_budget`, `limit`, a page cap) reached;
- a `since` cutoff or a label filter that stopped the walk server-side;
- a listing request that failed and yielded an empty list;
- **a stop** — Ctrl-C, or the UI's cancel — which ends the run at the
  next unit boundary by design ([`step_protocol.md` § Signals](step_protocol.md)).

So the rule has two halves:

1. **Gate the write on one predicate computed at the end, from what
   actually happened** — not on reaching the end of the function, not
   on `result.is_ok()`. Gmail's `FetchSummary::drained()` is the
   pattern: `!stopped_early() && messages_failed == 0`, consulted once,
   and both the cursor and the scope record are written under it.
   Slack's `walked_everything` is the same predicate by another name.
2. **Take the token early, store it late.** A live state token is
   often only available on the first response of a walk; hold it in a
   local and write it when the walk completes. Writing it where it was
   obtained turns "I have the token" into "I have everything the token
   covers", which is the claim.

**The test that catches it** is the same for every marker: end the run
early on purpose — the stop flag, raised from the progress sink after
the first unit, is the deterministic way — and assert the marker was
*not* written (`slack/tests/slack_tests/interrupt.rs`). A test that only
checks the happy path checks the write, not the gate.

`scripts/lint_repo.py` check 8 catches a provider that keeps a cursor
and never records the scope config at all. It does not catch a record
written too early; nothing mechanical does, which is why the rule
is written here.

### When the cursor swallows a config change

A forward-walk cursor answers "where do I start?" from stored data
alone, so it stops consulting the config that set it. The knob that set
the starting point is read only on the cold-start arm, which makes
*widening* it a silent no-op on an already-synced data root — and for a
strictly-forward walk, structurally unrepresentable: a widened `since`
is a request to go backwards.

Listing-diff providers are immune by construction. They re-apply the
config filter to a freshly-fetched listing every run, so moving `since`
back simply makes previously-out-of-scope items reappear as missing.
That's worth preferring when an upstream API allows it.

Where a cursor is unavoidable, `datalib_etl::scope_config` records
the scope-affecting config subset alongside it, in the raw store's
`sync_scope_config` table. The next run diffs current-vs-stored and
reacts proportionally rather than re-downloading wholesale — a widened
`since` backfills only the newly-in-scope window. Two invariants:

- **Only widenings do work.** A narrowed knob leaves an on-disk
  superset, and nothing in the pipeline deletes.
- **An absent record plans no work.** A store written before its
  provider kept a record has none, and reading that as "unknown,
  therefore re-download" would backfill every installed mirror at once
  on upgrade.

What belongs in the record is only what changes *which data lands on
disk* — not per-run budgets (`max_prs`, `limit`), not one-off overrides
(`conv_uuids`, `targets`, `full_sync`). That curation is why it isn't
just a diff of `sync_runs.config`, which is an audit log and records
everything.

Current consumers, and what each does when the knob widens:

| Provider | Knob | Reaction |
|---|---|---|
| slack | `since` | Walk `[since, min(ts)]` per channel |
| slack | `media` | Re-walk from `since` (the knob only reaches messages the walk visits); a raised `blob_size_limit_bytes` needs nothing, since every run retries the files it skipped |
| github, gitlab | `refresh_window_days` | `scope_state::since_for_scope`, given the prior record, reaches back to the earlier of the cursor and `now - window` |
| email (JMAP) | `only_extract_labels` | `Email/query` scoped to the newly-added mailboxes |
| email (Gmail) | `only_extract_labels` | `history.list` since the cursor as usual, plus a `messages.list` walk over the newly-added labels (or the whole account when the filter was removed) |
| email (mbox) | `only_extract_labels` | Re-read every file |
| garmin | `since` | Re-walk from the new start |
| notion | `refresh_window_days` | Re-examine the widened window |
| yolink | `devices[].start` | Re-walk that device from the new start |

The longest write-up of the reasoning, including what is deliberately
*not* recorded and why, is `providers/slack/INGEST.md` § "Config
changes the cursor would otherwise swallow"; Gmail's is in
[`email_download_modes.md`](email_download_modes.md).

**The rule is opt-in, and that is how it gets missed.**
`scripts/lint_repo.py` check 8 refuses a provider crate that writes
`sync_scope_state` without also calling `scope_config::store` — a
crate-level check, so it catches a new provider, not a new *mode*
inside an existing one (Gmail arrived as a new mode of `email` with a
cursor and no record, and widening its label filter was a silent no-op
until it was fixed). The structural fix — a cursor primitive that takes
the scope config on the way in and hands back the `FilterChange` with
the token, so a cursor cannot be read without saying what scope it is
read under — touches every consumer in the table and is worth doing as
its own change.

One known gap: **Gmail `blob_size_limit_bytes`.** JMAP is exempt
because `sync_blobs` re-scans every email for missing bytes on every
run, so a raised cap backfills by itself. Gmail has no such pass and
does not record the cap: an oversize message lands its row and no
blob, and the next run skips the id before ever asking again. Raising
the cap on a Gmail mirror leaves every previously-oversize message
without bytes.

Render has the same failure mode and resolves it differently —
wholesale invalidation rather than a proportional reaction: a change to
the render params a processor declares re-renders everything. See
[`data_architecture_parse_and_render.md`](data_architecture_parse_and_render.md#how-a-run-decides-what-to-render).

## Auth and credentials
Two patterns:

- **Most providers**: shell out to `latchkey curl` (see [`backend/etl/src/latchkey.rs`](/datalib/backend/etl/src/latchkey.rs)). Auth lives in the latchkey keyring, indexed by URL host. The provider's HTTP transport never sees the bearer token.
- **Yolink**: latchkey doesn't know about `us.yosmart.com`, and the consumer download path isn't bearer-authed — the URL itself is signed (`build_signed_url` in [`providers/yolink/src/ingest/mod.rs`](/datalib/backend/etl/providers/yolink/src/ingest/mod.rs)). Per-device secrets live in config (REDACT before publishing).

If you add a new provider with a new auth shape, prefer extending latchkey upstream before adding a third pattern.

## Error handling
We want enough transient error handling that syncs "usually" work. The goals are:

- The process must make progress within a certain amount of time, or it should stop.
- If more than X count of your last requests have errored, you better stop.

Distinctions every provider should try to follow.

- **Per-item failures are tolerated.** A transient failure on one window / page / blob — 5xx, network blip, timeout, parse error, transient permission denied, rate-limit response — should not kill the run. Log a `warn!`, increment an error counter, **leave durable evidence in the row** (next bullet), advance the cursor, keep going. The run's `FetchSummary` reports the count.
- **A failure about a record is a `problems` row, not only a `warn!`.** Record a per-record fetch failure through `record_object_error` / `record_object_attempt` in [`doltlite_raw.rs`](/datalib/backend/etl/src/doltlite_raw.rs): besides the sidecar's `last_error`, it writes the entity's `problems` row (`Reason::FetchFailed`), which render carries forward and the Manage row counts. A configured entry upstream does not have — a label, a channel, a conversation id — goes through [`download_problems::report`](/datalib/backend/etl/src/download_problems.rs), which writes one config-keyed row per entry, every run until the config is corrected. How the rows travel: [`etl/README.md` §"Problems flow downstream with the data"](/datalib/backend/etl/README.md#problems-flow-downstream-with-the-data).
- **Auth failures and consecutive-failure budgets are fatal.** A workspace-wide 401 / 403 from the auth provider, or N back-to-back per-item failures on the same source, should return `Err` from `fetch(...)`, which fails the step. The final commit does not happen: what the run sealed at its last checkpoint stands, and the rest is discarded at the next `open` and refetched from the cursor.

The yolink provider's `CONSECUTIVE_FAILURE_BUDGET = 30` is a template for the second pattern.

There are existing chokepoint mechanisms to enforce some of these rules, but not all can be generically enforced (Slack's HTTP-200 `error:"ratelimited"` body; GitHub's `403 + x-ratelimit-remaining:0`).

A rate limit is not slept through. The shared HTTP chokepoint ([`http.rs`](/datalib/backend/etl/src/http.rs)) honours `Retry-After` and backs off exponentially until the source's give-up guard ([`retry.rs`](/datalib/backend/etl/src/retry.rs)) says the run has gone too long without progress; then the provider stops cleanly with what it committed, and the next run resumes from the cursor. ChatGPT's `RateLimited` error is the worked example.

## Transient vs non-transient
The retry mechanism is for *transient* failures. Some signals deserve a different mark:

- **Confirmed-deletion (404 on a known-existed thing).** The upstream is telling us "this is gone." A retry will only ever return 404 again. The row should carry a distinct `deleted_upstream_at` marker so we don't burn API quota retrying forever, while still preserving the row (and any history) for backpointer / outlink purposes. Not built yet (see [The delta as a source](#the-delta-as-a-source-diff-groups)).
- **Workspace-wide auth failures.** Per [Error handling](#error-handling) above, these are fatal and bail the run; per-row retry doesn't apply.
