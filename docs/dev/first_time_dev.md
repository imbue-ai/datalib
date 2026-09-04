# Project Data Liberation ✊ - First-time dev guide

This guide is for people who want to **build and hack on** datalib. If you
just want to *run* the released tools against your own data, start with the
[**first-time user guide**](../user/first_time_user.md) instead.

## Setup pre-reqs

```sh
# 1. Host tools Bazel can't provide for itself. `cmake` is required by the
#    `boring-sys2` crate (BoringSSL bindings) at build time; `bazel` is the
#    build driver.
brew install bazel cmake

# 2. (Nothing to do here any more.) This step used to be
#    `mkdir -p ~/.cache/qmd/models`, because `.bazelrc` bind-mounted that
#    directory into every sandboxed action and a missing one failed the
#    build. qmd's three GGUF models are now pinned in MODULE.bazel
#    (`@qmd_model_*`) and reach both the fixture's index genrule and the
#    materialized demo root as ordinary bazel inputs, so neither
#    `bazel test //...` nor `bazelisk run //datalib:dev_tng` needs
#    anything in your home directory. The app you build still downloads
#    models there at sync time, as a user's would.

# 3. (Nothing to check here any more.) The build used to need host
#    `npx` on Bazel's pinned PATH, because `qmd-indexer` shelled out to
#    `npx -y @tobilu/qmd@<v>`. Node and the qmd package tree are now
#    Bazel inputs (`//third-party/qmd/runtime`), staged into a
#    `DATALIB_RUNTIME_DIR` layout by the fixture genrule — verified by
#    building it with neither `node` nor `npx` on PATH. A host Node is
#    still needed to RUN the shipped CLI (it shells out to latchkey and
#    qmd at sync time), just not to build the repo.
```

### Linux iteration via devcontainer

If you're debugging a Linux-only build issue (e.g. one the macOS host masks
because clang is more permissive than gcc), `.devcontainer/` ships an Ubuntu
24.04 container that mirrors GHA's release runner. Open in VS Code via
"Reopen in Container", or from the CLI:

```sh
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . bazelisk build //datalib/backend:dist -c opt
```

Caches (bazel output base, disk cache, qmd model cache, npm cache) live in
named volumes so rebuilds aren't cold.

## What's in the repo

Two coupled projects that mirror personal data into a queryable local store:

- **`datalib/backend/`** — Rust workspace that downloads + ingests LLM
  chat exports and other sources (Anthropic, OpenAI, Slack, GitHub, GitLab,
  Notion, and more — see the [README](../../README.md) table) into a
  doltlite DB, renders one Markdown file per conversation, builds a qmd
  search index, and serves the result over axum / Tauri.
- **`datalib/ui/`** — Vue 3 UI that searches and views the mirrored
  data, packaged as a Tauri desktop app and an Open Host container.

Backend row shapes are defined as hand-written Rust structs in two crates —
**`datalib/backend/schema`** (the *render schema*: `grid_rows` / `edges`
/ `markdowns`) and **`datalib/backend/app_schema`** (app-state tables:
`feedback` / `sync_jobs`). Each row struct derives its portable
`CREATE TABLE` DDL via `#[derive(PortableTable)]` (in
`datalib/backend/etl/macros`). The struct is the single source of truth —
there is no codegen step.

```
.
├── MODULE.bazel              Bzlmod root (rules_python + rules_rust)
├── BUILD.bazel               :all_tests aggregator
├── docs/                     dev/ architecture notes · user/ guides + config_examples
├── tests/fixtures/           checked-in fixture JSON + ingested_tng genrule
└── datalib/
    ├── backend/              Cargo workspace
    │   ├── schema/           render schema: grid_rows / edges / markdowns structs
    │   ├── app_schema/       app-state schema: feedback / sync_jobs structs
    │   ├── core/             query engine + deeplink grammar
    │   ├── etl/              shared render/load framework
    │   ├── etl/providers/*/  per-provider download/render crates
    │   ├── qmd_indexer/      qmd search index binary
    │   ├── dag/              datalib-dag DAG runner (sync orchestrator)
    │   ├── datalib_step/     datalib-step built-in step commands
    │   └── http/             axum binary
    ├── ui/                   Vue 3 + Vite + Pinia + Vue Router + Vitest
    ├── tauri/                Tauri shell (out of Bazel)
    └── openhost/             Dockerfile + openhost.toml stubs
```

## Building & testing

### One command for CI parity

```sh
bazel test //...
```

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
- Playwright e2e suite (`//datalib/ui:e2e_test`) — non-hermetic by
  design: the test shells out to host `pnpm` / `node` / Playwright browser
  cache rather than wiring `rules_js`.

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
bazelisk run //datalib:dev -- ~/datalib.thad
```

Both Vite and the backend default to ephemeral ports (printed at startup);
Vite's `/api/*` proxy is wired to the chosen backend port, so multiple
concurrent runs (different agents, different worktrees) don't collide. Pin
specific ports with `DATALIB_PORT` (Vite) and `DATALIB_BIND`
(backend). Ctrl-C tears both down.

Data root resolution (the rendered Markdown feeds the search index, but
`unified_index/grid/db.doltlite_db` remains the source of truth):

1. positional arg to `bazelisk run //datalib:dev` (or `:serve`)
2. `~/Documents/datalib`

The root is the *directory*, not the config file: `datalib-http` takes
it as a required positional and reads `<root>/config.toml` from inside
it. That is the only config format it reads — a root still holding a
pre-TOML `config.yaml` needs `datalib-migrate-config <root>` first.

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
`PUT /api/config` + `POST /api/sync/jobs` runs arbitrary `command:`
strings (issue #138). See
[`datalib/backend/http/src/auth.rs`](/datalib/backend/http/src/auth.rs)
for the design.

Both launchers handle it for you:

* `//datalib:serve` mints a token, exports `DATALIB_TOKEN` to the
  backend, and opens `<url>?token=…`. The browser trades that for an
  HttpOnly session cookie and is redirected to the clean URL.
* `//datalib:dev` mints one and gives it to *both* processes. The
  browser talks to Vite, not the backend, so it never gets a cookie —
  instead Vite's server-side `/api` proxy stamps
  `Authorization: Bearer …` on every forwarded request
  ([`vite.config.ts`](/datalib/ui/vite.config.ts)). Starting Vite by
  hand means exporting the same `DATALIB_TOKEN` the backend has, or
  every `/api` call comes back 401.

For curl, scripts, and coding agents, the running server publishes its
token to `<root>/system/api-token` (mode 0600):

```sh
curl -H "Authorization: Bearer $(cat ~/datalib.thad/system/api-token)" \
  http://127.0.0.1:<port>/api/health
```

It changes on every restart, so read the file rather than caching the
value. `DATALIB_TOKEN=<value>` pins it. The `/agent/*.md` guides stay
readable without a token — they're what tells an agent how to get one.

### Re-run ingestion

Ingestion is a DAG of subprocess steps orchestrated by
`//datalib/backend/dag:datalib_dag_bin`, which reads the data root's
`config.toml` (the `[[steps]]` format) and runs each step's `command`
as a subprocess. The built-in steps live in the `datalib-step` binary
(`//datalib/backend/datalib_step:datalib_step`): `download
<source_type>` fetches a provider's raw dir (each provider crate under
`datalib/backend/etl/providers/` also exposes a standalone
`*_download` binary), `render <source_type>` renders markdown + sidecars,
and `grid_index` loads them into
`<root>/unified_index/grid/db.doltlite_db`. See
[`step_protocol.md`](step_protocol.md) for the step contract and
[`pipeline_dag_architecture.md`](pipeline_dag_architecture.md) for the
DAG design.

To run one by hand, build `//datalib/backend:bin` — it stages every
shipped binary under its public dash-separated name in a single
directory, the same layout `scripts/install.sh` produces on a user's
machine:

```sh
bazelisk build //datalib/backend:bin
bazel-bin/datalib/backend/bin/datalib-dag ~/datalib/config.toml
```

No `--binary-dir` is needed: `datalib-dag` resolves each step's
`command` against PATH with its own directory as the fallback, so a
bare `datalib-step` in the config finds the sibling binary. Add
`--sync <step-id>[,<step-id>…]` to run a subset of the graph.

### QMD search index (default-on, incremental)

`datalib-step qmd_index` rebuilds the qmd search index over `<root>`
after the markdown tree is rendered + loaded. The indexer
(`datalib/backend/qmd_indexer/`) shells out to the qmd CLI — the
app-bundled runtime when one is staged (the Tauri bundle and the Bazel
fixture genrule both stage one), else `npx -y @tobilu/qmd@<version>` —
with `XDG_CACHE_HOME=<root>/system`, so the index lands at `<root>/unified_index/qmd/index.sqlite`
(the scan root stays `<root>` over the `*/rendered_md/**/*.md` mask), alongside the per-stanza
`<name>/rendered_md/` trees and `unified_index/grid/db.doltlite_db`. This is what the search bar's hybrid / vector
queries hit (see `datalib/backend/unified_index/src/qmd/`).

Design notes:

- **Incremental**. qmd's `documents` table keys on `(collection, path,
  content_hash)`, and `content_vectors` is keyed by hash, so a re-run only
  rechunks files whose bytes changed and only re-embeds content hashes with
  no existing vector row. Deletes are detected (rows marked `active=0`) and
  orphaned content is cleaned.
- **First run is slow** — embedding all chunks for a fresh `<root>` takes
  several minutes on CPU (a one-time cost, roughly 5–10 minutes per thousand
  unembedded chunks). qmd streams a live progress bar to stderr; `qmd embed`
  is resumable, so Ctrl-C and re-run is safe. Once the backlog drains,
  re-runs are no-ops (a couple of seconds).
- **Models cache**: qmd's embedding model (~300 MB) is shared across data
  roots via a symlink at `<root>/qmd/models -> ~/.cache/qmd/models` (qmd's
  own default, so a standalone `qmd` run shares the same cache). Override
  with `models_dir=` if you call the indexer directly.

### Manual integration tests (live provider APIs)

Several provider crates ship a `*_live` snapshot test that hits the real
service API through `latchkey`:
`//datalib/backend/etl/providers/anthropic:anthropic_live`, plus the
sibling `chatgpt_live`, `github_live`, `gitlab_live`, `notion_live`, and
`email:jmap_live` targets. Each downloads a small known fixture (e.g. one
conversation), then asserts a curated stable view against committed
[insta](https://insta.rs) snapshots. All are tagged `manual` + `external`
+ `no-sandbox`, so they are excluded from `bazel test //...`; they need
`latchkey` creds for the service and `LATCHKEY_CURL` pointing at the curl
shim:

```sh
bazel build //datalib/backend/etl:latchkey_curl_impersonate
export LATCHKEY_CURL="$(pwd)/bazel-bin/datalib/backend/etl/latchkey_curl_impersonate"
bazelisk test //datalib/backend/etl/providers/anthropic:anthropic_live \
    --test_arg=--ignored --test_env=PATH --test_env=HOME --test_env=USER \
    --test_env=LATCHKEY_CURL
```

When upstream content changes, the test will fail with a diff; accept the
change with the sibling `.update` target (e.g. `bazel run
//datalib/backend/etl/providers/anthropic:anthropic_live.update` —
see [`/AGENTS.md`](/AGENTS.md) § "Updating insta snapshots").

### Changing a row schema

Row shapes are plain Rust structs — no codegen. To add or change a column,
edit the struct directly:

- render-schema tables (`grid_rows` / `edges` / `markdowns`) in
  `datalib/backend/schema/src/`,
- app-state tables (`feedback` / `sync_jobs`) in
  `datalib/backend/app_schema/src/`.

Give each field a `#[col(sql = "…")]` portable type;
`#[derive(PortableTable)]` produces the matching `CREATE TABLE` DDL (and
`COLUMNS` / `TABLES` metadata) at compile time. Columns computed at load time
(e.g. `grid_rows.when_ts_utc`) are declared with
`#[derived(name = "…", sql = "…")]` on the field they trail.

## Version policy: 7-day burn-in

Toolchain and dependency versions are pinned to the newest release that is
**at least 7 days old at the time of the bump**. Hot releases are where
regressions hide; a week of community shake-out is cheap insurance. When
upgrading, check the upstream release date before pinning. If a useful
version exists but is too new, pin the previous patch and revisit next week.

`MODULE.bazel`, `.bazelversion`, and `datalib/ui/package.json` are the
source of truth for the current pins; as of this writing:

| Component       | Version        |
|-----------------|----------------|
| Bazel           | 9.1.0          |
| rules_python    | 2.0.0          |
| rules_rust      | 0.70.0         |
| Rust toolchain  | 1.95.0         |
| Vite            | 7.3.3 (exact)  |
| Vitest          | 4.1.5 (exact)  |
| vue-tsc         | 3.2.7 (exact)  |

Other deps follow standard semver caret ranges; `cargo update` and
`pnpm update` are safe within those ranges (Cargo.lock and pnpm-lock are
committed).

## What Bazel does not own (by design, v0)

- Tauri bundler (`pnpm tauri` / `cargo tauri`).
- OpenHost docker build (run `docker build` against Bazel outputs).
- The Vite build / Vitest under Bazel — currently driven by `pnpm`. A future
  `rules_js` + `rules_ts` integration is on the table once the surface area
  stabilises.
