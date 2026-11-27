# Datalib — agent user guide

You are (probably) an AI agent helping a user run **Datalib**: mirror
their personal data (chats, email, messages, contacts, …) into a local
store and do useful things with it. This doc maps the surfaces you'll
touch — config, sync, querying, extending — and points to the deeper
docs for each. It is about *using* Datalib; for working on the Datalib
codebase itself, see [`AGENTS.md`](../AGENTS.md).

From a shell, **`datalib-step help`** is the map: the steps from an
empty directory to a first sync, each with its command, then
`datalib-step help <type>` (what a source mirrors, how it signs in, its
ingest methods) and `datalib-step help <function>` (what a step does).
`datalib-step help docs` lists every doc compiled in, this one among
them; a link from one to another prints as the command that prints it.
Every binary's `--help` ends with examples.

Worth pointing the user at: the [first-time user
guide](user/first_time_user.md), from install to first sync, with the
warnings about what a pile of private data in one place means and what
these credentials can do; [getting your data](user/getting_your_data.md),
each source's sign-in or export; and [running in Docker](user/docker.md),
the sandboxed way to try it, with a demo library already loaded.

## The mental model

Everything lives under one **data root** directory. A sync is a DAG of
steps run by `datalib-pipeline`: per source (a `[[groups]]` entry) a
`<group>/ingest` step (bring the raw data in) and a
`<group>/render_markdown` step (raw → each document's markdown and
rows, in a per-source store), `unified_grid_and_search_index` under the `unified_grid_and_search_index` group (the SQL
index, which also puts every source it reads in the keyword search but
those its `params.not_searched` lists), and, for a source searched by
meaning, `<group>/embed`, which embeds that source's part of the search
index; `unified_grid_and_search_index` keeps only the embeddings the index still names. A
step's function
is the directory it writes (each `embed` is the exception: it writes
its vectors to the one cache in
`unified_grid_and_search_index/search_index/`):

```
<data_root>/
├── config.toml                     # the pipeline config (steps format)
├── <group>/ingest/                 # per-source raw stores (<group> is the source's id)
│   ├── entities.doltlite_db        #   (doltlite = SQLite + git-shaped history)
│   └── blobs.sqlite                #   attachment bytes by blake3 (plain SQLite)
├── <group>/render_markdown/        # per-source render store
│   └── indexed_markdown.doltlite_db  #   its documents, rows, edges + render problems
├── unified_grid_and_search_index/                  # derived; a sync rebuilds it
│   ├── unified_grid_and_search_index/db.sqlite  # the grid_rows SQL index, each row's search terms and the search index (plain SQLite) — query this
│   └── search_index/inference_cache.sqlite  # each chunk's vector, by its prompt's hash
├── datalib_curated/                # what a person curates, one directory per app; nothing rebuilds it
└── system/                         # the server's own state
    ├── supervisor.sqlite           # sync requests, steps turned off, and the loop's record (plain SQLite)
    ├── runs/runs.sqlite            # every run's step states, log lines and metrics (plain SQLite)
    ├── api-token                   # this process's bearer token
    ├── remote_media.doltlite_db    # remote media a person let a document load
    └── disk_stats.sqlite           # bytes on disk and free space, over time (plain SQLite)
```

The split is by writer: `unified_grid_and_search_index/` is produced by the pipeline
and fully derived, `system/` is the server's own state.

The binaries a release ships:

- `datalib-pipeline` — the sync runner.
- `datalib-step` — the built-in step commands.
- `datalib-http` — the API server and web UI.
- `datalib-applet` — the applets: one server for each `[[applets]]`
  entry, which the http server starts with itself (see "Applets"
  below); `datalib-applet search` is the search box from the command
  line.
- `datalib-doltlite` — the shell for reading and exporting the stores
  (see "Reading the mirrored data" below).
- `datalib-migrate-config` — rewrites a `config.toml` from a retired
  shape (see below).
- `datalib-file-inventory` — the directory-tree scanner, also reachable as a
  step.
- `latchkey-curl-router` and `curl-impersonate` — Cloudflare-safe HTTP
  for the downloaders.

A release's directory holds every one, with `latchkey` beside them.
End-to-end setup walkthrough:
[`docs/user/first_time_user.md`](user/first_time_user.md).

## Configuring sources

`<data_root>/config.toml` is TOML: a `[[groups]]` entry per source (an
`id`, a `name`, a `type`), one `[[steps]]` table per step with its `id`
(`<group>/<function>`, the tree it writes) and its `command`
(`datalib-step run <function> <type>` for datalib's own steps), and
`inputs` naming the steps it reads by that id. Datalib's steps also
write the `reads` and `locks` they need, and the file lists the verbs
`datalib-step` takes in a `[[programs]]` entry; `datalib-step examples`
writes all of it for you.
Top-level keys (`data_root`, `binary_dir`) go above the first `[[…]]`
header, and a step's `params` sub-tables come after its plain keys — a
`[…]` header ends the table it appears in.

- **From the CLI alone:** `datalib-pipeline init <data_root>` writes a
  `config.toml` with no steps and nothing else; `datalib-step examples
  <type> --append <data_root>/config.toml` adds a source (below), and
  the document index and its applets when the config has none yet.
- **Complete commented example:**
  [`configs/dag_example.toml`](../configs/dag_example.toml).
- **Every source, every option**:
  [`docs/user/config_examples/all_sources.toml`](user/config_examples/all_sources.toml)
  — one commented group per way into each source, with its steps and
  every option set, generated from the stanzas `datalib-step examples
  <type>` prints, so it always loads.
- **An old config:** one in a retired shape (steps written as `group` +
  `function`, a bare `command = "datalib-step"`, or a `datalib-step
  download …` command) is refused; the app rewrites it when it next
  reads it, and from a shell `datalib-migrate-config <data_root> --force`
  does.
- **Credentials**: web-API sources authenticate through `latchkey`,
  which ships beside the binaries. Per-source
  walkthroughs for getting cookies/tokens/exports:
  [`docs/user/getting_your_data.md`](user/getting_your_data.md).
  On auth failure, sync events include a `hint` with the exact
  `latchkey auth set …` recipe for that provider. With `LATCHKEY_GATEWAY`
  set, a latchkey gateway holds the credentials: the hint says so, and
  signing in or asking for access happens wherever that gateway is managed.
- **How far back (and ahead)**: every source that can mirror part of
  its history takes `since` in its method table (`[steps.params.api]`,
  `[steps.params.gmail]`, `[steps.params.google]`, …), and a calendar,
  Garmin and Baby Connect also take `until`. Each is a day (`"2026-09-01"`), an
  RFC 3339 instant, or an offset from the run's start (`"-30d"`,
  `"+21d"`; units `h`, `d`, `w`), so `since = "-30d"` keeps the last
  month without a daily edit. A record an offset slides past stays in
  the mirror, except in a windowed calendar, which drops events the
  window no longer reaches. An earlier `since` backfills on the next
  run. Each source's default and what the window bounds:
  [`config_model.md` §"Time windows"](dev/config_model.md#time-windows-since-and-until).
- The web UI's **Manage** tab scaffolds and validates the config
  (`GET /api/config/scaffold`, `PUT /api/config` validates before
  writing).

## Running a sync

CLI:

```sh
datalib-pipeline sync <data_root>                 # everything
datalib-pipeline sync <data_root> slack/ingest    # one source
datalib-pipeline check <data_root>                 # validate, run nothing
```

Every verb takes the data root or its `config.toml`. `check` prints *every* problem with the config rather than the first,
as `file:line:col: severity: message` with the offending line and a
`help:` line under each — so fixing a config takes one round-trip, not
one per typo. It also asks each step whose program takes `--check`
about its params, `datalib-step`'s included, as the app's config editor
does on a save (`PUT /api/config`, `POST /api/config/check`), so a
misspelled or misplaced option is reported at its line before a sync
rather than when the step runs, and an option that no longer does
anything is a warning there. Exit 0 when every entry loads, 1 if the
file is not a config at all, 2 if some entries were dropped or a step
would refuse its params. A root a newer Datalib wrote is not refused here: each
step refuses it when the runner starts, and is turned off until a build
that can read it runs.

`datalib-step examples <type>` prints each source's config stanzas:
for every way into the source, a minimal one (the least that syncs) and
a complete one (every option set, each with a comment). With
`--append <config>` it adds the minimal one to the config instead,
above the index groups, where the index steps read it by pattern
(`*/render_markdown`); in a config that names their inputs one by one
it adds the source's steps there, and it adds the time-series index
when a time-series source is the first. A config with no grid and
search index gets one, with its applets, and a config without the
`[[programs]]` and `[[locks]]` entries `datalib-step`'s steps need gets
those. A
source reached more than one way names the method (`--method gmail`),
and `--id` gives the group an id of its own, for a second account. It
refuses an id the config already has, and a result `datalib-pipeline
check` would refuse, writing nothing; the previous file is kept as
`config.toml.<UTC time>.bak`, one per append. Its page says how to do the same by hand.
`datalib-step help <type> <method>` lists every option with its type,
default and meaning. Both are read from the source's own config crate,
and a test loads and plans every stanza.

```sh
datalib-pipeline init ~/datalib
datalib-step examples slack --append ~/datalib/config.toml
datalib-step examples email --append ~/datalib/config.toml --method gmail --id work-mail
```

`sync` takes step ids after the root (several, or separated by commas):
it runs the named steps and everything downstream of them, and nothing
else — pending work in other sources waits for a full run. Its flags:
`--parallelism N`, and `--binary-dir DIR` (put at the front of every
step's `PATH`, so a bare `command` like `datalib-step` resolves there;
defaults to the config's `binary_dir`, else the directory
`datalib-pipeline` itself is in). The other verbs that run steps are
`reset <data_root> <step-id>…` (see **Resetting** below) and
`free-disk-space` (see **Giving back disk** below); like a sync, each
is done by the app's loop when the app is up, and otherwise by one it
runs itself. A sync that fails with "has a shape this build's
DDL cannot be reached from by adding columns" is a raw store an older
build wrote in a shape this one cannot keep; nothing was changed, and if
upstream still has the data, `datalib-pipeline reset <data_root>
<source>/ingest`, then a sync of it, is the way through.

**To run one step, sync it** — never run `datalib-step` for it by
hand. The runner is what sets the step's environment, takes the root's
lock and records the version the step published, which is how the steps
that read it learn it moved; a step run by hand records nothing. An
ingest step always runs when synced; any other runs only if what it
reads moved, so to rebuild a source's documents from what is already
downloaded, reset its render step first:
`datalib-pipeline reset <data_root> slack/render_markdown`, then
`datalib-pipeline sync <data_root> slack/render_markdown`.
`datalib-step help <function>` gives the commands for each kind of step.

**The stderr stream is NDJSON and made for you**: `run_plan` (every step's
id and group, in topo order), then `invocation_start` / `step_start` /
`progress_*` / `log` / `hint` / `invocation_end` per invocation (one
process of one step), and one `request_end` as your request closes,
with a row for each step it reached — parse it instead of scraping
human output. A step that reads one that streams its output runs once
per seal it reads (unless it says `start = "after_inputs_settle"`), so
it may have several `invocation_end`s, each with the `status` of that
process; one that failed and was run again is an `error` line in the
step's log even when the step ends `succeeded`, and its `request_end`
row counts them in `failed_before`. Failures carry a kind
(`transient` / `rate_limited` / `auth` / `permission` / `data` /
`cancelled` / `incomplete`); the
runner already retries transient/rate-limited ones with backoff (each
attempt its own invocation, the wait shown as `waiting_for: backoff`), and
what a failed step committed is still read downstream; only a step
whose inputs were never written at all is `blocked`. Ctrl-C is graceful:
steps checkpoint-commit partial progress and the next run resumes.
Syncs are incremental and idempotent — re-running is always safe.

**You can sync while the app, or another `datalib-pipeline`, is syncing the
same root.** A sync is a *request*: a row in
`<data_root>/system/supervisor.sqlite` naming the steps to sync (its
*roots*; everything downstream of them comes too) and who asked. One
process runs the loop that serves requests — the app while it is open,
or a `datalib-pipeline serve <config>`, which holds the loop until it is
stopped; otherwise the first `datalib-pipeline` to need one — and every other
`datalib-pipeline` hands its request to that loop, says `following request
<id>`, and waits for it. Your source runs beside theirs, not after. It
exits with *your* request's outcome: 0 done, 2 failed, 130 stopped, and
Ctrl-C stops your request only. Pass `--by <name>` so the request says
who asked.

There are four verbs, and each is the same row written two ways: from
a shell with no server needed, or over HTTP while the app is up.

| verb | CLI | HTTP |
|---|---|---|
| sync | `datalib-pipeline sync <config> <step> --by claude` | `POST /api/requests {"roots": ["<step>"], "by": "claude"}` (no roots: every source; one request per source) |
| stop a sync | `datalib-pipeline stop <config> <request-id> --by claude` | `POST /api/requests/<id>/stop {"by": "claude"}` |
| turn a step off | `datalib-pipeline turn-off <config> <step> --by claude` | `POST /api/steps/<step>/turn_off {"by": "claude"}` (`/` in the id as `%2F`) |
| turn it on | `datalib-pipeline turn-on <config> <step>` | `POST /api/steps/<step>/turn_on` |

`<config>` is `<data_root>/config.toml`. A stop, a turn-off and a
turn-on take effect within a second. Turning a step off (the switch on
its Manage row) stops it if it is running, skips it in every sync until
it is turned on, and makes whatever reads it wait. It does not hold a
sync open: a sync whose only work is a step turned off closes without
running it. Turning it on starts nothing by itself. `POST /api/requests` opens one
request per source the roots belong to, so each can be stopped alone,
and answers once the loop has taken them on, with the list of them, each
with its `id`. A sync of the same steps that is already open comes back
instead of a second one.

**Watching.** `GET /api/requests` lists the open requests, then the
newest closed ones: each with its `roots`, `by`, and `state` (`open`, or
how it ended: `done`, `failed`, `stopped`). `GET /api/requests/<id>`
adds each step the request covers, roots first, with its `phase` for
that request (`pending`, `running`, `done`), its last run and its live
progress: wait on a root's `done` to wait for a download without the
render and indexes after it, reading it again on each `GET
/api/sync/stream` frame naming `requests` rather than on a timer
(`building_on_datalib.md` §"Waiting, not waiting, and stopping"). `datalib-pipeline status
<config>` prints the open ones from the shell, with the steps turned off and the steps
running now: one line each, starting `request `, `off ` or `running `.
A line `owed <step>  <n>  as of run <id>` follows for each step at rest
whose last run ended short of done (stopped, failed, given up) with work
it saw waiting and did not get to — a Slack sync that was stopped, say,
with the conversations, threads and files it had not reached; the next
sync starts on them. With none of these there is one line starting
`idle:`. To wait for your own
sync, don't poll: `datalib-pipeline sync <config> <step>` returns when its
request closes. What each step is doing is in the loop's record, which the
Manage screen's Status column reads directly:

```sh
sqlite3 <data_root>/system/supervisor.sqlite \
  'select step, state, waiting_for, state_detail, turned_off_by, requests from steps'
```

`state` is `running`, `waiting` (for what kind of thing: `waiting_for`,
one of `upstream`, `reader`, `lock`, `disk_space`, `config`; for which:
`state_detail`), `off`,
`blocked`, `failed`, or at rest (`idle`, `stale`, `fresh`); `requests` is
the open requests it is being run for, a JSON list of their ids. Each request and each step turned
off records who did it, so the screen shows "turned off by claude" or
"Stop the sync of Work Slack, started by claude". **Don't turn on or stop
what a person started without saying so.**

**Resetting** empties what a source downloaded: every row of its store
goes, and the doltlite history keeps them; its attachments stay in
`blobs.sqlite`, which nothing resets. Then what reads it runs, so its documents leave the grid,
and its next sync downloads everything again from nothing. Resetting a
render step (`slack/render_markdown`) instead rebuilds its documents
from what is downloaded, at once. With the app up, use `POST /api/reset
{"targets": ["slack/ingest"], "by": "claude"}`: whatever else is syncing,
the step is stopped if it runs and emptied at once, and the request that
carries the emptiness downstream is opened. It answers 204 once the
store is empty, or 202 if the step had not stopped within ten seconds and
is emptied once it has. The
Manage screen's row menu offers the same, and so does
`datalib-pipeline reset <data_root> slack/ingest`, with or without the
app up: it waits until the store is emptied and what reads it has run.
`datalib-pipeline sync <data_root> slack/ingest` then downloads it
again.

**Giving back disk.** A doltlite store never edits in place, so every
sync leaves chunks behind that no commit reaches; a store that refills
whole tables each run (Lightroom, Apple Photos, Apple Messages) can carry
several times what it holds. Freeing disk space gives it back:

| | what goes | what stays |
|---|---|---|
| free disk space | chunks no commit reaches, in every store | every commit of every store |
| … and drop old versions on derived data | the same, and the older commits of each render store | every commit of every download store; the latest of each render store |

The first loses nothing. Derived data is what Datalib rebuilds from the
downloads: each source's rendered documents. Dropping its old versions
loses their history, which nothing but the History card reads. The grid
index is plain SQLite and is not touched.

```sh
datalib-pipeline free-disk-space ~/datalib
datalib-pipeline free-disk-space ~/datalib --drop-old-versions-on-derived-data
```

It prints each step's tree before and after. With the app up, `POST
/api/free_disk_space {"by": "claude"}`, adding
`"drop_old_versions_on_derived_data": true` for the second, and
`"targets": ["lightroom/ingest"]` for one step only. It answers 204 when done, or 202 while it runs on; the steps it
touches, and what reads them, wait meanwhile. Each store needs about its
own size free on disk while it is rewritten, and takes minutes per ten
gigabytes. `GET /api/config`'s `free_space` says how far it has got:
each step it asks, `waiting`, `running`, `done` with its store's
`store_bytes` before and after, or `failed` with why; `running` turns
false when it is over. The status bar's "Free up disk space…" (a
right-click on the data root's size) asks the same, and a pill in the
toolbar shows that progress.

**Logs.** The run store is where to read what happened.
`GET /api/runs` lists runs: a run is one request (a sync someone asked
for) or one job (a reset, freeing disk, the prepare pass at a start),
with the invocations started for it — one invocation is one process of
one step. `/api/runs/{run}/invocations` lists those,
`/api/runs/{run}/steps` gives each step's newest invocation in it with
its metrics, and `/api/runs/{run}/log?step=&after_seq=` is their log,
which you can tail by `seq`. Runs an older Datalib recorded, one per
stretch of the loop being busy, are listed too, with `"old": true`.
`GET /api/log?q=` is the same log across every run, in the
search bar's grammar (`level:warn -target:sqlx "history"`), and
`process:http` narrows it to what the server itself said (its sync
loop, the applets, requests that failed). `GET /api/sync/stream` is a
server-sent-event stream that pushes a `root` frame whenever any of
this moves.

All of it is one file, `<data_root>/system/runs/runs.sqlite`: every
line any Datalib process wrote — the runner, each step, the server,
the applets. It is plain SQLite, so stock `sqlite3` reads it with no
server up. `log` holds one row per line, `processes` says who wrote
each (join on `process_id`), `invocations` holds each invocation's
step, state, attempt and error, `invocation_requests` which requests
each served, and `invocation_metrics` an invocation's numbers:

```sh
sqlite3 <data_root>/system/runs/runs.sqlite \
  "SELECT l.ts_utc, p.process, l.step, l.level, l.target, l.msg
     FROM log l LEFT JOIN processes p USING (process_id)
    WHERE l.level IN ('warn', 'error')
    ORDER BY l.seq DESC LIMIT 50"
```

`select invocation_id, step, state, attempt, error from invocations
where error is not null` lists the invocations that failed. Every table and column is in
[`docs/dev/logging.md`](dev/logging.md).

## Before you answer from the copy

The copy is only as good as what was synced into it, and an agent that
answers from it without checking can be confidently wrong. Each time:

1. **Search what is already there before syncing anything.** The person
   may have mirrored the source already. An empty answer means nothing
   matching is in the copy; it does not mean the service has no such
   data. Check whether the source is configured at all (a `[[groups]]`
   entry with its `type` in `<data_root>/config.toml`), and offer to
   sync it.
2. **A sync is over when its request ends, and not before.**
   `datalib-pipeline sync` returns when its request closes, with its
   outcome as the exit code; over HTTP, a request's `state` leaves
   `open` (§ "Running a sync"). When the loop starts, it first runs a
   pass of its own (a run of kind `migrate`) that asks each step to bring
   what it stored up to date and downloads nothing; its steps end
   `succeeded` within seconds, and that is not your sync finishing.
3. **Check that the copy is complete.** A request that ended `failed`
   left part of the copy unfinished, and one that ended `done` can still
   hold records whose parts could not be read. Three checks, each of
   which should come back empty before you build an answer on the copy:
   - `datalib-pipeline status <config>`: a line `owed <step>  <n>` is work
     a run saw waiting and did not get to.
   - the source's `problems` table, which names each record a download
     could not fetch or read whole ([reading the raw
     stores](user/raw_stores.md) § "`problems`"):

     ```sh
     datalib-doltlite -readonly <data_root>/<source id>/ingest/entities.doltlite_db \
       "SELECT severity, reason, scope_key, sample FROM problems WHERE severity IN ('error', 'warning')"
     ```

   - the source's own check for records it still owes, where it keeps
     one, printed by `datalib-step help <type>` under "Is the copy
     complete?".

   When any is not empty, sync again first, and if the person has
   already seen an answer, tell them what has changed.
4. **Say what the copy covers.** Each source mirrors less than the whole
   service by default (Slack: the channels the account is in, no DMs),
   and some things it can never see. `datalib-step help <type>` lists
   both under "Limitations". Say what the copy covers in your first
   answer from it, or ask before the first sync when it would change the
   answer. To ask a narrower question, query the copy; don't narrow the
   copy with `since`, because anything older than `since` is never read.

## Reading the mirrored data

If you are building an app that reads this data instead of calling the
services (when to sync, how fresh the mirror is, deriving your own data
as a step, when to ask the service after all), read [building on
Datalib](user/building_on_datalib.md).

Pick the surface that fits the question:

- **The search box, from the command line** — `datalib-applet search
  <data_root> '<query>'` answers a search exactly as the app's search
  box does, with no server and no token, and prints the JSON
  `/search` would (below): the query language, `--limit`,
  `--offset`, `--sort` and `--within` are the same. Page with
  `--offset` set to the answer's `next_offset`. It reads only, so it is
  safe beside a running sync or app.

  ```sh
  datalib-applet search ~/datalib 'from:picard after:2026-10-01 is:document' --limit 20
  datalib-applet search ~/datalib 'warp core semantic:off'
  ```

  It exits 1 when the answer is not the whole search you asked for (a
  refusal, an error, a part that could not answer), with each reason on
  stderr; an empty answer with exit 0 means nothing matched.
  [Searching your mirror](user/searching.md) § "When the search says
  no" has each case. Free text loads the embedding
  model and the vectors first, each command afresh, which on a large root
  takes tens of seconds; `semantic:off` in the query loads neither. A running
  app holds them loaded, so `GET …/search` has no such wait.
- **SQL over everything** — the `grid_rows` union table in
  `unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite`: one row per
  message/document/entity across all sources, with `provider`, `kind`,
  `created_at`, `modified_at`, `touched_at`, `author`, `channel`, `conversation_uuid`,
  `preview` (the first 240 characters of the row's text; the whole text
  is in the document's markdown), `entire_chat`, etc. `is_document = 1`
  picks the one row per rendered document — the thread, the
  conversation, the PR, the page — and leaves out the messages inside
  them, which is usually the row count you meant. Every column:
  [grid rows](dev/grid_rows.md); the links between documents:
  [edges](dev/edges.md).

  Two ways to run SQL, against the grid index or any other store:

  - **Over HTTP, with nothing installed:** with the app up, `POST
    /applet/unified_grid_and_search_index/sql` runs one `SELECT` you
    write against any store under the data root (the grid index, every
    raw and render store, the log, the loop's record) and answers a page
    of rows as JSON. It reads only. This is the way in from a container,
    another machine, or anywhere you can make an HTTP request but not run
    `datalib-doltlite`. `GET …/sql/stores` lists the stores.

    ```sh
    curl -s -H "Authorization: Bearer $(cat <data_root>/system/api-token)" \
      -H "Content-Type: application/json" \
      -d '{"store": "unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite",
           "sql": "SELECT provider, count(*) FROM grid_rows GROUP BY 1"}' \
      <origin>/applet/unified_grid_and_search_index/sql
    ```

    Parameters, paging, a doltlite store's commits and what it refuses:
    [the applets](user/applets.md#reading-any-store-with-sql).

  - **From a shell:** **`datalib-doltlite`** ships beside `datalib-pipeline`
    (plain `doltlite` in the docker image). Its argv is `sqlite3`'s.
    **Always pass `-readonly`**: a transaction a writable session holds
    open stalls the sync.

    ```sh
    datalib-doltlite -readonly unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite \
      "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
    ```

    The grid index is plain SQLite in WAL mode, rebuilt from the sources
    and keeping no history. Stock `sqlite3 -readonly` refuses a WAL file
    nothing else has open (`unable to open database file`), and cannot
    read a doltlite store at all; `datalib-doltlite` reads both. One pipe
    turns either into a standalone plain SQLite copy (a doltlite store's
    copy carries its data, not its history):

    ```sh
    datalib-doltlite -readonly unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite .dump \
      | sqlite3 grid.sqlite
    ```

    A Python program that runs many queries can keep a store open with
    `imbue-datalib-reader` instead. [doltlite](dev/doltlite.md) has
    that, history, diffs and a crashed writer.
- **Markdown** — each conversation or document as human-readable
  markdown: `datalib-step markdown <data root> --source <group> --document
  <markdown_uuid>` prints one, `--out <folder>` writes every one as
  `<folder>/<group>/<markdown_uuid>.md`, and `GET
  /applet/unified_grid_and_search_index/chat/{markdown_uuid}` serves it.
  The render store keeps every document; there are no `.md` files under
  `<group>/render_markdown/` to read.
- **Raw stores** — the per-source doltlite stores under
  `<group>/ingest/` keep what the service sent, when the rendered form
  isn't enough: Slack's and Google Calendar's JSON, Gmail's original
  message. [Reading the raw stores](user/raw_stores.md) is the map: how
  to open one (`payload` is JSONB, so
  `json(payload)`), how to read one commit and follow changes, and
  each table and column, with the ids that trip people up. An
  attachment or a person's photo a page links as
  `/data/<group>/<function>/blob/<hash>` is a row of that step's
  `blobs.sqlite`: fetch the path from `datalib-http` with the token, or
  read the row (`SELECT bytes FROM cas_objects WHERE blake3 = '<hash>'`).
- **A calendar's occurrences** — what Google's
  `events.list?singleEvents=true` returns: a `calendar` source's
  `render_occurrences` step writes every occurrence of every event in a
  window of days around its last run (30 back, 400 ahead by default) to
  `<group>/render_occurrences/occurrences.doltlite_db`, one row each,
  with Google's instance ids and moved and cancelled dates applied:

  ```sh
  datalib-doltlite -readonly calendar/render_occurrences/occurrences.doltlite_db \
    "SELECT start_time, title FROM occurrences
      WHERE start_utc < '2026-11-02T00:00:00Z' AND end_utc > '2026-10-26T00:00:00Z'
      ORDER BY start_utc;"
  ```

  `occurrences_window` says which days the rows cover, and `unlisted`
  names any event whose occurrences could not be worked out. A Google
  `payload` has its times in the calendar's zone, as Google's API does;
  `start_time` is in the event's own. The columns:
  [Reading the raw stores](user/raw_stores.md) § "A calendar's
  occurrences".
- **HTTP API** — `datalib-http <data_root>` serves the app and, under
  `/applet/unified_grid_and_search_index/`, the search (`/search?q=…`,
  in [the query language](user/searching.md)), each document
  (`/chat/{uuid}`), the accounts, channels, labels and folders to look in
  (`/places/{uuid}`), who a handle is (`POST /people`) and `/sql`
  above: [the applets](user/applets.md) lists every route and what each
  answers. Beside them, `GET /data/<group>/<function>/blob/<hash>` is an
  attachment or photo a page links, `GET /api/log?q=…` the log in the
  search's grammar ([logging](dev/logging.md) §"Reading it"), `GET
  /api/dag` the step graph, and the sync routes above.

  Every route needs the server's per-process API token — loopback does
  not keep a *web page* out, and `PUT /api/config` runs arbitrary
  `command` strings. Read it from the running server and send it as a
  bearer token:

  ```sh
  TOKEN=$(cat <data_root>/system/api-token)
  curl -H "Authorization: Bearer $TOKEN" "<origin>/api/health"
  ```

  It is minted fresh on every start, so re-read the file rather than
  caching the value; `DATALIB_TOKEN=<value>` pins it. The onboarding
  guides at `<origin>/agent/cards.md` and `<origin>/agent/config.md`
  are readable without it.

## Building a tool or app on Datalib

[Building on Datalib](user/building_on_datalib.md) has the details.
Two decisions come first: whether to read the mirror at all, and where
to keep what your app adds to it.

**Decide what belongs on the mirror and what belongs on the API.**
Datalib is, for now, about getting your data out of its silos en masse.
It can also take a targeted part of a source: only recent history
(`since`), particular Slack channels (`channels`), particular mail
labels (`only_extract_labels`). Some things make less sense to build on
it:

- An app designed around always asking for the latest content may do
  better querying the service's API directly. The mirror is only as
  fresh as its last sync.
- An app that publishes (sends a message, files a label, makes an
  event) must use the API to do so. Datalib only reads.

Many apps successfully combine the two, say reading history in bulk
from the mirror and using the API for the newest items and for every
write. Datalib is designed for this: a raw store re-uses the service's
identifiers, so an id from an API response finds its row in the mirror,
and a row in the mirror names what to send the API. A few ids are
normalized on the way in, such as Gmail's, which are padded to 16 hex
digits; the "Which id is which" tables in [Reading the raw
stores](user/raw_stores.md) give the mapping. [Building on
Datalib](user/building_on_datalib.md) § "When to ask the service
instead" lists, source by source, what the mirror does not hold, and §
"Your app's own writes" says how a write reaches it.

**Extend the raw stores from beside them; never modify them.** To
annotate or extend what a source downloaded (a tag, a score, a field
you extracted), don't write to `<group>/ingest/entities.doltlite_db`.
Each raw store has one writer, its `ingest` step: a writable session's
commits are overwritten by the next sync without an error, and a
transaction left open stalls the sync.

Instead, create a parallel store of your own, a *sidecar*, whose rows
refer to records by their primary keys in the raw store (Slack's
`messages.id`, Gmail's `emails.id`). Those keys are meant to be stable,
so a row of yours stays attached to its record as the mirror syncs. Two
things to allow for:

- A record can leave the mirror for a while or for good: a reset empties
  the store until the next sync brings the records back, and a windowed
  calendar drops events the window has moved past. Treat a key with no
  row as "not here now", and keep your row.
- A rare migration changes what an id looks like, as when v0.46.0 changed
  Gmail's thread ids from decimal to hex. The release notes list every
  one under "Changes to the raw stores".

A sidecar can be doltlite, plain SQLite or any other backing store. **We
recommend doltlite** for its version control: every commit is kept, so
you can see what changed and when. You can always delete history you
have, but you can't recover history you never kept. `datalib-doltlite
<file>` creates one; commit each change with `SELECT dolt_commit('-Am',
'<what changed>')`, and keep to one writing process per file.

Where it lives depends on whether it can be rebuilt. Data computed from
the mirror belongs in a step's own tree ([Building on
Datalib](user/building_on_datalib.md) § "Your app's derived data as a
step"), which then runs whenever the mirror moves. Data nothing could
rebuild, such as a person's notes, stays out of `<data_root>/<group>/`,
because deleting a group deletes that directory. An app writing such
data keeps it in `<data_root>/datalib_curated/<your_app>/`, one
directory per app, which no reset or deletion of a source touches, or
in a clearly marked folder of its own under the data root, with a name
no group uses. Neither is for a step, which writes only its own tree.

The contacts app is this pattern in Datalib itself. Its store,
`datalib_curated/datalib_contacts/contacts.doltlite_db`, is a doltlite
file of its own that makes every write a commit, and no reset or removal
of a source touches it. It adds who a handle is without changing any
raw store. Its rows refer to a person's handles
(`email:riker@enterprise.org`) rather than a raw store's keys, since one
person spans many sources. See [the contacts
doc](dev/contacts.md) § "The contacts app".

## Extending Datalib

- **Custom step commands** — the headline extension point. Any
  executable can be a pipeline step: declare it in `config.toml` with
  `command`/`inputs`/`params`, and the runner feeds it
  flags + env vars and (optionally) parses NDJSON progress/outcome
  events from its stdout. A plain shell script works; adopting more of
  the protocol buys incrementality, live progress, and retry
  classification. **Read
  [`docs/dev/step_protocol.md`](dev/step_protocol.md)** — it is
  the complete contract, with minimal shell and Python examples. The
  rules behind the scheduler (what makes a step stale, what a dropped
  entry costs) are [the runner's README](../datalib/backend/dag/README.md).
- **Custom UI cards** — the web UI can host agent-authored components
  (ES modules whose default export is a card factory, saved with
  `PUT /api/lib/{name}`). The
  server serves its own guide for this at **`GET /agent/cards.md`**
  (and one for config-editing agents at **`GET /agent/config.md`**).

## Applets

An applet is a long-lived server the app runs, one per `[[applets]]`
entry in the config: `datalib-http` starts each when it starts and
serves it at `/applet/<id>/`, behind the API token. Search, the
contacts app, the time-series index, photos and the Slack browser are
all applets. A sync needs none of them; what they serve needs the app
up. [The applets](user/applets.md) lists what each one serves.

## Who a handle is: the contacts app

Sources know a person only by *handles* (`email:riker@enterprise.org`,
`tel:+12025550101`, `slack:T01/U02`), so one person arrives as several.
The contacts app links handles to one contact, which every chip draws
and `with:contact:<id>` searches follow at once, with no re-render. It
also holds labels on people and senders (`real`: in when not spam, out
for spam).

An agent **proposes**: send one batch to `POST
/applet/datalib_contacts/proposals`, and nothing is final until the
user confirms it on the contacts card's *Review*. **Don't reach for
`POST /link`:** it is recorded as the user's own decision and never
reviewed. [The contacts app](user/contacts_app.md) has the steps, from
finding the handles to what the user sees;
`GET /applet/datalib_contacts/` lists every route and which are whose.

## Feedback

Something broken, missing, or confusing? **File an issue at
<https://github.com/imbue-ai/datalib/issues>** — that is the preferred
channel, and every report is welcome. Two kinds are equally wanted:
what exists and misbehaves (a one-line "this doc is wrong", a full
bug), and what would make Datalib easier to wield for whatever you and
the user are trying to do with it — a surface that fought you, a query
you had to work around, a use case it doesn't serve yet. Paste the
`datalib-pipeline check` output or the failing step's `invocation_end` event
where you have one; it saves a round-trip. If you cannot reach GitHub,
write the report out for the user to file. **The tracker is public:
keep the user's private data out of it.** Log lines and events can
quote message text, email addresses, channel names and file paths —
redact before pasting, and never attach a store or a rendered
document.

## Troubleshooting quick hits

- **Auth failures**: look for the `hint` event in the sync stream — it
  contains the provider-specific `latchkey` walkthrough. Cloudflare
  403s despite a fresh cookie usually mean a flagged IP/UA; wait or
  change networks.
- **"Why did/didn't this step run?"**: `system/supervisor.sqlite` is
  plain SQLite. `select step, state, waiting_for, state_detail from steps` says what
  each step is doing and what it waits on; `select step, reads,
  last_status, last_failure_kind, last_error from steps` gives each
  step's input versions at its last success and how it last ended (a
  `permission` failure is the OS refusing a read: choose the folder
  again in the app, or grant Full Disk Access); `select * from sinks` the version each tree was published at.
  A step re-runs when an input version moved (download steps always run
  — their input is a remote service). `select step, started_at_utc from
  invocations where outcome is null` is what is running now.
- **A sync stuck on `database is locked`**: something holds a write
  transaction on that store — usually a writable `doltlite` session;
  close it. Uncommitted leftovers need no repair, since the next
  writer's open discards them
  ([`docs/dev/doltlite.md`](dev/doltlite.md#locks-and-writers)).
- **A config the runner rejects**: `datalib-pipeline check
  <data_root>/config.toml` lists every problem with a line number;
  `PUT /api/config` (or the Manage tab) returns the same list in
  `diagnostics`, `datalib-step` steps whose params a sync would refuse
  included, and writes nothing. A data root holding a pre-TOML
  `config.yaml` reads as unconfigured — set it up again from the app.
- **Every step reads `waiting for disk space`**: the data root's disk
  went under the config's `[disk_space]` pause line (10 GB unless the
  config moves it), so the loop stopped what ran and starts nothing
  until the disk is back to the resume line (15 GB). A one-shot
  `datalib-pipeline` that seems to hang after its `run_plan` line is this:
  each held step logs `waiting for disk space: …` once. Free space, or
  lower the lines; the open syncs carry on by themselves.
- **A step that silently stopped running**: check `diagnostics` on
  `GET /api/config`, or `datalib-pipeline check`. A config with one unusable entry
  still loads — that entry is dropped and everything else runs — so a
  source can leave the pipeline without anything failing. The Manage
  screen shows such a row as *Not loaded* or *Can't run*, with the
  reason; `check` prints it.
