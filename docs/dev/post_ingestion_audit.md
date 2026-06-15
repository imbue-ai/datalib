# Post-ingestion architecture audit

Audit of the codebase against
[`post_ingestion_architecture.md`](post_ingestion_architecture.md),
produced 2026-06-11.

Unlike the ingestion audit (which sliced by provider), this one
slices by **principle**, since most post-ingestion principles are
cross-cutting rather than per-provider. For each principle: what
holds today, what's violated or missing, and simplification
opportunities, with file:line evidence. Sections were produced by
audit agents reading the architecture doc against one slice of the
tree; expect some duplication at the margins.

Many findings land as "aspirational, not yet built" rather than
"violated" — the architecture doc is younger than the code and says
so. The distinction drawn below: a **violation** is code that
actively contradicts a principle (and will hurt later); a **gap** is
a principle with no implementation yet. The plan
([`post_ingestion_plan.md`](post_ingestion_plan.md)) sequences both.

---

## The thing model and links

### What holds

- **Doc/span edges work end-to-end.** The `edges` table
  ([`schemas/edges.schema.json`](../../schemas/edges.schema.json)) is
  created by load
  ([`load.rs:320-337`](../../frankweiler/backend/etl/src/load.rs)),
  written from sidecars (`insert_edge`, load.rs:758-777), read by
  the backend (`outgoing_edges` in
  [`dolt_repo.rs`](../../frankweiler/backend/core/src/dolt_repo.rs),
  served on `/api/chat/{uuid}` at
  [`http/src/lib.rs:379-383`](../../frankweiler/backend/http/src/lib.rs)),
  and rendered by the UI (`ChatBody.ce.vue:219-245` decorates
  `[data-section-uuid]` matches; hover-highlight across columns at
  lines 40-65; click opens the destination column). The "fiber"
  exists for documents and spans.
- **Row → document linking works** via `GridRow.markdown_uuid`.
- **Only Perseus emits edges** (`book_edges()` in
  `providers/perseus/src/translate/render.rs:240-269`). No other
  producer; the table is empty for every other provider. Expected —
  but it means the edge machinery has exactly one consumer-tested
  path.

### Violations

- **Load destroys edges indiscriminately on re-render.**
  `apply_markdown()` runs
  `DELETE FROM edges WHERE src_markdown_uuid = ?` then re-inserts
  the sidecar's edges (load.rs:667-673). Today every edge is derived
  so nothing is lost — but the moment any user- or agent-authored
  edge exists, **a rebake of the source document silently deletes
  it**. This is the doc's "derived is disposable, authored is
  sacred" principle violated in waiting. It should be fixed *before*
  authored edges ship, not after.
- **No `provenance` column on `edges`.** The architecture doc calls
  for `(src_ref, dst_ref, label, provenance)`; the schema has
  `(edge_uuid, src_markdown_uuid, src_anchor_uuid?,
  dst_markdown_uuid, dst_anchor_uuid?, label?)` — no way to mark an
  edge derived vs authored, which is the prerequisite for fixing the
  deletion bug above.

### Gaps

- **No generalized thing reference.** Edges hardcode
  `markdown_uuid` columns, so only documents and spans are
  linkable. A row in `grid_rows` is not an edge endpoint; a row in
  an arbitrary-schema table has no addressable identity at all; a
  blob (blake3) can't be linked. The doc lists the reference format
  as unresolved; nothing in the code anticipates it (e.g. the edge
  columns are typed `VARCHAR(96)` uuid slots, not opaque refs).

---

## Derived is disposable, authored is sacred

### What holds

- **An authored-data precedent already exists: `feedback`.**
  [`schemas/feedback.schema.json`](../../schemas/feedback.schema.json)
  — user-filed feedback rows, written by `POST /api/feedback`
  (http/src/lib.rs:436-478) with a per-row
  `DOLT_COMMIT('-Am', 'feedback: <uuid>')` for an audit trail
  (dolt_repo.rs:148-166). This is genuine authored data flowing
  through the system today.

### Violations

- **Authored and derived data share one database with no
  protection.** `feedback` (authored), `sync_jobs` and
  `download_runs` (operational/audit), and `grid_rows` /
  `markdowns` / `edges` (derived) all live in
  `backend_index.doltlite_db` (dolt_repo.rs:97-123). There is no
  rebake-exclusion mechanism: any future "wipe the index and
  re-load" path has to know, table by table, what it may truncate.
  Nothing encodes that knowledge today — it's only convention that
  no reset path currently touches `feedback`.

### Gaps

- **No designated authored store**, no rebake contract, no answer
  for how authored data participates in the `cp -r <data_root>`
  portability principle. The doc lists this unresolved; the audit
  confirms nothing in code contradicts whatever we pick, *except*
  the edge-deletion path above, which must learn provenance either
  way.

---

## Translate is an open interface

### What holds

- **Load is genuinely open-world.** `collect_sidecars()`
  (load.rs:559-573) walks **all** of `rendered_md/` recursively and
  loads every `*.grid_rows.json` it finds, regardless of provider
  directory or who wrote it. An external tool can drop a conforming
  sidecar tree anywhere under `rendered_md/` today and it loads —
  no registration, no manifest. This is the architecture's biggest
  already-true claim.
- **Fingerprints are opaque to load.** The skip check
  (load.rs:515-519) compares the sidecar's `source_fingerprint`
  string against `markdowns.source_fingerprint` — any opaque,
  stable string works. An external tool doesn't need our hash
  algorithm; it needs *a* deterministic string. (This should be
  documented as a feature, not left implicit.)
- **Subprocess precedents exist** for "the pipeline shells out":
  `latchkey curl` for authed HTTP (gitlab `client.rs:58` et al.),
  `sqlite3` CLI piping (beeper `index_db.rs:99-108`), `npx qmd`
  for the search index (qmd_indexer `lib.rs:85-138`). The
  invocation patterns an external-translate contract needs are
  already in the codebase, just not applied to translate.

### Gaps

- **No way to register an external translate step.** `SourceConfig`
  ([`config.rs:480-588`](../../frankweiler/backend/core/src/config.rs))
  has only built-in provider variants — no `External` / `Script` /
  `Command` type. Translate dispatch is one hardcoded match over
  providers (`sync/src/main.rs:2147-2400`). The only path to a
  custom translate step is editing this repo's Rust — exactly what
  the architecture doc says must not be the only path.
- **No per-source standalone translate.** `--skip-extract`
  (sync/src/main.rs:138-147) re-translates *all* enabled sources;
  there is no "run translate for source X only" invocation, which
  both humans iterating on a renderer and external tools need.
- **The sidecar contract is not documented Rust-free.**
  [`schemas/grid_rows.schema.json`](../../schemas/grid_rows.schema.json)
  covers the rows, but there is **no schema or doc for the sidecar
  envelope** (`header` + `rows` + `edges`); the semantics of
  `markdown_uuid` determinism, `source_fingerprint` (opaque skip
  token), and `render_version` live only in Rust doc-comments
  ([`index_lib/src/lib.rs:9-77`](../../frankweiler/backend/index_lib/src/lib.rs),
  load.rs:340-345). A Python author can get the row shape from the
  JSON schema but must read Rust to learn the invariants that make
  re-runs converge.
- **`render_version` is recorded but never read.** Load stores the
  sidecar header's `render_version` into `markdowns` but the skip
  logic checks only `source_fingerprint` (load.rs:517). The
  ingestion doc presents `RENDER_VERSION` as the rebake lever — in
  practice the lever only works because providers mix it into their
  fingerprints. That convention is load-bearing and entirely
  implicit. Either load should honor the field or the spec should
  say plainly "version goes into your fingerprint; the header field
  is informational."
- **Sidecar parsing is silently permissive.** Unknown JSON fields
  are dropped (serde default), missing `edges` defaults to empty
  (index_lib `lib.rs:64-77`), and a malformed or
  future-versioned sidecar fails with a parse error rather than a
  diagnosable "this sidecar speaks a newer contract." Permissive is
  the right default for vibe-coded producers; *silent* is not.

---

## The files are the API

### What holds — this principle is in good shape

- **The backend holds no private state.** The HTTP server opens one
  `DoltRepo` over `backend_index.doltlite_db` (http/src/main.rs:85-93)
  and keeps nothing in memory across requests; the sync orchestrator
  builds per-run maps and discards them. `/api/accounts` is a
  pass-through of `accounts.json` on disk (http/src/lib.rs:181-191).
- **The qmd index is honestly disposable** — a derived
  BM25+embedding index over `rendered_md/**/*.md` at
  `<root>/qmd/index.sqlite`, rebuildable by hand with the stock qmd
  CLI (qmd_indexer `lib.rs:85-138`). No state duplication.
- **No hidden write paths.** UI writes are exactly `POST
  /api/feedback` (doltlite + commit) and `POST /api/card`
  (content-addressed file); no edit/delete endpoints exist
  (route table at http/src/lib.rs:152-179).

### Gaps

- **Card sources are the one user artifact not (usefully) on
  disk.** Card source lives in URL state
  (`ui/src/router/columns.ts:10-34`); `POST /api/card` persists it
  only as `<root>/.frankweiler/cards/<sha256>.js`
  (http/src/lib.rs:494-521) — content-addressed, hash-named,
  no enumeration, retrievable only if you already know the hash.
  For the files-are-the-API rule (and the agent principle below),
  this is a miss: the user's authored views are effectively trapped
  in browser state.

---

## Presentation: the grid as a database explorer

### What holds

- The grid_rows path is solid: one query path
  (`DoltRepo::search`, dolt_repo.rs:148-170), AG Grid rendering,
  link-aware document columns.
- Card composition works as designed: two factories (`gridView`,
  `documentView` in `ui/src/cards/libs/index.ts`), arbitrary JS
  expressions compiled by `compileCardSource`
  (`cardSource.ts:10-25`).

### Gaps — the explorer is entirely aspirational

- **Everything is hardcoded to `grid_rows`.** The search SELECT
  names its 20 columns (dolt_repo.rs:148-170); the filter language
  enumerates grid_rows fields (`query.rs:20-70`); `/api/columns`
  returns a hardcoded manifest (http/src/lib.rs:572-594). There is
  no endpoint to list tables, describe a schema, or read rows from
  an arbitrary table in an arbitrary doltlite db — so a translate
  step that emits its own database is *legal* (per the conformance
  ladder) but **invisible**: nothing in the built-in UI can show
  it. Today the bottom rung of pay-as-you-go conformance buys
  nothing.
- **No `tableView()` card factory** or any generic-table component;
  the factory surface is exactly the two blessed views.

---

## Open to agent participation

### What holds

- `AGENTS.md` exists and orients an agent on repo layout, grid_rows,
  feedback persistence, conventions.
- All in-repo code, schemas, and configs are plain files an agent
  can read and edit.

### Gaps

- **No extension-building skills.** Nothing tells an agent how to
  build a translate step or author a card: no SKILL.md, no
  `skills/` tree, `.claude/` is empty, and `AGENTS.md` covers
  operating in this repo, not extending the system from outside it.
  Blocked in part on the Rust-free sidecar spec (above) — there is
  currently nothing correct to point a skill *at*.
- **Admitted code is not agent-reachable.** Card sources (URL
  state / hash-named files, above) cannot be discovered, inspected,
  or modified by an external agent. There are no registered
  external translate steps yet, so that half is moot until the
  config hook exists.

---

## Trust model

### What holds

- `compileCardSource` evaluates card JS via
  `new Function(...names, '"use strict"; return (<source>)')` with
  only the `viewLibs` factories in scope plus JS globals; rendering
  is confined to a per-card ShadowRoot (`cardSource.ts:14-18`,
  `vueCard.ts`). No fetch/localStorage/window handed in. Consistent
  with the stated shell-equivalent trust model for a single user.

### Notes

- Card source traveling in URLs means **opening a crafted URL is
  executing code**. Acceptable under the current trust model and
  flagged as unresolved in the architecture doc; becomes urgent the
  moment URLs are shared between people. No action needed now; do
  not ship URL-sharing features before revisiting.

---

## Summary

| Principle | Status | Sharpest finding |
|---|---|---|
| Things and links | Partial | Doc/span edges work; no provenance column; no generalized ref; load deletes edges indiscriminately (load.rs:667) |
| Derived vs authored | Violated-in-waiting | `feedback` (authored) shares a db with derived tables; no rebake-exclusion anywhere; authored edges would be destroyed on rebake |
| Translate openness | Half-true | Load is already open-world (collect_sidecars walks everything); but no config hook, no standalone invocation, no Rust-free spec |
| Files are the API | Held | Backend stateless over doltlite files; one miss: card sources trapped in URL/hash-store |
| Grid as explorer | Aspirational | Everything hardcoded to grid_rows; arbitrary-db outputs are legal but invisible |
| Agent participation | Aspirational | No skills; no spec to point them at; admitted code not agent-reachable |
| Trust model | Held | Sandboxed eval consistent with stated model; URL-sharing is the tripwire |

The recurring shape: **the seams the architecture promises mostly
exist and are honest (load's open-world walk, opaque fingerprints,
stateless backend), but every contract is implicit in Rust, and the
two "sacredness" mechanisms (edge provenance, authored store) are
missing entirely.** The first is documentation-and-config work; the
second is bytes-at-rest work that gets more expensive the longer
authored data accumulates without it.

The plan of attack derived from these findings:
[`post_ingestion_plan.md`](post_ingestion_plan.md).
