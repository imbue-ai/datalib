# personal-mirror

Two coupled projects that mirror personal data into a queryable local store:

- **`claude-mirror/`** — Python CLI that ingests Anthropic claude.ai exports
  into a Dolt DB and renders one QMD per conversation.
- **`frankweiler/`** — Vue 3 UI + Rust (axum/Polars) backend that searches and
  views the mirrored data, packaged as a Tauri desktop app and an Open Host
  container.

Both projects share row shapes through **`schemas/`**, the single
source-of-truth that emits Rust / Python / TypeScript types from one JSON
Schema.

## Repo layout

```
.
├── MODULE.bazel              Bzlmod root (rules_python + rules_rust)
├── BUILD.bazel               :all_tests aggregator
├── schemas/                  cross-language source of truth
│   ├── anthropic.schema.json
│   ├── codegen.py            JSON Schema → Rust/Python/TS types
│   └── BUILD.bazel           genrules per language
├── claude-mirror/            Python ingestion CLI
│   ├── src/claude_mirror/
│   ├── tests/
│   ├── pyproject.toml + uv.lock
│   ├── requirements.txt      uv-exported, consumed by Bazel pip.parse
│   └── BUILD.bazel
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
   claude-mirror   frankweiler/        frankweiler/ui
   (Python)         backend/schema      (TS types)
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

`claude-mirror` and `frankweiler/` may **only** share things via `schemas/`.
Cargo workspace + Bazel `visibility` enforce this.

## Building & testing

### One command for CI parity

```sh
bazelisk test //...
```

Runs:
- Python smoke tests (`//claude-mirror:test_smoke`)
- Rust unit tests (`//frankweiler/backend/{schema,core,tauri-backend}:*_unittests`)
- Cross-language deeplink fixture test (Rust loads the same JSON the Vitest
  suite loads, asserting both implementations agree)

### Launch the backend in a browser

```sh
bazelisk run //frankweiler:serve
```

Builds and runs `frankweiler_http_bin`, then opens your default browser at
`http://127.0.0.1:8731/api/health`. Override with `FRANKWEILER_URL=...`.
Ctrl-C tears the server down.

### Inner loop (per language, faster)

| Language       | Command (run in the package dir)                |
|----------------|--------------------------------------------------|
| Python         | `cd claude-mirror && uv run pytest`              |
| Rust           | `cd frankweiler/backend && cargo test`           |
| Vue / Vitest   | `cd frankweiler/ui && pnpm test`                 |
| Vite dev UI    | `cd frankweiler/ui && pnpm dev`                  |

### Regenerating the cross-language types

The generated files (`frankweiler/backend/schema/src/generated/anthropic.rs`,
`claude-mirror/src/claude_mirror/generated_schema.py`,
`frankweiler/ui/src/generated/anthropic.ts` once wired) are checked in. To
regenerate after editing `schemas/anthropic.schema.json`:

```sh
bazelisk build //schemas:anthropic_all
cp bazel-bin/schemas/anthropic.rs frankweiler/backend/schema/src/generated/
cp bazel-bin/schemas/anthropic.py claude-mirror/src/claude_mirror/generated_schema.py
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

1. `cd claude-mirror && uv add <pkg>`
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
