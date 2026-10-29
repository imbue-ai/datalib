# Audit: one "sync everything" on a real root, read from its logs

A record, not reference. It reads the run store
(`system/runs/runs.sqlite`), the supervisor's record
(`system/supervisor.sqlite`) and the stores on disk of one long-lived
personal root after a "sync everything" on 2026-10-29. The build was
`4d9f99261` (#1458). Seventeen sources ran, one of them a large
Slack workspace (about 336 conversations) still filling in its old
history. Nothing here comes from a test. Every number was read from that
root. The root's contents stay out of this file; its counts and timings
do not. How to run an audit like this one is in
[`testing.md`](../testing.md) §"Auditing a real sync's logs".

The run started at 09:03 UTC. Every source but the large Slack one was
done by 09:16. The search index was current by 09:35. The large Slack
download was stopped from the app at 09:42, 40 minutes in. Each finding
below says what was seen and how sure it is. They are ordered by what
they cost.

## 1. A Slack download fetches files one at a time

From 09:12 to 09:42 the large Slack download walked the old history of
two channels. It fetched one 200-message page every 70–110 s. The time
went to file downloads. In every ten-minute window, the
`duration_ms` of its `fetched one item` lines for `files.slack.com`
sum to 580–587 s, and each file took about 0.5 s. The fetches are
back to back, with nothing in parallel. The run downloaded 3,484
files. Rate limits cost about one minute of the 40: six HTTP 429s, all
on `conversations.replies`. Each one waited out Slack's 10 s
Retry-After, as it should.

File downloads are not Web API calls, and nothing here showed them
being limited. A small pool of concurrent file fetches would probably
cut a backfill several times over. Watch the 429 count when trying it.

## 2. The Slack ETA cannot converge on a backfill

From 09:09 to 09:42, `queued` stayed between 6,260 and 6,460 while
`done_total` rose by about 180 a minute. Each history page announces
its threads and files as it is listed. A deep channel therefore adds
work as fast as work comes off, and the queue only ever sees one page
ahead. A pace-based ETA reads about 35 minutes for 35 minutes. Earlier
in the run it reads far too short. Between 09:06:38 and 09:07:54,
`done_total` jumped from 128 to 9,953, mostly from files skipped as
over the source's 50 KB limit (finding 4) or already held. Those cost
no request, so they made the pace look 50 times faster than the
fetching that followed. That is the shape #1433 fixed for messages:
free units and costly units sharing one bar.

What the Manage row actually showed is not recorded. No metric or log
line holds the ETA.

## 3. Nothing shows whether the Slack backfill is converging

`slack_walks_planned` across the last five runs of the large source
reads `walking` = 142, 96, 68, 89, 86. That number mixes conversations
with something new at the head and conversations whose older history is
still owed. No line says how many conversations still owe history,
or how far back each walk has reached. The 10-28 run was stopped after
three hours, and today's after 40 minutes. Neither wrote a
`the download is done` summary. The stopped run logged `still running
10s after the interrupt; exiting without a final commit`, so its
counts survive only as metrics.

## 4. Files over the size limit are owed forever

A file over `blob_size_limit_bytes` is skipped and never held, so the
next run owes it again. The small Slack source logs
`media[too_large=206]` on every run since 10-27, the same 206 each
time. The large source skipped 12,750 today. Each skip is free in
requests, but it writes one debug line and one problems row, and it
counts as done on the bar (finding 2). A skipped file whose size has not
changed could be recorded as decided.

## 5. The run store's WAL has not been checkpointed since 10-21

`runs.sqlite` itself was last written on 10-21. Its `-wal` file is
3.8 GB. `datalib/backend/runs/src` sets WAL mode and never checkpoints.
SQLite's automatic checkpoint is passive and makes no progress while a
reader holds an older snapshot. The app's reads pay for it. Today
`GET /api/manage/rows` was answered 359 times, 194 of them flagged as
slow, the worst in 28 s. One warn-count query over the log took 9.2 s,
and an insert into `disk_free` took 3 s. Not checked: which reader holds
the checkpoint back, and what
`PRAGMA wal_checkpoint` reports. Running that pragma writes, so it was
not run on the live root.

## 6. The search index keeps everything it ever retired

qmd's `index.sqlite` is 4.2 GB. It holds 108k retired document rows
(`active = 0`), 58,161 embedded content hashes that no live document
uses, and 4,771 content rows likewise unused. Its full-text table holds
exactly the 167,897 live documents, so search results are right. Only
the file is bloated. Nothing in `qmd_indexer` or `datalib_step` calls
qmd's cleanup.

The grid index is a 9.9 GB doltlite file. That size was not examined.

## 7. Repeated work that is not the change of the day

Today every render ran in full ("render params changed", one source
"renderer version changed"). The keyword index then dropped and
re-added every document of every source: `N new, 0 updated, 0
unchanged, N removed`. That is #1458 moving each document from
`<uuid>/all.md` to `<xx>/<uuid>.md`, a one-time cost. On disk the move
is complete: no `all.md` and no empty directories in any source that
rendered. These do repeat:

- **The time-series stores run a migration on every open.** The
  shared-ladder rung "problems.last_seen_at_utc becomes changed_at_utc"
  logs `migrated` on each open of every `render_time_series` store and
  of the time-series index, five times today for one source. Rung
  application rides the owner's next commit, and a writer that finds
  nothing to write appears not to seal. A source that wrote chunks
  today still migrated again, so the cause is not settled.
- **One changed row re-renders every ChatGPT conversation.** On a run
  that fetched nothing, the ChatGPT render's incremental pass found
  `changed_rows: 1` and `stale_buckets: 582`, and re-rendered all 582
  conversations to identical output. It has done the same on earlier
  runs. Not checked: which row it was.
- **A Google Drive change re-fetches its images.** On the last three
  runs with a few files owed, `images=126` with `images_held` at 9 or
  22. The images of a changed document come down again instead of being
  recognized.
- **About 190–250 Google Calendar events "update" a day**, out of
  about 9,800, and each one re-renders and re-indexes. That may be real
  change. A volatile field Google bumps is worth ruling out.
- **Garmin re-reads the last eight days of all 20 metrics**, 174
  requests one after another, 96–122 s every run.
- **Slack spends 40–100 s before its first history page:** listing
  every channel in the workspace (23 pages, even for a source that
  mirrors 8), then bookmarks and read states one channel at a time
  (47 s for the large source).

## 8. Steps that queue behind one another

- **One embed holds the embed queue.** A Notion embed of 5,472 chunks
  ran 27 minutes. Six other sources' embeds, 2 to 39 chunks each, waited
  behind it and then took 14–36 s each, almost all of it start-up. The
  search index's last step waited for all of them.
- **A keyword index costs about 27 s even for two documents.** The 14
  that ran today went one at a time, 09:04 to 09:25.

## 9. Records that say something other than what happened

- **`runs.sqlite`'s `step_runs` gives a re-invoked step the run's end
  as its finish.** It says a Slack keyword index took 1,169 s, the grid
  index 1,151 s and the qmd aggregator 499 s. Their last invocations in
  `supervisor.sqlite`'s `invocations` took 81 s, 67 s and 28 s, and
  finished at 09:25 and 09:35, not at 09:43. Anything that reads the
  step's duration from `step_runs` overstates it.
- **A stopped step's error reads as a different failure.** The large
  Slack download's error is `stopped when asked to: slow statement:
  execution time exceeded alert threshold | HTTP 429 from slack: …`.
  The stop message is followed by the step's last warnings, so a person
  reading the Manage row sees a slow statement as the cause.

## 10. Lines this audit wanted and could not find

- One debug line per Slack Web API call: method, status, duration.
  Only file fetches and 429s have lines. Without them, the number of
  `conversations.replies` calls behind a history page can only be
  guessed.
- Per Slack history page: threads read, files fetched, time in each.
- The ETA the Manage row showed, as a sampled metric.
- For a backfill, the conversations still owing history and the oldest
  point each walk has reached.
- A summary line from a stopped download.
- Which key of the render params changed, beside "render params
  changed".
- For a keyword index or an embed, start-up time apart from the work.

## Leftovers on disk

A Google Calendar source removed from the config before this run
still has its tree under the root: 129 MB, about 9,800 rendered files
in the old layout, and its raw store. The grid index holds no rows for
it, and qmd no longer lists its collection. Removing a source from the
config does not remove its tree. It is not yet settled whether it
should.

## What the same branch did about it

Diagnosed and fixed, each with a test watched failing on the old code:

- **Finding 5:** doltlite never runs SQLite's automatic checkpoint on a
  plain file (a new fact in `doltlite.md`, with its test). `nBackfill`
  in the live store's WAL index was 0: nothing had been copied back
  since the WAL started. The run store's writer now checkpoints every
  30 s and caps the WAL at 64 MB. A copy of the live store checkpointed
  to 346 MB.
- **The time-series re-migration (finding 7):** the opens of the
  time-series and occurrences stores wrote `_datalib_meta` a second
  time, with the shared ladder at 0, after the shared open had recorded
  its top. The second write is gone.
- **The ChatGPT fan-out:** the changed row was `me`, whose `first_name`
  upstream flips between a value and null on most syncs. It is now
  volatile.
- **Calendar "updated":** Google's count now means a changed payload.
  Most of the daily 190–250 were re-stamps of the volatile `etag` and
  `updated`.
- **The 27 s keyword index (finding 8):** two statements over the whole
  qmd index, not start-up (`qmd_behaviour.md` finding 14). The script
  now skips one of them, reads the registry table instead of counting
  collections, and leaves the index analysed. On a copy of the live
  index, a two-file update went from 16–23 s to 7.8 s.

Lines added for finding 10:

- one per HTTP request, from every web source;
- each Slack batch of threads and of files with its cost;
- the ETA a running step's row showed, once a minute;
- which render params changed;
- each qmd run's phases.

Not done, and why:

- **Calendar re-stamps still re-render.** The page shows Google's
  `updated` as "Last changed", and the render cursor carries it, so a
  re-stamp still rewrites the document and re-indexes it. Dropping the
  line is a product decision.
- **CalDAV and CardDAV** count a re-download of an unchanged object as
  updated. The fix belongs in the shared DAV layer.
- **Google Drive images:** a new revision of a document fetches its
  images again. Their bytes are deduplicated in the blob store, and
  neither the URL nor the part name can safely identify an image (three
  parts kept their name and changed their bytes).
- **Garmin, and Slack's setup:** fetching either in parallel has to be
  tried against the live services' limits first.
- **The rest of finding 8:** qmd's orphan cleanup and its embed scans
  need an upstream change.
- **Findings 1–4 and 9** are their own changes.
