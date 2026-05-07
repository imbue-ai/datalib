# Frankweiler — view-and-search UI for the personal data mirror

## Overview

A Vue 3 web UI for querying the data ingested by `claude-mirror`. Same UI is packaged two ways:

1. **Local Tauri app**, registers a `frankweiler://` deep-link handler.
2. **Hosted app** running inside [Open Host](~/src/openhost) (containerized HTTP service, single-tenant for v0).

Backend is **Rust + Polars**, written as a library crate that is embedded in-process by the Tauri shell and wrapped by a thin `axum` HTTP server for the Open Host packaging — same code, two front doors.

For v0 the focus is "get a working dev UI"; Open Host packaging is scaffolded but not exercised.

## Goals (v0)

- Vue 3 dev UI runnable locally against the Rust backend.
- Tauri shell that bundles the same UI + backend lib in-process.
- Search bar that supports Gmail-style structured filters (`before:`, `after:`, `subj:`, `type:`, `author:`) plus free-text terms; free-text routed to `qmd search`, structured filters routed to Polars/Dolt, results joined.
- Result rows are messages **or** chats (implicit from query, with `type:` override).
- AG Grid v35.2.1 with a default-hidden right tool panel for column selection; max columns offered, sensible defaults; reorder/resize/sort/persist.
- "Snippet" column with inline expansion (capped); double-click "Entire Chat" → new window/pane rendering the conversation via `markdown-it` + Shiki + KaTeX + `markdown-it-anchor`, target message highlighted, per-message anchors, long messages collapsible, timestamps on hover.
- Theme defaults to `prefers-color-scheme`; Preferences page picks from the gp-treemap palettes/themes; persisted in `localStorage`.
- App state in URL hash, **human-readable path-style intents** plus an opaque `grid` blob; identical grammar for `frankweiler://` deep links so hosted-web and Tauri links round-trip.
- Single shared config: `~/.config/frankweiler/config.yaml` names the data root.

## Non-goals (v0)

- No Open Host deploy (containers/manifests scaffolded only).
- No multi-tenant auth / JWT verification (single-tenant; deferred).
- No write/edit operations on data (read-only viewer).
- No semantic re-ranking pipeline beyond what `qmd vsearch`/`qmd query` give us out of the box.
- No mobile/responsive polish; desktop-first.

## Inputs

```yaml
# ~/.config/frankweiler/config.yaml
root: /Users/thad/data/mirror   # mirrors claude-mirror's `root`; data is read-only here
qmd:
  index_path: ${root}/.frankweiler/qmd-index   # qmd CLI's working dir
backend:
  bind: 127.0.0.1:8731          # http binding for the dev server / openhost shell
```

## Data filesystem layout under `<root>` (read by Frankweiler)

```
<root>/
  dolt_repo/                    # produced by claude-mirror
  anthropic/<account_uuid>/llm_chats/<conversation_uuid>__<slug>.qmd
  .frankweiler/
    qmd-index/                  # qmd's BM25 + sqlite index
    prefs.yaml                  # reserved for future server-side prefs (unused in v0)
```

## Repo layout (Bazel-managed at the repo root; one folder per role)

```
<repo>/                            # bazel module root
  MODULE.bazel
  BUILD.bazel                      # :all_tests umbrella
  schemas/                         # cross-language source of truth for the Dolt schemas
    BUILD.bazel
    anthropic.schema.json
    schemas.bzl                    # genrule producing rust/python/ts types
  claude-mirror/                   # existing Python package, moved under this dir
    BUILD.bazel
    pyproject.toml, uv.lock, src/, tests/
  frankweiler/
    backend/
      Cargo.toml                   # cargo workspace (members: schema, core, http, tauri-backend)
      schema/                      # rust types — derived from //schemas
      core/                        # query engine: Dolt + Polars + qmd subprocess
      http/                        # axum HTTP server (binary)
      tauri-backend/               # tauri commands (lib)
    ui/                            # Vue 3 + Vite SPA — consumes generated TS types from //schemas
    tauri/                         # Tauri shell (no BUILD; cargo/pnpm-driven)
    openhost/                      # Dockerfile + openhost.toml (no Bazel container build in v0)
```

### Dependency graph (one-way arrows; enforced by Bazel `visibility` + Cargo deps)

```
                       schemas/  (single source of truth)
                          │
        ┌─────────────────┼─────────────────┐
        ▼                 ▼                 ▼
   claude-mirror   frankweiler/        frankweiler/ui
   (Python,         backend/schema      (TS types)
    ingestion)            │
                          ▼
                   frankweiler/backend/core ──► (qmd subprocess, dolt sql client, polars)
                          │             │
                          ▼             ▼
                   backend/http   backend/tauri-backend
                          │             │
                          ▼             ▼
                       openhost/     tauri/  ◄── ui/ (Vue)
```

- `schemas/` is the **only** thing both `claude-mirror` and `frankweiler/` may share.
- `claude-mirror` MUST NOT import anything in `frankweiler/`; the inverse is also forbidden.
- Within `frankweiler/`, each layer depends only on the one below it; back-edges fail Bazel.

---

## Components

### F1. Config (`backend/core::config`)
- **Responsibility:** Load `~/.config/frankweiler/config.yaml` (path overridable via `FRANKWEILER_CONFIG` env var) into a typed `Config { root, qmd, backend }`. Resolve paths, expand `~` and `${root}` templates, validate `<root>` exists and contains `dolt_repo/`.
- **Interface:** `pub fn load_config(path: Option<&Path>) -> Result<Config>`

### F2. Dolt client (`backend/core::dolt`)
- **Responsibility:** Connect to the user's local `dolt sql-server` — start one as a subprocess if not running on the configured port (mirrors `claude-mirror`'s logic). Issue read-only queries.
- **Interface:**
  - `DoltClient::new(config: &Config) -> Result<Self>`
  - `query(&self, sql: &str, params: &[Value]) -> Result<DataFrame>` — returns a Polars `DataFrame` directly via `mysql` crate → arrow → polars
- **Notes:** v0 reuses claude-mirror's repo dir (`<root>/dolt_repo/`). Frankweiler never writes.

### F3. QMD client (`backend/core::qmd`)
- **Responsibility:** Drive the `qmd` CLI as a subprocess. v0 supports `qmd search` (BM25); leaves room for `vsearch`/`query`. Re-indexes when the underlying `.qmd` files' mtime is newer than the qmd index's.
- **Interface:**
  - `pub fn ensure_index(config: &Config) -> Result<()>`
  - `pub fn search(query: &str, limit: usize) -> Result<Vec<QmdHit>>` where `QmdHit { path, score, snippets: Vec<Snippet> }`
- **Snippets:** parsed from qmd's JSON output; each snippet is `{ text, char_offset, line }`. Frankweiler caps the inline preview to `<= 280 chars` and exposes the rest via the chat-detail view.

### F4. Query parser (`backend/core::query`)
- **Responsibility:** Tokenize the search-bar string into structured filters + free text. Gmail-ish grammar: `before:`, `after:`, `subj:`, `type:`, `author:`, `account:`, `project:` (extensible). `type:` ∈ {`chat`, `message`}.
- **Interface:** `pub fn parse_query(s: &str) -> ParsedQuery { filters: HashMap<Field, Vec<String>>, free_text: String }`
- **Implicit type rule:** if `type:` not given:
  - non-empty `free_text` → `type=message`
  - empty `free_text` → `type=chat`
- **Notes:** Pure function; thoroughly unit-tested.

### F5. Search engine (`backend/core::search`)
- **Responsibility:** Execute `ParsedQuery`. Orchestrates qmd (free-text) + Polars/Dolt (structured) and joins. Returns a Polars `DataFrame` with all available columns; the UI picks which to display.
- **Algorithm:**
  1. Parse query (F4).
  2. If `free_text` non-empty: `qmd_hits = QmdClient::search(free_text)` → Polars DF keyed by `conversation_uuid` (path → uuid).
  3. Pull a base DF from Dolt: messages or conversations depending on resolved `type`.
  4. Apply structured filters via Polars expressions (`.filter(...)`). Date filters `before:`/`after:` parse with `chrono`.
  5. If both qmd and structured, **inner-join on the join key** (`conversation_uuid` for chats, `message_uuid` for messages — and qmd hits are first expanded to messages by overlapping the snippet's char range against the message text).
  6. Optionally `LEFT JOIN` qmd's sqlite embeddings table when present (`<index>/embeddings.sqlite`) — this is the placeholder for ranked semantic re-ranking later.
  7. Sort by `score DESC` if qmd contributed, else `updated_at DESC`. Limit (default 500).
- **Interface:** `pub fn search(config: &Config, query: &str, limit: usize) -> Result<SearchResult>`
- **`SearchResult`:** `{ rows: DataFrame, columns: Vec<ColumnSpec>, total_estimated: u64, query_echo: ParsedQuery }`

### F6. Chat assembler (`backend/core::chat`)
- **Responsibility:** Materialize a single conversation for the chat-detail view. Returns a structured tree (not markdown) so the renderer can do anchors/collapsing per message.
- **Interface:** `pub fn assemble_chat(config: &Config, conversation_uuid: &str) -> Result<ChatDoc>` where `ChatDoc { conversation: ConvMeta, messages: Vec<MsgNode> }` and `MsgNode { uuid, sender, created_at, blocks: Vec<Block>, attachments }`.
- **Notes:** Rendering happens in the UI (markdown-it). Frankweiler ships the structured doc + raw markdown text per block.

### F7. HTTP API (`backend/http`)
- **Responsibility:** Thin `axum` server exposing the `core` functions. CORS open to `localhost` in dev.
- **Endpoints (v0):**
  - `GET /api/health`
  - `GET /api/search?q=…&limit=…` → `SearchResult` JSON
  - `GET /api/chat/:conversation_uuid` → `ChatDoc` JSON
  - `GET /api/columns` → `Vec<ColumnSpec>` (so the UI can populate the custom column-selection panel)
- **Notes:** Single-tenant; no auth in v0; binds `127.0.0.1` by default.

### F8. Tauri commands (`backend/tauri-backend`)
- **Responsibility:** Same surface as F7, exposed as Tauri commands. Identical request/response types (re-exported from `core`) so the UI's transport layer is the only thing that differs.
- **Commands:** `health`, `search`, `chat`, `columns`.
- **Deep-link handler:** registers `frankweiler://` (via `tauri-plugin-deep-link`); on URL receipt, parses with the shared grammar (F11) and posts a `window.location.hash` change to the loaded UI.

### F9. UI shell (`ui/`)
- **Responsibility:** Vue 3 + Vite SPA. Top-level layout: search bar (top), result grid (center), preferences page (modal/route), chat-detail view (separate route + window).
- **Tooling:** Vue 3, Vite, TypeScript, Pinia (state), Vue Router, **AG Grid v35.2.1 Community** only (`ag-grid-vue3` + `ag-grid-community`). No Enterprise license required — the column-selection side panel is implemented as a small Vue component over AG Grid's public column APIs (`api.getColumnState()`, `api.setColumnsVisible()`, `api.moveColumns()`), not the Enterprise `sideBar`/`columnsToolPanel`.
- **Transport abstraction:** `useApi()` composable hides web-vs-tauri:
  - In Tauri: `import { invoke } from '@tauri-apps/api/core'`
  - In web: `fetch('/api/…')`
  - Selected at build time via Vite env (`VITE_TARGET=tauri|web`).

### F10. Result grid (`ui/src/components/ResultGrid.vue` + `ui/src/components/ColumnsPanel.vue`)
- **Responsibility:** AG Grid Community wired to `SearchResult.rows`. Column definitions derived from `SearchResult.columns`; default visibility set per row type (chat vs. message); reorder/resize/sort enabled. A **custom right-side panel** (`ColumnsPanel.vue`, default-hidden) lists every available column with a checkbox + drag handle; toggles drive `api.setColumnsVisible(...)`, drags drive `api.moveColumns(...)`. Built entirely on Community APIs.
- **Default columns by row type:**
  - **Message:** `Snippet` (with `<mark>`), `Sender`, `When`, `Conversation Name`, `Project`, `Account`, `Entire Chat` (button)
  - **Chat:** `Name`, `Updated`, `Messages`, `Project`, `Account`, `Open`
- **Snippet column behavior:** truncated to ~280 chars by default; click expands inline up to a hard cap (~2000 chars); a "View in chat" affordance opens the chat-detail view at the snippet's anchor.
- **"Entire Chat" cell:** custom Vue cell renderer with a button; double-click triggers navigation; in Tauri this opens a separate `WebviewWindow`, in web this opens a new browser tab/window. Both targets receive the deep-link grammar (F11).

### F11. Deep-link grammar (`backend/core::deeplink` + `ui/src/router/deeplink.ts`)
- **Grammar (uniform across web hash and `frankweiler://`):**
  - `search?q=<text>&type=message&before=2025-01-01&grid=<base64>` — search route
  - `chat/<conversation_uuid>?msg=<message_uuid>&grid=<base64>` — chat-detail route
  - `prefs` — preferences route
- **Hash form:** `#search?q=foo` (the `#` replaces the leading `/`); deep-link form: `frankweiler://search?q=foo`. Same parser handles both.
- **`grid` parameter:** opaque base64 of `JSON.stringify(GridState)` from AG Grid (`api.getColumnState()`, `api.getFilterModel()`, `api.getSortModel()`). Excluded when default. Hard cap ~6 KB; warn (toast) if exceeded.
- **Implementation:** Rust `backend/core::deeplink` and TypeScript `ui/src/router/deeplink.ts` share a tiny grammar spec; tests in both languages assert the same fixtures round-trip.
- **Restoration:** on app load, parse `window.location.hash` (or the URL Tauri delivers) into a `UIState`; Pinia store is hydrated; AG Grid mount waits for state before applying column model.

### F12. Chat-detail view (`ui/src/views/ChatView.vue`)
- **Responsibility:** Render a `ChatDoc` to HTML using **`markdown-it`** for the body, **Shiki** for code highlighting, **KaTeX** for math, **`markdown-it-anchor`** for stable per-message anchors (`#msg-<uuid>`).
- **Per-message UI:**
  - Header line with sender + timestamp (timestamp shown short with full ISO on hover via `<time title>`).
  - Anchor link icon → copies a `frankweiler://chat/<conv_uuid>?msg=<msg_uuid>` URL.
  - "Collapse / Expand" toggle when message > N chars (default 1500).
  - When opened with `?msg=<uuid>`, that message gets a highlight class and the page auto-scrolls.

### F13. Theme + Preferences (`ui/src/stores/prefs.ts`, `ui/src/views/Preferences.vue`)
- **Responsibility:** Load saved palette/theme from `localStorage`; default to `prefers-color-scheme` (light vs. dark variant of the chosen theme).
- **Themes shipped:** all named app themes from the gp-treemap palettes file: `nord`, `solarized`, `dracula`, `catppuccin`, `gruvbox`, `tokyo-night`, `rose-pine`, `one-dark`. Data palettes (`gp-default`, `viridis`, `plasma`, `inferno`, `magma`, `turbo`, `coolwarm`, `heatmap`, `rainbow`) are surfaced separately for any future visualization that wants a numeric scale (not used by v0 chrome).
- **Wiring:** CSS custom properties driven by the active theme; Vue Provide/Inject; AG Grid theme parameters (the new theme API) bound to those custom properties.

### F14. URL/state store (`ui/src/stores/uiState.ts`)
- **Responsibility:** Single Pinia store: `UIState { route, search: SearchState, grid: GridState, prefs: PrefsState }`. Watches the store, debounces 250ms, writes to `window.history.replaceState` so the URL hash always reflects current state without polluting history. On `hashchange`, hydrates the store.
- **Notes:** `prefs` lives in localStorage; `grid` and `search` live in the URL.

### F15. OpenHost packaging (`openhost/`)
- **Responsibility:** Scaffold only in v0. `openhost.toml` declares container port (8731), data mounts pointing at the user's data root, no auth. `Dockerfile` builds the `backend/http` binary and ships `ui/dist/` as static assets served by axum. Not exercised in v0; verifies only that `cargo build -p frankweiler-http --release` produces a runnable binary that serves the UI on port 8731.

---

## Data flow

### A search request (web shell)
```
search bar (UI) ──q──► useApi.search() ──HTTP──► axum (F7) ──► core::search (F5)
                                                                  │
                              ┌───────────────────────────────────┼───────────────────┐
                              ▼                                   ▼                   ▼
                       core::qmd (F3)                    core::dolt (F2)    sqlite embeddings (optional)
                              │                                   │                   │
                              └─────────────► Polars JOIN ◄───────┴───────────────────┘
                                                       │
                                                       ▼
                                             SearchResult JSON
                                                       │
                                                       ▼
                                  ResultGrid (AG Grid) + Snippet column
```

### A chat-detail open
```
double-click "Entire Chat" cell
           │
           ▼
new window/tab navigates to  #chat/<conv_uuid>?msg=<msg_uuid>
           │
           ▼
ChatView (F12) ──► useApi.chat(uuid) ──► core::chat::assemble_chat (F6)
           │                                       │
           │                                       ▼
           │                              Dolt: conversations + messages + content_blocks
           │
           ▼
markdown-it + Shiki + KaTeX render → highlight target msg → scroll to anchor
```

### Tauri deep link
```
OS receives  frankweiler://chat/<conv>?msg=<msg>
           │
           ▼
tauri-plugin-deep-link → tauri-backend (F8) → emit "deeplink" event
           │
           ▼
UI listens, sets window.location.hash → router/deeplink (F11) → same flow as above
```

---

## Bazel

We adopt **Bzlmod-only Bazel** at the **repo root** from day one. The goal is a single `bazel test //...` that exercises both `claude-mirror` (Python) and `frankweiler` (Rust + Vue), with cross-language schema sharing visible to the build graph. The Tauri bundler stays outside Bazel.

**Repo-root layout:**
```
<repo>/
  MODULE.bazel
  BUILD.bazel                       # umbrella :all_tests aggregator
  claude-mirror/                    # existing Python package (moved under this dir)
    BUILD.bazel                     # py_library + py_test, shares schema with frankweiler
    pyproject.toml, uv.lock, src/, ...
  frankweiler/
    backend/
      Cargo.toml                    # cargo workspace
      schema/    BUILD.bazel        # rust_library — mirrors the on-disk shape
      core/      BUILD.bazel
      http/      BUILD.bazel
      tauri-backend/ BUILD.bazel
    ui/          BUILD.bazel
    tauri/       (no BUILD)         # bundler driven by cargo/pnpm
    openhost/    BUILD.bazel        # stub
  schemas/                          # NEW — single source of truth shared across languages
    BUILD.bazel
    anthropic.schema.json           # JSON Schema or similar describing each Dolt table
    schemas.bzl                     # genrule wrappers to produce typed code per language
```

**Versions (lock at scaffolding time, then revisit):**
- Bazel: latest stable via Bazelisk (8.x).
- `rules_rust`: latest Bzlmod-compatible tag.
- `rules_js` + `rules_ts` (Aspect): latest stable.
- `rules_python`: 1.x (with `pip.parse` + `uv.lock` ingestion).
- `aspect_rules_py` (Aspect): for `py_pytest_main`/`py_test` ergonomics.

**uv ↔ Bazel — the honest story:**
- `rules_python` 1.x **does** consume `uv.lock` directly via `pip.parse(experimental_index_url = ..., requirements_lock = ":uv.lock")` (or via the newer `uv_pip_compile` extension). No re-resolution at build time.
- `uv sync` and `bazel build` won't cooperate live — Bazel materializes wheels via a `repository_rule` at MODULE setup; from then on, Bazel owns the runtime. In contrast, `uv` continues to own the local `.venv` for editor/IDE/inner-loop work.
- **Practice:** keep `uv` as the venv/inner-loop driver; have Bazel ingest the same `uv.lock` so test runs are hermetic. When a dep changes, `uv add ...` updates the lockfile, then a `MODULE.bazel.lock` refresh picks it up.
- This is a real-but-managed friction — flagged here so we don't pretend otherwise.

**What Bazel owns (in v0):**
- **Python (`claude-mirror`)**: `py_library` for `claude_mirror` package, `py_test` for unit tests, `py_binary` for the CLI. Deps come from `uv.lock` via `rules_python`.
- **Cross-language schema (`schemas/`)**: a single source-of-truth JSON Schema (or similar) describing each Dolt table the ingest writes. A `genrule` produces:
  - Rust types → consumed by `frankweiler/backend/schema`
  - Python `dataclass`/Pydantic types → consumed by `claude-mirror` (or at minimum a runtime validator)
  - TypeScript types → consumed by `frankweiler/ui` (the API response types are derived from the same shapes)
  This is the dependency the user called out: the UI schema points at the ingest schemas, but neither side imports the other directly — both consume the generated artifact.
- **Rust** (`frankweiler/backend/*`): `rust_library`/`rust_binary`/`rust_test` per crate; third-party Cargo deps via `crate_universe` reading the existing Cargo workspace.
- **UI** (`frankweiler/ui`): `rules_js` + `rules_ts` for typechecking + Vitest; `vite build` wrapped via `js_run_binary`.
- **Aggregators** at the repo root: `:all_libs`, `:all_tests`, `:ui_dist`.
- `bazel test //...` runs the full matrix (Python + Rust + Vitest + cross-language fixture tests).

**What Bazel does NOT own (in v0, by design):**
- The **Tauri bundler** (`pnpm tauri build`/`cargo tauri ...`) — driven directly.
- The **OpenHost container build** — `docker build` runs outside Bazel against Bazel outputs. (`rules_oci` is a fine future migration if/when we deploy.)
- The **Dolt subprocess** at test time — `claude-mirror` and `frankweiler/backend` tests that need Dolt either skip when `dolt` is missing or use a fixture. We don't try to make `dolt` a hermetic toolchain.
- The **`uv` venv** itself — `uv sync` continues to manage `.venv` for the inner loop.

**Visibility / dep enforcement (Bazel `visibility`):**
- `//schemas:*` is visible to both `//claude-mirror/...` and `//frankweiler/...`.
- `//claude-mirror/...` has **no** visibility into `//frankweiler/...`.
- `//frankweiler/backend/schema:*` visible only to `//frankweiler/backend/core:*`.
- `//frankweiler/backend/core:*` visible to `//frankweiler/backend/{http,tauri-backend}:*`.
- `//frankweiler/backend/http:*` and `//.../tauri-backend:*` visible only to `//frankweiler/{openhost,tauri}:*` respectively.
- Adding a back-edge (e.g. `core` importing `http`, or `claude-mirror` importing anything in `frankweiler/`) breaks `bazel build //...`.

**Local DX:**
- Inner loop unchanged: `cargo run`, `vite dev`, `uv run`. Cargo workspace and `pyproject.toml` remain the source of truth for deps.
- `bazel test //...` is the single command for "run every test the same way CI does". Pre-commit hook recommended.

---

## Open questions / explicit deferrals

- **Bazel scope (decided): use Bazel from day one for builds and tests, but keep the Tauri bundler outside.**
  See the dedicated **"Bazel"** section below.
- **Open Host packaging.** Container builds and `openhost.toml` are included as stubs but not exercised in v0.
- **Auth.** Single-tenant for v0; JWT verification middleware deferred.
- **qmd index lifecycle.** v0 re-indexes lazily on backend startup if file mtimes are newer than the index. A claude-mirror post-ingest hook to invalidate the index is a future polish.
- **AG Grid theme API churn.** v34 introduced a new theme API; v35 has further refinement. Lock to v35.2.x and use the new theme parameters API.

---

## Task list

### Phase 1 — Repo-root Bazel + workspace scaffolding
1. **Move existing `claude-mirror`** sources under `claude-mirror/` (currently they live at the repo root). Confirm the package still imports.
2. Create `schemas/` with the first schema (`anthropic.schema.json`) describing the Dolt tables produced by `claude-mirror`. Add `schemas.bzl` with stub genrules for Rust / Python / TypeScript type emission (real codegen tool TBD — `quicktype` or hand-rolled is fine for v0). Bazel `BUILD.bazel` exposes generated targets.
3. Create `frankweiler/backend/Cargo.toml` workspace with empty members `schema`, `core`, `http`, `tauri-backend`. The `schema` crate `use`s the Rust types emitted by `//schemas`.
4. Init `frankweiler/ui/` with Vite + Vue 3 + TS + Pinia + Vue Router + Vitest; lock pnpm. The TS API types are imported from `//schemas`'s emitted TS package.
5. Init `frankweiler/tauri/` (Tauri v2) shell pointing at `ui/dist/`.
6. Stub `frankweiler/openhost/openhost.toml` and `Dockerfile`.
7. **Bazel root**: write repo-root `MODULE.bazel` (Bzlmod) pinning `rules_rust`, `rules_js`, `rules_ts`, `rules_python`. `pip.parse` ingests `claude-mirror/uv.lock`. `crate_universe` ingests the Cargo workspace.
8. `claude-mirror/BUILD.bazel`: `py_library` for the package, `py_test` for unit tests, `py_binary` for the CLI; deps include `//schemas:py`.
9. `BUILD.bazel` per Rust crate (`rust_library`/`rust_binary`/`rust_test`); `frankweiler/backend/schema` depends on `//schemas:rust`.
10. `frankweiler/ui/BUILD.bazel`: `rules_js`/`rules_ts` for typecheck + Vitest; wraps `vite build` via `js_run_binary`; depends on `//schemas:ts`.
11. Repo-root `BUILD.bazel`: `:all_tests` aggregator. Verify `bazel test //...` runs the existing claude-mirror tests + empty-crate smoke targets + UI Vitest happy-path.
12. Top-level `README.md` documenting the dependency graph, the `schemas/` source-of-truth, per-package inner-loop commands (`uv`/`cargo`/`pnpm`), and the single-command CI parity (`bazel test //...`). Call out the uv↔Bazel friction explicitly.

### Phase 2 — Backend core (Rust + Polars + Dolt + qmd)
7. **F1 Config** with serde + tests; Bazel `rust_test` runs them.
8. **F2 DoltClient**: connect to the existing `dolt sql-server` (start one if absent), wrap a few read queries → Polars `DataFrame`.
9. **F3 QmdClient**: subprocess wrapper around `qmd search` + lazy `qmd index`. Test against the rendered QMDs in `~/backups/claude` after running `claude-mirror ingest`.
10. **F4 Query parser**: pure-function tokenizer + table-driven tests.
11. **F5 Search engine**: free-text + structured + Polars JOIN. Returns `SearchResult` with column specs.
12. **F6 Chat assembler**: builds `ChatDoc` from Dolt rows.

### Phase 3 — HTTP server
13. **F7 axum**: bind, CORS, the four endpoints, integration test against a temp dolt repo (Bazel `rust_test` with the dolt repo fixture as a `data` dep).

### Phase 4 — UI
14. **F9 shell** + routing + `useApi()` web transport.
15. **F13 Preferences + theming**: ship the eight named themes; system-theme detection; CSS custom properties.
16. **F10 ResultGrid**: AG Grid wiring; column-spec-driven definitions; tool panel; default columns; Snippet column with inline expansion + cap.
17. **F12 ChatView**: markdown-it + Shiki + KaTeX + anchor; per-message anchors; collapse/expand; scroll-to-message.
18. **F11/F14 deep-link grammar**: TS implementation; Pinia store hydration on hashchange; debounced URL writes; round-trip tests (Vitest under Bazel).
19. Cross-language fixture: a JSON fixture file is loaded by both the Rust `deeplink` test and the TS `deeplink` test; `bazel test //...` runs both and they must agree.

### Phase 5 — Tauri shell (mostly outside Bazel)
20. **F8 tauri-backend**: re-export the four commands; the crate itself is Bazel-built (`rust_library`).
21. Tauri build (driven via Cargo/pnpm directly, not Bazel) with `useApi()` swapped to `invoke`; verify the dev UI works inside the Tauri window.
22. `tauri-plugin-deep-link`: register `frankweiler://`; convert incoming URLs to hashchanges via shared grammar; smoke-test `frankweiler://chat/<uuid>` opens the right view.

### Phase 6 — End-to-end smoke
23. Run `claude-mirror ingest` against `~/backups/claude`, then run Frankweiler dev server, search for "treemap", open a result's chat detail, verify highlighting + anchors. Repeat in Tauri.
24. `bazel test //frankweiler/...` from a clean state passes.

### Phase 7 — Open Host scaffolding (no deploy)
25. `bazel build //frankweiler/backend/http:http_bin` produces a runnable binary that serves the UI on `127.0.0.1:8731` reading from `~/.config/frankweiler/config.yaml`. Dockerfile builds and runs locally (driven outside Bazel); deploy to Open Host deferred.

---

## Acceptance criteria (v0)

- `bazel test //frankweiler/...` passes from a clean checkout.
- `cargo run -p frankweiler-http` + `pnpm --filter frankweiler-ui dev` (inner-loop dev) boots a UI at `http://localhost:5173/` that:
  - Renders the search bar at the top and an empty grid below it on first load (URL hash `#search?q=`).
  - Searching `treemap` returns ≥1 message row (against the seeded `~/backups/claude` data); the Snippet column shows highlighted excerpts; the AG Grid tool panel reveals on toggle and lists every column from `/api/columns`.
  - Double-clicking the "Entire Chat" cell opens a new window at `#chat/<uuid>?msg=<uuid>`, the markdown renders, the target message is highlighted, the per-message anchor link copies a `frankweiler://chat/<uuid>?msg=<uuid>` URL.
  - Switching to the `dracula` theme on the Preferences page persists across reload via `localStorage`; system-theme default is honored on a fresh profile.
  - Pasting the URL of a non-default state into a fresh browser instance restores grid columns + sort + the open chat.
- `cargo run -p frankweiler-tauri` opens a desktop window with the same UI; opening `frankweiler://search?q=treemap` from a terminal (`open frankweiler://...` on macOS) brings the app forward and shows the search results.
- Cargo workspace's dependency graph forbids `claude-mirror` ↔ frankweiler back-edges (verified by `cargo check` failing if you try to add the import).
- Bazel `visibility` rules forbid intra-frankweiler back-edges (e.g. `core` importing from `http`); verified by deliberately adding such a back-edge in a throwaway branch and observing `bazel build //...` fail.
