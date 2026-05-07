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

## uv ↔ Bazel — the honest story

`rules_python` 1.x ingests `uv.lock` indirectly through a `requirements.txt`
that we commit. Procedure when adding a Python dep:

1. `cd claude-mirror && uv add <pkg>`
2. `uv export --no-emit-project --no-emit-workspace --no-hashes
   --format requirements-txt > requirements.txt`
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
