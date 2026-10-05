# Project Data Liberation ✊ - First-time dev guide

This guide is for people who want to **build and hack on** datalib. If you
just want to *run* the released tools against your own data, start with the
[**first-time user guide**](../user/first_time_user.md) instead.

## Setup pre-reqs

```sh
# Host tools Bazel can't provide for itself. `cmake` is required by the
# `protobuf-src` crate's build script; `bazelisk` is the build driver
# (it also answers to `bazel`).
brew install bazelisk cmake

# That is all: Node, qmd and latchkey are Bazel inputs, so the build
# needs nothing in your home directory. The binaries shell out to
# latchkey and qmd through a staged `runtime/` tree (the dev launchers
# stage one; see "Re-run ingestion" below), never through a host Node.
# A sync's first `embed` step fetches the pinned embedding model,
# sha256-verified, into qmd's cache (`~/.cache/qmd/models`).
```

### Linux iteration via devcontainer

If you're debugging a Linux-only build issue (e.g. one the macOS host masks
because clang is more permissive than gcc), `.devcontainer/` ships the
Ubuntu image CI runs `bazel test //...` in. Open in VS Code via
"Reopen in Container", or from the CLI:

```sh
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . bazelisk build //datalib/backend:dist -c opt
```

Caches (bazel output base, disk cache, npm cache) live in named volumes
so rebuilds aren't cold.

## What's in the repo

Two coupled projects that mirror personal data into a queryable local store:

- **`datalib/backend/`** — Rust workspace that downloads + ingests LLM
  chat exports and other sources (Claude, ChatGPT, Slack, GitHub, GitLab,
  Notion, and more — see the [README](../../README.md) table) into a
  doltlite DB, renders one Markdown file per conversation, builds a qmd
  search index, and serves the result over axum / Tauri.
- **`datalib/ui/`** — Vue 3 UI that searches and views the mirrored
  data, packaged as a Tauri desktop app and a Docker image.

```
.
├── MODULE.bazel              Bzlmod root
├── BUILD.bazel               :all_tests aggregator, :precommit, :lint
├── docs/                     dev/ architecture notes · user/ guides + config_examples
├── tests/fixtures/           the TNG fixture pipeline (the ingested_tng genrule)
└── datalib/
    ├── backend/              Cargo workspace
    │   ├── schema/           render schema: grid_rows / edges / markdowns structs
    │   ├── app_schema/       app-state schema: feedback / disk usage / remote media / runs
    │   ├── core/             the app stores + deeplink grammar
    │   ├── etl/              shared ingest machinery; etl/render/ the render framework
    │   ├── etl/providers/*/  per-provider ingest / render / config crates
    │   ├── qmd_indexer/      the qmd index's operations, over qmd's SDK
    │   ├── dag/              datalib-dag DAG runner (sync orchestrator)
    │   ├── datalib_step/     datalib-step built-in step commands
    │   └── http/             axum binary
    ├── ui/                   Vue 3 + Vite + Pinia + Vue Router + Vitest
    └── tauri/                Tauri shell (out of Bazel)
```

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
so you can eyeball the grid without a real on-disk root or any credentials:

```sh
bazelisk run //datalib:dev_tng
```

`:dev_perseus` is the same shape, but bootstraps from the in-crate Perseus
tiny fixture (Thucydides 1.1, both languages) so you can exercise the
bilingual `edges` UI.

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
`:serve`), else `~/Documents/Datalib/Default`, the library the desktop
app opens first (not the `Datalib` folder itself, which the app reads
as a library to move into `Default`); `dev_library_root` in
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

Ingestion is a DAG of subprocess steps orchestrated by `datalib-dag`
(`//datalib/backend/dag:datalib_dag_bin`), which reads the data root's
`config.toml` (the `[[groups]]` + `[[steps]]` format) and runs each step
as a subprocess. A step with no `command` is built in: it runs
`datalib-step` (`//datalib/backend/datalib_step:datalib_step`), which
reads its function and its group's type from the environment — `ingest`
brings a source's data into its raw store, `render_markdown` renders
markdown + its store, and `grid_index` loads every render store into
`<root>/unified_index/grid_index/db.doltlite_db`. Several provider
crates also build a standalone `<p>_ingest` binary. See
[`step_protocol.md`](step_protocol.md) for the step contract and
[`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md)
for the runner's rules.

To run one by hand, build `//datalib/backend:bin` — it stages every
shipped binary under its public dash-separated name in a single
directory, the same layout `scripts/install.sh` produces on a user's
machine:

```sh
bazelisk build //datalib/backend:bin
bazel-bin/datalib/backend/bin/datalib-dag ~/datalib/config.toml
```

No `--binary-dir` is needed: `datalib-dag` puts its own directory at the
front of every step's `PATH`, so `datalib-step` is found beside it. Add
`--sync <step-id>[,<step-id>…]` to run those steps and what is
downstream of them.

A sync also spawns `latchkey` and `qmd`, which the binaries run from a
`runtime/` tree (Node plus both package trees, lockfile-pinned by
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

### QMD search index (default-on, incremental)

Three kinds of step build the qmd search index. Each source's
`keyword_index` registers the source's qmd collection and brings its
keyword index in line with its rendered tree; its `embed` embeds what
that collection is missing, after putting the pinned, sha256-verified
embedding model in place through `datalib_qmd_models`, so qmd never
fetches a model itself. `unified_index/qmd_aggregator` reads every
source's pair, so it runs after them: it retires the collection of any
source it does not name and reports what each holds. All three drive qmd's
SDK from the staged runtime tree (above) through
`datalib/backend/qmd_indexer/`, with
`XDG_CACHE_HOME=<root>/unified_index/qmd_aggregator`, so the one index lands
at `<root>/unified_index/qmd_aggregator/qmd/index.sqlite` (each collection
scans `<root>` with the `<group>/render_markdown/**/*.md` mask),
alongside the per-source `<group>/render_markdown/` trees and
`unified_index/grid_index/db.doltlite_db`. This is what the search bar's
hybrid / vector queries hit (see `datalib/backend/unified_index/src/qmd/`).

Design notes:

- **Incremental**. qmd's `documents` table is unique on `(collection,
  path)` and records each file's content hash, and `content_vectors` is
  keyed by that hash, so a re-run only
  rechunks files whose bytes changed and only re-embeds content hashes with
  no existing vector row. Deletes are detected (rows marked `active=0`) and
  orphaned content is cleaned.
- **First run is slow** — embedding all chunks for a fresh `<root>` takes
  several minutes on CPU (a one-time cost, roughly 5–10 minutes per thousand
  unembedded chunks). Each `embed` step reports its progress in documents,
  with the chunks and bytes behind them in its message; an embed is resumable, so stopping and re-running is safe, and turning one
  source's `embed` off leaves the rest running. Once the backlog drains,
  re-runs are no-ops (a couple of seconds).
- **Models cache**: qmd's embedding model (~300 MB) is shared across data
  roots via a symlink at `<root>/unified_index/qmd_aggregator/qmd/models ->
  ~/.cache/qmd/models` (qmd's own default, `$XDG_CACHE_HOME/qmd/models`
  when that is set, so a standalone `qmd` run shares the same cache).
  Override with `datalib-step --models-dir`; `datalib-step pull-models`
  fetches them ahead of time.

### Manual integration tests (live provider APIs)

Several provider crates ship a `*_live` snapshot test that hits the real
service API through `latchkey`:
`//datalib/backend/etl/providers/claude:claude_live`, plus the
sibling `chatgpt_live`, `github_live`, `gitlab_live` and `notion_live`
targets. Each downloads a small known fixture (e.g. one conversation),
then asserts a curated stable view against committed
[insta](https://insta.rs) snapshots. `email:gmail_live` is the one that
does not: it downloads and checks the store it wrote, with no golden.
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
- app-state tables (`feedback` / `disk_usage` / `remote_media` and the
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
(`Cargo.lock` and `pnpm-lock.yaml` are committed).

## What Bazel does not own

- The Tauri shell and its bundler (`pnpm tauri` / `cargo tauri`); only
  `datalib/tauri/src/launcher.rs` is compiled under Bazel, to run its tests.
- The Docker image, built from the release tarball
  ([`docker.md`](docker.md)).
