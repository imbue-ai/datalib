# UI-driven incremental sync — productivity-suite shell, v0

## Overview

Turn the mixed-up-files from a CLI-driven pipeline into an Outlook-flavored
productivity app. The user clicks "Sync" in the UI; the backend runs the
download → ingest → render pipeline as a background job; a persistent
Lightroom Classic-style progress chrome reports status while the rest of
the app stays fully usable against the current database.

Three architectural layers stay strictly separated:

1. **Download / mirror** — pull native-format data into `$DATA_ROOT/raw/<source>/`.
2. **Ingest / index** — translate to the union SQL schema (`documents`, `grid_rows`)
   in Dolt and render markdown.
3. **Query / display / annotate** — Vue UI on top.

v0 ships the plumbing end-to-end for **at least one** real source (Slack)
plus one takeout-style source, with the rest of the providers migrating
incrementally on the same plumbing.

Note: We renamed personal-mirror to mixed-up-files

## Goals (v0)

- Single `~/.config/mixed-up-files/config.yaml` describes every source
  *and* drives every downloader. Standalone CLI invocation reads the same
  YAML block.
- New `/sync` route lists every source: name, provider, last sync, status,
  per-source "Sync" button + global "Sync all".
- Global Lightroom-style progress chrome in the top-left of `App.vue`,
  visible on every route. Collapsible. Click to expand for per-job
  progress, log tail, cancel button.
- Backend supervises a `mixed-up-files worker` child process (same
  pattern as `dolt sql-server`); worker consumes a Dolt-backed `sync_jobs`
  queue.
- `sync_jobs` row transitions land in `dolt log` (just like `feedback`).
- Concurrency caps live in `config.yaml`; default = 1 job per provider,
  configurable global cap.
- Workers report their state and progress so that you can mouse over a report and see what they're up to. And if they seem stuck on the current state for a long time, you can cancel them. 
- Cancel = SIGTERM the worker child; downloaders are already incremental.
- Incremental ingest at **document** granularity:
  - app-managed downloaders write a **new dated raw file per run**; ingest
    tracks ingested-vs-not via a `download_runs` table;
  - takeouts are re-ingested only when path-mtime changes;
  - `grid_rows` updates per-document are issued as bulk delete+insert
    (Dolt itself elides commits when the resulting set is unchanged);
  - `.md` re-render is skipped when the document's row-set hash is
    unchanged.
- `.qmd` → `.md` hard cutover. Output path moves to
  `$DATA_ROOT/rendered_md/<provider>/...`. Backend serves `.md` only.
- New `documents` table; every `grid_rows` row references a document by
  UUID (many-to-1, FK).
- App shell grows a hamburger menu → `/search`, `/sync`, `/prefs`.
- Some kind of end-to-end testing of the whole download and ingest pipeline, ideally driven by replaying the sample Star Trek themed data we have captured from individual services. 

## Non-goals (v0)

- No in-UI editor for `sources:` — managed by editing `config.yaml`.
- No Tauri packaging in this milestone (the UI stays web-only; we just
  avoid choices that would block Tauri later).
- No scheduling/cron of syncs from the UI (manual button only).
- No multi-user, auth, or remote workers.
- No migration tooling for existing `~/data-root` installs — re-ingest
  from scratch from raw data on disk (re-download raw if needed).
- No new download providers; this milestone is plumbing for the existing
  set (`claude_web`, `chatgpt_web`, `slack_web`, `github_web`,
  `gitlab_web`, plus the takeout/export-dir kinds).
- No semantic / vector indexing changes.

## Inputs

`~/.config/mixed-up-files/config.yaml` (unified):

```yaml
root: /Users/thad/data/mirror

concurrency:
  global: 3
  per_provider: 1

dolt:
  host: 127.0.0.1
  port: 3306
  user: root
  repo_dirname: dolt_repo

sources:
  - name: slack-imbue
    provider: slack
    managed: true                       # app owns the raw dir
    sync:                               # discriminated by provider
      kind: slack_web
      channels: [general, engineering]
      auth_profile: imbue
      # other slack_web flags...

  - name: claude-web
    provider: anthropic
    managed: true
    sync:
      kind: claude_web
      max_conversations: null
      auth_profile: default
    provenance: api

  - name: claude-export
    provider: anthropic
    managed: false                      # takeout; user manages path
    kind: export_dir
    path: ~/backups/claude
    provenance: export
    # no `sync:` block — sync button is hidden/disabled for this row
```

The `sync:` block is a **discriminated union** keyed by `kind`
(`claude_web`, `chatgpt_web`, `slack_web`, `github_web`, `gitlab_web`).
Each shape is a Pydantic model in `src/config/sync.py` and codegen'd to
TS + Rust (same path as the other shared schemas).

## Data filesystem layout under `<root>`

```
<root>/
  dolt_repo/                            # Dolt repo (existing)
  raw/                                  # NEW: app-managed downloader output
    slack-imbue/
      2026-05-13T14-22-05.jsonl
      2026-05-13T18-04-11.jsonl
    claude-web/
      2026-05-12T09-00-01.json
  rendered_md/                          # NEW: markdown output (was per-provider .qmd)
    anthropic/<account>/llm_chats/<conv_uuid>.md
    slack/<workspace>/channels/<chan>/<thread>.md
  media/                                # unchanged (Slack image symlinks etc.)
  mirror.sqlite                         # reference-only, unchanged
```

Takeout sources keep using the user-provided `path:` — they are *not*
moved under `raw/`.

## Schemas

Three new Dolt tables plus a column on `grid_rows`.

### `documents`
| col | type | notes |
|---|---|---|
| `document_uuid` | `VARCHAR(36)` PK | one per renderable document (conversation, thread, page, …) |
| `source_name`   | `VARCHAR(64)`    | FK-ish to `sources[].name` in YAML |
| `provider`      | `VARCHAR(32)`    | denormalized for query |
| `kind`          | `VARCHAR(32)`    | `chat`, `thread`, `page`, … |
| `title`         | `VARCHAR(512)`   | renderable title |
| `created_at`    | `VARCHAR(40)`    | ISO-8601 with offset (per project convention) |
| `updated_at`    | `VARCHAR(40)`    | ISO-8601 with offset |
| `md_path`       | `VARCHAR(1024)`  | relative to `<root>/rendered_md/` |
| `row_set_hash`  | `CHAR(64)`       | SHA-256 over the canonical row tuples that feed this doc |
| `renderer_version` | `VARCHAR(32)` | bumped on renderer change to force re-render |
| `rendered_at`   | `VARCHAR(40)`    | ISO-8601 with offset |

### `grid_rows` (existing) — add column
- `document_uuid VARCHAR(36) NOT NULL` (FK → `documents.document_uuid`).
- **Invariant**: every grid_rows row has exactly one document_uuid. Many
  grid_rows may share a document_uuid (e.g. many messages of one thread).

### `sync_jobs`
| col | type | notes |
|---|---|---|
| `id` | `VARCHAR(36)` PK | uuid |
| `source_name` | `VARCHAR(64)` | nullable for "all-sources ingest" jobs |
| `kind` | `VARCHAR(16)` | `download` \| `ingest` \| `render` \| `all` |
| `state` | `VARCHAR(16)` | `pending` \| `running` \| `done` \| `failed` \| `canceled` |
| `created_at` | `VARCHAR(40)` | ISO-8601 with offset |
| `started_at` | `VARCHAR(40)` | nullable |
| `finished_at` | `VARCHAR(40)` | nullable |
| `error` | `TEXT` | nullable |
| `pid` | `INT` | child process pid while running |
| `progress_pct` | `FLOAT` | 0.0..1.0 |
| `progress_msg` | `VARCHAR(512)` | latest line of human progress |

Transitions are committed via `CALL DOLT_COMMIT('-Am', 'sync_job: <id>
<state>')` on the same managed pool connection (same idiom as feedback).

### `download_runs`
| col | type | notes |
|---|---|---|
| `id` | `VARCHAR(36)` PK | uuid |
| `source_name` | `VARCHAR(64)` | |
| `raw_path` | `VARCHAR(1024)` | relative to `<root>/raw/` |
| `kind` | `VARCHAR(8)` | `full` \| `delta` |
| `started_at` | `VARCHAR(40)` | |
| `finished_at` | `VARCHAR(40)` | |
| `ingested_at` | `VARCHAR(40)` | nullable; set when ingest finishes |
| `doc_uuids_touched` | `JSON` | populated by ingest |

All four schemas live as `schemas/*.schema.json` and are codegen'd to
Python + Rust + TypeScript per the existing pattern (see
`docs/grid_rows.md`).

## Components

### 1. `src/config/` — unified config + discriminated `sync:` schema

- New `src/config/sync.py` with a Pydantic discriminated union keyed by
  `kind`. One subclass per existing downloader: `SlackWebSync`,
  `ClaudeWebSync`, `ChatgptWebSync`, `GithubWebSync`, `GitlabWebSync`.
  Each carries that downloader's existing CLI flags as typed fields.
- `src/ingest/config.py`'s `SourceConfig` gains `managed: bool` and
  optional `sync: SyncConfig | None`.
- Existing downloaders (`src/download/<provider>_web.py`) gain a
  `--source-name NAME` flag; when given, they load the corresponding
  `sources[i].sync` block out of `config.yaml` and use it instead of
  per-flag CLI args. Old per-flag CLI invocation continues to work
  unchanged.

### 2. App-managed downloader file layout — dated runs

- Every app-managed downloader writes its output to
  `$DATA_ROOT/raw/<source-name>/<ISO-timestamp>.<ext>` (always include a localized tz with offset, not utc)
- Each run produces a **delta** (items the downloader didn't already
  see), unless invoked with `--full`, which produces a full snapshot.
- Each run inserts a `download_runs` row at start (with `kind`,
  `started_at`, `raw_path`) and updates it at end (`finished_at`).
  NOTE: I'm not sure the downloaders themselves should be aware of the Dolt DB tables. I think they should maybe function beneath that level of abstraction. 
- Downloaders no longer write/read per-provider state files; the
  watermark is reconstructed from the most recent `download_runs` row
  for the source (`finished_at` becomes the next run's `since`).

### 3. `src/ingest/` — incremental, document-scoped

- New module `src/ingest/documents.py` populates the `documents` table
  per-provider. Each provider's existing per-doc renderer is refactored
  to emit a typed `Document` + a list of `GridRow` objects keyed by
  `document_uuid`.
- `src/ingest/grid_rows.py` switches from full-rebuild to per-document
  upsert: for each touched `document_uuid`, delete its existing rows and
  insert the new set inside one transaction. Dolt's content-addressed
  storage elides a no-op write into a no-op commit.
- `src/ingest/render.py` writes `.md` (no `.qmd`). Filename:
  `<document_uuid>.md` (drop the `__slug` infix; slug only appears in
  URLs, not on disk). Before writing, check `documents.row_set_hash` +
  `renderer_version`; skip the write if both unchanged.
- Ingest's input set comes from `download_runs WHERE ingested_at IS
  NULL` for app-managed sources, or from path+mtime change for takeout
  sources.
- After successful ingest of a `download_runs` row, set `ingested_at`
  and `doc_uuids_touched`, then `DOLT_COMMIT('-Am', 'ingest: <run-id>')`.
- Orphan-cleanup pass: after a successful per-source ingest, any
  `.qmd` files under the old layout for that provider are deleted; any
  `.md` file whose `document_uuid` is not in the `documents` table is
  deleted.

### 4. `src/worker/` (new) — `mixed-up-files worker`

- Long-running Python process; entrypoint `python -m worker`.
- Connects to Dolt (the backend's `dolt sql-server`), polls `sync_jobs`
  for `state='pending'` rows, respecting `concurrency.global` and
  `concurrency.per_provider`.
- For a `download` job, shells out to the corresponding
  `python -m download.<provider>_web --source-name <name>` and tails the
  child's stdout to populate `progress_pct` / `progress_msg`.
- For an `ingest`/`render` job, invokes the in-process Python ingest
  pipeline directly (no subprocess).
- For `all`, enqueues child download jobs + a final ingest job and
  marks itself done when children settle.
  NOTE: since ingest is now more incremental, can we run it immediately after download finishes, per source?
- On cancel (`state` flipped to `canceled` by the backend), the worker
  SIGTERMs the active child, records the cancel reason, and moves on.
- On worker startup, any `running` rows with a `pid` that is no longer
  alive are flipped to `failed` (state recovery).

### 5. `frankweiler/backend/` — supervise worker, expose API

- `dolt_server.rs` already supervises Dolt; add a parallel
  `worker.rs` that supervises the Python worker child the same way:
  spawn on startup, restart-on-crash with backoff, SIGTERM on shutdown,
  capture stdout/stderr to ring buffer for the UI's "log" pane.
  NOTE: Maybe there could be a way to view the status of these of adult database and backend from the frontend?
- New routes (in `frankweiler/backend/http/src/lib.rs`):
  - `GET  /api/sync/sources` — return the resolved `sources:` list
    (name, provider, managed, sync-supported bool, last-run summary
    from `download_runs`, last `sync_jobs` for this source).
  - `POST /api/sync/jobs` — body `{source_name, kind}`; inserts a
    `sync_jobs` row in `pending`; returns the row.
  - `POST /api/sync/jobs/all` — enqueues a parent `all` + per-source
    children.
  - `GET  /api/sync/jobs` — query params `state`, `since`; for the
    progress chrome.
  - `GET  /api/sync/jobs/{id}` — single row; UI polls this for progress.
  - `POST /api/sync/jobs/{id}/cancel` — flips `state` to `canceled`;
    worker picks up.
  - `GET  /api/sync/jobs/{id}/log` — last N lines of captured worker
    stdout for that job (worker writes log lines to a per-job ring
    buffer file under `<root>/state/job-logs/<id>.log`).
- All POST routes are localhost-only (matches existing bind policy).

### 6. `frankweiler/ui/` — sync page + Lightroom chrome

- New route `/sync` → `SyncView.vue`. Lists every source as a row:
  name, provider icon, "managed" badge, last-sync timestamp + status,
  row count delta since last sync, "Sync" button. Header row with
  "Sync all". Source rows without a `sync:` block render with the
  button disabled and a tooltip "manual import — no sync configured".
- New persistent component `<SyncProgressChrome>` mounted in
  `App.vue`'s top-left, visible on every route. Strip view:
  - Collapsed: single line with spinner, count of running jobs, current
    progress bar of the top-priority job. Hidden when no jobs are
    active **and** no jobs finished in the last N seconds.
  - Expanded: panel with one row per active+recently-finished job,
    progress bar, cancel button, expand-for-log control.
- New `App.vue` chrome: top bar with title + hamburger button on the
  right. Hamburger opens a side sheet linking `/search`, `/sync`,
  `/prefs`.
- API client (`frankweiler/ui/src/api.ts`) gains the matching
  `fetchSyncSources`, `enqueueSync`, `enqueueSyncAll`, `cancelSyncJob`,
  `fetchSyncJobs`, `fetchSyncJobLog`.
- Polling: while the chrome has any active job, it polls
  `/api/sync/jobs?state=running,pending` every 1s. When idle it polls
  every 5s for ~30s after the last completion then stops. (No SSE in
  v0; revisit if the polling load becomes a problem.)

### 7. `.qmd` → `.md` cutover

- All renderer call sites switch the extension and the destination
  directory in one commit.
- `frankweiler/backend/http/src/lib.rs` `/api/chat/{uuid}` lookup
  becomes `<root>/rendered_md/**/<uuid>.md` (uuid is the primary key now
  that the `__slug` infix is gone).
- No dual-read path; old `.qmd` files are blown away by the migration
  step (see below).
- `qmd_indexer` continues to invoke `@tobilu/qmd` via npx; we just feed
  it `.md` files instead of `.qmd`. (qmd accepts plain markdown; the
  rename is purely an extension change at the filesystem level.)

## Migration

There is no migration script. Steps for existing installs:

1. Stop the backend.
2. Wipe `grid_rows` in Dolt; drop the old per-provider tables if any
   still exist.
3. `rm -rf <root>/anthropic <root>/openai <root>/slack <root>/notion`
   (the old per-provider rendered_md tree).
4. Re-run downloads from scratch with the new code; raw data lands
   under `<root>/raw/<source-name>/`.
5. Run a full ingest.

## Test plan

- **Schema codegen**: `bazelisk build //schemas:...` round-trips
  Python + Rust + TS for the new `documents`, `sync_jobs`,
  `download_runs` shapes; `grid_rows` row mapper in
  `frankweiler/backend/core/src/db.rs` updated for the new
  `document_uuid` column.
- **Ingest snapshot tests** (`tests/test_snapshots.py`): re-bake
  `bazelisk test //tests:test_snapshots --test_arg=--snapshot-update`
  for the renamed `.md` outputs, the new `documents` rows, and the
  unchanged `grid_rows` content (modulo the added `document_uuid`).
- **New ingest unit test**: feed two consecutive raw runs where the
  second contains one updated conversation; assert that exactly one
  `.md` is rewritten, exactly one `documents.row_set_hash` changes,
  and the other documents' `rendered_at` is untouched.
- **Worker integration test** (pytest): spin up Dolt fixture, insert a
  `sync_jobs` row, point at a tiny fake downloader, assert state
  transitions land in `dolt log`.
- **Backend Rust unit tests**: route smoke for the new `/api/sync/*`
  endpoints against an in-memory mock pool.
- **E2E (Playwright)**: load `/sync`, assert at least one source row
  renders; click "Sync" against a fixture source backed by a fake
  downloader; assert the chrome shows the running job, the row's
  last-sync timestamp updates on completion, and cancel during run
  flips state to `canceled`.
- **`bazelisk test //...`** is the source of truth (per AGENTS.md).

## Open questions

- Worker concurrency under cancel: should an in-flight ingest job be
  cancel-able mid-document, or is "cancel" only honored at document
  boundaries? Default-assume document-boundary cancel for v0.
- `documents.title` for Slack threads — derive from first message's
  first 80 chars, or pull from channel name + timestamp? Default to
  current `render_slack` logic.
- Whether the hamburger menu lives at top-left (Outlook style) or
  top-right (Apple Mail style). Lightroom chrome takes the top-left,
  so this milestone puts the hamburger at top-right by default.
- Whether `provenance: api` vs `export` still matters once everything
  goes through the same `documents`/`grid_rows` pipeline. Keep the
  field for v0 because the merge_anthropic api-wins precedence still
  uses it.

## Task list

Roughly dependency-ordered. Each task is a single PR-sized unit.

### Phase A — schema + config plumbing
1. Add `documents.schema.json`, `sync_jobs.schema.json`,
   `download_runs.schema.json`; wire into existing codegen
   (`schemas/BUILD.bazel`).
2. Add `document_uuid` column to `grid_rows.schema.json`; regen.
3. Introduce `src/config/sync.py` discriminated union + per-provider
   subclasses; extend `SourceConfig` with `managed` + `sync`.
4. `uv export` → regenerate `requirements.txt` (per AGENTS.md).

### Phase B — downloaders read YAML
5. Add `--source-name` to every downloader; resolve `sync:` block from
   the unified `config.yaml`; preserve old CLI flag invocation.
6. Switch app-managed downloaders to write dated raw files under
   `<root>/raw/<source-name>/`; remove their previous state files.
7. Each downloader inserts/updates a `download_runs` row.

### Phase C — incremental ingest
8. Build the `documents` table; refactor renderers to emit
   `(Document, [GridRow])` keyed by `document_uuid`.
9. `grid_rows` writer switches to per-document delete+insert.
10. Renderer skips re-render when `row_set_hash` + `renderer_version`
    are unchanged; writes to `<root>/rendered_md/<provider>/...`.
11. Cutover `.qmd` → `.md` (rename + extension + backend lookup).
12. Orphan cleanup pass at end of ingest.

### Phase D — worker
13. New `src/worker/` package + `python -m worker` entrypoint.
14. Worker polls `sync_jobs`, respects concurrency caps, shells out to
    downloaders, calls ingest in-process.
15. Cancel handling (SIGTERM child, mark `canceled`).
16. Worker startup recovery (orphaned-`running` → `failed`).

### Phase E — backend API
17. `worker.rs` supervises the Python worker (mirror of
    `dolt_server.rs`).
18. New `/api/sync/*` routes (`sources`, `jobs`, `jobs/all`,
    `jobs/{id}`, `jobs/{id}/cancel`, `jobs/{id}/log`).
19. Per-job log ring buffer under `<root>/state/job-logs/<id>.log`.

### Phase F — UI
20. App shell: title bar + hamburger sheet linking `/search`, `/sync`,
    `/prefs`.
21. `SyncView.vue` listing sources + per-source / global buttons.
22. `<SyncProgressChrome>` global component + polling logic.
23. Playwright e2e for the sync flow against a fixture downloader.

### Phase G — verification
24. Re-bake snapshot goldens; `bazelisk test //...` green.
25. Smoke against a real source (Slack) end-to-end from the UI.
