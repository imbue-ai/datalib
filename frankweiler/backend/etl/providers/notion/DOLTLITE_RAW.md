# Notion raw extract — doltlite design

Replaces the JSONL `raw/notion-api/{type}/{created,updated}/events.jsonl` tree
with a single doltlite database file per data source:

    <root>/raw/notion-api.doltlite_db

The schema is owned by this provider and lives in
`frankweiler/backend/etl/providers/notion/src/extract/db/`
(sqlx migrations + a thin accessor module).

## Goals

- One file per data source, pre-indexed.
- Minimal columns: PK, promoted timestamp, JSON payload, fetch bookkeeping.
- Incremental: skip re-fetching pages whose `last_edited_time` hasn't moved.
- Pre-seedable: rows can be inserted with `payload IS NULL` and filled in later.
- `--retry-failed` re-fetches rows with errors / null payloads.
- Upserts written in **batches** during a sync so a crash keeps partial progress.
  Dolt's commit layer preserves history of upstream edits.
- No created/updated event lists. We store documents, not deltas.

## Tables

All object tables share the same bookkeeping columns:

    payload          JSON NULL          -- null = known-to-exist, not yet fetched
    fetched_at       TIMESTAMP NULL     -- set when payload becomes non-null
    attempt_count    INTEGER NOT NULL DEFAULT 0
    last_attempt_at  TIMESTAMP NULL
    last_error       TEXT NULL          -- cleared on successful fetch

### `pages`
    id                UUID PRIMARY KEY     -- Notion page UUID
    parent_id         UUID NULL
    last_edited_time  TIMESTAMP NOT NULL   -- promoted from payload
    <bookkeeping>

### `blocks`
    id                UUID PRIMARY KEY
    parent_id         UUID NULL
    last_edited_time  TIMESTAMP NOT NULL
    <bookkeeping>

### `databases`
    id                UUID PRIMARY KEY
    parent_id         UUID NULL
    last_edited_time  TIMESTAMP NOT NULL
    <bookkeeping>

### `users`
    id                UUID PRIMARY KEY
    <bookkeeping>     -- no last_edited_time on users

### `comments`
    id                UUID PRIMARY KEY     -- comment UUID (from share URL fragment)
    parent_id         UUID NOT NULL        -- block or page the comment hangs off
    <bookkeeping>

### `blobs`
    id            TEXT PRIMARY KEY
    kind          TEXT CHECK(kind IN ('uploaded','external','notion_hosted'))
    owning_id     UUID NOT NULL            -- block/page that references the file
    slot          TEXT NOT NULL            -- 'image' | 'icon' | 'cover' | 'file' | ...
    content_type  TEXT NULL
    sha256        TEXT NULL
    bytes         BLOB NULL
    source_url    TEXT NULL                -- may rotate (signed S3); informational
    <bookkeeping>

`id` is `file_upload_id` when Notion supplies one (`kind = 'uploaded'`),
otherwise the synthetic key `{owning_id}:{slot}`.

### `sync_runs`
    run_id       INTEGER PRIMARY KEY AUTOINCREMENT
    started_at   TIMESTAMP NOT NULL
    finished_at  TIMESTAMP NULL
    config       JSON NOT NULL    -- the config object used for this run
    status       TEXT NOT NULL    -- 'running' | 'ok' | 'error'
    summary      JSON NULL        -- counts, errors, etc.

Append-only. One row per sync invocation.

## Fetch loop

For each object type:

1. **List/discover.** Walk Notion's list/search endpoints (paginated in-memory;
   no persisted cursor). For every `(id, last_edited_time)` we see, upsert into
   the object table.
2. **Decide what to fetch.** A row needs a detail fetch when
   `payload IS NULL` **or** the incoming `last_edited_time` is newer than the
   stored one.
3. **Fetch details.** Pull the full object, upsert payload + bump `fetched_at`,
   clear `last_error`, leave `attempt_count` as-is on success.
4. **On failure.** Increment `attempt_count`, set `last_attempt_at` and
   `last_error`. Leave previous `payload` intact.

Writes are flushed to the DB in **batches** (e.g. every N rows or every M
seconds) inside a single transaction per batch, so a crash mid-run keeps the
already-flushed rows.

### `--retry-failed`

Re-fetches rows where `last_error IS NOT NULL` OR
`(payload IS NULL AND attempt_count > 0)`. A successful fetch clears
`last_error`.

### Pre-seeding

Callers can inject `(id, NULL payload)` rows before a sync; the fetch pass
picks them up exactly like any other unfetched row.

## What's removed (Notion only, for now)

- `raw/notion-api/{type}/{created,updated}/events.jsonl`
- All calls from the Notion provider into `etl/src/{raw_store, event_store,
  sidecar, latchkey}.rs`. Those modules stay in place for Slack/GitHub/etc.
  until we generalize.

## What's not changing

- Synthesize / translate stages still consume "raw" data — they will read from
  the doltlite DB instead of JSONL.

## Open follow-ups (after Notion lands)

- Generalize the schema/accessor pattern so Slack/GitHub can adopt it.
- Decide whether to delete `etl/src/{raw_store,event_store,sidecar,latchkey}.rs`
  once all providers are migrated.
