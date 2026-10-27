# Todoist: how the download works

The `todoist` source copies one Todoist account through Todoist's API
v1 (`https://api.todoist.com/api/v1/`). Each kind of Todoist object
gets its own table of `(id, payload)` rows, keyed by Todoist's id; the
payload is the object as the API sent it. The render half,
`todoist_render`, turns each project into one document
(`../todoist_render/TRANSLATE.md`).

**Nothing here was checked against a live account.** This machine's
latchkey held no Todoist credential when the provider was written, so
every request and response shape is taken from Todoist's published
reference (read 2026-10-26), cited below. The tests replay made-up TNG
data in those shapes. `tests/todoist_tests/live.rs` is the check to run
first against a real account (see "Checking it against a real
account").

## Signing in

Todoist is built into latchkey (`latchkey services info todoist`: type
`built-in`, base URL `https://api.todoist.com/`, browser and pasted
sign-in, and it detects the login account). The browser login reads the
personal API token from Settings → Integrations → Developer:

```sh
latchkey auth browser todoist
```

Or paste the token yourself:

```sh
latchkey auth set todoist -H "Authorization: Bearer $(pbpaste)"
```

The token gives full read and write access; Todoist has no read-only
personal token. The download only reads.

## What one run does

Two parts, in this order (`src/ingest/mod.rs`):

1. **`POST /sync`**, form-encoded, with `sync_token` and
   `resource_types=["user","projects","sections","items","notes","project_notes","labels","collaborators"]`.
   The token is the one the last answer carried, or `*` on the first
   run, which asks for everything active. Every record the answer
   carries is written, and the answer's new `sync_token` with them, in
   the same SQL transaction (`sync_state`), so the token never claims
   records the store does not hold. After a full sync the download asks
   once more with the new token: Todoist says a large account's full
   sync may be generated a while before it is sent
   (`full_sync_date_utc`), and that an incremental sync right after
   catches what it missed.
2. **`GET /tasks/completed/by_completion_date`**, for the tasks you
   have completed. A full sync sends only active tasks, and Todoist
   answers for at most three months of completion time per request,
   so the range is cut into windows of twelve weeks
   (`src/ingest/windows.rs`), read newest first, each window paged with
   `limit=200` and the `next_cursor` of the page before. A window's
   span is written to `coverage` (scope `completed_items`) in the
   transaction that stores its last page. What is left to read is the
   range wanted, from `completed_since` (else the account's
   `joined_at`) up to now, less the spans held.

The one position kept is Todoist's own sync token, which AGENTS.md
§"A network source owes what upstream listed…" allows. The cursor
within a window is never stored: a run cut off part-way through a
window reads that window again, and a task already held at its
`updated_at` (its sidecar's `held_version`) is not written again.

## The store's shape

| table | key | written from | notes |
|---|---|---|---|
| `user` | Todoist's user id | `/sync` `user` | one row, the account. `token`, `websocket_url` and `urgent_reminder_device` are dropped before storing: the first two carry the API token. |
| `projects` | project id | `/sync` | |
| `sections` | section id | `/sync` | |
| `items` | task id | `/sync` | open tasks, and any task completed or deleted after the first sync, as `/sync` reported it (`checked`, `is_deleted`) |
| `notes` | comment id | `/sync` | comments on tasks |
| `project_notes` | comment id | `/sync` | comments on projects |
| `labels` | label id | `/sync` | personal labels only; shared labels appear only by name on the tasks that carry them |
| `collaborators` | user id | `/sync` | people who share a project with the account: their names and emails, for showing who wrote what |
| `completed_items` | task id | the completed-tasks endpoint | its own shape (`completed_by_uid`, `note_count`, `postponed_count`), so kept apart from `items` |
| `sync_state` | `sync` | `/sync` | the token the next sync sends, and when the last full sync was generated |
| `coverage` | `(scope, lo)` | the completed walk | spans of completion time read, `YYYY-MM-DDTHH:MM:SSZ` |

Each record table has its `<table>_bookkeeping` sidecar. Every key is
Todoist's own id, which API v1 makes an opaque string.

A record `/sync` sends is stamped as fetched (`Stamp::Fetched`), since
the sync sends only what changed. The account comes on every answer,
changed or not, so its sidecar is stamped only the first time
(`Stamp::FirstSeen`) and an unchanged account commits nothing.

### Volatile fields

A field that moves without anyone editing the record goes into the
sidecar's `volatile_payload` instead of the payload
(`src/ingest/schema_raw.rs`):

- a task's `day_order`, its place in the Today and Upcoming views,
  which moves as other tasks come and go there;
- the account's `karma`, `karma_trend`, `completed_count`,
  `completed_today`, `status` and `features`.

A task's `labels`, which Todoist promises in no order, are sorted
before storing.

## What a second run costs

On the tapes: a first run is 6 requests (the full sync, the incremental
one after it, and four pages over three windows of completed tasks). A
run with nothing new is one request, the sync, which writes nothing. A
run a week later is two: the sync, and the one window of completed
tasks since the last run. These are counts on made-up data; a real
account's first run grows with how many quarters it has existed and how
many tasks it completed in each, at most 200 a page.

Todoist's limits (reference, "Request limits"): 1000 incremental and
100 full syncs per user per 15 minutes, and a 15-second processing
timeout on an ordinary request. A 429 is retried after the
`Retry-After` header, or the `error_extra.retry_after` Todoist puts in
its error body.

## Deletion

Nothing is deleted from the store. `/sync` sends a deleted project,
section, task, comment or label with `is_deleted: true`; that record is
stored as sent, and render leaves it out.

## What it deliberately does not do

- **Comments on tasks completed before the first sync.** `/sync` sends
  comments on active tasks; the completed-tasks endpoint carries only
  each task's `note_count`. Comments on a task completed after the
  first sync are kept, since they arrived while it was open. Fetching
  the rest is a `GET /comments?task_id=` per task, which is not done.
- **File attachments.** A comment's `file_attachment` is kept as
  Todoist sent it, URL included; the file is not downloaded.
- **Archived projects.** A full sync sends "active resource data"; an
  archived project's own row may not come (`GET /projects/archived` is
  not read). Its completed tasks still come, and render gives them a
  document of their own titled by the project id.
- **Filters, reminders, view options, workspaces, activity log.** Not
  asked for.
- **Recurring tasks.** Completing a recurring task moves its due date
  and leaves it open; whether the completed-tasks endpoint lists each
  such completion under the same task id has not been seen.

## Not verified against the live API

Every one of these comes from the reference, not from an answer:

- that `/sync` takes a form-encoded `POST` with `resource_types` as a
  JSON array, and answers with the keys and record shapes above;
- that an incremental sync reports a task completed since the last one
  as an `items` record with `checked: true`, and a deleted one with
  `is_deleted: true`;
- that `since` and `until` are accepted as `YYYY-MM-DDTHH:MM:SSZ`, that
  `until` is exclusive, and that twelve weeks is within the endpoint's
  three months;
- that `next_cursor` is `null` (or absent) on a window's last page;
- that a refused token answers HTTP 401 with Todoist's JSON error body;
- what an expired or unknown `sync_token` answers. The download treats
  any non-2xx `/sync` answer other than 401 or 403 as a failed run,
  and keeps the token it had.

## Sources

- Todoist API v1 reference, <https://developer.todoist.com/api/v1/>:
  §"Sync" (read resources, response fields, incremental sync, full sync
  data delay), §"Tasks", §"Sections", §"Projects", §"Comments",
  §"Labels", §"User", §"Collaborators", §"File Attachments",
  §"Tasks Completed By Completion Date", §"Pagination",
  §"Request limits", §"Migrating from v9" (task URLs, the `/sync`
  exceptions to the renames).
- `latchkey services info todoist`, latchkey 3.19.3.

## Checking it against a real account

```sh
bazel run //datalib/backend/etl/providers/todoist:todoist_live
```

runs `tests/todoist_tests/live.rs` against the account latchkey holds
for `todoist` (`TODOIST_TEST_ACCOUNT` picks one of several;
`TODOIST_TEST_COMPLETED_SINCE=YYYY-MM-DD` keeps the first run short).
It runs a full sync, checks no API token reached the store, and runs
again to check the second run is incremental. It prints only counts and
the store's path.

## How to inspect the result

Stock `sqlite3` cannot open a `.doltlite_db`; use the doltlite shell
(`bazelisk build //third-party/doltlite:doltlite`):

```sh
S=<data root>/todoist/ingest/entities.doltlite_db
datalib-doltlite -readonly "$S" "SELECT sync_token, full_sync_date_utc FROM sync_state"
datalib-doltlite -readonly "$S" "SELECT id, json_extract(payload, '$.name') FROM projects"
datalib-doltlite -readonly "$S" \
  "SELECT json_extract(payload, '$.content'), json_extract(payload, '$.checked') FROM items LIMIT 20"
datalib-doltlite -readonly "$S" "SELECT scope, lo, hi FROM coverage"
datalib-doltlite -readonly "$S" \
  "SELECT COUNT(*), MIN(json_extract(payload, '$.completed_at')) FROM completed_items"
```

Or turn the whole store into a plain SQLite file:

```sh
datalib-doltlite -readonly "$S" .dump | sqlite3 todoist.sqlite
```
