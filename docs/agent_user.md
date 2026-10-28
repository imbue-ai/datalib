# datalib — agent user guide

You are (probably) an AI agent helping a user run **datalib**: mirror
their personal data (chats, email, messages, contacts, …) into a local
store and do useful things with it. This doc maps the surfaces you'll
touch — config, sync, querying, extending — and points to the deeper
docs for each. It is about *using* datalib; for working on the datalib
codebase itself, see [`AGENTS.md`](../AGENTS.md).

From a shell, **`datalib-step help`** is the map: the steps from an
empty directory to a first sync, each with its command, and
`datalib-step help <type>` (what a source mirrors, how it signs in, its
ingest methods) and `datalib-step help <function>` (what a step does
and how to run it) from there. `datalib-step help agent` prints this
guide, which a release also carries beside its binaries as
`docs/agent_user.md`, with the two guides for building on the data
(`help raw-stores`, `help apps`) beside it. Every binary's `--help`
ends with examples.

The human-facing guides are worth reading first, and worth pointing the
user at: the [first-time user guide](user/first_time_user.md) is the
walkthrough from install to first sync, with the warnings about what a
pile of private data in one place means and what these credentials can
do; [getting your data](user/getting_your_data.md) is the per-source
recipe for credentials and exports; and [running in
Docker](user/docker.md) is the sandboxed way to try it, with a demo
library already loaded. The README's source table and "what we are
aiming for" say what datalib is for and where it is going, which is
the context for anything the user asks you to build on top of it.

## The mental model

Everything lives under one **data root** directory. A sync is a DAG of
steps run by `datalib-dag`: per source (a `[[groups]]` entry) a
`<group>/ingest` step (bring the raw data in) and a
`<group>/render_markdown` step (raw → markdown + a per-source index
database), `grid_index` under the `unified_document_index` group (the SQL
index), and, for a searched source, `<group>/keyword_index` and
`<group>/embed`, which fill that source's collection of the search
index. `unified_document_index/qmd_aggregator` runs after every source's pair
and keeps the search index to the sources it names. A step's function
is the directory it writes (the qmd steps are the exception: they all
write the one index file under `unified_document_index/qmd_aggregator/`):

```
<data_root>/
├── config.toml                     # the pipeline config (steps format)
├── <group>/ingest/                 # per-source raw stores (<group> is the source's id)
│   ├── entities.doltlite_db        #   (doltlite = SQLite + git-shaped history)
│   └── blobs.sqlite                #   attachment bytes by blake3 (plain SQLite)
├── <group>/render_markdown/        # per-source markdown tree
│   └── indexed_markdown.doltlite_db  #   its rows, edges + render problems
├── unified_document_index/                  # derived; carries a CACHEDIR.TAG
│   ├── grid_index/db.doltlite_db   # the grid_rows SQL index — query this
│   ├── grid_index/search_terms.sqlite     # each row's ids, people, labels and names, full-text (plain SQLite)
│   └── qmd_aggregator/qmd/index.sqlite  # semantic search index
└── system/                         # the server's own state
    ├── supervisor.sqlite           # sync requests, steps turned off, and the loop's record (plain SQLite)
    ├── runs/runs.sqlite            # every run's step states, log lines and metrics (plain SQLite)
    ├── api-token                   # this process's bearer token
    ├── feedback.doltlite_db        # filed feedback (nothing regenerates it)
    ├── remote_media.doltlite_db    # remote media a person let a document load
    └── disk_stats.sqlite           # bytes on disk and free space, over time (plain SQLite)
```

The split is by writer: `unified_document_index/` is produced by the pipeline
and fully derived, `system/` is the server's own state. Canonical
definition — the constants both sides read — is
[`datalib/backend/runtime/src/layout.rs`](../datalib/backend/runtime/src/layout.rs).

Ten binaries ship in a release: `datalib-dag` (the sync runner),
`datalib-step` (the built-in step commands), `datalib-http` (API
server + web UI), `datalib-applet` (the applet host, spawned on demand
by the http gateway; `datalib-applet search` is the search box from the
command line), `latchkey-curl-router` +
`curl-impersonate` (Cloudflare-safe HTTP for downloaders),
`datalib-doltlite` (the shell for reading and exporting the stores —
see "Reading the mirrored data" below), `datalib-fsindex` (the
directory-tree scanner, also reachable as a step) and
`datalib-dirtree-diff` (diffs two of its scans into one HTML page),
and `datalib-migrate-config` (rewrites a `config.toml` from a retired
shape; see below). The authoritative list is the `:dist`
filegroup in
[`datalib/backend/BUILD.bazel`](../datalib/backend/BUILD.bazel).
End-to-end setup walkthrough:
[`docs/user/first_time_user.md`](user/first_time_user.md).

## Configuring sources

`<data_root>/config.toml` is TOML: a `[[groups]]` entry per source (an
`id`, a `name`, a `type`), one `[[steps]]` table per step declared as
`group` + `function` — its id, `<group>/<function>`, is composed rather
than written — and `inputs` naming the steps it reads by that id.
Top-level keys (`data_root`, `binary_dir`) go above the first `[[…]]`
header, and a step's `params` sub-tables come after its plain keys — a
`[…]` header ends the table it appears in.

- **From the CLI alone:** `datalib-dag init <data_root>` writes the
  starter `config.toml` (the index steps and their applets, no
  sources) and nothing else; `datalib-step examples <type> --append
  <data_root>/config.toml` adds a source (below).
- **Complete commented example:**
  [`configs/dag_example.toml`](../configs/dag_example.toml).
- **Every source, every option**:
  [`docs/user/config_examples/all_sources.toml`](user/config_examples/all_sources.toml)
  — one commented group per way into each source, with its steps and
  every option set, and the index groups reading them all. It is
  generated from the complete stanzas `datalib-step examples <type>`
  prints, so it is always one that loads. A release ships both files
  beside this guide, at the same paths under its directory. A `config.toml` in the retired
  shape (steps naming a `datalib-step download …` command) is refused;
  rewrite it once with `datalib-migrate-config <data_root> --force`.
- **Credentials**: web-API sources authenticate through
  [`latchkey`](https://github.com/imbue-ai/latchkey). Per-source
  walkthroughs for getting cookies/tokens/exports:
  [`docs/user/getting_your_data.md`](user/getting_your_data.md).
  On auth failure, sync events include a `hint` with the exact
  `latchkey auth set …` recipe for that provider.
- **How far back (and ahead)**: every source that can mirror part of
  its history takes `since` in its method table (`[steps.params.api]`,
  `[steps.params.gmail]`, `[steps.params.google]`, …), and a calendar
  and Garmin also take `until`. Each is a day (`"2026-09-01"`), an
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
datalib-dag <data_root>/config.toml            # everything
datalib-dag <data_root>/config.toml --sync slack/ingest      # one source
datalib-dag --check <data_root>/config.toml   # validate, run nothing
```

`--check` prints *every* problem with the config rather than the first,
as `file:line:col: severity: message` with the offending line and a
`help:` line under each — so fixing a config takes one round-trip, not
one per typo. It also has `datalib-step` plan every built-in step's
params, so a misspelled or misplaced option is reported at its line
before a sync rather than when the step runs. Exit 0 when every entry
loads and every step's params plan, 1 if the file is not a config at
all (or the root was written by a newer datalib), 2 if some entries
were dropped or some step's params would be refused.

`datalib-step examples <type>` prints each source's config stanzas:
for every way into the source, a minimal one (the least that syncs) and
a complete one (every option set, each with a comment). With
`--append <config>` it adds the minimal one to the config instead,
above the index groups, and puts the source's steps in the `inputs` of
the index steps that read them (`grid_index`, and the time-series
index, which it adds when a time-series source is the first). A
source reached more than one way names the method (`--method gmail`),
and `--id` gives the group an id of its own, for a second account. It
refuses an id the config already has, and a result `datalib-dag
--check` would refuse, writing nothing; the previous file is kept as
`config.toml.bak`. Its page says how to do the same by hand.
`datalib-step help <type> <method>` lists every option with its type,
default and meaning. Both are read from the source's own config crate,
and a test loads and plans every stanza.

```sh
datalib-dag init ~/datalib
datalib-step examples slack --append ~/datalib/config.toml
datalib-step examples email --append ~/datalib/config.toml --method gmail --id work-mail
```

Useful flags: `--sync <step-id>` (repeatable; runs the named steps and
everything downstream of them, and nothing else — pending work in other
sources waits for a full run), `--parallelism N`, `--reset
<step-id>` (see **Resetting** below; alone it does nothing else,
with `--sync` it runs first), `--compact` (folds the history of every
render store and the grid index into one commit and gives back the disk
it held; their content stays, so nothing re-runs; refused while the app
is up, where `POST /api/compact {}` does the same), `--binary-dir DIR` (put at the front of
every step's `PATH`, so a bare `command` like `datalib-step` resolves
there; defaults to the config's `binary_dir`, else the directory
`datalib-dag` itself is in). A sync that fails with "has a shape this build's DDL
cannot be reached from by adding columns" is a raw store an older build
wrote in a shape this one cannot keep; nothing was changed, and if
upstream still has the data, `--reset <source>/ingest --sync
<source>/ingest` is the way through.

**To run one step, `--sync` it** — never run `datalib-step` for it by
hand. The runner is what sets the step's environment, takes the root's
lock and records the version the step published, which is how the steps
that read it learn it moved; a step run by hand records nothing. An
ingest step always runs when synced; any other runs only if what it
reads moved, so to rebuild a source's documents from what is already
downloaded, reset its render step in the same command:
`datalib-dag <config> --reset slack/render_markdown --sync
slack/render_markdown`. `datalib-step help <function>` gives the
command for each kind of step.

**The stderr stream is NDJSON and made for you**: `run_plan` (every step's
id and group, in topo order), then `step_start` / `progress_*` / `log` / `hint` /
`step_finish` per step, closed by one `run_summary` — parse it instead
of scraping human output. Failures carry a kind
(`transient` / `rate_limited` / `auth` / `permission` / `data` /
`cancelled` / `incomplete`); the
runner already retries transient/rate-limited ones with backoff, and
what a failed step committed is still read downstream; only a step
whose inputs were never written at all is `blocked`. Ctrl-C is graceful:
steps checkpoint-commit partial progress and the next run resumes.
Syncs are incremental and idempotent — re-running is always safe.

**You can sync while the app, or another `datalib-dag`, is syncing the
same root.** A sync is a *request*: a row in
`<data_root>/system/supervisor.sqlite` naming the steps to sync (its
*roots*; everything downstream of them comes too) and who asked. One
process runs the loop that serves requests — the app while it is open,
otherwise the first `datalib-dag` to need one — and every other
`datalib-dag` hands its request to that loop, says `following request
<id>`, and waits for it. Your source runs beside theirs, not after. It
exits with *your* request's outcome: 0 done, 2 failed, 130 stopped, and
Ctrl-C stops your request only. Pass `--by <name>` so the request says
who asked.

There are four verbs, and each is the same row written two ways: from
a shell with no server needed, or over HTTP while the app is up.

| verb | CLI | HTTP |
|---|---|---|
| sync | `datalib-dag <config> --sync <step> --by claude` | `POST /api/requests {"roots": ["<step>"], "by": "claude"}` (no roots: every source; one request per source) |
| stop a sync | `datalib-dag stop <config> <request-id> --by claude` | `POST /api/requests/<id>/stop {"by": "claude"}` |
| turn a step off | `datalib-dag turn-off <config> <step> --by claude` | `POST /api/steps/<step>/turn_off {"by": "claude"}` (`/` in the id as `%2F`) |
| turn it on | `datalib-dag turn-on <config> <step>` | `POST /api/steps/<step>/turn_on` |

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
how it ended: `done`, `failed`, `stopped`). `datalib-dag status
<config>` prints the same from the shell, with the steps turned off and the steps
running now. What each step is doing is in the loop's record, which the
Manage screen's Status column reads directly:

```sh
sqlite3 <data_root>/system/supervisor.sqlite \
  'select step, state, state_detail, turned_off_by, requests from steps'
```

`state` is `running`, `waiting` (on what: `state_detail`), `off`,
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
Manage screen's row menu offers the same. With no app up,
`datalib-dag --reset slack/ingest` empties the store alone; add
`--sync slack/ingest` to download it again at once.

**Logs.** The run store is where to read what happened.
`GET /api/runs` lists runs: a run is one stretch of the loop being busy,
and every request served in that stretch is in it.
`/api/runs/{run}/steps` gives every step's state and metrics, and
`/api/runs/{run}/log?step=&after_seq=` is the log, which you can tail
by `seq`. `GET /api/log?q=` is the same log across every run, in the
search bar's grammar (`level:warn -target:sqlx "history"`), and
`process:http` narrows it to what the server itself said (its sync
loop, the applets, requests that failed). `GET /api/sync/stream` is a
server-sent-event stream that pushes a `root` frame whenever any of
this moves.

All of it is one file, `<data_root>/system/runs/runs.sqlite`: every
line any datalib process wrote — the runner, each step, the server,
the applets. It is plain SQLite, so stock `sqlite3` reads it with no
server up. `log` holds one row per line, `processes` says who wrote
each (join on `process_id`), `runs` and `step_runs` hold each run and
each step's state, attempt and error, and `metrics` holds a step's
numbers:

```sh
sqlite3 <data_root>/system/runs/runs.sqlite \
  "SELECT l.ts_utc, p.process, l.step, l.level, l.target, l.msg
     FROM log l LEFT JOIN processes p USING (process_id)
    WHERE l.level IN ('warn', 'error')
    ORDER BY l.seq DESC LIMIT 50"
```

`select run_id, step, state, attempt, error from step_runs where error
is not null` lists the steps that failed. Every table and column is in
[`docs/dev/logging.md`](dev/logging.md).

## Reading the mirrored data

If you are building an app that reads this data instead of calling the
services (when to sync, how fresh the mirror is, deriving your own data
as a step, when to ask the service after all), read [building on
datalib](user/building_on_datalib.md) (`datalib-step help apps`).

Pick the surface that fits the question:

- **The search box, from the command line** — `datalib-applet search
  <data_root> '<query>'` answers a search exactly as the app's search
  box does, with no server and no token, and prints the JSON
  `/search` would (below): the query language, `--tab`, `--limit`,
  `--offset`, `--sort` and `--within` are the same. Page with
  `--offset` set to the answer's `next_offset`. It reads only, so it is
  safe beside a running sync or app.

  ```sh
  datalib-applet search ~/datalib 'from:picard after:2026-10-01 is:document' --limit 20
  datalib-applet search ~/datalib 'warp core' --tab words
  ```

  It exits 1, with each reason on stderr, when the answer is not the
  search you asked for: a query it refused (`refused`), an error, a
  free-text search qmd could not answer, or free text on a root with no
  search index yet (a source's words are searchable once its
  `keyword_index` step has run). An empty answer with exit 0 means
  nothing matched. Free text without `--tab` starts qmd and loads its
  models, which takes 10–20 seconds on a large root; `--tab words` and
  `--tab fields` take well under a second.
- **SQL over everything** — the `grid_rows` union table in
  `unified_document_index/grid_index/db.doltlite_db`: one row per
  message/document/entity across all sources, with `provider`, `kind`,
  `created_at`, `modified_at`, `touched_at`, `author`, `channel`, `conversation_uuid`,
  `preview` (the first 240 characters of the row's text; the whole text
  is in the rendered markdown), `entire_chat`, etc. `is_document = 1`
  picks the one row per rendered document — the thread, the
  conversation, the PR, the page — and leaves out the messages inside
  them, which is usually the row count you meant.

  Read it with **`datalib-doltlite`**, which is in the release tarball
  and so sits next to `datalib-dag` in `~/.local/bin` (it is plain
  `doltlite` in the docker image, and
  `bazelisk build //third-party/doltlite:doltlite` from a checkout).
  Its argv is `sqlite3`'s. **Always pass `-readonly`**: a writable
  session's commits are overwritten by the next sync without an error,
  and a transaction it holds open stalls the sync:

  ```sh
  datalib-doltlite -readonly unified_document_index/grid_index/db.doltlite_db \
    "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
  ```

  Stock `sqlite3` **cannot** open the file itself (`file is not a
  database`), but one pipe writes a plain SQLite database with the same
  tables, schemas and indexes, for any tool that speaks only SQLite.

  ```sh
  datalib-doltlite -readonly unified_document_index/grid_index/db.doltlite_db .dump \
    | sqlite3 grid.sqlite
  ```

  The snapshot carries the data, not the commit history — see
  [`docs/dev/doltlite.md`](dev/doltlite.md) for what that costs and for
  a single-table variant. If you would rather not touch the store at
  all, `datalib-http`'s endpoints below serve the same rows.

  Column semantics: [`docs/dev/grid_rows.md`](dev/grid_rows.md).
  Cross-document links: [`docs/dev/edges.md`](dev/edges.md).
  doltlite recipes (history, diffs, a crashed writer):
  [`docs/dev/doltlite.md`](dev/doltlite.md).
- **Markdown** — `<group>/render_markdown/` holds human-readable QMD
  markdown per conversation/document. Read files directly, or serve
  them via `GET /applet/unified_document_index/chat/{markdown_uuid}`.
- **Raw stores** — the per-source doltlite stores under
  `<group>/ingest/` keep what the service sent, when the rendered form
  isn't enough: Slack's and Google Calendar's JSON, Gmail's original
  message. [Reading the raw stores](user/raw_stores.md) (`datalib-step
  help raw-stores`) is the map: how to open one (`payload` is JSONB, so
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
- **Semantic search** — the qmd index:

  ```sh
  rt=$(echo ${XDG_CACHE_HOME:-~/.cache}/datalib/runtime/*/)   # the fetched runtime; /opt/datalib/runtime in the image
  INDEX_PATH=<data_root>/unified_document_index/qmd_aggregator/qmd/index.sqlite \
      "$rt/node/bin/node" "$rt"/qmd/*/node_modules/@tobilu/qmd/dist/cli/qmd.js query "that thing about the boat"
  ```
- **HTTP API** — `datalib-http <data_root>` serves the UI plus:
  `GET /applet/unified_document_index/search?q=…` (the query language,
  `from:me` included, is [Searching your mirror](user/searching.md),
  `datalib-step help search`; `GET …/search/keys` lists every key and `…/search/values?key=…&typed=…&q=…`
  the values one takes, most rows first. A key the search does not
  have is refused by name, in `refused`, rather than ignored. Free text
  made only of uuids and handles (an email address, `tel:+…`,
  `slack:T…/U…`) is looked up in `grid_index/search_terms.sqlite`, not sent to
  qmd: every row that answers to each one, best match first, with
  `score` saying how (5 its own id, 4 its author or an addressee,
  3 someone copied or mentioned, or what it is in, 2 its title or a label, 1 a name
  it shows) and `snippet` naming the match.
  `tab=` picks how free text is answered, each its own list:
  `fields` (the search terms file, any word as the start of one), `words`
  (every document's text by BM25, from qmd's keyword index) or
  `meaning` (qmd's vectors alone: the 20 nearest documents across every
  source, or within one under `source_id:`); with no `tab`, the free text goes to
  qmd's hybrid query, identifiers aside. It answers a page: `limit=` rows from `offset=`, with `total`
  and the `next_offset`; `sort=created_at:desc,author` orders by grid
  columns in turn. `GET /applet/unified_document_index/search/groups?q=…&by=kind`
  counts the groups, and `within=[["kind","Chat"]]` on `search` lists
  one group), `GET /api/log?q=…` (the
  runner's log lines in the same grammar — keys `run:`, `process:`,
  `process_id:`, `step:`, `attempt:`, `level:`, `stream:`, `target:`,
  `thread:`, `msg:`, and `commit:` by prefix; free text is a
  substring of the line; `run:`, `process_id:`, `step:` and `attempt:`
  narrow it to a run, a process or a step's attempt; the newest `limit=`
  lines, oldest first, with `before_seq=` paging back and `after_seq=`
  tailing),
  `GET /applet/unified_document_index/docs`, `GET /applet/unified_document_index/chat/{uuid}`,
  `GET /data/<group>/<function>/blob/<hash>` (an attachment or photo a
  page links), `GET /api/dag` (the derived step
  graph), and the config/sync endpoints above.

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
  are readable without it. Design notes:
  [`datalib/backend/http/src/auth.rs`](../datalib/backend/http/src/auth.rs).

## Extending datalib

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
  entry costs) are in
  [`datalib/backend/dag/README.md`](../datalib/backend/dag/README.md).
- **Custom UI cards** — the web UI can host agent-authored components
  (ES modules whose default export is a card factory, saved with
  `PUT /api/lib/{name}`). The
  server serves its own guide for this at **`GET /agent/cards.md`**
  (and one for config-editing agents at **`GET /agent/config.md`**);
  source reference: [`docs/dev/cards.md`](dev/cards.md).

## Feedback

Something broken, missing, or confusing? **File an issue at
<https://github.com/imbue-ai/datalib/issues>** — that is the preferred
channel, and every report is welcome. Two kinds are equally wanted:
what exists and misbehaves (a one-line "this doc is wrong", a full
bug), and what would make datalib easier to wield for whatever you and
the user are trying to do with it — a surface that fought you, a query
you had to work around, a use case it doesn't serve yet. Paste the
`datalib-dag --check` output or the failing step's `step_finish` event
where you have one; it saves a round-trip. **The tracker is public:
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
  plain SQLite. `select step, state, state_detail from steps` says what
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
- **A config the runner rejects**: `datalib-dag --check
  <data_root>/config.toml` lists every problem with a line number;
  `PUT /api/config` (or the Manage tab) returns the same list in
  `diagnostics`, built-in steps whose params a sync would refuse
  included, and writes nothing. A data root holding a pre-TOML
  `config.yaml` reads as unconfigured — set it up again from the app.
- **Every step reads `waiting for disk space`**: the data root's disk
  went under the config's `[disk_space]` pause line (10 GB unless the
  config moves it), so the loop stopped what ran and starts nothing
  until the disk is back to the resume line (15 GB). A one-shot
  `datalib-dag` that seems to hang after its `run_plan` line is this:
  each held step logs `waiting for disk space: …` once. Free space, or
  lower the lines; the open syncs carry on by themselves.
- **A step that silently stopped running**: check `diagnostics` on
  `GET /api/config`, or `--check`. A config with one unusable entry
  still loads — that entry is dropped and everything else runs — so a
  source can leave the pipeline without anything failing. The Manage
  screen shows such a row as *Not loaded* or *Can't run*, with the
  reason; `--check` prints it.
