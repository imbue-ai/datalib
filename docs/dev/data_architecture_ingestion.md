# Data architecture: ingestion

# Introduction and Context
We have an incremental, resumable, layered ETL-shaped architecture that downloads raw data from many upstream sources and stores it as **JSON API responses preserved** in versioned doltlite tables, with attachment **BLOBs in a content-addressable store** (CAS, also doltlite, but a separate sibling database per source), then applies transformations (rendering, indexing) and presents the rendered data in a UI.

Parts of this are not novel — the data pipeline aspect shares shape with Flume / Apache Beam / Dask / Prefect / Airflow ETL pipelines. What we optimize for that those tools don't:

- Single user, single laptop
    - No cluster, no scheduler service, no DAG server
- Easy to install, configure, run, and monitor
    - One config file, one orchestrator binary, one local data directory
- User can "own" their data
    - It exists in files they can see and inspect themselves in non-proprietary formats.

This document describes the principles we strive towards for the **ingestion (download) side**: how raw data lands on disk, what shape it has at rest, and the operational properties (monitorable, stoppable, resumable, incrementally cheap, verifiable) the download stage aims for. It is aspirational as much as descriptive: a new provider, table, or transformation should be judged against it, and divergences should be either justified or fixed.

## Related documents
The downstream stages — render, grid_index, qmd indexing, view, annotation — are covered by the focused dev notes [`docs/dev/grid_rows.md`](grid_rows.md) and [`docs/dev/edges.md`](edges.md). Where understanding "download" requires a downstream concept (the sidecar contract render emits, the `GridRow` projection the UI reads), this document touches on it briefly.

Practitioner-facing material — how we test, how to add a provider, how the schema evolves, and the open questions — lives in the companion [`data_architecture_ingestion_practices.md`](/docs/dev/data_architecture_ingestion_practices.md).

# General pipeline structure

The ETL pipeline currently has three stages, each running as a **subprocess step under the `datalib-dag` DAG runner** ([`frankweiler/backend/dag`](/frankweiler/backend/dag)) — one process per step, each step an invocation of the `datalib-step` binary ([`frankweiler/backend/datalib_step`](/frankweiler/backend/datalib_step)); see [`pipeline_dag_architecture.md`](pipeline_dag_architecture.md) for the orchestration design and [`step_protocol.md`](step_protocol.md) for the step contract:

1. **Download** — pull from upstream, UPSERT into `<data_root>/<data_source>/raw/entities.doltlite_db` (entities) and `<data_root>/<data_source>/raw/blobs.doltlite_db` (a single `cas_objects` table keyed by blake3 hash).
2. **Render** — derive sidecar `.md` + `.grid_rows.json` under `<stanza>/rendered_md/...` from the raw store, deterministically (indexing with qmd is the separate `qmd_index` step).
3. **Grid index (currently: view in UI)** — feed the sidecar tree into the canonical `grid_rows` table to drive the UI

Each provider (data source) is its own crate at [`frankweiler/backend/etl/providers/<name>/`](/frankweiler/backend/etl/providers), named `frankweiler-etl-<name>`. The provider crate owns its Download + Render code, its bins, its integration tests, and the sample fixtures the tests run against — keeping sample data next to the code under test serves as documentation of "what this provider's wire format looks like." Grid index is provider-agnostic and lives at [`src/grid_index.rs`](/frankweiler/backend/etl/src/grid_index.rs) (`build_grid_index`); a new provider needs no grid_index-side changes.

## Layering of concerns: download is downstream-agnostic
The per-stage modules within a provider crate form a strict layer with a single allowed dependency direction:

```
upstream → download → render → grid_index
```

- **`download`** owns the bytes-at-rest. It fetches from upstream and persists into `<data_root>/<name>/raw/entities.doltlite_db`, and nothing else. It must NOT depend on `render`, `frankweiler_schema::grid_rows::GridRow`, sidecar types, or the qmd index. The per-provider `schema_raw.rs` rustdoc deliberately avoids describing how render consumes the tables.
- **`render`** depends on `download` (it reads the raw store and projects to the normalized POD + sidecar shape). `download::schema_raw` is part of the contract render consumes. **Render reads only the raw store, never the original source.** Its sole input is `<data_root>/<name>/raw/` (`SourceEntry::raw_path`); it must never reach back into upstream (the API) or into a file-backed source's `input_path` (the `.mbox`, the Takeout export, …). Render shows us **what we have captured and internalized**, not what is currently live at the source.
- **`grid_index`** is provider-agnostic; it lives at [`src/grid_index.rs`](/frankweiler/backend/etl/src/grid_index.rs) and depends on no provider's download or render. Its input contract is the sidecar tree.

Why the discipline matters: download is its own deliverable — a user can run it, stop, inspect the raw store, and have something useful (a backup, or mirror, at the very least) even if render has bugs or hasn't been written yet. Render can then be re-implemented or extended without touching download, and disabling a render path for one provider doesn't disturb that provider's download.

Why render's read-only-from-the-raw-store rule matters: the raw store is the boundary between "the outside world" and "our copy." Once download has captured the bytes, everything downstream is reproducible offline and stable — re-rendering yields the same result whether or not the upstream still exists, has changed, or is reachable. The original `input_path` export can be deleted, the API token can expire, the phone backup can be wiped: render still produces exactly what we hold. This is also what lets `raw_path` move a source's store anywhere (a bigger disk, an archive) without the renderer caring — it reads the resolved raw directory and nothing else.

# Schemas first, but also simple
> *"Show me your flowchart and conceal your tables, and I shall continue to be mystified. Show me your tables, and I won't usually need your flowchart; it'll be obvious."* -- Fred Brooks, The Mythical Man Month (1975)

The single most load-bearing principle of this whole document is that **the schema is the design**. When we add a new data source, or sketch a new feature, the **first** artifact is the table — its columns, its primary key, its uniqueness constraints, its foreign-key relationships, and inline comments explaining what each row and column *means* and why it is there.

Concretely, when starting any non-trivial piece of work in this codebase:

1. **Write the DDL first**
2. **Document each table *in the same file as the DDL***. Per-provider `schema_raw.rs` files (`etl/providers/<p>/src/download/schema_raw.rs`) are the canonical home for both the `CREATE TABLE` text and the prose commentary on it. Tables without their prose are half-finished.

## Download schemas must be simple: mostly PK + payload as JSONB
However, we don't want our DB schema tightly coupled to upstream schemas, so we don't try to translate upstream data into a complete set of SQL columns.

Rule: download schemas should often be extremely simple — often just a stable primary key and a JSONB payload column.

The download portion of our system captures raw data from sources in its native format with as little translation as possible: typically JSON payloads as they arrived off the wire, with enough indexing that related payloads can be updated and grouped together. 

A few cases justify more schema:

- When the JSON payload is missing contextual data the requester knew (account_id, say), it can be stored alongside the payload as extra columns in the table.
- If the payload returned by the data source includes fields that always change regardless of the object's state, like a fetch time, then we should move those fields over into a separate bookkeeping table so that we don't explode the number of rows in the main payload table.
- For attachments (which are stored separately in a BLOB CAS), we often need to know which attachments belong with which payloads. DB column schema allows linking event payloads and BLOBs in a many-to-many relationship via a dedicated edge table.

## Object identity: Ship of Theseus on UUIDs
We lean **heavily** on upstream-provided UUIDs to establish permanent object identity.

- Every raw-store entity table keys by the upstream provider's identifier — no surrogate `AUTOINCREMENT`. That's what makes `dolt diff` stable across re-fetches, what makes `ON CONFLICT(id) DO UPDATE` work, and what makes cross-table references (e.g. `messages.conversation_id`) mean something.
- When an upstream doesn't expose a stable UUID, we **synthesize one via UUIDv5** from a per-provider namespace and the most stable available fields. This is done in the data source's schema_raw.rs DDL.
- We do **not** use row autoincrement or hashes-of-content as identity for objects. Both break the Ship-of-Theseus property: autoincrement isn't deterministic across re-ingest; content hashes change every time the content does.
- **(NOT download/ingest related) Backpointers and outlinks are first-class** in the projection schema. `GridRow` (one of our indexed representations, not a raw format) carries:
    - `uuid` — the Ship-of-Theseus identity, deterministic from upstream so re-ingest is idempotent.
    - `external_id` — the provider-native primary id (numeric GH/GL id, PR number, …) preserved alongside our UUID so we can round-trip back to the provider's API.
    - `source_url` — the canonical URL on the provider's web UI (e.g. `pull_request.html_url`, GitLab `note.web_url` with `#note_<id>` anchor), populated everywhere we can construct it.
    - `qmd_path` — the path to the rendered markdown sidecar.
    - Provider-specific cross-references (`notion_page_uuid`, `notion_block_uuid`, `slack_link`, `git_sha`, …) so the UI can link sideways as well as out.

### `schema_raw.rs`: Per-provider schema layout
Within each provider crate the bytes-at-rest schema is its own file, deliberately declarations-only:

- **`providers/<name>/src/download/schema_raw.rs`** — the raw-store schema: DDL constants (one per table / index / bookkeeping sidecar), schema-evolution migration constants co-located with the table they touch, any synthesized-PK recipe functions, and a tiny `full_ddl()` composer that splices in `dr::bookkeeping_ddl_for(table)` for each entity. **No manipulation code** — `RawDb`, UPSERTs, SELECTs, and parameter binding stay in `download/db.rs` and import from `schema_raw`. The convention is proto/pydantic-flavored: opening the `schema_raw.rs` files at the same fixed path answers "what does the world look like at rest?" without opening anything else.
- **`providers/<name>/src/render/schema_translate.rs`** (aspirational, landing per provider) — the normalized representation render emits: mostly serde-shaped Rust types, not SQL DDL, the in-memory POD form before it's shredded into sidecar rows. A provider may have multiple `schema_translate_<family>.rs` files; where a shape is shared across providers (chat-human, code-review, time-series, …) the canonical type lives in a shared crate and the per-provider file re-exports.

Each entity table has a JSONB `payload` column holding the raw upstream wire payload, plus a small number of typed columns the writer must populate at insert time (synthesized-PK components, FKs into parent tables that aren't in the payload, namespace discriminators). On disk `payload` is stored as JSONB (SQLite 3.45 binary JSON, via `jsonb(?)` on write and `json(payload)` on read; see [port guide §6a](/frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#6a-jsonb-storage-for-payloads)) — purely a storage encoding; the principle is wire-fidelity (see [Wire-fidelity of the raw store](#wire-fidelity-of-the-raw-store)).

### More details

**Fields derivable from the payload** (`updated_at`, `state`, `name`, `html_url`, `display_name`) — even when we want to query or index them — should **not** be duplicated as stored columns. Use either a `CREATE INDEX … ON t(payload->>'$.path')` expression index or a `VIRTUAL` generated column plus an index over it. Both produce COVERING index plans in DoltLite v0.11.9; the VIRTUAL+index variant additionally restores `SELECT col FROM t` ergonomics. Either way, `ALTER TABLE ADD COLUMN … VIRTUAL` (or a new expression index) is a no-refetch additive change against existing user data. See [Schema evolution](/docs/dev/data_architecture_ingestion_practices.md#schema-evolution).

### Events vs bookkeeping: where each column lives
Every entity table `<t>` is paired with a sidecar `<t>_bookkeeping`. The split is load-bearing — three buckets to think about when adding a column:

1. **Upstream payload data** (a Slack `text`, a GitHub `state`, a Notion `last_edited_time`) → lives inside `payload`. If we need to query or index it, use a VIRTUAL generated column + index or an expression index over `payload->>'$.path'`. Do **not** copy it into a stored column.
2. **Writer-supplied identity / joins** (synthesized-PK components, FK references to parent entities the walker knows but the payload doesn't, namespace discriminators like beeper's `source`/`network`) → stored typed columns on `<t>`. These are the only typed columns the entity table should grow.
3. **Writer-supplied per-row state** (`fetched_at`, `attempt_count`, `last_attempt_at`, `last_error`, per-row cursors like CardDAV `etag`, ChatGPT `last_listing_update_time`, YoLink `last_ts_ms`, server-supplied freshness markers like `ctag`/`sync_token`) → `<t>_bookkeeping` sidecar.

The split matters because bookkeeping changes on every attempt regardless of upstream change. Storing it on the entity table makes every `dolt diff` noisy, defeats the wire-fidelity of `payload`, and forces re-renders of unchanged content. Keeping it on the sidecar means `<t>` mutates only when upstream actually changed, and the sidecar churn stays out of any cross-stage fingerprint.

### Blobs and the CAS split
Attachment bytes are split out of the entity database into a sibling content-addressable store. We do this because:

- `dolt diff` over the entity db stays small and human-grep-able. A re-fetch that picks up one new attachment doesn't drown the commit in a many-MB BLOB row.
- The CAS by nature is append-only.
- Attachments can be big, and Dolt DBs are (purposefully) difficult to erase from.  Even garbage collecting unused attachments wouldn't delete them from the doltlite DB storage.
- Someday we might want to share a BLOB store across multiple data sources (Perkeep-style).

 Each source has both `<name>/raw/entities.doltlite_db` (entities + a per-provider `<provider>_attachments` edge table mapping `(owning, ref) → blake3`) and `<name>/raw/blobs.doltlite_db` (`cas_objects` keyed by blake3). Full schema + helpers in [port guide §7](/frankweiler/backend/etl/DOLTLITE_RAW_PORT_GUIDE.md#7-blobs).

**Per-provider CAS edge tables**

Every provider with attachments owns a small four-column edge table that maps `(owning_id, ref_id) → blake3`. Bytes still live in the shared `cas_objects`; the edge table is provider-specific so providers that happen to use the same upstream id format don't collide, and so per-provider semantics (refetch policies, dolt_diff fanout) don't bleed across sources. The legacy shared `blob_refs` table has been retired entirely.

The four-column shape is universal — `id` (synth PK `{owning}#{ref}`) + owning FK + ref id + nullable blake3 — so the declaration in `schema_raw.rs` is a single `#[derive(CasEdgeRow)]` struct. The derive emits the DDL, the two indices, the synth-PK recipe, and the `BulkUpsertable` impl. See [`provider_migration_dolt_diff_and_cas_edge.md`](/docs/dev/provider_migration_dolt_diff_and_cas_edge.md) for the full recipe.

### Shared attachment-flush primitives
Per-bucket attachment-fetch flow is consolidated into three shared pieces in `frankweiler_etl::blob_cas`:

- **`load_blake3_index(pool, table, ref_id_column)`** — one SQL scan at fetch entry produces the run-scoped `(ref_id → blake3)` map. The per-file dedupe check is a HashMap hit, not a SQL round trip queued behind preceding multi-MB CAS commits on the single-connection doltlite pool.
- **`CasEdgeAccumulator`** — per-bucket walker. Three add paths: `add_fetched`, `add_known`, `add_failed`. Tracks the `BlobBundle`, the `(owning, ref)` edge list, and per-`ref_id` error strings. Dedupes by `(owning, ref)`.
- **`flush_cas_edges(pool, cas, cas_inserts, rows, errors)`** — the canonical 3-step end-of-bucket flush: CAS `put_many` → `bulk_upsert_in_tx` the edge rows → stamp `last_error` on the bookkeeping sidecar for every failed `(synth_pk, err)` pair → commit. `CasEdgeAccumulator::flush` delegates to it via a provider-supplied row-builder closure.

## [Doltlite](https://github.com/dolthub/doltlite) is our primary raw store

For raw ingestion, each data source owns a directory `<data_root>/<name>/raw/` holding two DBs:

- entities.doltlite_db: Event payloads and metadata, attachment edges
- blobs.doltlite_db: a CAS of BLOB data specific to that database.

That directory is resolved by `SourceEntry::raw_path` — defaulting
to `<data_root>/<name>/raw` but overridable per source via `raw_path:` in the
config, identically for every source. It's a single resolver used by both
sides: the downloader writes there and the renderer reads there. The
filenames inside it (`entities.doltlite_db`, `blobs.doltlite_db`, `events/`)
are the constants in `frankweiler_etl::raw_layout`, the one place the layout
is defined. This is distinct from a file-backed source's `input_path:`, which
says where the data is read *from* (a `.mbox`, a Takeout export, …).

We use doltlite because:

- At the API level, it effectively "is-a" sqlite, supporting all sqlite behavior (JSONB, etc.)
    - Except it has its own binary format and thus needs a differently compiled sqlite binary.
- It supports data versioning (commit, branch, merge, tag, etc.)
    - Different versions of the data are stored space-efficiently.
    - SQL operations (even DROP TABLE) do not actually delete anything.
    - It can enumerate deltas between any two versions of the data (including "DROP TABLE" and recreate with new schema), enabling incremental processing ("what changed since commit X (the last I saw)?")
- Thad believes it is the future for local-first software: "skate to where the puck is going to be."

We acknowledge these risks:

- It is very young technology and changing quickly.
- There's a small space and time penalty for the versioning.

If we had to, we could return to plain-old-sqlite, with these options:

- Drop support for data versioning and incremental data processing (always rerender)
- Implement "what changed since moment X" ourselves

## Wire-event tape (JSONL)
But doltlite is also a binary file you need a tool to open. So alongside the doltlite raw store, downloads also write a **plain-text, append-only JSONL log of what came off the wire for debugging purposes.  This file can be safely deleted.**

This is the simplest view of the raw data: one event per line, in the order the downloader saw it. No schema, no migrations — just a tape you can `tail -f`, `grep`, `jq`, or open in any editor.

The doltlite store is what the stateful, incremental, version-controllable pipeline reads; the JSONL tape is what a human reads when they want to see what the upstream actually sent us, with no tooling in the way.

Layout — one directory per source, one file per entity table:

```
<data_root>/<name>/raw/events/
  <table>.jsonl                       # one line per upsert
  <provider>_<attachments>.jsonl      # the per-provider CAS edge table
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

- **The pipeline never reads it.** Render, grid_index, resume, retry — all of those go through doltlite. The JSONL is a write-only mirror. Deleting the `events/` directory does not break anything.
- **Same bytes as the upsert.** We tap right next to the `ON CONFLICT(id) DO UPDATE`, so the tape carries the same wire-fidelity payload that the doltlite row gets. No second parse, no second normalize.

# Operational principles
## Monitorable
The first sync from a given source is often very long (hours to days, many GB, subject to rate limits). Every stage must surface progress the user can watch in real time.

- Every binary flattens [`obs::ObsArgs`](/frankweiler/backend/obs/src/lib.rs) into its clap parser, so every stage takes the same logging / OTLP / progress-bar flags. On a TTY, pretty log lines on stderr; otherwise NDJSON events. Log emissions route through an `IndicatifWriter` coordinating with the shared `MultiProgress` (`frankweiler_obs::shared_multi()`) so caller progress bars don't get stomped by log lines.
- `--otlp-endpoint http://host:4317` exports spans + events via OTLP, so a single Tempo/Jaeger collector can ingest every stage. (See [the privacy-boundary unresolved question](/docs/dev/data_architecture_ingestion_practices.md#observability-and-the-privacy-boundary) for the contract that constrains what may be in those spans.)
- Each stage emits `*_start`, `*_complete`, and per-document progress events with a stable provider-prefixed name (`slack_download_*`, `grid_rows_load_*`, …). The `*Summary` structs are `Serialize`, so a web UI can consume the final stats line provider-agnostically.
- Long-running operations must report something visible every few seconds; a download that walks 100k items silently for an hour is a bug.

## Stoppable and resumable
A sync that gets interrupted — ^C, OOM, laptop sleep, upstream 5xx — must be able to make forward progress on the next run. We **do not require runs to complete to be useful**.

The dedup index *is* the resume cursor:

- Provider-side dedup keys every UPSERT on the upstream identifier, so re-walking already-fetched items is cheap and correct.
- There are no separate checkpoint files. The data we already have tells us where to resume.

## Efficiently incremental
Any subsequent sync should pick up as close as possible to where the last one left off: walk what the upstream API forces us to walk, although it should also be safe to fetch with a bit of overlap, too.

Two layers do the work:

- **Provider-side dedup**: every UPSERT uses the upstream identifier as PK with `ON CONFLICT(id) DO UPDATE`; unchanged rows are no-op writes. `dolt diff` reports an empty changeset and the trailing orchestrator commit is skipped.
- **Render-side dedup**: the sidecar carries a `source_fingerprint`; if the existing `.md` already matches, the write is skipped. The grid_index step honors the same fingerprint in `markdowns_loaded`.

Different upstreams expose different surfaces for "what changed since X", and that drives the cursor pattern (see [Cursor / resume strategy](#cursor--resume-strategy)).

## Wire-fidelity of the raw store
The raw store preserves the **semantic content** of upstream responses verbatim — every field, every value, with no loss and no pre-shaping into our internal model. The on-disk *encoding* of that content is a separate question; we pick whichever encoding is human-readable and inspectable. Concretely:

- **JSON-shaped sources** (HTTP API responses from Slack, Anthropic, Notion, GitHub, etc.) store the response JSON verbatim, as JSONB.
- **Binary-protocol sources** (Signal's encrypted protobuf backup, future binary feeds) are **decoded** at download time into JSON of equal semantic content. Encryption layers, compression, binary wire encodings, and other transport-level packaging are artifacts of how the data got to us, not part of the wire data itself. Storing them raw on disk would be "too raw" — the point of the raw store is that a human can `tail`/`grep`/`jq` it without a decoder in the loop.
- **File-imported sources** (mbox `.eml` bytes, vCard `.vcf` files, WhatsApp `msgstore.db`, Beeper `index.db`) promote the *semantic* content (typed columns, JSONB payloads) into the entity tables. **File-tree imports go through download** just like API-backed sources: a directory of `.vcf` files lands in the same raw-store row shape CardDAV produces, an mbox lands in the same shape JMAP produces. Render has exactly one input contract per provider regardless of whether the data came over the wire or off disk.

The rationale: **if all we wanted was a copy of the upstream bytes, we'd just use `cp`.** The raw store earns its keep by being *queryable and human-inspectable* in a way the original bytes aren't — JSONB rows, typed columns, predictable structure across providers. That's the criterion for "is this decoding step OK at download time?" If the alternative to decoding is asking the user to install a special tool to see their own data, the decoding belongs in download.

Two rules follow:

- **Normalize at render time, not download time.** A lesson learned on the anthropic port: we used to pre-normalize the API response (renaming fields, collapsing shapes, dropping subtrees) at fetch time. We don't anymore. Decoding a binary wire encoding to JSON of the **same** semantic content is **not normalization** — every field upstream sent us is still present, with the same values. Normalization means pre-shaping into our internal model (renaming, collapsing, projecting), which we defer to render.
- **Don't pollute payloads with downloader-synthesized keys.** `_fetched_at`, `_listing_update_time` etc. are bookkeeping, not upstream data; promote them to real columns on the entity table (or its `_bookkeeping` sidecar), not into the JSON.

Corollary: **the raw store is the source of truth; downstream stages are rebakeable.** Anything we render, project to `grid_rows`, or index into qmd can be recomputed from raw without re-touching the network. `RENDER_VERSION` (in each provider's `render/render.rs`) is the explicit lever for "force a rebake of all sidecars even when payloads are unchanged."

## Verifiable via `--reset-and-redownload`
A long chain of incremental syncs can in principle silently drop data (an upstream that doesn't surface a deletion, a cursor that skipped a page on a 5xx, a bug in our delta logic). One check is to wipe the entity tables and the incremental cursors, refetch from scratch, and **let dolt's diff tell you what was missing**.

- **`--reset-and-redownload`** wipes every entity table + its `_bookkeeping` sidecar. Per-provider CAS edge tables (`<provider>_attachments`) are preserved so already-fetched blob bytes are not re-pulled. Missing-from-the-prior-pass blobs are still picked up via the normal entity-walk → blob-fetch path.
- **`--refetch-blobs`** clears the `blake3` column on the per-provider edge tables, forcing every attachment to re-download. The re-fetched bytes hash to the same blake3, `INSERT OR IGNORE` into `cas_objects` is a no-op, no disk grows.
- Pass both for a full reset. Pass `--reset-and-redownload` alone for the common "check for entity gaps without burning bandwidth on blobs" case.

The skip-check is keyed by the **upstream identifier** (known before fetch), not by content hash (only known after). The per-provider edge table is the cache index over the CAS, and `--reset-and-redownload` is the "invalidate entity data, keep the cache" path.

`cas_objects` has no reset path either way. Bytes are byte-stable; the only legitimate way to remove them is `blob_cas::gc_orphans()`.

## Time and ordering discipline

If [Object identity](#object-identity-ship-of-theseus-on-uuids) is "UUIDs give global object identity," this is its temporal sibling: **timestamps give global temporal ordering** across every provider that has a time-shape to its data. That global ordering is what makes the UI's union grid time-sortable, what makes `before:` / `after:` queries mean the same thing across Slack and GitHub and Notion, and what lets a sync delta be "what happened in the last week" instead of "what happened to be at the top of each provider's result list."

The principle: **every event-shaped `GridRow` carries an ISO-8601 timestamp with explicit offset.** Concretely, in `GridRow.when_ts`:

- **Real upstream timestamp when one exists.** A Slack message's `ts`, a GitHub PR's `created_at`, a Notion page's `last_edited_time`. Preserved with the explicit offset upstream gave us (typically `+00:00` for APIs that hand back UTC).
- **Microsecond-bump for synthesized timestamps.** Blocks and sub-items that lack their own timestamp (chat blocks within a message, ChatGPT messages within a conversation that only has a create_time) get a synthesized one by bumping microseconds off the parent's stamp. This keeps within-parent order stable across re-runs and guarantees no collision with real stamps (real timestamps don't carry per-row µs precision from upstream).
- **Strict ISO-8601 with offset, not bare `Z` or naive.** A naive timestamp can't be globally sorted alongside a `+02:00` one without a hidden timezone assumption.

### Single source of truth: `frankweiler-time`
Every `now()` call and every inbound RFC 3339 parse in the workspace funnels through the `frankweiler-time` crate (`frankweiler/backend/time/`). The crate exposes:

- `IsoOffsetTimestamp::now_local()` — the canonical "now," returning the wall clock with the **generating system's local-tz offset** (e.g. `2026-06-10T14:23:00-07:00`). An offset-bearing timestamp is strictly more information than the same instant in UTC: you can recover UTC from `-07:00`, but you can't recover the originating offset once it's been normalized away. This is the policy for every generated `fetched_at` / `created_at` / run-marker stamp.
- `parse_strict(s)` — accepts only strings that already carry an explicit offset. Most parse callsites should use this.
- `parse_with_assumed_utc(s)` — **the single function in the whole repo** where "the upstream string lacked an offset, assume UTC" is allowed. Reach for it only after auditing an upstream feed and confirming naive-means-UTC. Any other fallback (local time, midnight, run start, epoch) is fabrication.
- `IsoOffsetTimestamp::bump_micros(n)` / `bump_micros_str(s, n)` — the canonical sub-item synthesized-stamp recipe.

### No fabricated timestamps
A logical corollary of the broader "[don't make up data](#wire-fidelity-of-the-raw-store)" principle, called out here because timestamps are the easiest place to accidentally violate it:

- When upstream gives us no timestamp and we can't pick one up from a parent (no `bump_micros` source), `when_ts` is **null**. Not "epoch," not "now," not "midnight UTC of the row's date."
- When upstream's timestamp string is naive and we haven't audited that feed, parsing returns an error — surfaced as a warning in the per-run summary, not silently rescued.
- Fallback paths that synthesize a value when upstream is silent are anti-patterns even when they "look plausible." They mask incompleteness in ways the consumer can't tell apart from real data.

### Entities without a time-shape
Some upstream object types genuinely don't have a meaningful timestamp:

- **Contacts (vCards).** A person doesn't have a creation event; they exist. The vCard's `REV` field is sometimes set, but most contacts lack one.
- **Perseus texts and other immutable corpora.** The corpus is upstream-frozen; per-section "timestamps" would be nonsense.
- **Workspace/account metadata** (Slack `team`, GitHub `org`): arguably has a creation date, but it isn't shown in any time-ordered view.

For these `when_ts` is **null** and the consumer query filters them out of time-ordered views — the principle is "**event-shaped** rows get real timestamps," not "every row everywhere." A new provider should decide explicitly which of its row types are event-shaped and document the source of `when_ts` for each.

## Commit lifecycle
**Providers do not call `dolt_commit` or `commit_run` themselves.** The orchestrator wraps each source's download in exactly one commit at the end. A run that touches N upstream pages / windows / items produces **one** entry in `dolt_log()`, not N. The commit message is `download <name>: <stats>`.

Two consequences:

- `dolt diff HEAD^1 HEAD` for any raw store is exactly "what this sync run pulled" — a clean unit of analysis for incremental delta UI surfaces and audits.
- Provider authors don't have to think about commit boundaries. If you find yourself reaching for `commit_run` inside a provider, you almost certainly want UPSERT instead.

The only other commits allowed in a raw store are `rescue:` commits. Anything else is a bug.

## One writer per row
**Each write to a raw entity row is complete as of that write.** The writer's job is to assemble everything it knows about the row — `payload` plus all writer-supplied identity columns — and emit it in one UPSERT. We do not have a notion of "partial" writes that leave NULL columns the writer chose not to populate, and we do not have multi-pass enrichment where writer A populates some columns and writer B fills in the rest. Both are anti-patterns.

Consequences:

- **One `ON CONFLICT(id) DO UPDATE` shape, everywhere.** Every column on the entity table (other than `id`) is updated with `excluded.<col>`. No `COALESCE(excluded.<col>, <table>.<col>)` — that pattern only exists to protect a stale-but-known value from being clobbered by a fresh-but-incomplete write, and we don't allow incomplete writes. The uniform shape is what lets the generalized bulk-upsert helper (see [Bulk-upsert as the standard write path](#bulk-upsert-as-the-standard-write-path)) cover every table.
- **One writer per row, normally.** Typically each raw entity table has a single producing downloader. If two producers can in principle write the same id (e.g. a JMAP API downloader and an mbox file-import both targeting the `emails` table), it is a configuration error to enable both for the same destination, and the semantics if you did are **last-writer-wins, not merged**. The system is not built to maintain a hodgepodge of two writers' partial knowledge of the same row.

Why the discipline matters: the alternative is per-column conflict policies (COALESCE on some columns, replace on others), which makes the UPSERT shape diverge per table, makes the chunked-multi-row helper proliferate variants, makes `dolt diff` harder to read, and makes "which writer last touched this row?" an ambiguous question. None of that is value we want to maintain.

## Bulk-upsert as the standard write path
Every download is shaped the same at the bottom: for some entity table `<t>`, upsert N rows of `(id, payload, …extras)`, paired with N rows on `<t>_bookkeeping` for `(id, fetched_at, attempt_count, last_error)`, and (if the source produced blobs) M rows on the CAS of `(blake3, byte_len, content_type, bytes)`. Doltlite charges a prolly-tree manifest mutation per `BEGIN … COMMIT`, so the right shape is **one entity-pool tx + one CAS-pool tx per batch**, each containing chunked multi-row `INSERT … ON CONFLICT(id) DO UPDATE` statements. Email's mbox downloader proved this in practice: 25k emails dropped from many minutes to ~75 seconds at FLUSH_BATCH=2000.

The principle: **every provider's download uses shared chunked-multi-row helpers for the entity-table UPSERT, the `<t>_bookkeeping` upsert, and the CAS write.** Per-row UPSERTs are an anti-pattern outside ad-hoc maintenance code.

### The shared pieces, all in `frankweiler_etl`:

- **`bulk::SQL_CHUNK` + `bulk::push_placeholders` / `bulk::push_placeholder_list`** — chunking utilities the provider's per-table multi-row `INSERT` builders use.
- **`bulk::bulk_upsert_bookkeeping(tx, table, ids, now)`** — the generic `<t>_bookkeeping` UPSERT. Call directly inside the provider's tx after the entity UPSERT.
- **`bulk::EventBatch<'a>`** — the per-table `(table, &[(id, &payload)])` shape the batch primitives share.
- **`blob_cas::BlobCas::put_many`** — chunked multi-row `INSERT OR IGNORE` over `cas_objects`, one tx per call. The per-doc `blob_cas::BlobBundle` accumulates a document's attachments during download (`add` / `add_error`) and exports its `cas_inserts()` and edge rows for these writes; the same bundle is reloaded at parse and consumed at render.
- **`doltlite_raw::bulk_upsert_events(tx, tape, &[EventBatch], now)`** — the **wire-event** chokepoint. The caller has already issued its multi-row entity UPSERTs inside `tx`; this stamps `<t>_bookkeeping` for every batch in the same tx, commits, and (if a tape is attached) appends one JSONL line per row via `EventTape::append_batch`. Tape errors log but don't fail the upsert — doltlite is the source of truth.
- **`doltlite_raw::bulk_upsert_with_tape(pool, tape, rows, payloads)`** — all-in-one variant (open tx → `bulk_upsert_in_tx` → commit → tape append). Use when the caller has a `&[T: BulkUpsertable]` vec in hand (every `WirePayloadRow`-derived provider does). Same "doltlite is truth, tape is best-effort mirror" semantics.

The chokepoint is the right tool **only for tables whose rows came off a wire**. For everything else — CAS edge tables, sidecars, file-imported entities like mbox or vcf where there is no upstream "event" — call `bulk_upsert_bookkeeping` directly inside the same tx and skip the tape. Synthesizing a fake wire payload just to feed the chokepoint would be making up data we don't have.

The `ON CONFLICT` clause is **the same shape on every table**: every non-PK column is set to `excluded.<col>`. The column list itself still varies per table (different writer-supplied extras — see [Events vs bookkeeping](#events-vs-bookkeeping-where-each-column-lives) for which extras belong on the entity table vs the bookkeeping sidecar vs as VIRTUAL+index over payload), but the conflict policy does not vary — see [One writer per row](#one-writer-per-row). That uniformity is what makes a single generic `bulk_upsert<T>(rows: &[T])` helper feasible (in flight): the only per-table input is the column list, which we can derive from the row type at compile time. What these shared helpers own today is the cross-provider boilerplate — chunked SQL, bookkeeping, commit, and (for the wire-event subset) the tape mirror.

## dolt_diff supersedes per-bucket fingerprints
The per-bucket fingerprint pattern has been **replaced with `dolt_diff_<table>` virtual tables driven by a per-source render cursor**. The render-side fingerprint CTE is gone; doltlite's prolly-tree diff answers "what changed since last render?" directly.

Mechanism: on render success, the per-source render cursor stamps the doltlite HEAD into `_render_cursor.json`. On the next render, `parse` reads that hash and runs `doltlite_raw::scan_buckets(pool, last_hash, &DiffScanSpec { global_fanout_tables, bucket_query })`, which cold-starts if any `dolt_diff_<global_fanout_table>` row is non-`unchanged` (those fan out to "render everything"), otherwise runs the per-bucket `bucket_query` across the relevant `dolt_diff_*` vtabs. Parse then loads payloads only for the surviving bucket keys.

Sidecar `source_fingerprint` and the grid_index-step compare stay — they still gate the grid_index step ("have we already loaded this sidecar's rows?"). `source_fingerprint` is now just the stable bucket UUID. This swap moves the "what's different?" decision from Rust-computed hash trees to doltlite's native diff, which it maintains anyway for `dolt diff`. The per-row `payload_blake3` columns are gone — the `WirePayloadRow` derive no longer emits them.

**Rule for new stages.** Any new derivation added to the pipeline (future Annotate step, future index shard, future projection) follows the same recipe: declare what the inputs are (content + dependency hashes), compute a deterministic hash over them, store it alongside the output, compare on re-run. The compare-and-skip loop is what makes the system feel responsive on a laptop with months of accumulated data.

## Cursor / resume strategy
Cursor / resume is the **download-side specialization** of the [Incremental update](#efficiently-incremental) pattern: "what was the last upstream identifier we successfully recorded?" answers "what's the inputs hash for the next walk?" Two patterns in the tree, picked by upstream API shape:

- **Forward-walk + refresh window** (slack, github, gitlab): resume from `max(ts)` of previously-recorded items; also re-query the trailing `refresh_window_days` to catch edits / late-arriving items. Dedup collapses the overlap to zero writes.
- **Listing diff** (anthropic, chatgpt): re-list everything each run and compare each item's listing `updated_at`/`update_time` against the stored copy; only new/changed items get a detail fetch. An optional `sync.since:` bounds the diff — items updated before it are never detail-fetched, and chatgpt's newest-first paginated listing additionally stops walking once it pages past the cutoff.
- **Time-windowed sampling** (yolink): walk `[start, now]` in fixed-stride windows. Windows align across runs and devices. Per-window UPSERT dedups re-fetched samples.

No checkpoint files. The dedup index is the resume cursor.

## Auth and credentials
Two patterns:

- **Most providers**: shell out to `latchkey curl` (see [`backend/etl/src/latchkey.rs`](/frankweiler/backend/etl/src/latchkey.rs)). Auth lives in the latchkey keyring, indexed by URL host. The provider's HTTP transport never sees the bearer token.
- **Yolink**: latchkey doesn't know about `us.yosmart.com`, and the consumer download path isn't bearer-authed — the URL itself is signed (`build_signed_url` in [`providers/yolink/src/download/mod.rs`](/frankweiler/backend/etl/providers/yolink/src/download/mod.rs)). Per-device secrets live in config (REDACT before publishing).

If you add a new provider with a new auth shape, prefer extending latchkey upstream before adding a third pattern.

## Error handling
We want enough transient error handling that syncs "usually" work.  The goals are:

- The process must make progress within a certain amount of time, or it should stop.
- If more than X count of your last requests have errored, you better stop.

Distinctions every provider should try to follow.  

- **Per-item failures are tolerated.** A transient failure on one window / page / blob — 5xx, network blip, timeout, parse error, transient permission denied, rate-limit response — should not kill the run. Log a `warn!`, increment an error counter, **leave durable evidence in the row** (see [Retry and fetch durability](#transient-vs-non-transient) below), advance the cursor, keep going. The run's `FetchSummary` reports `errors=N`.
- **Auth failures and consecutive-failure budgets are fatal.** A workspace-wide 401 / 403 from the auth provider, or N back-to-back per-item failures on the same source, should return `Err` from `fetch(...)`. Even on auth failure, the orchestrator should still `dolt_commit` to record what *did* get pulled before the failure plus a note about the problem, then exit non-zero once other pipeline pathways finish.

The yolink provider's `CONSECUTIVE_FAILURE_BUDGET = 30` is a template for the second pattern.

There are existing chokepoint mechanisms to enforce some of these rules, but not all can be generically enforced (Slack's HTTP-200 `error:"ratelimited"` body; GitHub's `403 + x-ratelimit-remaining:0`)

ChatGPT seems to have a 200 requests/hour rate limit.  You have to stop for a while once you hit it.  What's the right approach?  Do you want to sleep for an hour?  Or just run it again in an hour?  Josh: right option is run forever, up to some "how long to run without making progress before giving up".

## Transient vs non-transient
The retry mechanism is for *transient* failures. Some signals deserve a different mark:

- **Confirmed-deletion (404 on a known-existed thing).** The upstream is telling us "this is gone." A retry will only ever return 404 again. The row should carry a distinct `deleted_upstream_at` marker so we don't burn API quota retrying forever, while still preserving the row (and any history) for backpointer / outlink purposes.
- **Workspace-wide auth failures.** Per [Error handling](#error-handling) above, these are fatal and bail the run; per-row retry doesn't apply.