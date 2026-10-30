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
  what it did and why it stopped early, if it did.

- **What the source still owes**, asked of its raw store:

  - **Slack**: a conversation whose `history:<channel id>` scope in
    `coverage` is one row, reaching back to the source's `since`, has
    no gap. Two rows or more, or a `lo` later than `since`, means the
    walk is not done there. A DM closed in the sidebar has no row at all
    until it opens again, which is not the same as "not done".

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
  started it against; for a raw store that is a commit hash. Open that
  commit (`<data_root>/gmail/ingest/entities.doltlite_db/<hash>`, see
  [raw_stores.md](raw_stores.md) § "Reading one commit, and following
  changes") rather than `main`, which may have moved on.
  `DATALIB_DAG_CHANGED_INPUTS` names the inputs that moved since its
  last success.
- **It is run again, often.** Make it safe to repeat. Keep the last
  hash you processed per input, under your tree, and process only what
  `dolt_diff_<table>` says changed since then.
- **Stop when told.** On SIGINT, finish or abandon the current unit of
  work, write nothing half-done, and exit; you have fifteen seconds.

Run it alone with `datalib-dag <data_root>/config.toml --sync today/owed`;
a step run by hand outside the loop records nothing, and what reads it
never learns it moved.

## Following changes

To process only what changed since last time, keep the commit hash of
`main` you last processed as your cursor, and ask the store for the
difference:

```sh
NEW=$(datalib-doltlite -readonly slack/ingest/entities.doltlite_db "SELECT dolt_hashof('main')")
datalib-doltlite -readonly -json slack/ingest/entities.doltlite_db \
  "SELECT diff_type, from_id, to_id FROM dolt_diff_messages('$CURSOR', '$NEW')"
```

Store `NEW` as the cursor only once your own work is saved. A raw
store's history is never folded away, so a cursor stays good; the
recipe and its edge cases are in [raw_stores.md](raw_stores.md)
§ "Reading one commit, and following changes".

## When to ask the service instead

The mirror answers for what it has fetched. Ask the service when a
question reaches outside that:

- **Slack**: anything before the source's `since` (2024-01-01 unless
  set); a time in a conversation outside every `coverage` span; direct
  messages unless the source sets `dms = true`; a DM closed in the
  account's sidebar, unless it is named in `dm_conversations` (it is
  walked again once it opens with something new); channels the account is
  not in unless it sets `all_channels`; files when `media = false`. A
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
`unified_document_index/grid_index/db.doltlite_db`. That last copy is
the one to query, since it covers every source:

```sh
datalib-doltlite -readonly unified_document_index/grid_index/db.doltlite_db \
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
  size limit kept an attachment or `.eml` out); `not_found` or
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
that is fixed, after editing, wait until `datalib-dag status` shows no
open request (or stop them) before syncing again. `datalib-dag --check
<data_root>/config.toml` checks an edit without running anything.

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
