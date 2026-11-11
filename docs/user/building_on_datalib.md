# Building an app on datalib

This guide is for an app that reads a person's mail, chat or calendar
through datalib instead of calling Gmail, Slack or Google Calendar
itself. datalib keeps a local mirror of each source, and your app reads
the mirror. The app's job shrinks to three things: ask for a sync when
it wants fresher data, read what is there, and, if it derives data of
its own, do that as a step in datalib's pipeline so it runs whenever
the mirror moves.

The payoff is real. The Today app (a list of what you owe people,
built from Gmail, Slack and Google Calendar) moved its data gathering
onto datalib. On the same 11 work items it went from 46 live API calls
in 17.1 s to 1 call in 1.7 s, and the records it saved were
byte-identical to the ones the API path produced.

Read [`agent_user.md`](../agent_user.md) first for the data root's
layout and the commands. [Reading the raw stores](raw_stores.md) says
what each source's tables hold. This page answers the questions an app
meets after that.

## Nothing syncs on its own: your app asks

datalib has no schedule. Nothing in the config, the command line or
the app (`datalib-http`) syncs on a timer; a sync happens when someone
asks for one. So an app that wants fresh data asks, from cron, from
launchd, or from its own loop:

```sh
datalib-dag <data_root>/config.toml --sync gmail/ingest --sync slack/ingest --by today
```

A sync is a **request**: a row in `<data_root>/system/supervisor.sqlite`
naming the steps to sync and who asked (`--by`). Everything that reads
those steps runs after them: the source's render, the search index,
and any step of your own that reads them. With the app up, the same
request over HTTP is

```sh
curl -X POST -H "Authorization: Bearer $(cat <data_root>/system/api-token)" \
  -d '{"roots": ["gmail/ingest", "slack/ingest"], "by": "today"}' \
  <origin>/api/requests
```

How often is up to you. A request made while the same download is
already running does not start a second copy beside it: the step runs
once more after the current run ends, which picks up whatever arrived
meanwhile. Over HTTP, a request for the same steps as one already open
comes back as that one instead of a new one; the command line always
opens a new one. `datalib-dag status
<data_root>/config.toml` lists the open requests, if you would rather
not add one.

When picking how often: a routine Slack sync asks for history only in
the conversations with something new (Slack's own sidebar says which),
so it costs about what happened. Once an hour it also re-reads each
conversation's last `refresh_window_days` for edits and deletions,
which costs about one request per channel, one after another: a minute
or so for a hundred channels. `refresh_every_minutes` sets that hour.

An app that only needs what involves the account (its DMs, mentions and
its own messages) can set `scope = "involves_me"`: a few searches choose
the channels, and the first sync walks only those, each from a day
before the oldest such message, rather than every channel the account
is in.

## Waiting, not waiting, and stopping

`datalib-dag --sync` waits until its request is done and exits with
its outcome: 0 done, 2 failed, 130 stopped. One process runs the
**loop** that serves every request on a data root: the app while it is
open, otherwise the first `datalib-dag` that needed one. Any other
`datalib-dag` hands its request to that loop, prints `following
request <id>`, and waits for it.

**If your app cannot wait** as long as a first sync takes (hours, for a
large mailbox), start `datalib-dag` detached, in its own process
group with its output going to files, wait as long as you are willing
to, and then read what the mirror holds. That is the right pattern
today. Each download seals what it has fetched as it goes, so the
stores are readable and consistent while the sync carries on (see "How
fresh is the mirror?"). With the app up, `POST /api/requests` is the
open-and-return form: it answers as soon as the loop has taken the
request on. The command line has no flag that does the same.

**To wait for the downloads only**, not the render and indexes after
them, ask about your request's steps rather than the request. A
request stays open until everything downstream of its roots has run,
which for an app that reads only the raw stores is time spent waiting
on the search index. With the app up, `GET /api/requests/<id>` answers
with the request and each step it covers, its roots first:

```sh
curl -H "Authorization: Bearer $(cat <data_root>/system/api-token)" \
  <origin>/api/requests/<request id>
```

```json
{"id": "…", "roots": ["slack/ingest"], "by": "today", "state": "open", …,
 "steps": [
   {"id": "slack/ingest", "root": true, "phase": "done",
    "last_run": {"status": "succeeded", …}, "progress": {…}, …},
   {"id": "slack/render_markdown", "root": false, "phase": "running", …},
   …]}
```

Each step's `phase` is where it stands for your request: `pending` (it
has work left: waiting its turn, or the loop has not taken the request
on yet), `running`, or `done` (nothing left for it). Once a root is
`done`, `last_run.status` says how its run ended: `succeeded`, `failed`
or `stopped`. A step turned off is done without running, and its
`state` says `off`. `progress` is what the step has reported in the
loop's current run: the same numbers as `GET /api/runs/{run}/steps`
(see "How fresh is the mirror?").

**Don't poll on a timer; subscribe.** `GET /api/sync/stream` is the
server-sent-event stream the app's own pages use to know when to fetch
again, and any client with the token can hold it. Each frame names
what moved and carries nothing else:

```sh
curl -N -H "Authorization: Bearer $(cat <data_root>/system/api-token)" \
  <origin>/api/sync/stream
```

```text
event: root
data: {"kind":"table_changed","table":"requests"}
```

Open the stream first, then read `GET /api/requests/<id>`, then read
it again on every frame whose `table` is `requests`: one is sent when
a step's phase may have moved, and at most once a second while a
step's progress moves. Each `table` names the route to read again
(`dag` is `GET /api/dag`, `runs` is `GET /api/runs`); skip the ones
you don't read. A frame with `"kind":"heartbeat"` says only that the
stream is alive. If the stream drops, reconnect and read again, since
frames sent while you were away are not replayed.

With no app up, read the loop's record yourself. A download is done
for your request once its `steps` row says it started after the
request opened and has finished since:

```sh
sqlite3 <data_root>/system/supervisor.sqlite "
  SELECT s.step, s.last_status
    FROM steps s, requests r
   WHERE r.id = '<request id>' AND s.step = 'slack/ingest'
     AND s.last_started_at_utc > r.opened_at_utc
     AND s.last_finished_at_utc >= s.last_started_at_utc"
```

A row back means done, and `last_status` says how: `succeeded`,
`failed` (with `last_failure_kind`) or `stopped`. The start is
compared, not the finish, because a download already running when you
asked runs once more after it ends. `POST /api/requests` answers with
a list of requests, one per group it names, each with its `id`.

**To stop your sync**, send your `datalib-dag` one SIGINT or SIGTERM,
or run `datalib-dag stop <data_root>/config.toml <request-id>`. That
stops your request and nothing else: its steps stop at their next
consistent point and keep what they had sealed, and the next sync
carries on from there. If that process was running the loop, it goes
on serving the other open requests and exits once they are closed.

**Do not kill it hard** (SIGKILL, or a second signal, which kills the
steps at once). What happens then:

- The built-in steps notice the loop is gone and stop. Whatever they
  wrote after their last seal is discarded by the next writer; nothing
  is torn.
- Every open request, yours included, stays open in
  `supervisor.sqlite`. The next process to take the loop, the next
  `datalib-dag` or the app starting, closes the dead run and serves
  them.
- **A `datalib-dag` that was already following a request when the loop
  was killed does not take the loop over.** It tries for the loop only
  when another process announces something on the data root, and a
  killed process announces nothing; measured, it was still waiting a
  minute later. Starting another `datalib-dag --sync` wakes it: one
  of the two takes the loop and serves every open request. This is a
  gap, not a design.
- A follower killed hard leaves its request open. The loop runs it to
  the end anyway; nobody waits for the outcome.

## How fresh is the mirror?

A download writes in batches and **seals** a batch by moving the
store's `main` branch to it. A reader sees `main`, so what you read is
always a consistent state, and it lags the download by up to one seal.
The built-in downloads seal about 15 s into a run and then each time
the run has doubled in age since the last seal: at 30 s, 1, 2, 4 and 8
minutes, and so on. The doubling is there because every seal keeps the
pages it touched on disk for good. On a three-hour first sync, the
last seals are more than an hour apart, so the mirror can be that far
behind the download.

The first interval is set for the whole data root by a table in
`config.toml`:

```toml
[checkpoint_cadence]
at_most_every_secs = 60
```

It moves where the doubling starts, not whether it doubles. Only `0`
seals at every consistent point and never grows, at a cost in disk.
There is no setting for "seal at least every N minutes"; that is a gap.

**How far along is a sync?** Every step reports how much it has done
and how much it knows is left. These are the numbers behind the Manage
screen's Queue and ETA, and they live in `system/runs/runs.sqlite`,
plain SQLite that any `sqlite3` reads:

- `done_total` counts what the step has done this run.
- `queued` is what it has announced and not done yet. `0` means it is
  finished; no `queued` row at all means it never said how much there
  was. The total only grows, as a download learns what it owes (a
  first Gmail sync adds messages as its listing finds them), so
  `queued` can climb before it falls.

```sh
sqlite3 <data_root>/system/runs/runs.sqlite \
  "SELECT run_id, name, value, updated_at_utc FROM metrics
    WHERE step = 'gmail/ingest' AND name IN ('done_total', 'queued') AND labels = ''
    ORDER BY updated_at_utc DESC LIMIT 2"
```

`metrics` holds each series' newest value per run, and `metric_samples`
holds the same series over time, for a rate or an ETA of your own. A
`queued` with a `from=<step>` label is something else: the seals of
that producer this step has not read yet. Other ways to watch the same
numbers:

- The `datalib-dag` that runs the loop prints `progress_length` and
  `progress_inc` events on stderr. One that follows another's loop
  prints only its request's outcome.
- With the app up, `GET /api/runs/{run}/steps` gives each step's state
  and current metrics, and `GET /api/sync/stream` pushes a frame
  whenever any of it moves.
- `GET /metrics` serves them for Prometheus as `datalib_step_queued`
  and `datalib_step_done_total`, beside
  `datalib_step_last_success_timestamp_seconds` for an alert on a
  source that has stopped syncing ([`logging.md`](../dev/logging.md)
  § "From outside the app").

**Is it current, or still catching up?** No one flag says so. These
are the signals, from cheapest to most exact:

- **The loop's record.** In `<data_root>/system/supervisor.sqlite`
  (plain SQLite), the `steps` row of `<source>/ingest`: `state` is
  `running` while a download is in flight; `last_success_at_utc` is
  when a run last finished cleanly, which a first sync that has not
  finished does not have; `last_status` and `last_failure_kind` say how
  the last run ended (`incomplete` means it stopped with work left).

  ```sh
  sqlite3 <data_root>/system/supervisor.sqlite \
    "SELECT step, state, last_status, last_failure_kind, last_success_at_utc
       FROM steps WHERE step LIKE '%/ingest'"
  ```

- **The store's own runs.** In the raw store, the newest `sync_runs`
  row: `status` is `running` until the run ends, and `summary` says
  what it did and why it stopped early, if it did. A run killed part
  way stays `running` until the next run starts and marks it
  `interrupted`.

- **What the source still owes**, asked of its raw store:

  - **Slack**: a conversation whose `history:<channel id>` scope in
    `coverage` is one row, reaching back to the source's `since`, has
    no gap. Two rows or more, or a `lo` later than `since`, means the
    walk is not done there. A conversation walked and found empty has
    one row with `lo = hi`: the point the walk began.

    ```sql
    SELECT scope, count(*) AS spans, min(lo), max(hi)
      FROM coverage WHERE scope LIKE 'history:%' GROUP BY scope;
    ```

  - **Gmail** (and JMAP): the listing is complete for a label when
    `listed_whole` has a row for it (or `*` for the whole account), and
    every listed message is fetched when this counts zero:

    ```sql
    SELECT count(*) FROM listed_messages l
      LEFT JOIN listed_messages_bookkeeping b ON b.id = l.id
     WHERE b.fetched_at_utc IS NULL OR b.held_version IS NOT l.stamp;
    ```

  - **CalDAV**: the same over `dav_resources` and its sidecar. Google
    Calendar lists a calendar's events whole and stores them page by
    page, so a calendar with a `sync_token` has finished its listing
    (a calendar with `since` or `until` set keeps none).

The `_bookkeeping` and listing tables are the download's own records.
Their shape can change with a migration, and the test that keeps
[raw_stores.md](raw_stores.md) honest will say so when it does.

**As of when does it know?** "There is nothing newer" needs a time, and
the data does not hold one. The newest row says what the newest thing
was, and so does a cursor: Slack's `coverage.hi` and Linear's and
Notion's are the newest stamp a walk saw, and stay put through every walk
that finds nothing, so a quiet channel looks like one nobody has read in
a day. Ask the record of when the download looked instead:

- **A whole source:** the start of its newest `ok` run. As of then, the
  mirror holds what the service listed, apart from what `problems` names:
  a record that would not fetch, or a part of the listing that failed
  (`scope_key` `listing:…` or `phase:…`).

  ```sql
  SELECT started_at_utc FROM sync_runs
   WHERE status = 'ok' ORDER BY run_id DESC LIMIT 1;
  ```

  Read this on `main`. The commit `DATALIB_READS` hands a step is the last
  one whose content moved, so its `sync_runs` stops there, however many
  syncs have since found nothing. A source with no `sync_runs` rows
  (Garmin, YoLink, contacts, most that read local files) has
  `last_success_at_utc` in the loop's record above, which is when the run
  ended rather than when it began.

- **One Slack conversation:** `checked_at_utc` in `coverage_checked`,
  since a conversation can fall behind the rest of its source. The rule
  for one message is in [raw_stores.md](raw_stores.md) § "Slack".

## Usable in minutes, complete later: a staged `since`

A first sync of a large source can take hours: a whole Gmail mailbox
goes at about 80 messages a minute, which is Google's quota. To have
something useful in minutes, sync a short window first, then widen it in
two or three stages: a few days, then a month, then everything. Each
record is still fetched once, because a source owes what its listing
names and its store does not hold.

The middle stage is worth its extra step. A backfill fetches newest
first either way, but a stage that ends gives your app a seal and a
"done" it can check, so it knows when the last month is whole instead
of guessing how far a long backfill has reached.

This works for any source whose `since` backfills: Slack, Claude and
ChatGPT, Gmail, Todoist and Garmin
([config_model.md](../dev/config_model.md) § "Time windows"). A calendar
is the exception: a windowed one is listed whole every run, and an event
outside the window goes. The steps below use Gmail; for another source
change the table and the step's name, and read its own line under "What
the source still owes" for step 5, where it has one.

1. **Start with a short window**, in the source's params:

   ```toml fragment
   [steps.params.gmail]
   since = "-3d"
   ```

2. **Sync it and wait.** The run ends with a seal, so everything it
   fetched is readable:

   ```sh
   datalib-dag <data_root>/config.toml --sync gmail/ingest --by myapp
   ```

   Your app can start now. From this run on, the source follows the
   mailbox's change history, so later syncs pick up new mail, archives
   and relabels for a few quota units.

3. **Widen the window to the next stage.** Set `since` to `"-30d"`, and
   on the last stage to what you want in the end (`"-365d"`, a date, or
   remove it for the whole mailbox). See "Changing the config while a
   sync runs": with no app up, let the open requests finish first, or
   the loop runs the old params.

4. **Backfill without waiting for it.** File the sync and go on; older
   mail arrives newest first while your app works from what is there:

   ```sh
   curl -X POST -H "Authorization: Bearer $(cat <data_root>/system/api-token)" \
     -d '{"roots": ["gmail/ingest"], "by": "myapp"}' <origin>/api/requests
   ```

   With no app up, `datalib-dag --sync` waits for its request, so run it
   as a background job. Keep your regular syncs going meanwhile: asking
   again while the backfill runs starts no second copy ("Nothing syncs on
   its own").

5. **Know when it is done:** the newest `sync_runs` row is `ok` and
   started after your edit, and the count in "What the source still owes"
   above is zero. A `listed_whole` row says nothing here, since the short
   window's walk already wrote it. Then go back to step 3 for the next
   stage.

A few things to keep in mind:

- **Start narrow, and only widen.** Widening backfills, but narrowing
  does not cancel what a wider listing already owes.
- **What you compute from history is over the current stage's window**
  until the last stage finishes (who the account has written to, where a thread
  starts). Work it out again when the mirror moves, as you would anyway.
- **A kept message brings its whole thread** with `only_extract_labels`
  and `whole_threads`, so a short window can still bring in older mail.
- **Not everything needs the same depth.** On Gmail, `fetch` sections give
  each label its own `since`: all of the Inbox and 90 days of Sent, say
  ([getting_your_data.md](getting_your_data.md) § "Different labels to
  different depths"). They are written in the config; the Add source form
  does not support them.
- **Fastmail and other JMAP mail has no `since`,** though the Gmail table
  of the same source does. It needs no stages for this: a run writes every
  message's envelope and preview before it downloads any `.eml`.
- **A Takeout does the most**, if you have one: it supplies the old mail
  and leaves only the labels to ask for
  ([getting_your_data.md](getting_your_data.md) § "Gmail").

## Reading from your program

Stock SQLite cannot open a raw store, so your program needs one of two
things, in this order:

1. **A connection in your own process.** A query on an open connection
   takes about 0.01 ms. From Python, under any interpreter, uv's included,
   `uv add imbue-datalib-reader==<version>`, the version `datalib-dag --version`
   prints ([Reading a store from Python](../dev/doltlite.md#reading-a-store-from-python)).
   It calls `libdoltlite`, which a release also installs beside
   `datalib-doltlite` under `lib/`. The library has SQLite's C API, so any
   language that can call C can do the same. It is built from the same
   source as the binaries, so it matches your stores.
2. **The shell, as a subprocess.** `datalib-doltlite -readonly -json`
   costs a process start and a store open per query, about 15 ms. Use it
   where there is no library: outside Python on a musl tarball, or on an Intel
   Mac.

## Showing a rendered document

The rendered documents (an email thread, a Slack channel's month) are
markdown, served by `GET /applet/unified_document_index/chat/{markdown_uuid}`.
Two shapes in them are a contract your app can build on:

- **A person** is a link whose href is their handle as a URI:
  `[Will Riker](mailto:riker@enterprise.org "Will Riker <riker@enterprise.org>")`.
  Identify them by the href, never the text, and ask
  `POST /applet/unified_document_index/people` who each handle is.
- **A time** is `<time datetime="2026-01-05T09:00:00+01:00" …>…</time>`,
  on the sender's clock where the source knows it, and in `Z` where it
  does not. Format the `datetime` yourself, in your reader's zone.

Attachments are linked at `/data/…` on the same server, behind the same
token. [Showing a rendered document in your app](rendered_documents.md)
has every shape, and what a viewer must sanitize.

## Your app's derived data as a step

If your app computes something from the mirror (a work list, a
summary, an index of its own), make it a step. The loop then runs it
after every sync of what it reads, and even at a download's seals
during a long one, so your data follows the mirror without your app
polling.

```toml
[[groups]]
id = "today"
name = "Today"

[[steps]]
group = "today"
function = "owed"
command = "/Users/me/today/bin/derive-owed"
inputs = ["gmail/ingest", "slack/ingest"]
[steps.params]
lookback_days = 14
```

That step's id is `today/owed`. Its rules, in full, are in
[`step_protocol.md`](../dev/step_protocol.md); the ones an app trips on
first:

- **It runs in the data root.** The working directory is the data root,
  so `command` needs an absolute path, or a tool that is told where the
  project is (`uv run --project /Users/me/today …`). It is split like a
  shell line but not run by one: no `$VAR`, globs or pipes unless you
  wrap it in `sh -c '…'`.
- **It is handed flags.** The runner appends `--params-file <path>`
  (your `params`, as JSON, in a file deleted when the step exits) and
  `--inputs '["gmail/ingest","slack/ingest"]'`. Accept them.
- **It writes only its own tree**, `<data_root>/today/owed/`. Your app
  reads the result from there.
- **It reads one moment of each input.** `DATALIB_READS` in its
  environment is a JSON object from each input to the version the loop
  started it against, written `<fingerprint>:<version>`: a hash of the
  producing step's definition, a colon, then the version that step
  reported. For a raw store that version is a commit: the last one at
  which the store's content moved, which can be older than `main`, since
  every sync commits its own record whether or not it found anything.
  The commit is everything after the first `:`
  (`v=$(printf '%s' "$DATALIB_READS" | jq -r '."gmail/ingest"')`, then
  `${v#*:}`). Open that commit
  (`<data_root>/gmail/ingest/entities.doltlite_db/<commit>`, see
  [raw_stores.md](raw_stores.md) § "Reading one commit, and following
  changes") rather than `main`, which may have moved on.
  `DATALIB_DAG_CHANGED_INPUTS` names the inputs that moved since its
  last success, and every input on its first run; empty, nothing moved.
  `DATALIB_DAG_FAILED_INPUTS` names the inputs whose last run did not
  succeed: the step still runs, on what they committed before they
  ended, and decides for itself whether a partial input is worth
  reading.
- **It is run again, often, and may start before a download ends.** A
  long download lets the steps that read it run at each of its seals
  and once more when it ends, so make it safe to repeat. Keep the last
  commit you processed per input, under your tree, and process only
  what `dolt_diff_<table>` says changed since then. A step that would
  rather run once, after the downloads it reads have ended, says
  `start = "after_inputs_settle"` beside its `inputs`. The rules, from
  the step's side, are
  [When you are started](../dev/step_protocol.md#when-you-are-started).
- **Stop when told.** On SIGINT, finish or abandon the current unit of
  work, write nothing half-done, and exit; you have fifteen seconds.

Run it alone with `datalib-dag <data_root>/config.toml --sync today/owed`;
a step run by hand outside the loop records nothing, and what reads it
never learns it moved.

## Following changes

To process only what changed since last time, keep the commit you
last processed as your cursor, and ask the store for the difference
between it and the commit the loop started your step against, taken
from `DATALIB_READS` (above):

```sh
v=$(printf '%s' "$DATALIB_READS" | jq -r '."slack/ingest"')   # <fingerprint>:<commit>
NEW=${v#*:}
datalib-doltlite -readonly -json slack/ingest/entities.doltlite_db \
  "SELECT diff_type, from_id, to_id FROM dolt_diff_messages('$CURSOR', '$NEW')"
```

Store `NEW` as the cursor only once your own work is saved. With no
cursor yet, read the whole commit instead. Read `NEW`, not `main`:
`main` may already be past it, and the loop runs your step again for
what it moved to. Outside a step, `dolt_hashof('datalib_content')` is
the commit the loop would hand you, and `dolt_hashof('main')` the
store's latest, which may be later only by the sync's own record: read
`sync_runs` or `coverage` at `main`. A raw
store's history is never folded away, so a cursor stays good; the
recipe and its edge cases are in [raw_stores.md](raw_stores.md)
§ "Reading one commit, and following changes".

## When to ask the service instead

The mirror answers for what it has fetched. Ask the service when a
question reaches outside that:

- **Slack**: anything before the source's `since`, when it sets one;
  a time in a conversation outside every `coverage` span; direct
  messages unless the source sets `dms = true`; the newest messages of a
  DM closed in the account's sidebar, for up to a day (a closed DM is
  checked once a day); channels the account is
  not in unless it sets `all_channels`; archived channels unless it
  sets `archived = true` or names them; with `scope = "involves_me"`,
  a channel where nothing involves the account, and a chosen channel
  before `context_before` ahead of its first such message; files when
  `media = false`. A
  root's `reply_count` is Slack's; the mirror holds every reply up to
  `latest_reply` only once `threads_bookkeeping.held_version` equals
  it. A message deleted in Slack more than `refresh_window_days` ago
  (30 by default) is still in the mirror.
- **Email**: with `only_extract_labels` set, only messages in those
  labels and the rest of their threads are mirrored; with
  `whole_threads = false` as well, a thread can be partial, and
  `threads.email_count` (what the mirror holds) is then less than
  `threads.upstream_email_count` (what the service has). On Gmail, with
  `since` set, mail older than it is not listed unless it shares a
  thread with a kept message (what a relative `since` slides past stays
  in the mirror). A message over the source's size limit has no `.eml`.
  A run with `message_budget` stops early on purpose.
- **Calendar**: with `since` or `until` set, only events with an
  occurrence in that window, and an event a relative window moves past is
  dropped from the mirror. Repeating events are stored as series; the
  `render_occurrences` step lists their dates, but only inside its
  `occurrences_window` ([raw_stores.md](raw_stores.md) § "A calendar's
  occurrences").
- **Anything sent a moment ago**: see "Your app's own writes" below.

## What the mirror knows it is missing: problems

A download that cannot get something does not fail the sync over it.
It records a **problem** and goes on with the rest; only a failure that
leaves it nothing to do, such as a refused credential, fails the step.
A problem is one
row saying what it is about and why, and it stays until a later run
tries the same thing and it works. Problems are how an app tells "not
in the mirror because it does not exist" from "not in the mirror
because the download could not get it".

Each source's problems travel with its data. The download writes them
into its raw store's `problems` table, where `source_id` is empty. The
render step copies them under the source's id, and `grid_index` copies
every source's into one `problems` table in
`unified_document_index/grid_index/db.sqlite`. That last copy is
the one to query, since it covers every source:

```sh
datalib-doltlite -readonly unified_document_index/grid_index/db.sqlite \
  "SELECT source_id, severity, outcome, reason, scope_key, sample
     FROM problems WHERE stage = 'fetch' ORDER BY changed_at_utc DESC LIMIT 20"
```

With the app up, `GET /applet/unified_document_index/problems?q=…`
serves the same rows a page at a time, in the search bar's grammar:
`source:gmail severity:error`, with `outcome:`, `reason:`, `stage:`
and `field:` too. Each step's counts are its `problems` series in the
run store's `metrics`, one per `severity=` label; they are the
errors and warnings cell on the Manage screen. The copies lag the
download by a render and an index run. The raw store's own `problems`
table is current at every seal.

Reading a download's problem (`stage = 'fetch'`):

- **`scope_key`** says what it is about: `<table>:<id>` for one record
  (`emails:018f…`, `slack_attachments:…`), `listing:…` for a listing
  that failed or stopped part way (a label walk, a channel list), and
  `phase:…` for a part of the run that gave up early.
- **`severity`** is `error` for a record the mirror never got, and
  `warning` where an earlier fetch left a copy that may be stale, or
  where a limit in the config kept the record out on purpose.
- **`reason`** is why: `fetch_failed`; `over_size_limit` (the source's
  size limit kept an attachment, an `.eml` or a local file out); `not_found` or
  `forbidden` for something the config names that the service does not
  have or will not show (a misspelled label or channel, reported every
  run until the config is fixed).
- **`sample`** is what the service or the download said.

So before your app concludes that something does not exist, look for a
problem about that record, or a `listing:` or `phase:` problem for the
part of the source that would have found it. If there is one, the
mirror's silence means nothing there, and the service is the one to
ask. The full column list is in [raw_stores.md](raw_stores.md)
§ "`problems`".

## Shared rate limits

The mirror and your app spend the same allowance when they use the
same account, and datalib does nothing to share it out. During a
first sync a download makes calls as fast as the service lets it, so
your app's own calls can be refused.

- **Slack** answers `ratelimited` (or HTTP 429) to whichever caller
  goes over. The download waits as Slack's `Retry-After` says and
  carries on; your app should do the same, and read from the mirror
  what the mirror already has.
- **Gmail**: the download keeps itself under `quota_units_per_minute`
  (5000 by default, of the 6000 a minute Google allows one user), so
  about 1000 a minute are left for your app. Lower it in the source's
  `[steps.params.gmail]` to leave more.
- **Google Calendar and CalDAV** have no such setting; a refusal is
  retried with backoff.

## Changing the config while a sync runs

**With the app (`datalib-http`) running**, an edit to `config.toml` is
picked up at once: a step that is running finishes under the
definition it started with, and its next start uses the new one. So to
shrink a first sync that is taking too long (a narrower `since`, fewer
channels), edit the config, then stop the request
(`datalib-dag stop`, or `POST /api/requests/<id>/stop`) and sync again:
what the stopped run sealed is kept.

**With no app up**, the loop is a `datalib-dag`, and it reads the
config only when it goes from idle to busy. A `datalib-dag --sync`
started after your edit hands its request to that loop, which runs it
with the **old** params and does not say so
([#1363](https://github.com/imbue-ai/datalib/issues/1363), open). Until
that is fixed, after editing, let the open requests finish (or stop
them) before syncing again. A `datalib-dag --sync` waits for its own
request and exits when it closes (0 done, 2 failed, 130 stopped), so
wait for the one you started to exit: run it in the foreground, or
`wait` on it. For a request someone else opened, poll `datalib-dag
status`, which prints a line starting `request ` for each open request
and `running ` for each step in progress:

```sh
while datalib-dag status <data_root>/config.toml | grep -Eq '^(request|running) '; do sleep 10; done
```

`datalib-dag --check <data_root>/config.toml` checks an edit without
running anything.

**Starting a source over is a reset, not a setting.** When a mail
service stops honouring the download's change token (Gmail's
`historyId` has aged out, or a JMAP server answers
`cannotCalculateChanges`), the next run lists the account again by
itself. To start over on purpose, reset the source's ingest step: the
app's Reset, `POST /api/reset`, or `datalib-dag --reset <source>/ingest
--sync <source>/ingest` with no app up. That empties the store and
downloads everything again.

## Your app's own writes

datalib only reads. It never sends a message, files a label or makes
an event, so what your app does to a service reaches the mirror only
through the next sync of that source. If your app needs to read its own
write back from the mirror, ask for a sync of the source after the
write and wait for it, or read that one item from the service.

Even then, a few writes take longer to show:

- **A Slack reply, an edit or a deletion.** A sync re-reads the last
  `refresh_window_days` of each conversation when it has not done so
  for `refresh_every_minutes` (60 by default), so an edit or a deletion
  in that window shows on the first sync after that, and so does a
  reply under a root in it. A reply under an older root is
  found only through Slack's search, which the download asks up to half
  an hour before now: it shows on the first sync more than half an hour
  later, if the search finds it at all.
- **A Gmail message filed outside the mirrored labels.** With
  `only_extract_labels` set, a message the app sends is mirrored only
  if one of those labels (`SENT`, say) is on it.
