# datalib — agent user guide

You are (probably) an AI agent helping a user run **datalib**: mirror
their personal data (chats, email, messages, contacts, …) into a local
store and do useful things with it. This doc maps the surfaces you'll
touch — config, sync, querying, extending — and points to the deeper
docs for each. It is about *using* datalib; for working on the datalib
codebase itself, see [`AGENTS.md`](../AGENTS.md).

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
database), then two shared fan-in steps under the `unified_index`
group — `grid_index` (SQL index) and `qmd_index` (semantic search
index). A step's function is the directory it writes:

```
<data_root>/
├── config.toml                     # the pipeline config (steps format)
├── <name>/ingest/                  # per-source raw stores
│   ├── entities.doltlite_db        #   (doltlite = SQLite + git-shaped history)
│   └── blobs.doltlite_db
├── <name>/render_markdown/         # per-source markdown tree
│   └── indexed_markdown.doltlite_db  #   its rows, edges + render problems
├── unified_index/                  # derived; carries a CACHEDIR.TAG
│   ├── grid_index/db.doltlite_db   # the grid_rows SQL index — query this
│   └── qmd_index/qmd/index.sqlite  # semantic search index
└── system/                         # the server's own state
    ├── supervisor.sqlite           # sync requests, pauses, and the loop's record (plain SQLite)
    ├── api-token                   # this process's bearer token
    ├── feedback.doltlite_db        # filed feedback (nothing regenerates it)
    └── usage.doltlite_db           # bytes-on-disk timeseries
```

The split is by writer: `unified_index/` is produced by the pipeline
and fully derived, `system/` is the server's own state. Canonical
definition — the constants both sides read — is
[`datalib/backend/runtime/src/layout.rs`](/datalib/backend/runtime/src/layout.rs).

Ten binaries ship in a release: `datalib-dag` (the sync runner),
`datalib-step` (the built-in step commands), `datalib-http` (API
server + web UI), `datalib-applet` (the applet host, spawned on demand
by the http gateway), `latchkey-curl-router` +
`curl-impersonate` (Cloudflare-safe HTTP for downloaders),
`datalib-doltlite` (the shell for reading and exporting the stores —
see "Reading the mirrored data" below), `datalib-fsindex` (the
directory-tree scanner, also reachable as a step) and
`datalib-dirtree-diff` (diffs two of its scans into one HTML page),
and `datalib-migrate-config` (rewrites a `config.toml` from a retired
shape; see below). The authoritative list is the `:dist`
filegroup in
[`datalib/backend/BUILD.bazel`](/datalib/backend/BUILD.bazel).
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

- **Complete commented example:**
  [`configs/dag_example.toml`](../configs/dag_example.toml).
- **Per-source knobs and step pairs**:
  [`docs/user/config_examples/all_sources.toml`](user/config_examples/all_sources.toml)
  — one commented group with its `ingest` + `render_markdown` step pair
  per supported source, ready to copy. (A `config.toml` whose steps
  still name a `datalib-step download …` command is rewritten once
  with `datalib-migrate-config <data_root> --force`, the only program
  that still knows that shape.
  Pre-TOML `config.yaml` roots are set up again from the app.)
- **Credentials**: web-API sources authenticate through
  [`latchkey`](https://github.com/imbue-ai/latchkey). Per-source
  walkthroughs for getting cookies/tokens/exports:
  [`docs/user/getting_your_data.md`](user/getting_your_data.md).
  On auth failure, sync events include a `hint` with the exact
  `latchkey auth set …` recipe for that provider.
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
one per typo. Exit 0 clean, 1 if the file is not a config at all, 2 if
some entries were dropped.

Useful flags: `--sync <step-id>` (repeatable; runs the named download
steps and everything downstream of them, and nothing else — pending
work in other sources waits for a full run), `--parallelism N`,
`--reset <step-id>[+blobs]` (empties what that step wrote — every row
of its store, and with `+blobs` an ingest step's blob CAS too — keeping
its doltlite history, so the
next run does its work from the start; alone it does nothing else, with
`--sync` it runs first), `--binary-dir DIR` (where bare `command:` names
like `datalib-step` resolve; defaults to the directory `datalib-dag`
itself is in). A sync that fails with "has a shape this build's DDL
cannot be reached from by adding columns" is a raw store an older build
wrote in a shape this one cannot keep; nothing was changed, and if
upstream still has the data, `--reset <source>/ingest --sync
<source>/ingest` is the way through.

**The stderr stream is NDJSON and made for you**: `run_plan` (all step
ids in topo order), then `step_start` / `progress_*` / `log` / `hint` /
`step_finish` per step, closed by one `run_summary` — parse it instead
of scraping human output. Failures carry a kind
(`transient` / `rate_limited` / `auth` / `data` / `cancelled`); the
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
| sync | `datalib-dag <config> --sync <step> --by claude` | `POST /api/requests {"roots": ["<step>"], "by": "claude"}` (no roots: every source) |
| stop a sync | `datalib-dag stop <config> <request-id> --by claude` | `POST /api/requests/<id>/stop {"by": "claude"}` |
| pause a step | `datalib-dag pause <config> <step> --by claude` | `POST /api/steps/<step>/pause {"by": "claude"}` (`/` in the id as `%2F`) |
| resume it | `datalib-dag resume <config> <step>` | `POST /api/steps/<step>/resume` |

`<config>` is `<data_root>/config.toml`. A stop, a pause and a resume
take effect within a second. A pause stops a step that is running,
keeps it from starting until it is resumed, and makes whatever reads it
wait. It does not hold a sync open: a sync whose only work is a paused
step closes without running it. `POST /api/requests` answers once the
loop has taken the request on, with the request's `id`.

**Watching.** `GET /api/requests` lists the open requests, then the
newest closed ones: each with its `roots`, `by`, and `state` (`open`, or
how it ended: `done`, `failed`, `stopped`). `datalib-dag status
<config>` prints the same from the shell, with the pauses and the steps
running now. What each step is doing is in the loop's record, which the
Manage screen's Status column reads directly:

```sh
sqlite3 <data_root>/system/supervisor.sqlite \
  'select step, state, state_detail, paused_by, request from steps'
```

`state` is `running`, `waiting` (on what: `state_detail`), `paused`,
`blocked`, `failed`, or at rest (`idle`, `stale`, `fresh`); `request` is
the open request it is being run for. Each request and pause records
who made it, so the screen shows "paused by claude" or "Stop the sync of
Work Slack, started by claude". **Don't resume or stop what a person
started without saying so.**

**Resetting** empties what a source downloaded: every row of its store
goes (with `+blobs`, its attachments too), and the doltlite history
keeps them. Then what reads it runs, so its documents leave the grid,
and its next sync downloads everything again from nothing. Resetting a
render step (`slack/render_markdown`) instead rebuilds its documents
from what is downloaded, at once. It needs the
root to itself, so it runs only when nothing is syncing. With the app
up, use `POST /api/reset {"targets": ["slack/ingest+blobs"], "by":
"claude"}`: it answers once the store is empty, opens the request that
carries the emptiness downstream, and refuses while a sync runs. The
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
loop, the applets, requests that failed). All of it is
`system/runs/runs.sqlite`, plain SQLite, so `sqlite3` reads it directly
too. `GET /api/sync/stream` is a server-sent-event stream that pushes a
`root` frame whenever any of this moves.

## Reading the mirrored data

Pick the surface that fits the question:

- **SQL over everything** — the `grid_rows` union table in
  `unified_index/grid_index/db.doltlite_db`: one row per
  message/document/entity across all sources, with `provider`, `kind`,
  `created_at`, `modified_at`, `author`, `channel`, `conversation_uuid`,
  `preview` (the first 240 characters of the row's text; the whole text
  is in the rendered markdown), `entire_chat`, etc. `is_document = 1`
  picks the one row per rendered document — the thread, the
  conversation, the PR, the page — and leaves out the messages inside
  them, which is usually the row count you meant.

  Read it with **`datalib-doltlite`**, which is in the release tarball
  and so sits next to `datalib-dag` in `~/.local/bin` (it is plain
  `doltlite` in the docker image, and
  `bazelisk build //third-party/doltlite:doltlite` from a checkout).
  Its argv is `sqlite3`'s. **Always pass `-readonly`** — a stray writer
  can wedge later syncs:

  ```sh
  datalib-doltlite -readonly unified_index/grid_index/db.doltlite_db \
    "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
  ```

  Stock `sqlite3` **cannot** open the file itself — a `.doltlite_db` is
  a prolly-tree store, not a SQLite file, and `sqlite3` says `file is
  not a database`. But nothing is trapped in there: one pipe writes a
  plain SQLite database with the same tables, schemas and indexes, for
  any tool that speaks only SQLite.

  ```sh
  datalib-doltlite -readonly unified_index/grid_index/db.doltlite_db .dump \
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
- **Markdown** — `<name>/render_markdown/` holds human-readable QMD
  markdown per conversation/document. Read files directly, or serve
  them via `GET /applet/unified_index/chat/{markdown_uuid}`. The raw per-source
  doltlite stores under `<name>/ingest/` keep full wire fidelity when the
  rendered form isn't enough.
- **Semantic search** — the qmd index:

  ```sh
  rt=$(echo ~/.cache/datalib/runtime/*/)   # the fetched runtime; /opt/datalib/runtime in the image
  INDEX_PATH=<data_root>/unified_index/qmd_index/qmd/index.sqlite \
      "$rt/node/bin/node" "$rt"/qmd/*/node_modules/@tobilu/qmd/dist/cli/qmd.js query "that thing about the boat"
  ```
- **HTTP API** — `datalib-http <data_root>` serves the UI plus:
  `GET /applet/unified_index/search?q=…` (Gmail-flavored query language:
  `field:value`, `-field:value`, quoted values; fields include
  `source:`, `source_id:` (`source_name:` is an accepted alias),
  `kind:`, `channel:`, `author:`, `account:`,
  `project:`, `before:`/`after:`, `convo:`, `is:document` for the one
  row per rendered document and `-is:document` for the rows inside
  them), `GET /api/log?q=…` (the
  runner's log lines in the same grammar — keys `run:`, `step:`,
  `level:`, `stream:`, `target:`, `thread:`, `msg:`; free text is a
  substring of the line; `run=`/`step=` narrow it, `after_seq=` tails),
  `GET /applet/unified_index/docs`, `GET /applet/unified_index/chat/{uuid}`,
  `GET /applet/unified_index/asset/{uuid}/{path}`, `GET /api/dag` (the derived step
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
  [`datalib/backend/http/src/auth.rs`](/datalib/backend/http/src/auth.rs).

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
- **Custom UI cards** — the web UI can host agent-authored views
  ("cards", small JS view factories, `PUT /api/lib/{name}`). The
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
  last_status, last_error from steps` gives each step's input versions
  at its last success and how it last ended; `select * from sinks` the version each tree was published at.
  A step re-runs when an input version moved (download steps always run
  — their input is a remote service). `select step, started_at_utc from
  invocations where outcome is null` is what is running now.
- **Wedged doltlite file** (`commit conflict` after a stray writer):
  recovery recipes in [`docs/dev/doltlite.md`](dev/doltlite.md).
- **A config the runner rejects**: `datalib-dag --check
  <data_root>/config.toml` lists every problem with a line number;
  `PUT /api/config` (or the Manage tab) returns the same list in
  `diagnostics` and writes nothing. A data root still holding a
  pre-TOML `config.yaml` reads as unconfigured — set it up again from
  the app.
- **A step that silently stopped running**: check `diagnostics` on
  `GET /api/config`, or `--check`. A config with one unusable entry
  still loads — that entry is dropped and everything else runs — so a
  source can leave the pipeline without anything failing. The Pipeline
  table shows such a row as *Not loaded* or *Can't run*, with the
  reason; `--check` prints it.
