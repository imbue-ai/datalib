# datalib — agent user guide

You are (probably) an AI agent helping a user run **datalib**: mirror
their personal data (chats, email, messages, contacts, …) into a local
store and do useful things with it. This doc maps the surfaces you'll
touch — config, sync, querying, extending — and points to the deeper
docs for each. It is about *using* datalib; for working on the datalib
codebase itself, see [`AGENTS.md`](../AGENTS.md).

## The mental model

Everything lives under one **data root** directory. A sync is a DAG of
steps run by `datalib-dag`: per source a `<name>.download` step (fetch
raw data) and a `<name>.render` step (raw → markdown + a per-source
index database), then two shared fan-in steps —
`grid_index` (SQL index) and `qmd_index` (semantic search index):

```
<data_root>/
├── config.toml                     # the pipeline config (steps format)
├── <name>/raw/                     # per-source raw stores
│   ├── entities.doltlite_db        #   (doltlite = SQLite + git-shaped history)
│   └── blobs.doltlite_db
├── <name>/rendered_md/             # per-source markdown tree
│   └── indexed_markdown.doltlite_db  #   its rows, edges + render problems
├── unified_index/                  # derived; carries a CACHEDIR.TAG
│   ├── grid/db.doltlite_db         # the grid_rows SQL index — query this
│   └── qmd/index.sqlite            # semantic search index
└── system/                         # the server's own state
    ├── dag_state.json              # scheduler state (per-step versions)
    ├── api-token                   # this process's bearer token
    ├── feedback.doltlite_db        # filed feedback (nothing regenerates it)
    ├── jobs.doltlite_db            # sync job queue + history
    └── usage.doltlite_db           # bytes-on-disk timeseries
```

The split is by writer: `unified_index/` is produced by the pipeline
and fully derived, `system/` is the server's own state. Canonical
definition — the constants both sides read — is
[`datalib/backend/core/src/layout.rs`](/datalib/backend/core/src/layout.rs).

Ten binaries ship in a release: `datalib-dag` (the sync runner),
`datalib-step` (the built-in step commands), `datalib-http` (API
server + web UI), `datalib-applet` (the applet host, spawned on demand
by the http gateway), `latchkey-curl-dispatch` +
`latchkey-curl-impersonate` (Cloudflare-safe HTTP for downloaders),
`datalib-doltlite` (the shell for reading and exporting the stores —
see "Reading the mirrored data" below), `datalib-fsindex` (the
directory-tree scanner, also reachable as a step) and
`datalib-dirtree-diff` (diffs two of its scans into one HTML page),
and `datalib-migrate-config` (one-shot conversion of a pre-TOML
`config.yaml`; see below). The authoritative list is the `:dist`
filegroup in
[`datalib/backend/BUILD.bazel`](/datalib/backend/BUILD.bazel).
End-to-end setup walkthrough:
[`docs/user/first_time_user.md`](user/first_time_user.md).

## Configuring sources

`<data_root>/config.toml` is TOML: one `[[steps]]` table per step,
declaring the steps directly; edges are derived from input/output
paths, never written by hand. Top-level keys (`data_root`,
`binary_dir`) go above the first `[[steps]]`, and a step's `params`
sub-tables come after its plain keys — a `[…]` header ends the table it
appears in.

- **Complete commented example:**
  [`configs/dag_example.toml`](../configs/dag_example.toml).
- **Per-source knobs and step pairs**:
  [`docs/user/config_examples/all_sources.toml`](user/config_examples/all_sources.toml)
  — one commented `<name>.download` + `<name>.render` step pair per
  supported source, in the steps format, ready to copy. (Two pre-TOML
  `config.yaml` formats still exist in the wild — a YAML steps config
  and the older stanza-based `sources:` one. Neither is read by
  anything any more: convert once with `datalib-migrate-config
  <data_root>`, which is the only program that still knows them.)
- **Credentials**: web-API sources authenticate through
  [`latchkey`](https://github.com/imbue-ai/latchkey). Per-source
  walkthroughs for getting cookies/tokens/exports:
  [`docs/user/getting_your_data.md`](user/getting_your_data.md).
  On auth failure, sync events include a `hint` with the exact
  `latchkey auth set …` recipe for that provider.
- The web UI's **Setup** tab scaffolds and validates the config
  (`GET /api/config/scaffold`, `PUT /api/config` validates before
  writing).

## Running a sync

CLI:

```sh
datalib-dag <data_root>/config.toml            # everything
datalib-dag <data_root>/config.toml --sync slack.download   # one source
datalib-dag --check <data_root>/config.toml   # validate, run nothing
```

`--check` prints *every* problem with the config rather than the first,
as `file:line:col: severity: message` with the offending line and a
`help:` line under each — so fixing a config takes one round-trip, not
one per typo. Exit 0 clean, 1 if the file is not a config at all, 2 if
some entries were dropped.

Useful flags: `--sync <step-id>` (repeatable; runs the named download
steps and everything downstream of them, and nothing else — pending
work in other sources waits for a full run), `--parallelism N`, `--reset-and-redownload`,
`--refetch-blobs`, `--binary-dir DIR` (where bare `command:` names like
`datalib-step` resolve; defaults to the directory `datalib-dag` itself
is in).

**The stderr stream is NDJSON and made for you**: `run_plan` (all step
ids in topo order), then `step_start` / `progress_*` / `log` / `hint` /
`step_finish` per step, closed by one `run_summary` — parse it instead
of scraping human output. Failures carry a kind
(`transient` / `rate_limited` / `auth` / `data` / `cancelled`); the
runner already retries transient/rate-limited ones with backoff, and a
failed step blocks only its downstream subtree. Ctrl-C is graceful:
steps checkpoint-commit partial progress and the next run resumes.
Syncs are incremental and idempotent — re-running is always safe.

Via the server instead: `POST /api/sync/jobs` enqueues, `GET
/api/sync/stream` streams the same events, `/api/sync/jobs/{id}/log`
and `/cancel` do what they say.

## Reading the mirrored data

Pick the surface that fits the question:

- **SQL over everything** — the `grid_rows` union table in
  `unified_index/grid/db.doltlite_db`: one row per
  message/document/entity across all sources, with `provider`, `kind`,
  `when_ts`, `author`, `channel`, `conversation_uuid`, `text`,
  `entire_chat`, etc.

  Read it with **`datalib-doltlite`**, which is in the release tarball
  and so sits next to `datalib-dag` in `~/.local/bin` (it is plain
  `doltlite` in the docker image, and
  `bazelisk build //third-party/doltlite:doltlite` from a checkout).
  Its argv is `sqlite3`'s. **Always pass `-readonly`** — a stray writer
  can wedge later syncs:

  ```sh
  datalib-doltlite -readonly unified_index/grid/db.doltlite_db \
    "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
  ```

  Stock `sqlite3` **cannot** open the file itself — a `.doltlite_db` is
  a prolly-tree store, not a SQLite file, and `sqlite3` says `file is
  not a database`. But nothing is trapped in there: one pipe writes a
  plain SQLite database with the same tables, schemas and indexes, for
  any tool that speaks only SQLite.

  ```sh
  datalib-doltlite -readonly unified_index/grid/db.doltlite_db .dump \
    | sqlite3 grid.sqlite
  ```

  The snapshot carries the data, not the commit history — see
  [`docs/dev/doltlite.md`](dev/doltlite.md) for what that costs and for
  a single-table variant. If you would rather not touch the store at
  all, `datalib-http`'s endpoints below serve the same rows.

  Column semantics: [`docs/dev/grid_rows.md`](dev/grid_rows.md).
  Cross-document links: [`docs/dev/edges.md`](dev/edges.md).
  doltlite recipes (history, diffs, rescue):
  [`docs/dev/doltlite.md`](dev/doltlite.md).
- **Markdown** — `<name>/rendered_md/` holds human-readable QMD
  markdown per conversation/document. Read files directly, or serve
  them via `GET /applet/unified_index/chat/{markdown_uuid}`. The raw per-source
  doltlite stores under `<name>/raw/` keep full wire fidelity when the
  rendered form isn't enough.
- **Semantic search** — the qmd index:

  ```sh
  INDEX_PATH=<data_root>/unified_index/qmd/index.sqlite \
      npx -y @tobilu/qmd query "that thing about the boat"
  ```
- **HTTP API** — `datalib-http <data_root>` serves the UI plus:
  `GET /applet/unified_index/search?q=…` (Gmail-flavored query language:
  `field:value`, `-field:value`, quoted values; fields include
  `source:`, `source_name:`, `kind:`, `channel:`, `author:`, `account:`,
  `project:`, `before:`/`after:`, `convo:`), `GET /applet/unified_index/docs`, `GET /applet/unified_index/chat/{uuid}`,
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
  `command`/`inputs`/`outputs`/`params`, and the runner feeds it
  flags + env vars and (optionally) parses NDJSON progress/outcome
  events from its stdout. A plain shell script works; adopting more of
  the protocol buys incrementality, live progress, and retry
  classification. **Read
  [`docs/dev/step_protocol.md`](dev/step_protocol.md)** — it is
  the complete contract, with minimal shell and Python examples. The
  design behind the scheduler (edge derivation, skipping, subtree
  poisoning) is
  [`docs/dev/pipeline_dag_architecture.md`](dev/pipeline_dag_architecture.md).
- **Custom UI cards** — the web UI can host agent-authored views
  ("cards", small JS view factories, `PUT /api/lib/{name}`). The
  server serves its own guide for this at **`GET /agent/cards.md`**
  (and one for config-editing agents at **`GET /agent/config.md`**);
  source reference: [`docs/dev/cards.md`](dev/cards.md).

## Troubleshooting quick hits

- **Auth failures**: look for the `hint` event in the sync stream — it
  contains the provider-specific `latchkey` walkthrough. Cloudflare
  403s despite a fresh cookie usually mean a flagged IP/UA; wait or
  change networks.
- **"Why did/didn't this step run?"**: `system/dag_state.json`
  records each step's last input/output versions; a step re-runs when
  an input version moved (download steps always run — their input is a
  remote service).
- **Wedged doltlite file** (`commit conflict` after a stray writer):
  recovery recipes in [`docs/dev/doltlite.md`](dev/doltlite.md).
- **A config the runner rejects**: `datalib-dag --check
  <data_root>/config.toml` lists every problem with a line number;
  `PUT /api/config` (or the Setup tab) returns the same list in
  `diagnostics` and writes nothing. A data root still holding a
  pre-TOML `config.yaml` reads as unconfigured — run
  `datalib-migrate-config <data_root>` first.
- **A step that silently stopped running**: check `diagnostics` on
  `GET /api/config`, or `--check`. A config with one unusable entry
  still loads — that entry is dropped and everything else runs — so a
  source can leave the pipeline without anything failing. The Pipeline
  table shows such a row as *Not loaded* or *Can't run*, with the
  reason; `--check` prints it.
