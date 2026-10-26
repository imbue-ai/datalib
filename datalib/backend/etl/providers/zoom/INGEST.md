# `zoom` — download

One Zoom user's meetings, over Zoom's REST API v2
(`https://api.zoom.us/v2/`): every meeting with a cloud recording, the
recording's transcript and other small files, who attended, the
meetings that were not recorded, and the AI Companion's summary of
each. The render is `zoom_render/TRANSLATE.md`.

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
stops before creating anything and says to ask an admin for it. The same app made by hand
is stored with `latchkey auth set-nocurl zoom <account-id> <client-id>
<client-secret>`, which the wizard's "Paste a key" tab runs with three
boxes. latchkey mints a fresh access token from those three whenever
the old one expires (`grant_type=account_credentials`).

latchkey signs `https://api.zoom.us/v2/` and nothing else
(`baseApiUrls` in its `services/zoom.js`; `latchkey curl` on any other
host exits 1, "No service matches URL"). A recording's `download_url`
is on `zoom.us` (or a regional `*.zoom.us`), so downloads go out as
plain `curl` with the meeting's `download_access_token`, which
`GET /meetings/{uuid}/recordings?include_fields=download_access_token`
hands out, in an `Authorization: Bearer` header, as Zoom documents. A
redirect is followed without that header. The token is never stored.

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
3. Recording files. Every file a stored `recordings` row names with a
   `download_url` is owed until `recording_files` holds it at its
   `file_size`: the transcript (`TRANSCRIPT`, VTT), the closed captions
   (`CC`), the chat (`CHAT`, TXT), the timeline (`TIMELINE`, JSON), the
   poll results (`CSV`), the summary file (`SUMMARY`, JSON) when Zoom
   lists one. Video and audio (`MP4`, `M4A`) only with `api.media`, and
   each over `common.blob_size_limit_bytes` is skipped with an
   `over_size_limit` warning and stays owed. Each meeting's
   `download_access_token` is asked for once per run.
4. With `participants` on, `GET /past_meetings/{uuid}/participants`
   (every page) for each stored meeting that has none, once each.
   Zoom keeps who attended for 15 months, so an older meeting is not
   asked about.
5. With `summaries` on, `GET /meetings/{uuid}/meeting_summary` for each
   listed summary not held at its `summary_last_modified_time`.

Steps 3 to 5 run on `datalib_etl_web::owed`, so a stop, a failure or a
give-up is handled there; the owed set is a query over what the store
holds, never a position in a walk. A meeting UUID is percent-encoded in
a path, and encoded twice when it starts with `/` or holds `//`, as
Zoom asks.

### What a second run costs

On the TNG tape (`tests/zoom_tests/tape.rs`): the first sync is 20
requests, and a second with nothing changed is 8 (the user and two
windows of each listing, one spanning two pages), and fetches nothing.
A transcript someone edits in Zoom's web portal comes back another
size and is fetched again; a summary edited in Zoom moves its
`summary_last_modified_time`. Both are only seen while the meeting is
inside the refresh window.

### Why video and audio are off by default

The transcript is what makes a meeting searchable, and it is a few
tens of KB; an hour of video is several hundred MB, and the
`shared_screen_with_speaker_view` and gallery variants of one meeting
are each that size again. Copying them by default would put gigabytes a
week into the blob store of anyone who records meetings, for bytes
Zoom already keeps and nothing here renders. `api.media = true` with a
`common.blob_size_limit_bytes` copies them up to that size.

## The store

| table | key | what |
|---|---|---|
| `users` | Zoom user id | `GET /users/{user}`; `last_login_time` and `last_client_version` in the sidecar's `volatile_payload` |
| `recordings` | meeting UUID | the recordings listing's entry, `recording_files` sorted by file key; `start_time` promoted |
| `reported_meetings` | meeting UUID | the past-meetings report's entry; `start_time` promoted |
| `participants` | meeting UUID | `{"participants": [...]}`, every page in order; `{"participants": [], "unavailable": "<why>"}` where Zoom would not say |
| `summary_listings` | meeting UUID | the summaries listing's entry; `last_modified` is the version a summary is held at |
| `summaries` | meeting UUID | `GET /meetings/{uuid}/meeting_summary`, or `{"unavailable": "<why>"}` |
| `recording_files` | `{meeting uuid}#{file key}` | CAS edge: the file's bytes, `blake3` NULL for a file Zoom no longer has (with a warning). The file key is Zoom's file id, or for a `CC` or `TIMELINE` file, which Zoom gives no id, the lower-cased type and the recording stretch's start, `timeline@2026-10-05T09:01:00Z` |
| `coverage` | `(scope, lo)` | `recordings:<user id>`, `reports:<user id>`, `summaries:<user id>`: the dates each listing has read |

A meeting UUID names one meeting instance: each occurrence of a
recurring meeting has its own. Listing entries are written first-seen,
so an unchanged meeting listed again changes nothing. Nothing is ever
pruned: a recording deleted in Zoom, or past its auto-delete date,
keeps its row and every file copied of it.

## What it does not do

- Webinars (`/webinars/…`) and Zoom Phone recordings.
- The trash (`trash=true`), and recordings of other users than
  `api.user`. One source copies one user; a second user is a second
  source, sharing the sign-in.
- `participant_audio_files` (each participant's own audio track).
- The newer `GET /meetings/{id}/transcript` endpoint, for a transcript
  that exists without a cloud recording.
- Meeting chat outside a recording (Team Chat), polls and Q&A beyond the
  recording's CSV file, and the `TIMELINE` and `CHAT` files' contents in
  the render: they are kept, and linked from the meeting's page.
- A summary or transcript edited after its meeting left the refresh
  window is not seen until that window is read again (a wider
  `refresh_days`, or an earlier `since` with coverage reset).

## Inspecting the result

`.doltlite_db` files need a doltlite-linked binary; stock `sqlite3`
cannot open them. `datalib-doltlite -readonly <store> .dump | sqlite3
out.sqlite` turns one into plain SQLite.

```sh
S=<data root>/zoom/ingest/entities.doltlite_db
datalib-doltlite -readonly "$S" "SELECT id, start_time, json_extract(payload,'$.topic') FROM recordings ORDER BY start_time DESC LIMIT 10"
datalib-doltlite -readonly "$S" "SELECT r.id, json_extract(f.value,'$.file_type'), json_extract(f.value,'$.file_size') FROM recordings r, json_each(r.payload,'$.recording_files') f"
datalib-doltlite -readonly "$S" "SELECT id, blake3 IS NOT NULL FROM recording_files"
datalib-doltlite -readonly "$S" "SELECT scope, lo, hi FROM coverage"
datalib-doltlite -readonly "$S" "SELECT id, held_version, last_error FROM recording_files_bookkeeping WHERE fetched_at_utc IS NULL OR last_error IS NOT NULL"
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
- The transcript's VTT layout, `Speaker Name: words` per cue, and that a
  cue without a known speaker has no prefix. A cue whose text starts
  with a phrase and a colon would be read as that speaker.
- That a `download_url` answers 200 with the bytes, or a redirect to a
  signed URL, given the `download_access_token` in a header. The
  reference documents the header form; the redirect handling follows
  its "the URL may be a redirect".
- The error codes judged: 124, 4700 and 4711 as a sign-in that must be
  redone, 200 as a paid-plan feature, 12702, 2305, 3301 and 3322 as
  something Zoom will not give for one meeting.
- Whether `file_size` changes when a transcript is edited in the web
  portal, which is what makes an edited transcript owed again.
- The volatile fields of `users` (`last_login_time`,
  `last_client_version`), and that nothing in a recordings entry changes
  on every read. If something does, every listing of the refresh window
  rewrites its rows, which costs a commit, not data.

`ZOOM_TEST_USER=<email> bazel run
//datalib/backend/etl/providers/zoom:zoom_live` is the check to run
first against a real account (`tests/zoom_tests/live.rs`).

## Sources

- Zoom Meeting API reference, <https://developers.zoom.us/docs/api/meetings/>:
  "List all recordings" (`GET /users/{userId}/recordings`), "Get meeting
  recordings" (`GET /meetings/{meetingId}/recordings`, `include_fields`),
  "Get past meeting participants", "Get meeting reports"
  (`GET /report/users/{userId}/meetings`), "Get a meeting or webinar
  summary", "List a user's meeting or webinar summaries", "Get a meeting
  transcript". Read 2026-10-26.
- Zoom Users API reference, <https://developers.zoom.us/docs/api/users/>:
  "Get a user", "List users". Read 2026-10-26.
- Using Zoom APIs, <https://developers.zoom.us/docs/api/using-zoom-apis/>:
  the `me` keyword per app type, double-encoding a meeting UUID. Read
  2026-10-26.
- latchkey 3.19.3, `dist/src/services/zoom.js`: the service's base URL,
  its login flow, `set-nocurl`'s three arguments, token renewal, and the
  note on `/users/me`.
