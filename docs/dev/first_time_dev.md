# Datalib (Project Data Liberation ✊) — first-time dev guide

This guide is for people who want to **build and hack on** Datalib. If you
just want to *run* the released tools against your own data, start with the
[**first-time user guide**](../user/first_time_user.md) instead.

## Setup pre-reqs

```sh
# The one host tool Bazel can't provide for itself: `bazelisk`, the
# build driver (it also answers to `bazel`).
brew install bazelisk

# Only to run a script from `scripts/` or `tools/` by hand: `uv`, which runs
# it on the Python the repo pins. The build and the tests do not use it.
brew install uv

# That is all: Node and latchkey are Bazel inputs, so the build
# needs nothing in your home directory. The binaries shell out to
# latchkey through a staged `runtime/` tree (the dev launchers
# stage one; see "Re-run ingestion" below), never through a host Node.
# A sync's first search-index pass fetches the pinned embedding model,
# sha256-verified, into the shared cache (`~/.cache/qmd/models`).
```

### Linux iteration via devcontainer

If you're debugging a Linux-only build issue (glibc, GNU tools, the
hermetic LLVM toolchain Linux builds with), `.devcontainer/` ships the
Ubuntu image CI runs `bazel test //...` in. Open in VS Code via
"Reopen in Container", or from the CLI:

```sh
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . bazelisk build //datalib/backend:dist -c opt
```

Caches (bazel output base, disk cache, npm cache) live in named volumes
so rebuilds aren't cold.

### Claude Code cloud environments

A Claude Code session on the web runs in a VM built from Anthropic's
Ubuntu 24.04 image, which cannot be swapped for ours. What it lacks is
installed by [`scripts/cloud_env_setup.sh`](../../scripts/cloud_env_setup.sh):
bazelisk and the few packages the devcontainer adds. In the
environment's settings on claude.ai/code:

- **Setup script**: paste that file's text. The VM is snapshotted after
  it runs, and the snapshot is reused until the pasted text changes, so
  a change to the file reaches the environment only when it is pasted
  again.
- **Network access**: Limited, with "Allow package managers" on, and
  these added to the allowed domains: `bcr.bazel.build`,
  `releases.bazel.build`, `mirror.bazel.build`, `nodejs.org`,
  `huggingface.co`, `*.huggingface.co`, `*.hf.co`, and
  `imbue.buildbuddy.io` for the cache. GitHub, crates.io, PyPI, npm and
  `static.rust-lang.org` are allowed already. The list comes from the
  URLs in `MODULE.bazel` and its lockfile. A fetch that fails naming
  another host means that host belongs on the list.
- **Environment variables**: `BUILDBUDDY_API_KEY=<your key>`, for the
  contributor cache. At the start of every session the SessionStart hook
  in [`.claude/settings.json`](../../.claude/settings.json) runs
  [`scripts/cloud_session_start.sh`](../../scripts/cloud_session_start.sh),
  which writes `.bazelrc.user` from it the way CI's `prepare-bazel`
  does. Without the variable the hook does nothing, and the first build
  in a session compiles everything on the VM's 4 vCPUs.

**Keep an environment that holds a key to yourself.** Anyone who uses
an environment can read its variables. One you create is personal; the
"Who can use it" row is what shares it with the whole organization, so
leave it alone. Inside the VM the key is as exposed as on a laptop: any
build script can read `.bazelrc.user`
([`security_posture.md`](security_posture.md)).

The VM has about 4 vCPUs, 16 GB of memory and 30 GB of disk. The hook
turns bazel's local disk cache off, since BuildBuddy is the cache and
the disk is small.
`//datalib/ui:e2e_test` needs Playwright's browsers, which the script
does not install.

## What's in the repo

Two coupled projects that mirror personal data into a queryable local store:

- **`datalib/backend/`** — Rust workspace that downloads + ingests LLM
  chat exports and other sources (Claude, ChatGPT, Slack, GitHub, GitLab,
  Notion, and more — see the [README](../../README.md) table) into a
  doltlite DB, renders each document's markdown into a store, builds a
  search index, and serves the result over axum / Tauri.
- **`datalib/ui/`** — Vue 3 UI that searches and views the mirrored
  data, packaged as a Tauri desktop app and a Docker image.

Where each crate and directory is, and what belongs in it:
[`AGENTS.md`](../../AGENTS.md) §"Repo layout".

## Building & testing

### The one test command

```sh
bazelisk test //...
```

Before pushing, `bazelisk run //:precommit` runs the whole CI gate
(`AGENTS.md` § "Running tests").

**Always run tests through Bazel.** It's the source of truth for "do the
tests pass?", and the disk cache (`--disk_cache` in `.bazelrc`) is
content-addressed and shared across every checkout on your machine — two
clones of this repo at different paths get the same cache hits, and your
second invocation only re-executes what your changes actually touched.
Skipping Bazel skips that cache.

Runs:
- Rust unit tests (`//datalib/backend/{schema,core,etl,http}:*_unittests`)
- Cross-language deeplink fixture test (Rust loads the same JSON the Vitest
  suite loads, asserting both implementations agree)
- Playwright e2e suite (`//datalib/ui:e2e_test`) — not hermetic: under
  `bazel test` it runs from the `rules_js`-linked `node_modules` and a
  Bazel-managed Node, but the browsers come from Playwright's cache under
  `$HOME` (`env_inherit = HOME`), fetched on demand. That is why it is
  tagged `requires-network` + `no-sandbox` — see [`testing.md`](testing.md).

### Quickest first run (no data root needed)

`:dev_tng` is the best command to run first: it needs nothing but the repo.
It materializes a one-shot data root from the checked-in TNG fixtures
(`//tests/fixtures:ingested_tng`) into a tmpdir and points the backend at it,
so you can eyeball the grid without a real on-disk root or any credentials.
The root has the contacts app too, starting empty, so a chip can be linked
to a contact. Every source in it is render-only: browsing and search work,
and syncing a source fails on its missing raw store.

```sh
bazelisk run //datalib:dev_tng
```

`:dev_perseus` is the same shape, but bootstraps from the in-crate Perseus
tiny fixture (Thucydides 1.1, both languages) so you can exercise the
bilingual `edges` UI.

### Run the desktop app

The desktop app is the main way to use Datalib. To build it from this
checkout and launch it (macOS):

```sh
./run-app.sh                     # a folder picker chooses the library
./run-app.sh ~/Datalib/Default   # open this library directly
```

It Bazel-builds the bundled binaries, builds the Tauri shell in
`datalib/tauri/` and opens the `.app`. A rebuild with nothing changed
takes seconds. More in [`datalib/tauri/README.md`](../../datalib/tauri/README.md).

### Launch the dev UI against your own data

Full dev — backend (`datalib_http_bin`) **and** Vite (`pnpm dev`) at the
same time, browser opens at the Vite URL. The trailing path is the data root:

```sh
bazelisk run //datalib:dev -- <root>
```

Both Vite and the backend default to ephemeral ports (printed at startup);
Vite's `/api/*` proxy is wired to the chosen backend port, so multiple
concurrent runs (different agents, different worktrees) don't collide. Pin
specific ports with `DATALIB_PORT` (Vite) and `DATALIB_BIND`
(backend). Ctrl-C tears both down.

The data root is the positional arg to `bazelisk run //datalib:dev` (or
`:serve`), else `~/Datalib/Default`, the library the desktop app opens
first; `dev_library_root` in
`datalib/dev_lib.sh` is where both launchers decide it. It is the
*directory*, not the config file: `datalib-http` takes it as a required
positional and reads `<root>/config.toml` from inside it.

The backend starts even if the root is missing — `/api/health` reports
`root_exists: false` and the search grid shows zero rows. (`/api/health`
needs the API token like every other route; see below.)

For a backend-only launch (no Vite), use `bazelisk run //datalib:serve`.
Override the listen address with `DATALIB_BIND=127.0.0.1:<port>` (or set
`DATALIB_URL=...` to point the browser at a different URL than the one
being bound — useful behind a reverse proxy).

### The API token

Every backend route requires a per-process API token — Jupyter's scheme,
and for Jupyter's reason: loopback does not keep a *web page* out, and
`PUT /api/config` + `POST /api/requests` runs arbitrary `command:`
strings. See
[`datalib/backend/http/src/auth.rs`](../../datalib/backend/http/src/auth.rs)
for the design.

Both launchers handle it for you:

* `//datalib:serve` mints a token, exports `DATALIB_TOKEN` to the
  backend, and opens `<url>?token=…`. The browser trades that for an
  HttpOnly session cookie and is redirected to the clean URL.
* `//datalib:dev` mints one and gives it to *both* processes. The
  browser talks to Vite, not the backend, so it never gets a cookie —
  instead Vite's server-side `/api` proxy stamps
  `Authorization: Bearer …` on every forwarded request
  ([`vite.config.ts`](../../datalib/ui/vite.config.ts)). Starting Vite by
  hand means exporting the same `DATALIB_TOKEN` the backend has, or
  every `/api` call comes back 401.

For curl, scripts, and coding agents, the running server publishes its
token to `<root>/system/api-token` (mode 0600):

```sh
curl -H "Authorization: Bearer $(cat <root>/system/api-token)" \
  http://127.0.0.1:<port>/api/health
```

It changes on every restart, so read the file rather than caching the
value. `DATALIB_TOKEN=<value>` pins it. The `/agent/*.md` guides stay
readable without a token — they're what tells an agent how to get one.

### Re-run ingestion

Ingestion is a DAG of subprocess steps orchestrated by `datalib-pipeline`
(`//datalib/backend/dag:datalib_pipeline_bin`), which reads the data root's
`config.toml` (the `[[groups]]` + `[[steps]]` format) and runs each step
as a subprocess. Datalib's own steps write
`command = "datalib-step run <function> <type>"`
(`//datalib/backend/datalib_step:datalib_step`) — `ingest`
brings a source's data into its raw store, `render_markdown` renders
markdown + its store, and `unified_grid_and_search_index` loads every render store into
`<root>/unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite`. See
[`step_protocol.md`](step_protocol.md) for the step contract and
[`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md)
for the runner's rules.

To run one by hand, build `//datalib/backend:bin` — it stages every
shipped binary under its public dash-separated name in a single
directory, the same layout `scripts/install.sh` produces on a user's
machine:

```sh
bazelisk build //datalib/backend:bin
bazel-bin/datalib/backend/bin/datalib-pipeline sync ~/datalib
```

No `--binary-dir` is needed: `datalib-pipeline` puts its own directory at the
front of every step's `PATH`, so `datalib-step` is found beside it. Name
step ids after the root to run those steps and what is downstream of
them.

### One source's download, with no config

There is no per-provider download program: `datalib-step` is the one,
and it runs without the runner too. Give it the environment the runner
would, with the step's params as JSON, and the data root as its working
directory. For a Lightroom catalog:

```sh
bin=$PWD/bazel-bin/datalib/backend/bin
mkdir -p ~/lr-root && cd ~/lr-root
DATALIB_DAG_DATA_ROOT=$PWD DATALIB_DAG_STEP=lightroom/ingest \
DATALIB_DAG_PARAMS='{"catalog": {"path": "/Users/me/Pictures/Catalog.lrcat"}}' \
  $bin/datalib-step run ingest lightroom
```

The params are the step's `[steps.params]` table as JSON (each
provider's `INGEST.md` shows its table). The store lands where a sync
would put it, here `lightroom/ingest/entities.doltlite_db`, committed.
What a hand run gives up, and how to stop one without losing what it
fetched: [`step_protocol.md`](step_protocol.md) §"Running
`datalib-step` by hand". Use a root no app is serving; on one the app
serves, `datalib-pipeline sync <config> <step>` is the way.

A sync also spawns `latchkey`, which the binaries run from a
`runtime/` tree (Node plus latchkey's package tree, lockfile-pinned by
Bazel) found beside themselves or through `DATALIB_RUNTIME_DIR` — a
release install fetches it instead, see `runtime_fetch.md`. The
`bazel run //datalib:serve|dev|dev_tng` launchers stage one for you;
for a hand-run binary, stage it once and point at it:

```sh
scripts/stage_runtime.sh ~/.cache/datalib/staged-runtime
export DATALIB_RUNTIME_DIR=~/.cache/datalib/staged-runtime
```

Without a tree the spawn fails with a message naming the fixes. One of
them, `DATALIB_ALLOW_NPX=1`, runs the tool through `npx -y` from the
live registry instead — a dev-only escape hatch that warns once per
process per package, because the transitive packages are unpinned and
their install scripts run.

### Search index (default-on, incremental)

Two kinds of step build the search index
([`datalib/backend/search_index`](../../datalib/backend/search_index)).
`unified_grid_and_search_index`, after the grid, brings each searched source's documents in
the index in line with its render store: it writes each changed
document's words into `search_*` tables of the grid index's file,
`unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite`,
SQLite's FTS5 ranked by BM25, with no embedding model. Each source's
`embed` cuts each of its documents the keyword index holds into chunks,
when the document changed since it last did, and files them in the
inference cache,
`unified_grid_and_search_index/search_index/inference_cache.sqlite`, then
embeds every chunk there with no vector, in process
(`datalib/backend/embedder`, llama.cpp). It puts the pinned,
sha256-verified embedding model in place through
`datalib_embedding_models` only when it has something to cut or embed.
`unified_grid_and_search_index` also sweeps the vectors no chunk names,
and reports what each source holds. This is what a search matches words and meaning against (see
`datalib/backend/unified_grid_and_search_index/src/search_index.rs`).

Design notes:

- **Incremental**. A document is indexed, and cut into chunks, under its
  render's content hash, so a keyword pass and an embed each redo only
  documents whose render changed, and a vector is kept under the hash of
  exactly what the model was shown, so an embed embeds only chunks whose
  prompt is new. The chunks sit beside the vectors, so emptying the
  keyword index for a new shape cuts nothing again.
- **First run is slow**: embedding every chunk of a fresh root takes
  minutes on CPU. An embed is resumable, so stopping and re-running is
  safe, and turning one source's `embed` off leaves the rest running.
  Once the backlog drains, re-runs are no-ops.
- **Chunks are cut in the model's tokens**: 900 a chunk, so every prompt
  fits the 2,048 the model reads. An embed reads the tokenizer from the
  model file without its weights, and only when a document needs cutting;
  a chunk filed earlier is shown to the model from its span, with no
  tokenizer (`search_index/src/chunk.rs`).
- **Models cache**: the embedding model (~300 MB) is shared across data
  roots in `~/.cache/qmd/models` (`$XDG_CACHE_HOME/qmd/models` when that
  is set), unless a root has its own `unified_grid_and_search_index/models`.
  Override with `datalib-step --models-dir`; `datalib-step pull-models`
  fetches it ahead of time.

### Manual integration tests (live provider APIs)

Several provider crates ship a `*_live` target that hits the real
service API through `latchkey` (`git grep -n 'name = ".*_live"' --
'*.bazel'` lists them). `claude_live`, `chatgpt_live`, `github_live`,
`gitlab_live` and `notion_live` each download a small known fixture
(e.g. one conversation), then assert a curated stable view against
committed [insta](https://insta.rs) snapshots. The others
(`email:gmail_live`, `calendar_live`, `google_drive_live`,
`todoist_live`) check what they downloaded with no golden.
They are `bazel run` targets, not tests, so `bazel test //...` never
runs them. They need `latchkey` creds for the service and
`LATCHKEY_CURL` pointing at the router curl (which hands
Cloudflare-fronted hosts to the bundled `curl-impersonate` next to it —
see [`curl_impersonate.md`](curl_impersonate.md)):

```sh
bazel build //third-party/latchkey-curl-shims
export LATCHKEY_CURL="$(pwd)/bazel-bin/third-party/latchkey-curl-shims/latchkey-curl-router"
bazelisk run //datalib/backend/etl/providers/claude:claude_live
```

Each runs the `live` module of its package's ordinary test binary, with
your shell's environment; [`testing.md`](testing.md) § "The `live`
module" says why.

When upstream content changes, the test will fail with a diff; accept the
change with the sibling `.update` target (e.g. `bazel run
//datalib/backend/etl/providers/claude:claude_live.update` —
see [`testing.md`](testing.md) § "Updating insta
goldens").

### Changing a row schema

Row shapes are plain Rust structs — no codegen. To add or change a column,
edit the struct directly:

- render-schema tables (`grid_rows` / `edges` / `markdowns`) in
  `datalib/backend/schema/src/`,
- app-state tables (`feedback` / `disk_usage` / `disk_free` / `remote_media` and the
  run store's) in `datalib/backend/app_schema/src/`.

Give each field a `#[col(sql = "…")]` portable type;
`#[derive(PortableTable)]` (in `datalib/backend/etl/macros`) produces
the matching `CREATE TABLE` DDL (and `COLUMNS` / `TABLES` metadata) at
compile time. Columns computed at load time (e.g.
`grid_rows.created_at_utc`) are declared with
`#[derived(name = "…", sql = "…")]` on the field they trail.

What a shape change does to a store that already exists, and how to
migrate one, is in
[`datalib/backend/etl/README.md`](../../datalib/backend/etl/README.md)
§ "Schema self-healing" and § "The migration ladder"; a new `grid_rows`
column has its own checklist in [`grid_rows.md`](grid_rows.md).

## Version policy: 7-day burn-in

Toolchain and dependency versions are pinned to the newest release that is
**at least 7 days old at the time of the bump**. Hot releases are where
regressions hide; a week of community shake-out is cheap insurance. When
upgrading, check the upstream release date before pinning. If a useful
version exists but is too new, pin the previous patch and revisit next week.

The pins live in `MODULE.bazel`, `.bazelversion` and
`datalib/ui/package.json`. Other deps follow standard semver caret
ranges; `cargo update` and `pnpm update` are safe within those ranges
(`Cargo.lock` and `pnpm-lock.yaml` are committed). The backend's
`Cargo.lock` is moved weekly by `.github/workflows/cargo-update.yml`,
which applies the same 7-day burn-in.

## What Bazel does not own

- The Tauri shell and its bundler (`pnpm tauri` / `cargo tauri`); Bazel
  compiles only the shell's files that are kept free of `tauri`, to run
  their tests ([`datalib/tauri/README.md`](../../datalib/tauri/README.md)
  names them).
- The Docker image, built from the release tarball
  ([`docker.md`](docker.md)).
