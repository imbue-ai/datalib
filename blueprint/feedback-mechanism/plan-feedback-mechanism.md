# Feedback Mechanism — Component Spec + Task List

> **Refined prompt**
>
> I'd like to build a feedback mechanism directly into the app. This is because I am bumping into so many problems with the way that data gets transformed and rendered. On the right-click context menu for any "entity" on the app surface, there should be a "Feedback…" option.
>
> That option pops a modal dialog with Submit / Cancel buttons, a text box for describing the problem, and thumbs-up / thumbs-down buttons for sentiment.
>
> Feedback is persisted into the **Dolt** database, not SQLite. The row is a strongly-typed schematized object whose body is mostly JSON, and it includes: all UUIDs being selected or pointed at, the current URL, which column was being pointed at (if any), the app version and git hash, etc.
>
> Lightroom right-click semantics apply to row targeting: if multiple rows are selected and the right-clicked row is one of them, all selected rows are the target; if the right-clicked row is *not* in the selection, only that one row is the target. This is already documented in `SearchView.vue:94-96` and `resolveTargetRows()` — we re-use that function as-is.
>
> The goal is to make it as easy as possible to find what the user was pointing at and reconcile feedback with the underlying data.
>
> Feedback can also be filed from the right-hand preview panel, which today has no context-menu handler — we add one. We capture either the message under the cursor (via `[data-msg-index]`) or the active text selection (`window.getSelection()`), whichever applies.
>
> * **Persistence path:** Stop treating `mirror.sqlite` as the runtime store. The Rust backend spawns and owns a managed `dolt sql-server` subprocess (assumes `dolt` on `$PATH`) on a random unused port and talks to it over MySQL. `mirror.sqlite` continues to be materialized by ingest, but only as a backwards-compatibility reference — **the running app always uses Dolt**.
> * **Data access layer:** All backend SQL goes through a single trait so the same call sites can target Dolt-over-MySQL or `mirror.sqlite`. SQLite impl stays around as a reference implementation; Dolt is the production path.
> * **Entity surfaces:** "Feedback…" appears on every UUID-bearing surface — grid rows, filter-bar chips, column headers, chat preview pane, and page-level header. Lightroom semantics apply where multi-select exists.
> * **Preview-pane target resolution:** capture `window.getSelection()` if non-empty (text + start/end message UUIDs), else the message UUID under cursor. Always include conversation UUID. Also capture DOM path.
> * **DOM-path encoding:** store both a compact breadcrumb array `[{tag, id?, classes?, data-attrs?}, ...]` and a flat CSS selector string inside `context_json`.
> * **Row shape:** typed columns — `feedback_uuid` PK, `created_at`, `sentiment` (`up`/`down`/null), `comment` TEXT, `app_version`, `git_hash`, `context_json` JSON. Everything else (URL, target UUIDs, column, surface, DOM path, selection text) lives inside `context_json`.
> * **Surface discriminator:** `context_json.surface` is a discriminated enum (`grid_cell`, `grid_row`, `preview_message`, `preview_selection`, `page_header`, `filter_chip`, `column_header`) with a typed payload per variant. The discriminated-union types are defined in `schemas/feedback.schema.json` and emitted to Rust / Python / TypeScript via the existing codegen so all three languages agree.
> * **Submit rules:** Submit enabled only when `comment` is non-empty; sentiment is optional. `Esc` cancels, `Cmd/Ctrl+Enter` submits.
> * **Dolt commit semantics:** every successful feedback insert is followed by `CALL DOLT_COMMIT('-Am', 'feedback: <uuid>')` so each row has its own audit-friendly commit in `dolt log`.
> * **Git hash injection:** Bazel `--workspace_status_command` writes the hash into a stamp file the binary reads at startup; falls back to `"unknown"` for non-Bazel builds.
> * **Read-back:** out of scope. The user will query Dolt directly. The feature is purely about recording.

---

## Components

### C1. Managed `dolt sql-server` subprocess

**Purpose.** Own the Dolt server lifetime for as long as the backend runs.

**Location.** New module `frankweiler/backend/core/src/dolt_server.rs`.

**Inputs.**
- `dolt_repo_path` (PathBuf) — from config; reuses the path ingest writes into.
- `dolt_binary` (Option<PathBuf>) — defaults to `dolt` on `$PATH`.

**Outputs / public API.**
- `DoltServer::spawn(repo_path, dolt_binary) -> Result<DoltServer>` — picks a random unused TCP port by binding `127.0.0.1:0` then immediately dropping the listener, passes it via `--port` to `dolt sql-server`, waits for readiness (poll TCP connect + `SELECT 1`), returns the live handle.
- `DoltServer::mysql_url(&self) -> String` — `mysql://root@127.0.0.1:<port>/<db>`.
- `Drop` impl sends SIGTERM, joins with a short timeout, falls back to SIGKILL.

**Behavior.**
- On startup, fail loudly if `dolt` is not on `$PATH` or the repo path is missing.
- Logs subprocess stdout/stderr to backend log at `INFO`/`WARN`.
- Health check (`SELECT 1`) on a timer; if the subprocess dies, the HTTP layer returns 503 for queries until restart.

**Dependencies.** `tokio::process`, `tokio::net::TcpListener` for port discovery, existing config loader.

---

### C2. SQL repository seam (`MirrorRepo` trait)

**Purpose.** Single point that all backend SQL flows through, so we can swap Dolt vs SQLite.

**Location.** Refactor existing `frankweiler/backend/core/src/db.rs` and `core/src/query.rs`.

**Public API.**
```rust
trait MirrorRepo: Send + Sync {
    async fn search(&self, q: &Query) -> Result<Vec<SearchRow>>;
    async fn columns(&self) -> Result<Vec<ColumnMeta>>;
    async fn get_chat(&self, uuid: &Uuid) -> Result<ChatMeta>;
    async fn get_media(&self, uuid: &Uuid) -> Result<MediaBlob>;
    async fn insert_feedback(&self, row: &FeedbackRow) -> Result<()>;
}
```

**Implementations.**
- `DoltRepo` — `sqlx::MySqlPool` against `DoltServer::mysql_url()`. Default and only production backend.
- `SqliteRepo` — `sqlx::SqlitePool` against `mirror.sqlite` (read-only). Reference / backwards-compat only. `insert_feedback` returns `Err(UnsupportedBackendError)`.

**Wiring.** `AppState` holds `Arc<dyn MirrorRepo>` instead of `rusqlite::Connection`. Selection happens once at startup: backend always tries Dolt first; SQLite is reachable only via an explicit `--backend sqlite` CLI flag for debugging.

**Behavior.**
- SQL strings are written in MySQL-compatible dialect where possible; per-backend branches only where dialects diverge (e.g. `JSON_EXTRACT` semantics, parameter binding).
- Existing query-builder grammar in `query.rs` (filters, columns, sort) is preserved; only the executor is swapped.

**Dependencies.** New crate deps: `sqlx` with `mysql`, `sqlite`, `runtime-tokio-rustls`, `json`, `uuid` features. Drop `rusqlite` once SQLite reads run through `sqlx::SqlitePool`.

---

### C3. `feedback` table schema + codegen

**Purpose.** Strongly-typed cross-language definition of the feedback row and its discriminated surface payload.

**Location.** New `schemas/feedback.schema.json`. Wired into `schemas/BUILD.bazel` codegen the same way `grid_rows.schema.json` is.

**Table shape (typed columns).**

| Column | SQL type | Notes |
|---|---|---|
| `feedback_uuid` | `VARCHAR(36)` PK | client-generated UUIDv4 |
| `created_at` | `VARCHAR(40)` | ISO-8601 with local offset (per AGENTS.md timestamp convention) |
| `sentiment` | `VARCHAR(8)` NULL | `"up"`, `"down"`, or NULL |
| `comment` | `TEXT NOT NULL` | non-empty (validated in UI and server) |
| `app_version` | `VARCHAR(32)` | from `CARGO_PKG_VERSION` |
| `git_hash` | `VARCHAR(40)` | from Bazel stamp; `"unknown"` allowed |
| `context_json` | `JSON NOT NULL` | see surface payload below |

**`context_json` shape (discriminated union).**

Top-level fields always present:
- `url` (string) — `window.location.href` at submit time.
- `surface` (string enum) — discriminator.
- `dom_path_breadcrumb` (array of `{tag, id?, classes?, data}`)
- `dom_path_selector` (string) — flat CSS selector.
- `target_uuids` (string[]) — all UUIDs the feedback applies to.

Per-surface payload (`payload` field is typed on `surface`):
- `grid_cell` — `{ column: string, row_uuids: string[], cell_value?: string }`
- `grid_row` — `{ row_uuids: string[] }` (Lightroom multi-select expansion)
- `preview_message` — `{ conversation_uuid: string, message_uuid: string, message_index: number }`
- `preview_selection` — `{ conversation_uuid: string, start_message_uuid: string, end_message_uuid: string, selected_text: string }`
- `page_header` — `{ entity_kind: "conversation" | "...", entity_uuid: string }`
- `filter_chip` — `{ key: string, value: string }`
- `column_header` — `{ key: string }`

**Codegen outputs.** Rust struct + DDL, Python dataclass + DDL, TypeScript types (no DDL). Discriminated union surfaces as `enum` in Rust, `Literal`-tagged dataclasses in Python, discriminated union in TS.

**Dependencies.** Extend `schemas/codegen.py` if necessary to support tagged unions; existing `x-mapping` machinery is not needed here.

---

### C4. `POST /api/feedback` HTTP endpoint

**Purpose.** Receive a feedback payload from the UI and insert it via `MirrorRepo::insert_feedback`.

**Location.** Add to `frankweiler/backend/http/src/lib.rs`.

**Route.** `POST /api/feedback` — `application/json` body matches the codegenned TypeScript type from C3.

**Behavior.**
- Validates `comment` non-empty, `sentiment` in `{"up","down",null}`, `surface` is a known variant, `target_uuids` is non-empty.
- Generates `feedback_uuid` server-side as UUIDv4. (Client may pass one; if so, validated.)
- Sets `created_at = chrono::Local::now().format ISO-8601` per AGENTS.md timestamp convention.
- Stamps `app_version` (from `CARGO_PKG_VERSION`) and `git_hash` (from build stamp, see C7).
- Calls `repo.insert_feedback(row).await`.
- On success, runs `CALL DOLT_COMMIT('-Am', 'feedback: <uuid>')` against Dolt (no-op on SQLite path, which is rejected upstream anyway).
- Returns `201 { feedback_uuid }` on success, `4xx` on validation, `5xx` on backend error.

---

### C5. Git-hash stamping via Bazel workspace_status_command

**Purpose.** Inject the current git hash into the backend binary so it can be written to every feedback row.

**Approach.**
- Add `tools/workspace_status.sh` that prints `STABLE_GIT_HASH <sha>` (plus `--dirty` suffix if working tree dirty).
- Wire it via `--workspace_status_command=tools/workspace_status.sh` in `.bazelrc`.
- The Rust binary reads the stamp file (path passed via `env!("BAZEL_STABLE_GIT_HASH")` written by a small `genrule` or `cargo_build_script`) at startup.
- Non-Bazel builds (e.g. `cargo run` for local dev) fall back to `"unknown"`.

**Exposure.** `core::version::git_hash() -> &'static str` — used by C4.

---

### C6. `FeedbackModal.vue`

**Purpose.** Modal dialog used by every entry point.

**Location.** New `frankweiler/ui/src/components/FeedbackModal.vue`.

**Props.**
- `open: boolean`
- `context: FeedbackContext` — fully populated `context_json` (sans `comment` and `sentiment`).

**Emits.**
- `submit(payload: FeedbackPayload)` — parent handles the API call.
- `close()` — Cancel button, Esc, backdrop click.

**Behavior.**
- Textarea for `comment` (autofocus on open).
- Thumb-up / thumb-down toggle (mutually exclusive, deselectable — third click clears).
- Submit disabled while `comment.trim()` is empty.
- `Cmd/Ctrl+Enter` submits when valid; `Esc` cancels.
- A small collapsible "Captured context" disclosure shows the JSON for the user to verify.

**Style.** Reuses the CSS conventions from `SearchView.vue`'s `.ctx-overlay` / `.ctx-menu` for backdrop + box.

---

### C7. Context-collection utility

**Purpose.** Single point where the front-end assembles a `FeedbackContext` from a right-click event so every entry point is consistent.

**Location.** New `frankweiler/ui/src/feedback/context.ts`.

**Public API.**
```ts
export function buildContext(
  surface: SurfaceVariant,
  event: MouseEvent,
  payload: SurfacePayloadFor<SurfaceVariant>,
): FeedbackContext;
```

**Behavior.**
- Reads `window.location.href` for `url`.
- Walks `event.target` up to `[data-feedback-root]` (or `<body>`) emitting both:
  - `dom_path_breadcrumb`: array of `{tag, id, classes, data}` (data-attributes only; no PII).
  - `dom_path_selector`: a compact CSS selector string for the same path.
- Captures `window.getSelection()` if `surface === "preview_selection"`.
- All types are imported from the codegenned TS module from C3 (single source of truth).

---

### C8. `api.ts` — `submitFeedback()`

**Purpose.** Thin client wrapper.

**Location.** `frankweiler/ui/src/api.ts` — add alongside `fetchSearch()`.

**Signature.**
```ts
export async function submitFeedback(payload: FeedbackPayload, signal?: AbortSignal): Promise<{ feedback_uuid: string }>;
```

Mirrors the existing `getJson` / fetch pattern. POSTs to `/api/feedback`.

---

### C9. Grid integration — `SearchView.vue` context menu

**Purpose.** Add the "Feedback…" item to the existing menu, plus filter-chip and column-header surfaces.

**Changes.**
- Add a `<div class="ctx-item">Feedback…</div>` block alongside the existing "Copy UUID(s)" / "Open in Slack" entries (`SearchView.vue:588-615`).
- Use the existing `resolveTargetRows()` for Lightroom semantics (do not re-implement) and re-emphasize the semantics in a comment at the new menu-item handler — "this respects the Lightroom rule from `resolveTargetRows`; see `SearchView.vue:94-96`."
- When the right-click happened on a filterable cell, the `surface` is `grid_cell` (with `column` and `cell_value` from existing `contextFilter`); otherwise `grid_row`.
- Filter-bar chips and column headers get their own right-click handler emitting `surface: filter_chip` / `column_header` respectively.
- On menu-item click, build context via C7 and open `FeedbackModal` (C6).

---

### C10. Preview-pane integration — `ChatBody.vue` / `ChatPreviewPane.vue` / `ChatView.vue`

**Purpose.** Add a context-menu handler to the preview pane and resolve the target message or selection.

**Changes.**
- New `@contextmenu.prevent` handler on the root of `ChatBody.vue`'s rendered `v-html` container.
- Resolution rule, in order:
  1. If `window.getSelection()` is non-empty and inside the chat body, build `preview_selection` with `selected_text`, plus the start/end message UUIDs derived from the closest `[data-msg-index]` ancestor of the anchor and focus nodes.
  2. Else, walk up from `event.target` to the nearest `[id^="m-"][data-msg-index]` and build `preview_message`.
  3. Else, fall back to `surface: page_header` with the conversation UUID.
- Open `FeedbackModal` with the resulting context.
- `ChatView.vue` and `ChatPreviewPane.vue` get a small "Feedback" button next to the existing `CopyUuidButton` / "Open in …" links — that's the `page_header` entry point.

---

### C11. App-version exposure to the UI

**Purpose.** Stamp `app_version` from the **same** value the server uses.

**Approach.** The UI does not need to know the version directly — `app_version` and `git_hash` are stamped by the server in C4. The submit payload from the UI carries only the user-controllable fields plus `context_json`; the server fills in the rest.

(No UI change needed beyond not sending these fields. Kept as its own component spec because it's a deliberate choice — the alternative would have been a `/api/version` endpoint and client-side stamping.)

---

## Task list

Tasks are grouped by phase. Each task is small enough to ship/review independently. ★ marks a task that *must* land before downstream tasks compile.

### Phase 1 — Backend cutover to Dolt

1. ★ **Add `dolt_repo_path` and (optional) `dolt_binary` to backend config loader.** Mirror the `~/.config/mixed-up-files/config.yaml` schema ingest already uses.
2. ★ **Implement `DoltServer` (C1).** Random-port discovery, spawn, readiness probe, drop-on-shutdown, log piping.
3. ★ **Add `sqlx` dep** with `mysql`, `sqlite`, `runtime-tokio-rustls`, `json`, `uuid` features. Remove `rusqlite` once tasks 5–6 land.
4. ★ **Introduce `MirrorRepo` trait** (C2) and refactor existing `search`, `columns`, `get_chat`, `get_media` call sites in `db.rs` / `query.rs` / `http/src/lib.rs` to go through it. **No behavior change yet** — only the seam.
5. **Implement `DoltRepo`** with `sqlx::MySqlPool`. Port each query string from `db.rs` to MySQL-compatible SQL.
6. **Implement `SqliteRepo`** with `sqlx::SqlitePool` (read-only). Verify byte-for-byte parity with the previous `rusqlite` behavior via existing snapshot tests.
7. **Wire backend startup** to default to `DoltRepo` and accept `--backend sqlite` for debug/reference.
8. **Integration test:** start backend with a tiny Dolt repo fixture, hit `/api/health` + `/api/search`, confirm parity with the SQLite path.

### Phase 2 — Schema, stamping, and write endpoint

9. ★ **Add `schemas/feedback.schema.json`** (C3), including discriminated-union surface payload. Extend `schemas/codegen.py` if tagged unions aren't supported.
10. ★ **Wire codegen** in `schemas/BUILD.bazel` for the feedback schema; emit Rust / Python / TS artifacts.
11. ★ **Bazel workspace_status_command** (C5): add `tools/workspace_status.sh`, register in `.bazelrc`, surface `STABLE_GIT_HASH` to the Rust binary, expose `core::version::git_hash()`. Fall back to `"unknown"` for `cargo run`.
12. **Implement `DoltRepo::insert_feedback`** with parameterized INSERT, followed by `CALL DOLT_COMMIT('-Am', 'feedback: <uuid>')`.
13. **Add `POST /api/feedback`** in `http/src/lib.rs` (C4). Validation, server-side `feedback_uuid` + `created_at` + `app_version` + `git_hash` stamping.
14. **Integration test** for `POST /api/feedback` end-to-end: payload → row in Dolt → row visible via `dolt log` + `dolt sql -q "SELECT * FROM feedback"`.

### Phase 3 — UI: modal + context plumbing

15. ★ **`FeedbackModal.vue`** (C6): markup, props, emits, keyboard handlers, "Captured context" disclosure.
16. ★ **`feedback/context.ts`** (C7): `buildContext`, DOM-path walker (breadcrumb + selector), selection capture, type imports from codegenned TS.
17. **`api.ts::submitFeedback`** (C8).
18. **`data-feedback-root` attribute** on the app root so DOM-path walkers have a stable stop boundary.

### Phase 4 — UI: entry points

19. **Grid (C9):** add "Feedback…" item to `SearchView.vue`'s context menu; reuse `resolveTargetRows()`; emit `grid_cell` vs `grid_row` based on the existing `contextFilter` presence. Re-quote the Lightroom comment at the new call site.
20. **Filter-bar chips + column headers (C9):** add right-click handler on each; surface `filter_chip` / `column_header` payloads. Where multi-selection exists, apply the same Lightroom rule.
21. **Preview pane context menu (C10):** add `@contextmenu.prevent` to `ChatBody.vue`'s root, implement the selection-vs-message-vs-page resolution cascade.
22. **Page-header Feedback affordance (C10):** add a "Feedback" button next to `CopyUuidButton` in `ChatPreviewPane.vue` and `ChatView.vue`.

### Phase 5 — Verification

23. **Visual / manual pass:** file feedback from each surface (`grid_cell`, `grid_row`, `preview_message`, `preview_selection`, `page_header`, `filter_chip`, `column_header`); confirm each row in Dolt has the right `surface` discriminator and a recoverable `context_json`.
24. **`bazelisk test //...`** clean (per AGENTS.md "default to bazelisk").
25. **Update `AGENTS.md`** with: "the app talks to Dolt at runtime; `mirror.sqlite` is now reference-only" and the `feedback` table semantics.

---

## Open questions

- **Dolt repo location and writability.** Ingest currently writes to a Dolt repo (path TBD in `config.yaml`); does the *running* app open the same repo, or a sibling working copy? If shared, what happens if `ingest` and the running backend both have `dolt sql-server` instances open at once? (Dolt allows one server per repo path.)
- **Schema migrations.** `feedback` is a brand-new table — how do we deploy the DDL into an existing Dolt repo on first launch? Option: backend runs `CREATE TABLE IF NOT EXISTS feedback (...)` at startup, derived from the codegenned DDL constant; same idempotent pattern ingest already uses for `grid_rows`.
- **PII in DOM data-attributes.** The DOM-path breadcrumb captures `data-*` attributes. Some of those carry UUIDs and user-visible text (e.g. `data-uuid`, conversation names). Acceptable since feedback is local-only — but worth a one-line acknowledgement in the schema doc.
- **Selection across messages.** When a text selection spans multiple `[data-msg-index]` wrappers, do we want to enumerate *all* intermediate message UUIDs in `target_uuids`, or only the endpoints?
- **Dolt commit author identity.** `DOLT_COMMIT('-Am', '...')` will use whatever git/dolt config is on the host. Do we set an explicit `--author` for app-generated commits to distinguish them from ingest-driven commits?
- **What about the existing context-menu items copying multiple UUIDs?** They already use `resolveTargetRows`. We can also offer "Copy as feedback context (JSON)" later as a low-cost debugging aid — out of scope here, but worth flagging.
