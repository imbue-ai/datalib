# `zoom` — download

One Zoom user's meetings, over Zoom's REST API v2
(`https://api.zoom.us/v2/`): every meeting with a cloud recording, the
meetings Zoom's past-meetings report lists, who attended each, and the
AI Companion's summary of each it summarized. The render is
`zoom_render/TRANSLATE.md`.

**Nothing here has been run against a live Zoom account.** It was
built from Zoom's published API reference and from the pinned
latchkey's source; every response shape the tests replay is the one
the reference documents, with made-up TNG values. What that leaves
unverified is listed at the end.

## Signing in

latchkey's built-in `zoom` service (latchkey 3.19.3, the pin in
`datalib/backend/runtime/src/node_runtime.rs`). Its browser login
(`latchkey auth browser zoom`) signs in to Zoom, then creates a
**Server-to-Server OAuth app** in the Zoom App Marketplace with every
admin scope the account offers, activates it and keeps its account id,
client id and client secret. Creating such an app needs a Zoom account
admin, or a user granted the developer privilege; otherwise latchkey
stops before creating anything and says to ask an admin for it. The
same app made by hand is stored with `latchkey auth set-nocurl zoom
<account-id> <client-id> <client-secret>`, which the wizard's "Paste a
key" tab runs with three boxes. latchkey mints a fresh access token
from those three whenever the old one expires
(`grant_type=account_credentials`).

Every request goes out as `latchkey curl` to `https://api.zoom.us/v2/`.

### Whose meetings: `api.user`

A Server-to-Server app is the whole Zoom account, not a person in it.
Zoom's guide says such an app "wouldn't use the `me` keyword"
(Using Zoom APIs, below), and latchkey's own code notes that its tokens
"have no user context and error on /users/me". So the config names the
user, by email or Zoom user id, in `api.user`. Left out, the download
tries `GET /users/me` (which a user-level OAuth token answers), then
`GET /users?status=active&page_size=2`, and copies the account's user
if it has exactly one. With several, the run fails and says to set
`api.user`.

## Transcripts and recording files are not downloaded

A recording's transcript (VTT), chat, timeline, captions, video and
audio are each a `download_url` on `zoom.us` or a regional `*.zoom.us`
host. latchkey 3.19.3's `zoom` service signs `https://api.zoom.us/v2/`
and nothing else (`baseApiUrls` in its `services/zoom.js`; `latchkey
curl` on any other host exits 1, "No service matches URL"), so this
source does not fetch them. Fetching them would take a credential
outside latchkey. They can be added once latchkey's `zoom` service
covers the download host.

What the store does keep is the recordings listing's entry for each
meeting as Zoom sent it, `recording_files` included: each file's type,
size, recording span and links. The render names the kinds of file Zoom
holds, and a later download of the files could be owed from those rows
without listing anything again.

## What one run does

1. `GET /users/{user}` → `users`.
2. Three dated listings, each a window of at most 28 days at a time,
   newest window first:
   - `GET /users/{user}/recordings?from&to` → `recordings`
     ("The maximum range can be a month");
   - with `participants` on, `GET /report/users/{user}/meetings?type=past`
     → `reported_meetings`. Zoom's report "is limited to a month and the
     month must fall within the past six months", so this one is asked
     only about the last 175 days; it lists only meetings with two or
     more participants;
   - with `summaries` on, `GET /users/{user}/meeting_summaries?from&to`
     → `summary_listings`.

   Which windows: every stretch from `api.since` (default 2013-01-01) to
   today that the listing's `coverage` does not hold, plus the newest
   `refresh_days` (default 30) every run. A window's entries and its
   coverage are written in one transaction, so a window counts as read
   only once its every page is stored. Neighbouring windows share their
   end day so the spans merge. A window that fails ends that listing as
   a `listing:<name>` problem; the windows after it stay owed.
3. With `participants` on, `GET /past_meetings/{uuid}/participants`
   (every page) for each stored meeting that has none, once each.
   Zoom keeps who attended for 15 months, so an older meeting is not
   asked about.
4. With `summaries` on, `GET /meetings/{uuid}/meeting_summary` for each
   listed summary not held at its `summary_last_modified_time`.

Steps 3 and 4 run on `datalib_etl_web::owed`, so a stop, a failure or a
give-up is handled there; the owed set is a query over what the store
holds, never a position in a walk. A meeting UUID is percent-encoded in
a path, and encoded twice when it starts with `/` or holds `//`, as
Zoom asks.

### What a second run costs

On the TNG tape (`tests/zoom_tests/tape.rs`): the first sync is 13
requests, and a second with nothing changed is 8 (the user and two
windows of each listing, one spanning two pages), and fetches nothing.
A summary edited in Zoom moves its `summary_last_modified_time`, and is
fetched again while its meeting is inside the refresh window.

## The store

| table | key | what |
|---|---|---|
| `users` | Zoom user id | `GET /users/{user}`; `last_login_time` and `last_client_version` in the sidecar's `volatile_payload` |
| `recordings` | meeting UUID | the recordings listing's entry, `recording_files` sorted by file id (or type and start, for the files Zoom gives no id); `start_time` promoted |
| `reported_meetings` | meeting UUID | the past-meetings report's entry; `start_time` promoted |
| `participants` | meeting UUID | `{"participants": [...]}`, every page in order; `{"participants": [], "unavailable": "<why>"}` where Zoom would not say |
| `summary_listings` | meeting UUID | the summaries listing's entry; `last_modified` is the version a summary is held at |
| `summaries` | meeting UUID | `GET /meetings/{uuid}/meeting_summary`, or `{"unavailable": "<why>"}` |
| `coverage` | `(scope, lo)` | `recordings:<user id>`, `reports:<user id>`, `summaries:<user id>`: the dates each listing has read |

A meeting UUID names one meeting instance: each occurrence of a
recurring meeting has its own. Listing entries are written first-seen,
so an unchanged meeting listed again changes nothing. Nothing is ever
pruned: a recording deleted in Zoom, or past its auto-delete date,
keeps its row. There is no blob store.

## What it does not do

- Recording files of any kind (above).
- Webinars (`/webinars/…`) and Zoom Phone.
- The trash (`trash=true`), and other users than `api.user`. One source
  copies one user; a second user is a second source, sharing the
  sign-in.
- A summary edited after its meeting left the refresh window is not
  seen until that window is read again (a wider `refresh_days`).

## Inspecting the result

`.doltlite_db` files need a doltlite-linked binary; stock `sqlite3`
cannot open them. `datalib-doltlite -readonly <store> .dump | sqlite3
out.sqlite` turns one into plain SQLite.

```sh
S=<data root>/zoom/ingest/entities.doltlite_db
datalib-doltlite -readonly "$S" "SELECT id, start_time, json_extract(payload,'$.topic') FROM recordings ORDER BY start_time DESC LIMIT 10"
datalib-doltlite -readonly "$S" "SELECT r.id, json_extract(f.value,'$.file_type'), json_extract(f.value,'$.file_size') FROM recordings r, json_each(r.payload,'$.recording_files') f"
datalib-doltlite -readonly "$S" "SELECT id, json_array_length(payload,'$.participants') FROM participants"
datalib-doltlite -readonly "$S" "SELECT scope, lo, hi FROM coverage"
datalib-doltlite -readonly "$S" "SELECT id, held_version, last_error FROM summaries_bookkeeping WHERE fetched_at_utc IS NULL OR last_error IS NOT NULL"
```

## Not verified against the live API

- Every response shape. The tape follows the reference's field names
  and its examples' envelopes (`meetings`, `participants`, `summaries`,
  `next_page_token`), but no real account has answered.
- That `GET /users/me` fails under a Server-to-Server token, and how
  (latchkey's comment says it errors; the download treats any failure
  as "no user of its own").
- That `page_size=300` is accepted by the summaries listing (the
  reference gives no maximum; 300 is Zoom's usual one).
- The error codes judged: 124, 4700 and 4711 as a sign-in that must be
  redone, 200 as a paid-plan feature, 12702 and 2305 as something Zoom
  will not give for one meeting.
- The volatile fields of `users` (`last_login_time`,
  `last_client_version`), and that nothing in a recordings entry changes
  on every read. If something does, every listing of the refresh window
  rewrites its rows, which costs a commit, not data.

`ZOOM_TEST_USER=<email> bazel run
//datalib/backend/etl/providers/zoom:zoom_live` is the check to run
first against a real account (`tests/zoom_tests/live.rs`).

## Sources

- Zoom Meeting API reference, <https://developers.zoom.us/docs/api/meetings/>:
  "List all recordings" (`GET /users/{userId}/recordings`), "Get past
  meeting participants", "Get meeting reports"
  (`GET /report/users/{userId}/meetings`), "Get a meeting or webinar
  summary", "List a user's meeting or webinar summaries". Read
  2026-10-26.
- Zoom Users API reference, <https://developers.zoom.us/docs/api/users/>:
  "Get a user", "List users". Read 2026-10-26.
- Using Zoom APIs, <https://developers.zoom.us/docs/api/using-zoom-apis/>:
  the `me` keyword per app type, double-encoding a meeting UUID. Read
  2026-10-26.
- latchkey 3.19.3, `dist/src/services/zoom.js`: the service's base URL,
  its login flow, `set-nocurl`'s three arguments, token renewal, and the
  note on `/users/me`.
