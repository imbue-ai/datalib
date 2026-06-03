# mixed-up-files

> Codenames in this project (`frankweiler`, etc.) are inspired by
> [_From the Mixed-Up Files of Mrs. Basil E. Frankweiler_](https://en.wikipedia.org/wiki/From_the_Mixed-Up_Files_of_Mrs._Basil_E._Frankweiler).

## First-time setup

```sh
# 1. Install host tools Bazel can't provide for itself. `cmake` is
#    required by the `boring-sys2` crate (BoringSSL bindings) at
#    build time; `bazel` is the build driver.
brew install bazel cmake

# 2. Create the shared qmd model cache directory. `.bazelrc`
#    bind-mounts this into every sandboxed action via
#    `--sandbox_add_mount_pair=$(HOME)/.cache/qmd/models` so the
#    qmd-indexer genrule doesn't re-download ~2 GB of GGUF models
#    on every build — Bazel can read the dir, but won't create it.
#    Path matches qmd's own default, so a standalone `qmd` populates
#    the same cache.
mkdir -p ~/.cache/qmd/models

# 3. Verify Bazel can resolve `npx` on the pinned PATH. Bazel
#    actions inherit a fixed PATH from `.bazelrc`
#    (`/opt/homebrew/bin:/usr/bin:/bin`) instead of your interactive
#    shell's PATH — host `direnv` / `nvm` / shell-init pnpm aren't
#    in scope. `qmd-indexer` shells out to `npx`, so it has to be
#    findable here. Empty output = trouble; expect
#    `/opt/homebrew/bin/npx` (Homebrew Node).
PATH=/opt/homebrew/bin:/usr/bin:/bin command -v npx
```

### Linux iteration via devcontainer

If you're debugging a Linux-only build issue (e.g. one the macOS host
masks because clang is more permissive than gcc), `.devcontainer/`
ships an Ubuntu 24.04 container that mirrors GHA's release runner.
Open in VS Code via "Reopen in Container", or from the CLI:

```sh
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . bazelisk build //frankweiler/backend:dist -c opt
```

Caches (bazel output base, disk cache, qmd model cache, npm cache)
live in named volumes so rebuilds aren't cold.

Two coupled projects that mirror personal data into a queryable local store:

- **`frankweiler/backend/`** — Rust workspace that downloads + ingests
  LLM chat exports and other sources (Anthropic, OpenAI, Slack, GitHub,
  GitLab, Notion) into a doltlite DB, renders one QMD per conversation,
  builds a qmd search index, and serves the result over axum / Tauri.
- **`frankweiler/ui/`** — Vue 3 UI that searches and views the mirrored
  data, packaged as a Tauri desktop app and an Open Host container.

Both projects share row shapes through **`schemas/`**, the single
source-of-truth that emits Rust / Python / TypeScript types from one JSON
Schema.

## Slug + UUID identifiers (Notion-style)

Wherever a row has both a stable UUID and a human-readable name —
conversations, accounts, projects, Notion pages — the canonical reference
form is `slug-uuid` (e.g.
`picard-jean-luc-00000001-1701-4d00-8000-000000000001`).

- **UUID is load-bearing.** Filters, deeplinks, and equality comparisons
  use only the trailing UUID. Renames don't break old links.
- **Slug rides along.** It's a non-load-bearing prefix that makes tokens
  and URLs self-describing when read by a human, the way Notion's
  `My-Page-Title-abc123…` URLs do. Backend strips it on the way in
  (`extract_uuid_suffix` in `frankweiler/backend/core/src/query.rs`).
- **Right-click "filter by this cell"** assembles the form automatically:
  it has both the row's UUID (for the column the click landed on) and the
  human display label (from accounts.json, conversation name, etc.).

## Repo layout

```
.
├── MODULE.bazel              Bzlmod root (rules_python + rules_rust)
├── BUILD.bazel               :all_tests aggregator
├── schemas/                  cross-language source of truth
│   ├── grid_rows.schema.json union row shape backing the grid (see docs/grid_rows.md)
│   ├── codegen.py            JSON Schema → Rust/TS types + DDL
│   └── BUILD.bazel           genrules per language
├── docs/                     architecture notes
│   └── grid_rows.md          how the grid_rows union table works
├── tests/fixtures/           checked-in fixture JSON + ingested_tng genrule
└── frankweiler/
    ├── backend/              Cargo workspace
    │   ├── schema/           re-exports //schemas:*_rs types
    │   ├── core/             query engine + deeplink grammar
    │   ├── etl/              shared Translate/Load framework
    │   ├── etl/providers/*/  per-provider Extract/Translate crates
    │   ├── qmd_indexer/      qmd search index binary
    │   ├── sync/             incremental ETL orchestrator (drives genrule)
    │   ├── http/             axum binary
    │   └── tauri-backend/    Tauri command surface
    ├── ui/                   Vue 3 + Vite + Pinia + Vue Router + Vitest
    ├── tauri/                Tauri shell (out of Bazel)
    └── openhost/             Dockerfile + openhost.toml stubs
```

## Dependency graph

```
                       schemas/
                          │
                ┌─────────┴─────────┐
                ▼                   ▼
       frankweiler/backend     frankweiler/ui
       (Rust ETL + axum)        (TS types)
                │
                ▼
        frankweiler/backend/core ──► doltlite + qmd
                │             │
                ▼             ▼
        backend/http   backend/tauri-backend
                │             │
                ▼             ▼
            openhost/     tauri/  ◄── ui/
```

## Building & testing

### One command for CI parity

```sh
bazelisk test //...
```

**Always run tests through Bazel.** It's the source of truth for "do the
tests pass?", and the disk cache (`--disk_cache` in `.bazelrc`) is
content-addressed and shared across every checkout on your machine —
two clones of this repo at different paths get the same cache hits,
and your second invocation only re-executes what your changes actually
touched. Skipping Bazel skips that cache.

Runs:
- Rust unit tests (`//frankweiler/backend/{schema,core,etl,http,tauri-backend}:*_unittests`)
- Cross-language deeplink fixture test (Rust loads the same JSON the Vitest
  suite loads, asserting both implementations agree)
- Playwright e2e suite (`//frankweiler/ui:e2e_test`) — non-hermetic by
  design: the test shells out to host `pnpm` / `node` / Playwright
  browser cache rather than wiring `rules_js`. The contract is "`bazel
  test //...` actually exercises the e2e suite", not full Bazel
  ownership of the JS toolchain.

### Launch the dev UI

```sh
bazelisk run //frankweiler:dev -- ~/mixed_up_files.thad
```

The trailing path is the data root (see resolution order below). It can
be omitted if `$FRANKWEILER_ROOT` or `~/.config/frankweiler/config.yaml`
already points where you want.

Builds and runs `frankweiler_http_bin` (Rust) **and** Vite (`pnpm dev`) at
the same time, and opens your browser at the Vite URL
(`http://127.0.0.1:5173/`). Vite proxies `/api/*` to the backend on
`127.0.0.1:8731`. Ctrl-C tears both down.

Data root resolution (the QMDs feed the search index —
`backend_index.doltlite_db` remains the source of truth):

1. positional arg to `bazelisk run //frankweiler:dev` (or `:serve`)
2. `$FRANKWEILER_ROOT`
3. `root:` from `~/.config/frankweiler/config.yaml` (or `$FRANKWEILER_CONFIG`)
4. `~/Documents/mixed-up-files`

The backend starts even if the root is missing — `/api/health` reports
`root_exists: false` and the search grid shows zero rows.

For a backend-only launch (no Vite), use `bazelisk run //frankweiler:serve`,
which opens the browser at `/api/health`. Override the URL with
`FRANKWEILER_URL=...`.

### Re-run ingestion

Each provider crate under `frankweiler/backend/etl/providers/` exposes
its own `*_translate` (and where applicable `*_download`) binary; the
shared Load step is `//frankweiler/backend/etl:grid_rows_load`. The
ETL orchestrator at `//frankweiler/backend/sync` shows the
end-to-end wiring: parse each provider's `raw_api/` dir, render markdown
+ sidecars, then load them into `<root>/backend_index.doltlite_db`.

### QMD search index (default-on, incremental)

`grid_rows_load --qmd-index` rebuilds the qmd search index over `<root>`
after the markdown tree is rendered + loaded. It lives at
`<root>/.frankweiler/qmd/index.sqlite` and is what the search bar's
hybrid / vector queries hit (see `frankweiler/backend/core/src/qmd/`).

Design notes:

- **One indexer**: `frankweiler/backend/qmd_indexer/` shells out to
  `npx -y @tobilu/qmd@<version>` with
  `XDG_CACHE_HOME=<root>/.frankweiler` so the index lands at
  `<root>/.frankweiler/qmd/index.sqlite`. Used both by `grid_rows_load`
  and by the Bazel fixture genrule.
- **Incremental**. qmd's `documents` table keys on `(collection, path,
  content_hash)`, and `content_vectors` is keyed by hash, so a re-run
  only rechunks files whose bytes changed and only re-embeds content
  hashes with no existing vector row. Deletes are detected (rows
  marked `active=0`) and orphaned content is cleaned.
- **First run is slow** — embedding all chunks for a fresh `<root>`
  takes several minutes on CPU (one-time cost). qmd streams a live
  progress bar (ETA + chunks/s) to stderr for both `update` and
  `embed`; subprocess inherits stderr, so you see it in the ingest
  terminal. `qmd embed` is resumable: if it gets interrupted, the
  next run picks up where it left off (it skips content hashes that
  already have vectors), so paying the cost in chunks is fine.

  After a no-op render + dolt_commit, you'll see something like:

  ```
  [1/1] mirror (**/*.qmd)
  Collection: /Users/thad/mixed_up_files.thad (**/*.qmd)
  Indexing: 7294/7294 ETA: 0s
  Indexed: 0 new, 0 updated, 7294 unchanged, 0 removed

  ✓ All collections updated.

  Run 'qmd embed' to update embeddings (3282 unique hashes need vectors)
  2026-05-11 15:26:08,654 INFO ingest.qmd_index: qmd-indexer: $ npx -y @tobilu/qmd@2.1.0 embed
  Model: hf:ggml-org/embeddinggemma-300M-GGUF/embeddinggemma-300M-Q8_0.gguf

  ██████████████████░░░░░░░░░░░░  59% 4540/4587 14.9 KB/s ETA 4m 3s
  ```

  Embedding ~15 KB/s on CPU works out to roughly 5–10 minutes per
  thousand unembedded chunks. Once the backlog is drained, re-runs
  are no-ops (a couple of seconds).
- **Models cache**: qmd's embedding model (~300 MB) is shared across
  data roots via a symlink at `<root>/.frankweiler/qmd/models ->
  ~/.cache/qmd/models` (qmd's own default, so a standalone `qmd` run
  shares the same cache). Override with `models_dir=` if you call
  `build_qmd_index` directly.

### Manual integration tests (live Slack)

Two tests exercise the full Slack pipeline against the real Slack API
(`#thad-testing-channel`). Both are excluded from `bazelisk test //...`
and require `latchkey` on PATH with creds set for the `slack` service.

- **Rust downloader snapshot test** (`frankweiler/backend/etl/providers/slack/tests/slack_live.rs`):
  downloads the test channel via the Rust port of the Slack downloader,
  then asserts each per-entity `events.jsonl` against committed
  [insta](https://insta.rs) snapshots under
  `frankweiler/backend/etl/providers/slack/tests/snapshots/`. Volatile fields
  (signed URLs, timestamps) are redacted; channel/user records are
  trimmed to those relevant to the test channel so the rest of the
  workspace doesn't churn the snapshot.

  Tagged `manual` + `no-sandbox` because it shells out to host
  `latchkey`:

  ```sh
  bazelisk test //frankweiler/backend/etl/providers/slack:slack_live \
      --test_arg=--ignored --test_env=PATH --test_env=HOME --test_env=USER
  ```

  After posting new messages or attachments in the channel, the test
  will fail with a diff; accept the change with `cargo insta review`.

### Regenerating the cross-language types

The generated Rust file at
`frankweiler/backend/schema/src/generated/grid_rows.rs` is checked in.
To regenerate after editing `schemas/grid_rows.schema.json`:

```sh
bazelisk build //schemas:grid_rows_all
cp bazel-bin/schemas/grid_rows.rs   frankweiler/backend/schema/src/generated/
```

## Version policy: 7-day burn-in

Toolchain and dependency versions are pinned to the newest release that is
**at least 7 days old at the time of the bump**. Hot releases are where
regressions hide; a week of community shake-out is a cheap insurance policy.

When upgrading, check the upstream release date before pinning. If a useful
version exists but is too new, pin the previous patch and revisit next week.

Current pins (set 2026-05-07):

| Component       | Version        | Released   |
|-----------------|----------------|------------|
| Bazel           | 9.1.0          | 2026-04-20 |
| rules_python    | 2.0.0          | 2026-04-28 |
| rules_rust      | 0.70.0         | 2026-04-22 |
| Rust toolchain  | 1.95.0         | 2026-04-16 |
| Vite            | 8.0.10 (exact) | 2026-04-23 |
| Vitest          | 4.1.5 (exact)  | 2026-04-21 |
| vue-tsc         | 3.2.7 (exact)  | 2026-04-19 |

Other deps follow standard semver caret ranges; `cargo update` and
`pnpm update` are safe within those ranges (Cargo.lock and pnpm-lock are
committed).

## What Bazel does not own (by design, v0)

- Tauri bundler (`pnpm tauri` / `cargo tauri`).
- OpenHost docker build (run `docker build` against Bazel outputs).
- The Vite build / Vitest under Bazel — currently driven by `pnpm`. A future
  `rules_js` + `rules_ts` integration is on the table once the surface area
  stabilises.
