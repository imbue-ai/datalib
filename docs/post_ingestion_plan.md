# Post-ingestion plan of attack

Plan derived from the audit
([`post_ingestion_audit.md`](post_ingestion_audit.md)). Companion to
[`post_ingestion_architecture.md`](post_ingestion_architecture.md)
(the principles) and the audit (the findings). Parallel in spirit to
[`data_architecture_plan.md`](data_architecture_plan.md) on the
ingestion side.

Priorities below are proposals pending review — unlike the ingestion
plan, this one hasn't absorbed a round of inline comments yet.

## Guiding priorities

1. **Don't strand authored data.** The post-ingestion analogue of
   "bytes at rest come first": provenance markers and the authored
   store are schema decisions whose cost grows with every authored
   row created before they land. The edge-deletion bug
   (load.rs:667) must be fixed *before* anything ships that lets a
   user or agent author an edge.
2. **Contracts before conveniences.** Publish the seams (sidecar
   spec, config hook, thing refs) before building features on top
   of them. A written contract makes every downstream item — the
   skills, the explorer, external tools — buildable by anyone,
   including agents; an unwritten one makes them all blocked on us.
3. **Every item must beat the baseline.** Per the architecture doc's
   minimal-plumbing principle: "take the ingested data and run" is
   always valid, so each plan item should say what it's better
   *at* than that. Items that can't, get cut.
4. **Agent-legibility is part of done.** A contract that an agent
   can't discover and follow from a skill isn't finished.

---

## P0 — contracts and sacredness

### P0.1 Rust-free sidecar specification

**Today**: the sidecar envelope and its invariants exist only as
Rust doc-comments (`index_lib/src/lib.rs`, `load.rs:340-345`);
[`schemas/grid_rows.schema.json`](../schemas/grid_rows.schema.json)
covers rows but not the envelope. An external author must read Rust
to learn what makes re-runs converge.

**Goal**:

- A `schemas/sidecar.schema.json` for the envelope (`header
  {markdown_uuid, source_fingerprint, render_version}, rows,
  edges?`), referenced from the codegen pipeline like the others.
- A `docs/sidecar_format.md` stating the semantics the schema
  can't: `markdown_uuid` must be deterministic across re-runs
  (UUIDv5 recommended, recipe documented); `source_fingerprint` is
  an **opaque** string — load only ever compares it for equality,
  any stable digest works; renderer-version changes must be mixed
  into the fingerprint (state plainly that load does not read the
  header `render_version` field — see P0.5); file layout
  (`<id>.md` + `<id>.grid_rows.json`, anywhere under
  `rendered_md/`); the pay-as-you-go ladder.
- The Rust `Sidecar` struct gains a comment pointing at the spec as
  the source of truth, not vice versa.

**Why first**: cheapest item with the most downstream unblocks —
P1.2 (skills), P0.4 (config hook), and any external tool all need
something correct to point at. The audit confirmed the *behavior*
is already right (load is open-world, fingerprints are opaque);
this is writing down what the code already promises.

### P0.2 Edge provenance + rebake-safe edge application

**Today**: `edges` has no provenance column, and
`apply_markdown()` deletes **all** edges with a matching
`src_markdown_uuid` before re-inserting the sidecar's
(load.rs:667-673). Any future authored edge dies on the next rebake
of its source document.

**Goal**:

- Add a `provenance` column to `edges`
  (`derived` | `authored:<agent>` — exact vocabulary to be
  designed; the schema change is additive, per the
  schema-evolution principle).
- Load's delete narrows to
  `DELETE FROM edges WHERE src_markdown_uuid = ? AND provenance =
  'derived'`.
- Sidecar-emitted edges are stamped `derived` at insert; the
  authored write path (P0.3) stamps everything else.

**Why now**: bytes-at-rest shape. Trivial while the table has one
producer (Perseus) and no authored rows; a migration headache
later. This lands independently of — and before — the full
thing-reference redesign (P0.6); the existing markdown/anchor
columns stay as they are for now.

### P0.3 The authored-data store decision

**Today**: `feedback` (authored) lives in
`backend_index.doltlite_db` beside derived tables; nothing encodes
which tables a rebake may truncate (dolt_repo.rs:97-123).

**Goal**: decide and document, then do the (small) code motion:

- **Where authored data lives.** Leading option: a separate
  `<data_root>/authored.doltlite_db` holding `feedback`, authored
  edges, and future annotations — making "derived is disposable" a
  *file-level* property (`backend_index.doltlite_db` becomes
  safe to delete wholesale). Alternative: stay in the backend
  index with per-table sacredness markers; cheaper now, but every
  future reset path must consult the marker.
- **The rebake contract**: a stated, single list of what a full
  re-load may wipe.
- **Portability**: authored store participates in
  `cp -r <data_root>` backup like raw does.
- Reclassify `sync_jobs` / `download_runs` explicitly
  (operational/audit — survives rebakes, lives wherever decided).

**Why now**: same logic as P0.2 — every authored row created before
this lands is a future migration. `feedback` already accumulates.

### P0.4 External translate registration + invocation contract

**Today**: `SourceConfig` has no external variant
(config.rs:480-588); translate dispatch is a hardcoded provider
match (sync/src/main.rs:2147-2400); no per-source standalone
translate run exists.

**Goal**:

- A config shape for external steps, e.g.:

  ```yaml
  translates:
    - name: my_custom_view
      command: ["python3", "tools/my_translate.py"]
      reads: raw/slack_w.doltlite_db        # advisory
      writes: rendered_md/my_custom_view/   # or a doltlite path
  ```

- The invocation contract, documented in `docs/sidecar_format.md`
  or a sibling: args/env the orchestrator passes (data root, the
  source's raw db path), exit-code semantics (reuse the
  per-item-tolerated / fatal split from the ingestion doc), and
  progress via the existing NDJSON-on-stderr obs contract.
- **Per-source standalone translate** (`--translate-only <name>`)
  for in-tree providers too — humans iterating on a renderer need
  it as much as external tools do.
- Read-side concurrency stance: document what an external reader
  of a raw store may assume while sync runs (single-writer rule
  extended to "readers open read-only and tolerate a snapshot").

**Why P0**: this is the load-bearing half of "translate is an open
interface." Subprocess precedent already exists in-tree (latchkey
curl, sqlite3, npx qmd) — the pattern is established, just not
applied to translate.

### P0.5 Load honesty fixes (small, do alongside P0.1)

- **`render_version`**: load records it but never reads it
  (load.rs:517). Pick one: honor it in skip logic, or document it
  as informational and rely on fingerprint-mixing (the current de
  facto behavior). The spec (P0.1) must match the choice.
- **Diagnosable sidecar failures**: a sidecar that fails to parse
  should produce "this file doesn't match the sidecar contract
  (see docs/sidecar_format.md), field X" — not a bare serde error
  — and be counted in the load summary, not abort the walk.
  Permissive stays; silent goes.

---

## P1 — participation surfaces

### P1.1 Cards as named files on disk

**Today**: card source lives in URL state
(`ui/src/router/columns.ts`); `POST /api/card` writes only
hash-named blobs under `.frankweiler/cards/` with no enumeration
(http/src/lib.rs:494-521).

**Goal**: a `<data_root>/cards/<name>.js` tree as the home for
saved cards — user-named, enumerable, agent-editable plain files.
UI: save-as / load-by-name on the column header; a card factory or
URL form that references a named card. The hash store can remain as
the immutable-snapshot layer underneath, or be dropped.

**Beats the baseline at**: today even the *built-in* UI's own users
can't hand a card to an agent; this is the smallest change that
makes "allow external agents to modify and inspect custom source
code it already admits" true.

### P1.2 Skills for extension building

A skills tree (e.g. `skills/build-translate-step/SKILL.md`,
`skills/edit-cards/SKILL.md`) covering: the sidecar contract (point
at P0.1's spec), registering via P0.4's config block, the
conformance ladder, fixture conventions (TNG rule applies to
extension examples too), and where cards live (P1.1). Each skill
states which contract version it describes. `AGENTS.md` links to
them.

Blocked on: P0.1, P0.4, P1.1 (each skill needs a true contract to
describe — write each skill as its dependency lands, not as one
batch at the end).

### P1.3 Schema discovery + generic table browsing (explorer MVP)

**Today**: every endpoint and the filter language are hardcoded to
`grid_rows` (dolt_repo.rs:148-170, query.rs:20-70,
http/src/lib.rs:572-594); a translate step that emits its own
doltlite db is legal but invisible.

**Goal**, Datasette-shaped, read-only:

- `GET /api/dbs` — doltlite files under the data root;
  `GET /api/dbs/{db}/tables` — tables + columns (sqlite
  introspection); `GET /api/dbs/{db}/{table}/rows` — paged reads
  with simple column filters. Read-only by construction.
- A `tableView(db, table)` card factory rendering any such table
  in AG Grid, degrading gracefully when no universals are present.
- Universals enhancement (sortable timeline when a `when_ts`-shaped
  column is declared, followable links) comes *after* the
  declaration convention (P1.4) — the MVP is plain rows.

This is the item that makes the bottom rung of pay-as-you-go
conformance actually pay.

### P1.4 The universals declaration convention

Decide how an arbitrary table declares its universals: magic column
names (`uuid`, `when_ts`, `text`) vs a small per-db manifest table.
Resolve together with "what does full-text search index in an
arbitrary table." Decision feeds P1.3's enhancement layer and the
eventual generalized search. (Architecture doc lists this
unresolved; the plan item is to *decide*, prototype on one external
table, and write it into the spec.)

### P1.5 Reframe the ingestion doc's shared-schema families

Per the architecture doc's stance section: edit
[`data_architecture_ingestion_practices.md` §Shared schemas](data_architecture_ingestion_practices.md#shared-schemas-across-similar-sources)
from *requirement* ("should be massaged into a shared canonical
schema") to *convention with benefits* (the `grid_rows` profile and
family shapes are what you opt into for the union grid, search, and
shared rendering). Also fix its two stale `sidecar.rs` links
(the struct lives in `index_lib/src/lib.rs`).

---

## P2 / later (brief)

- **The thing-reference format + generalized edges** — design the
  ref form covering rows/documents/spans/blobs, then migrate
  `edges` to `(src_ref, dst_ref, label, provenance)`. Deliberately
  *after* P0.2 (provenance is additive now; the ref redesign wants
  the universals convention from P1.4 settled first).
- **Authored-edge UI** — create/label links from the UI and from
  agents; depends on P0.2/P0.3.
- **Graph navigation beyond doc/span** — row→row links in the grid;
  depends on the ref format.
- **More view factories** as real needs appear (timeline, graph) —
  resist building speculatively; cards can compose `tableView` +
  `documentView` for a while.
- **Card/URL sharing trust story** — required before any sharing or
  sync feature; explicitly not before.
- **Search across arbitrary tables** — after P1.4.

---

## Open questions to resolve before P0 work starts

1. **P0.3's fork**: separate `authored.doltlite_db` vs sacredness
   markers in the backend index. The plan leans separate-file
   (sacredness as a file-level property is harder to get wrong);
   needs an explicit call.
2. **Provenance vocabulary** (P0.2): is `derived`/`authored` enough,
   or do we want the authoring principal (`authored:user`,
   `authored:<tool>`) from day one? Day-one principal is cheap and
   hard to retrofit.
3. **Does `feedback` move** when P0.3 lands, or is it grandfathered
   where it is with a marker?
4. **External translate scheduling** (P0.4): do external steps run
   inside `frankweiler-sync`'s normal walk (after their source's
   extract), or only on demand? Proposal: in the walk, with a
   per-step `enabled:`/`manual:` flag.
5. **`render_version`** (P0.5): honor or document-as-informational?
   Proposal: document-as-informational — fingerprint-mixing is
   already the universal provider behavior and is simpler for
   external tools.

---

## Explicit "do not do" list

- **No in-process plugin API.** The unit of extension is a separate
  program speaking files (P0.4), not dynamic loading, not a trait
  registry, not WASM. Subprocess + documented contract only.
- **No generic SQL-over-HTTP *write* endpoint.** P1.3 is read-only.
  Writes flow through the pipeline (derived) or the authored store
  (authored); a write-anything endpoint would dissolve the
  derived/authored distinction we're building.
- **No strict sidecar validation that rejects unknown input.**
  Permissive load is a feature for vibe-coded producers. Warn and
  count (P0.5); never refuse a sidecar for unknown fields.
- **No card-JS sandboxing work now.** The shell-equivalent trust
  model holds while everything is single-user and local. Revisit
  only with sharing features (and then *before* shipping them).
- **No raw-store changes from this plan.** Everything here is
  downstream of extract; the ingestion plan owns raw.
- **No new `kind` taxonomy enforcement.** Pending the families
  reframe (P1.5), don't add validation that would reject
  open-vocabulary kinds from external producers.

---

## Proposed sequencing

1. **Week 1**: P0.1 (sidecar spec + envelope schema) with P0.5
   folded in; resolve open questions 1-3; P0.2 (provenance column +
   narrowed delete).
2. **Week 2**: P0.3 (authored store, per the week-1 decision);
   start P0.4 (config shape + invocation contract draft).
3. **Week 3**: land P0.4 with one reference external step (a
   trivial Python sidecar producer as the worked example +
   fixture); P1.1 (cards on disk).
4. **Week 4**: P1.2 (skills, now that contracts exist); P1.3
   explorer MVP.
5. **After**: P1.4 universals convention, P1.5 doc reframe, then
   P2 items as pulled.

Adjust as we get into the work and discover dependencies.
