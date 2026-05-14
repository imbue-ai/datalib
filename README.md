# mixed-up-files

Two coupled projects that mirror personal data into a queryable local store:

- **`src/`** — Python packages that download and ingest LLM chat exports
  (Anthropic + OpenAI) into a Dolt DB and render one QMD per conversation:
  - `src/download/` — per-provider incremental downloaders.
  - `src/ingest/` — config-driven CLI that ingests raw + takeout-style
    inputs into Dolt and emits qmd markdown.
- **`frankweiler/`** — Vue 3 UI + Rust (axum/Polars) backend that searches and
  views the mirrored data, packaged as a Tauri desktop app and an Open Host
  container.

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

### Why no visual chips (yet)

The filter bar is a single text input on purpose. Visual chip components
are nice to look at but **hard to copy-paste out of** — the user can't
grab a filter to send in chat or paste into a deeplink without
re-typing. The Notion-shaped tokens are already self-describing as plain
text, so they round-trip through any text channel cleanly.

A reasonable future compromise: render chips visually but make
double-click (or a chip menu) turn the chip back into editable plain
text. Several editors do this for hashtags / mentions. Worth considering
once the tokens themselves stabilise.

## Repo layout

```
.
├── MODULE.bazel              Bzlmod root (rules_python + rules_rust)
├── BUILD.bazel               :all_tests aggregator
├── schemas/                  cross-language source of truth
│   ├── grid_rows.schema.json union row shape backing the grid (see docs/grid_rows.md)
│   ├── codegen.py            JSON Schema → Rust/Python/TS types + DDL
│   └── BUILD.bazel           genrules per language
├── docs/                     architecture notes
│   └── grid_rows.md          how the grid_rows union table works
├── pyproject.toml + uv.lock  Python project (mixed-up-files) — src layout
├── requirements.txt          uv-exported, consumed by Bazel pip.parse
├── src/
│   ├── download/             per-provider downloaders (claude.ai, chatgpt.com, slack)
│   └── ingest/               Dolt ingest + qmd renderer CLI
├── tests/                    pytest suite + fixtures + golden snapshots
└── frankweiler/
    ├── backend/              Cargo workspace
    │   ├── Cargo.toml
    │   ├── schema/           re-exports //schemas:anthropic_rs types
    │   ├── core/             query engine + deeplink grammar
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
        ┌─────────────────┼─────────────────┐
        ▼                 ▼                 ▼
       src/           frankweiler/        frankweiler/ui
   (Python ingest)    backend/schema      (TS types)
                          │
                          ▼
                   frankweiler/backend/core ──► dolt + qmd + polars
                          │             │
                          ▼             ▼
                   backend/http   backend/tauri-backend
                          │             │
                          ▼             ▼
                       openhost/     tauri/  ◄── ui/
```

`src/` (download + ingest) and `frankweiler/` may **only** share things
via `schemas/`. Cargo workspace + Bazel `visibility` enforce this.

## Building & testing

### One command for CI parity

```sh
bazelisk test //...
```

**This is the source of truth for "do the tests pass?" — always prefer it
over per-language inner loops when you want a real answer.** Bazel's
action cache makes re-runs cheap: untouched targets are served from cache,
so the second invocation only re-executes what your changes actually
affected. The per-language commands below are convenient for tight inner
loops, but they don't see cross-language goldens, the deeplink fixture
test, or the e2e suite — `bazelisk test //...` does.

Runs:
- Python smoke tests (`//tests:test_smoke`)
- Rust unit tests (`//frankweiler/backend/{schema,core,tauri-backend}:*_unittests`)
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

Data root resolution (the QMDs feed the search index — Dolt remains the
source of truth):

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

Re-ingests every enabled source from the config, commits to Dolt, and
re-renders the qmd tree. Run after editing the renderer, the schema, or
your downloads.

```sh
# Bazel (uses an absolute path so the binary's CWD doesn't matter)
bazelisk run //src/ingest:cli -- ingest --config $(pwd)/configs/thad_dev.yaml

# uv (paths are repo-relative)
uv run python -m ingest --config configs/thad_dev.yaml
```

Omit `--config` to use the default (`~/.config/mixed-up-files/config.yaml`).

### QMD search index (default-on, incremental)

`ingest ingest` rebuilds the qmd search index over `<root>` after the
markdown tree is rendered. It lives at `<root>/.frankweiler/qmd/index.sqlite`
and is what the search bar's hybrid / vector queries hit (see
`src/qmd_bridge/`). Pass `--no-qmd-index` to skip.

Design notes:

- **Two indexers, one shape**: the production path is Python
  (`src/ingest/qmd_index.py`); the Bazel-driven fixture path is the Rust
  binary at `frankweiler/backend/qmd_indexer/`. Both shell out to
  `npx -y @tobilu/qmd@<version>` with `XDG_CACHE_HOME=<root>/.frankweiler`
  so the index lands next to `mirror.sqlite`. Keep them in sync when
  bumping the pinned qmd version.
- **Incremental** in the production path. qmd's `documents` table keys
  on `(collection, path, content_hash)`, and `content_vectors` is
  keyed by hash, so a re-run only rechunks files whose bytes changed
  and only re-embeds content hashes with no existing vector row.
  Deletes are detected (rows marked `active=0`) and orphaned content
  is cleaned. We do **not** wipe `<root>/.frankweiler/qmd/` between
  runs — the prior wrapper did, which defeated all of this.
- **Non-incremental** in the Bazel fixture path: that binary clears
  the index dir each run because hermetic fixture builds want a clean
  rebuild every time.
- **First run is slow** — embedding all chunks for a fresh `<root>`
  takes several minutes on CPU (one-time cost). qmd streams a live
  progress bar (ETA + chunks/s) to stderr for both `update` and
  `embed`; subprocess inherits stderr, so you see it in the ingest
  terminal. `qmd embed` is resumable: if it gets interrupted, the
  next run picks up where it left off (it skips content hashes that
  already have vectors), so paying the cost in chunks is fine.

  To watch it run end-to-end:

  ```sh
  bazelisk run //src/ingest:cli -- ingest \
      --config $(pwd)/configs/thad_dev.yaml --no-report
  ```

  After a no-op render + dolt commit, you'll see something like:

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
  ~/.cache/qmd-models`. Override with `models_dir=` if you call
  `build_qmd_index` directly.

### Inner loop (per language, faster)

| Language       | Command (run in the package dir)                |
|----------------|--------------------------------------------------|
| Python         | `uv run pytest`                                  |
| Rust           | `cd frankweiler/backend && cargo test`           |
| Vue / Vitest   | `cd frankweiler/ui && pnpm test`                 |
| Vite dev UI    | `cd frankweiler/ui && pnpm dev`                  |
| Playwright e2e | `bazelisk run //frankweiler/ui:e2e`              |

### Regenerating the cross-language types

The generated files (`frankweiler/backend/schema/src/generated/grid_rows.rs`,
`src/ingest/generated_grid_rows.py`) are checked in. To regenerate after
editing `schemas/grid_rows.schema.json`:

```sh
bazelisk build //schemas:grid_rows_all
cp bazel-bin/schemas/grid_rows.rs   frankweiler/backend/schema/src/generated/
cp bazel-bin/schemas/grid_rows.py   src/ingest/generated_grid_rows.py
```

(A future `bazel run //schemas:update_generated` will fold these copies into
one command.)

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

Other deps follow standard semver caret ranges; `cargo update`, `pnpm
update`, and `uv lock --upgrade` are safe within those ranges (Cargo.lock and
pnpm-lock are committed).

## uv ↔ Bazel — the honest story

`rules_python` 1.x ingests `uv.lock` indirectly through a `requirements.txt`
that we commit. Procedure when adding a Python dep:

1. `uv add <pkg>`
2. `uv export --no-emit-project --no-emit-workspace
   --format requirements-txt > requirements.txt`
   (Hashes are kept — `rules_python` warns loudly if absent.)
3. `bazelisk test //...` to refresh the Bazel hub repo.

`uv` continues to own the local `.venv` for editor / inner-loop work. Bazel
owns hermetic CI runs.

## What Bazel does not own (by design, v0)

- Tauri bundler (`pnpm tauri` / `cargo tauri`).
- OpenHost docker build (run `docker build` against Bazel outputs).
- The `dolt sql-server` subprocess (test fixtures or skip if `dolt` is
  missing).
- The `uv` venv.
- The Vite build / Vitest under Bazel — currently driven by `pnpm`. A future
  `rules_js` + `rules_ts` integration is on the table once the surface area
  stabilises.
