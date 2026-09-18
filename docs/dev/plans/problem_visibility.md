# Problem visibility: every error and warning a step meets, shown to the user

**Status: built through PR 5 (2026-09-18), with a tail.** §1 is the
audit as it stood at `137cb5c1`, before any of this landed — read it
as the record of what was true then, not as a description of the
tree. §3 says per PR what landed and what is still open: the
per-provider fetch migration, R3's lossy-rules table, R4's drop
budget. Where this doc and the tree disagree, the tree wins.

## 0. What we want

The rule is already in the repo's voice — R1 in
[`data_architecture_parse_and_render.md` §4](../data_architecture_parse_and_render.md#4-data-quality-rules):
*drop, count, log; never abort, never hide*. This doc is about the
second half of that sentence, which the tree only half honours: a
problem that is counted and logged but that no screen shows is still
hidden. Concretely:

1. **On the Manage screen**, every source row carries the number of
   errors in red and warnings in yellow — or a green zero — alongside
   the columns it has now.
2. **Double-clicking that cell** opens a grid of those problems, read
   straight from the store they sit in, with the search bar's `q=`
   grammar over them.
3. **A problem about a particular record says which one**, and the grid
   and the document view hyperlink to it — the document, and the
   message inside it when the record survived as a row.
4. **A rendered document with problems shows them at the top**, before
   its body, in the same red and yellow.
5. **Every problem instance has a stable id**, deterministic in the
   inputs that produced it, so a re-run of the same code over the same
   data mints the same id, and so a filed feedback row or a URL can
   name it.
6. **Reprocessing clears what it reprocesses.** Fix the projection,
   re-run the step, and the problems on the records that run touched
   are gone — from the document, from the grid, from the counts —
   while problems on records the run did not look at stay.

## 1. What exists

There is more here than the docs admit.
`data_architecture_parse_and_render.md` §4 opens with "**Status: not
implemented**" and then, four paragraphs later, names
`GridRowBuilder::build_or_record` as R1's sink. The second sentence is
the true one; §4's banner is stale and this plan's first commit fixes
it.

### 1a. The render sink — built, and wired into every renderer

| piece | where | state |
| --- | --- | --- |
| The table: `render_problems` | `datalib/backend/schema/src/render_problems.rs` | Built. One row per *item* (`uuid` = the `grid_rows.uuid` the record would have had), a sweep key `(scope_kind, scope_key)`, a `problems` JSON list, `first_seen_at_utc` / `last_seen_at_utc` stamped by the store, `render_version`. |
| The vocabulary | same file | `Outcome {dropped, nulled, ok}`, `Reason {undeserializable, no_identity, coercion_failed, uncovered_type, deliberate_loss, noted}`, `ScopeKind {markdown, entity}`, `Stage {parse, render, grid_row}`; strum + serde with the agreement test. |
| The writer for the grid-row stage | `datalib/backend/schema/src/grid_rows_builder.rs::build_or_record` | Built and used by **every** render crate — the seven provider crates that call it directly (airvisual, garmin, github, gitlab, notion, perseus, yolink), plus every chat provider through `chat-common/src/render.rs` and every contact provider through `contact-common`. A bad `created_at` / `modified_at` is nulled and recorded; a row with no identity is dropped and recorded under a `noid:<blake3>` surrogate key. |
| The sweep | `datalib/backend/etl/render/src/indexed_markdown.rs::sweep_problems` | Built. A document that is re-rendered has its prior problem rows deleted and the new ones inserted in the same transaction, carrying `first_seen_at_utc` forward. Three tests cover it, including "a problem on a document this run skipped survives" and "a fixed document loses its problems". This is requirement 6 for the render stage, already done. |
| The entity-scoped sweep | same file, `put_entity_problems` | Built, **no callers** outside its own tests. |
| The report | `datalib/backend/datalib_step/src/render.rs:90-111` | The step ends with one `tracing::warn!` and one `progress.set_message` giving whole-store counts by outcome. That line is the whole of what a person can currently see. |
| The fixture guard | `tests/fixtures/ingested_tng_test.py:845` | Asserts the sink is **empty** for the TNG fixture, naming any row. Keeps the sink honest in one direction; means nothing in the fixture ever exercises it end to end. |
| The contract harness | `tests/fixtures/render_contract_test.py` | Knows the table and which columns are stamps. |

### 1b. Where the render sink stops

- **Keyed by item, not by instance.** A record whose `created_at` and
  `modified_at` both fail pushes two `RenderProblemRow`s with the same
  `uuid` (`grid_rows_builder.rs:229-251`), and `insert_problems` is a
  plain `INSERT` into a table whose primary key is `uuid`
  (`indexed_markdown.rs:655`). Read from the code, not run: that is a
  constraint failure on the second insert, which fails the document's
  transaction. No test covers two bad stamps on one row.
- **The parse stage is dead code.** `Problem::record`,
  `Problem::lossy` and `put_entity_problems` have no callers.
  `RenderCtx` (`etl/render/src/processor.rs`) has no way to report a
  problem that is not attached to a document. So a stored payload that
  will not deserialize is dropped with nothing recorded:
  `slack_render/src/render/parse.rs:314` and `:460`,
  `chatgpt_render/src/render/parse.rs:672`,
  `email_render/src/render/parse.rs:386` and `:408` — each a
  `let Ok(v) = serde_json::from_str(..) else { continue }`. This is
  exactly the class of loss the rule was written for.
- **No severity.** `Outcome` is data-loss semantics (was the record
  dropped, degraded, or intact), which is the right thing to store but
  not the thing a red/yellow count needs.
- **It never leaves the source's render store.** `grid_index` copies
  `markdowns`, `grid_rows`, `edges` and `measurements` into the unified
  index and deliberately not `render_problems`
  (`etl/render/src/grid_index.rs:888`). The `unified_index` applet
  reads only the index; `datalib-http` reads render stores only for
  commit history (`datalib/backend/history`). So no API serves a
  problem row, no card shows one, and `ChatResponse`
  (`applets/src/unified_index/mod.rs:528`) has no field for them.
- **R3 (the lossy-rules table) and R4 (the drop budget)** are unbuilt.
  `Problem::lossy` exists for R3 and nothing calls it.

### 1c. The ingest side — a sidecar four providers use

| piece | where | state |
| --- | --- | --- |
| Per-object bookkeeping | `etl/src/doltlite_raw.rs::record_object_attempt` | `<table>_bookkeeping.{attempt_count, last_attempt_at_utc, last_error}`; a failure before any success also inserts a stub data row with a NULL `payload`. `failed_ids` reads it back. |
| Providers that write it | `chatgpt`, `claude`, `garmin`, `notion` ingest, plus `blob_cas` | 4 of 26. |
| Everyone else | the other 22 ingest crates | A `warn!` (or in `github`/`gitlab` an `error!`) and `continue`. Counted over `providers/*/src`: 157 `warn!` sites, 8 `error!`, ~110 `Err(..) =>` branches, and no per-item record of any of them. `apple_messages`, `apple_photos`, `lightroom`, `perseus` log nothing at all on the ingest side. |

A log line goes to the run store (§1d) and is real, but it is a line
about a run, not a fact about a record: it does not say which record,
it is not cleared when the record later fetches cleanly, and it is
findable only by opening the log of the run it happened in.

The stub row that `record_object_attempt` leaves behind matters for
the plan: render reads a NULL payload as undeserializable and — today —
`continue`s past it (§1b). So a fetch failure is *already* a render
problem that nothing records. Once the parse stage sinks, a never-fetched
record surfaces there by itself, under the right scope.

### 1d. The run store — per run, not per source

`system/runs.sqlite` (`datalib/backend/app_schema/src/runs/`): `log`
rows with a `LogLevel`, `metrics` with a current value per series per
step per run, `step_runs.error` for a step that failed. On the Manage
screen, `manage/activity.rs` turns "warn + error lines so far this
run" into an `N ⚠` chip on a **running** step, and the Status
double-click opens `RunLogPanel` where `level:warn` filters. This is
the right shape for "what is happening now" and the wrong shape for
"what is wrong with this source": the chip disappears when the step
finishes, and a warning from last Tuesday is in last Tuesday's log.

### 1e. Config diagnostics — already at the bar

`datalib_dag::Diagnostic` with its own `Severity {Warning, Blocked,
Rejected, …}` is served as `ManageRow.dropped` and drawn on the row.
Config problems already meet requirement 1. Nothing here changes that;
note only that the name `Severity` is taken, and the type below must
not collide with it in any crate that links both.

### 1f. The UI pieces the plan builds on

- `tableView({ url })` (`ui/src/cards/libs/tableView.ts`) renders any
  endpoint that declares `columns` + `rows`; the Manage tree and the
  commit history are on it, on slickgrid since #502.
- `datalib_columns::ColumnType` has `Count`, `Chips` (with
  `ChipKind::{Info, Idle, Metric, Warning, Error}`), `Datetime`,
  `Identity`, and `MarkdownUuid` — "shown as its title; opens the
  document on click".
- `DocCard` accepts a section uuid to scroll to and highlight
  (`DocCard.ce.vue:34`, `chatSections.js`), keyed on the
  `data-section-uuid` the renderers emit; `chatLink.ts` turns
  `/chat/<uuid>` hrefs in a body into card navigation.
- The Manage join is server-side (`http/src/manage/mod.rs`) and its
  columns are `ColumnSpec`s the browser draws by type — a new column
  is a new spec and a new field on `ManageRow`, not a new grid.
- `datalib_query` is the one filter grammar; an endpoint joins it by
  accepting `q=` and owning what each key means.

## 2. Design decisions

Six, each answering one requirement in §0.

### D1. One `problems` table, one row per instance, with a severity

Rename `render_problems` → `problems` and move the row type from
`datalib_schema` to the ingest side (`datalib_etl::problems`), because
§D5 has ingest writing it and `datalib_schema` is unreachable from
there (AGENTS.md, "Ingest and render are separate crates"). The render
crates reach it through `datalib_etl` as they reach everything else on
that side.

The row becomes one row per problem instance:

| column | what |
| --- | --- |
| `problem_uuid` | **primary key**, deterministic — §D2 |
| `source_id` | as now |
| `stage` | `fetch` \| `parse` \| `render` \| `grid_row` — `fetch` is new |
| `severity` | `error` \| `warning` \| `info` — new, stored, set by the writer; `Severity::default_for(outcome)` gives `dropped → error`, `nulled → warning`, `ok → info`, and a writer that knows better overrides (a refetch that failed but left a usable older copy is `ok` + `warning`) |
| `outcome`, `reason`, `rule` | as now; `rule` still the R3 group key |
| `scope_kind`, `scope_key` | the sweep key, as now; `markdown`-scoped rows are also the link to the document (`scope_key` **is** the `markdown_uuid`) |
| `item_uuid` | the `grid_rows.uuid` the record has or would have had; NULL for a problem about the whole document or entity. Replaces today's overloaded `uuid` |
| `field`, `path`, `sample` | flattened out of the JSON list |
| `first_seen_at_utc`, `last_seen_at_utc`, `tz_offset`, `render_version` | as now; `render_version` NULL on a `fetch` row |

Flattening the JSON list is what makes the grid a plain `SELECT` and
what makes `severity:error field:created_at` a filter rather than a
JSON walk. It also removes the two-bad-stamps collision by
construction: two problems on one record are two rows with two ids.

**Every closed vocabulary here is an enum, never a naked string**, per
AGENTS.md's "Name a closed set of strings": `Severity`, `Stage`,
`Outcome`, `Reason` and `ScopeKind` are `strum` enums with `as_str` /
`parse -> Option`, the strum-serde agreement test, and the stored
`VARCHAR` bound from `as_str` — a writer never formats one into SQL
and a reader never compares one against a literal. The row struct's
fields *are* the enums — `#[col(sql = "…", enum)]` on `PortableTable`
binds `as_str()` and `ProblemRow::from_row` parses back — so the
string exists only in the store. The TypeScript mirror in
`ui/src/api.ts` is the matching string-literal union, changed in the
same commit. `Severity` lives in `datalib_problems` and is never
glob-imported beside `datalib_dag::Severity`.

### D2. The id is minted from what produced the problem, nothing else

```
problem_uuid = uuid_from_blake3(
    source_id, stage, scope_kind, scope_key, item_uuid, field, reason, rule
)
```

through `datalib_id` — the same crate and the same "one rule for
minting a uuid" in [`entity_ids.md`](../entity_ids.md), with
`"problem"` as the entity kind so it can never collide with a record
id. **Not in the recipe:** `sample`, `path`, the stamps, the render
version. Two runs of the same code over the same record must agree,
and a re-run after a fix must produce *no* row rather than a different
one; anything that can vary between two runs of the same code is
excluded. The `noid:` surrogate today mixes the error's display text
into its key (`grid_rows_builder.rs:346`), which changes when the
message wording does; it becomes `reason` + `field`, which do not.

### D3. Problems flow downstream with the data

A store's `problems` table is owned by the step that owns the store,
and it is swept by that step under the rules it already has (per
document, per entity). Each consumer that reads a store pinned copies
that store's `problems` for the source **wholesale** into its own,
then adds its own. Two hops:

- **raw → render.** The render step already pins the raw store; it
  reads the raw `problems` at that pin and replaces every `fetch`-stage
  row for the source in its render store. The pinned raw store is the
  complete truth about fetch problems at that commit, so there is
  nothing to sweep — the copy *is* the sweep.
- **render → index.** `grid_index` already pins every render store; it
  replaces every row for the source in the unified index's `problems`
  the same way, and drops a source's rows when the source leaves the
  config (the same event that drops its `grid_rows`).

Stamps travel with the row rather than being restamped, so
`first_seen_at_utc` in the index is when the problem was first seen
where it happened. The applet then serves one table with one `SELECT`,
which is the shape everything else in the unified index already has.
Problems are few — a source with tens of thousands is R4's business —
so wholesale replacement per source per run is cheap.

The alternative — `datalib-http` opening every render store on every
Manage poll — is ruled out by the pool rules in
`etl/README.md` § "Connection pools" and by the poll rate.

### D4. The counts come from the run store; the rows come from the index

Every step that owns a `problems` table ends by emitting
`problems{severity=error}` and `problems{severity=warning}` as
metrics (`datalib_step::render::PROBLEMS_METRIC`) — whole-store
current counts, zero included, the same numbers the render step
already logs. The Manage join reads them from the step's `last_run_id`
(one query on `metrics`, which the join does not do today —
`DagStepRun` carries no metrics — but the table and the id are both
already in hand). The group row shows the render step's numbers, since
after §D3 its store is the union of fetch and render problems for the
source; the `unified_index` row shows `grid_index`'s.

The cell is a `Chips` column, `problems`: `[Error "3 errors", Warning
"12 warnings"]`, or one `Ok "0"` chip — `ChipKind` grows `Ok` so that
zero can be green rather than idle-grey. Double-click opens
`tableView({ url: "/applet/unified_index/problems?q=source_id:<id>" })`
the way the Status double-click opens the log. A step whose last run
predates this change has no metric and shows nothing rather than a
false green zero — the "unknown" state is real and is drawn as such.

### D5. The parse and fetch stages get a sink, then the providers get migrated

- **Parse.** `RenderCtx::problem(row)` accumulates entity-scoped rows
  and the driver flushes them through `put_entity_problems` inside the
  batch transaction, so a payload that will not deserialize lands as
  `parse / undeserializable / dropped / error`, scoped to the raw
  entity id, and clears when that entity next parses. The five
  `continue`s in §1b become one helper call each.
- **Fetch.** `record_object_attempt`'s failure arm writes a
  `fetch`-stage row (`error` when there is no payload, `warning` when
  an older payload is still there) and its success arm deletes it —
  the entity sweep, in the one function every provider that records
  failures already calls. Then the 22 providers that `warn!` and
  `continue` are moved onto `record_object_error`, one provider per
  commit, in the order the practices doc gives for the render side
  (`notion`, `chatgpt`, `slack`, `email` — the biggest surfaces — then
  the API providers where a per-item fetch actually fails: `github`,
  `gitlab`, `linkedin`, `google_takeout`).

Per-provider migration is the long tail and is deliberately after the
UI, not before it: once the grid exists, an unmigrated provider is
visibly a source whose problems column is always green, which is the
pressure that gets it migrated.

### D6. The document view reads the index, and links inward

`ChatResponse` grows `problems: Vec<ProblemView>` — the index's rows
`WHERE scope_kind = 'markdown' AND scope_key = ?`, plus any
`entity`-scoped row whose `item_uuid` is one of the document's rows.
`DocCard` draws them above `.chat-header`: one line per problem in the
severity's colour, `field` and `reason` in words, the sample in
monospace, and — when `item_uuid` is a section the body has — a link
that scrolls and highlights it through the machinery `DocCard` already
has. A dropped record has no section; its line says "dropped" and does
not link. The grid's document column is `MarkdownUuid` over
`scope_key`, and its item column opens
`documentView(markdown_uuid, item_uuid)`.

## 3. Sequence

Each PR is useful on its own and the first two are where the user sees
the change. The store schema changes in PR 1, which invalidates every
render store (a re-render, never a re-fetch) — the commit message says
so.

**PR 1 — the table.** Done (#553), except the `poison` fixture, which
moves to PR 4 where the parse stage gives it something to poison.
`datalib_problems` per §D1/§D2; `render_problems` deleted;
`build_or_record` and `sweep_problems` moved onto it; the
two-bad-stamps case tested; the render step emits the two metrics. The
`data_architecture_parse_and_render.md` §4 banner corrected. Nothing
visible yet.

**PR 2 — the grid and the counts.** Done. `grid_index` copies per §D3;
`/applet/unified_index/problems` serving `columns` + `rows` with `q=`
over `source_id`, `severity`, `stage`, `reason`, `field`, `outcome`,
`rule`, `scope`, `doc`, `item`; the Manage `problems` column and its
double-click per §D4; `ChipKind::Ok`. **This is the milestone**:
requirements 1–3 met for everything the grid-row stage records today.
The `poison` fixture (PR 4) is what will first put a non-zero count on
the screen in a test.

**PR 3 — the document.** Done. §D6: `ChatResponse.problems`, the
banner above the body, a line about a surviving record jumps to its
section in place. Requirement 4.

**PR 4 — the parse stage.** Done, minus R3.
`NormalizedChatItem::problems` and `own_stamp_ms` carry what a
provider could not do with an item into a document-scoped row keyed to
the item; every `TODO(problem-sink)` stamp site (slack, claude, chatgpt,
email, google_takeout, linkedin) is on it. `RenderCtx::report_unparsed`
takes the five silent `continue`s (slack users and messages, chatgpt
conversations, email accounts/mailboxes/threads) with a `ReadScope`
saying which tables the parse read whole — the rule that lets an
entity-scoped row clear when its row reads cleanly again, and keeps it
when the row was not looked at. `RenderCtx::report_document_failed`
takes `pdf_render`'s per-document conversion failure
(`Reason::RenderFailed`). The TNG fixture carries a `Poisoned:`
conversation whose reply's `created_at` is `stardate 47988.1`, and
`ingested_tng_test` pins its one row — id included — through the
store, the index, a steady-state re-run and a from-scratch rebuild.
Still open: `Problem::lossy` has no callers, so R3's table cannot be
generated yet.

**PR 5 — the fetch stage.** Done, minus the provider tail. Every raw
store has a `problems` table (`doltlite_raw::SHARED_DDL`);
`record_object_attempt`'s failure arm writes the entity's fetch row
(`Reason::FetchFailed`, an error when the record never fetched, a
warning when an earlier fetch left a copy) and both success paths clear
it; the render step reads the raw store's rows at the commit it
rendered from and replaces its own fetch-stage rows with them, minted
again under the source's id; the ingest step reports its store's
counts. On the fixture this immediately recorded two things that were
only log lines before — a Claude attachment with no bytes and the
facebook video deliberately absent from the export — and
`ingested_tng_test` pins both beside the poisoned reply.

Also on it: `download_problems::report` — a configured label,
channel or conversation id upstream does not have or will not show —
which was one `warn!` per entry and is now also a row keyed
`config:<setting>:<value>`, replaced whole each run so a corrected
config clears it (claude, slack, email, gmail).

**The provider tail, still open.** A provider that `warn!`s and
`continue`s past a per-record fetch failure records nothing until it
calls `record_object_error`. The mechanism is proven by the four that
already do (`chatgpt`, `claude`, `garmin`, `notion`) and by
`blob_cas`, which every attachment-bearing provider reaches. Left to
migrate, in the order the practices doc gives (biggest surfaces first):
`slack`, `email`, `github`, `gitlab`, `linkedin`, `google_takeout`,
`beeper`, `signal`, `whatsapp`, `sms_backup_restore`, `contacts`,
`airvisual`, `yolink`, `fsindex`, `media`, `pdf`, `facebook` (its
non-media rows), `claude_code`. One commit each; the test is that the
provider's fixture, given one unfetchable record, produces the row.

**Later, not in this plan:** R3's table (`Problem::lossy` still has
no callers); R4's drop budget (a run that drops more
than a fraction stops), which needs the counts from PR 1 and a
decision about the threshold; a per-message marker in the body rather
than only the banner; problems on `edges`.

## 4. Tests

- `Severity`, `Stage` and `Reason`: strum and serde agree
  (`render_problems.rs` has the pattern).
- Two problems on one record are two rows, and a second run of the
  same input mints the same two ids (PR 1; this is the test that would
  have caught §1b's collision).
- The store: a fixed document loses its rows, a skipped document keeps
  them — already there, ported.
- The copy: a source removed from the config loses its rows in the
  index; a source whose render store gained and then lost a problem
  ends with none in the index (PR 2).
- The fixture: `poison` rows exist with the expected ids and
  severities, TNG rows do not (PR 1), and the same after a second
  render with no change (the ids and `first_seen_at_utc` are stable,
  `last_seen_at_utc` moved).
- The API: `q=severity:error` returns only errors; the columns
  declaration names `scope_key` as `markdown_uuid` (PR 2).
- The two-process doltlite test runs whatever new statement the applet
  adds, per the etl README's rule that a reader's new statement is
  guilty until it has.

## 5. Docs to touch

- `data_architecture_parse_and_render.md` §4 — banner, and R1's
  paragraph rewritten around `problems` (PR 1).
- `etl/README.md` — a "Problems" section beside "Bookkeeping lives in
  a sidecar table", stating the copy rule of §D3 (PR 2, PR 5).
- `app_stores.md` — `problems` in each store's table list.
- `grid_rows.md` — the index has a second table the grid reads.
- `AGENTS.md` — one line under the data-quality conventions: an error
  or a warning about a record goes through `problems`, never only to
  the log; and where the counts show.
- This doc: move to `plans/completed/` when PR 5's checklist is done,
  or delete it if by then §D1–D6 have been folded into the reference
  docs above.

## 6. Open questions

- **Decided: `info` rows count nowhere.** Requirement 1 names two
  colours; `info` is a grid filter (`severity:info`) and never a
  badge — an `info` row is R6's "finding worth publishing".
- **Where does a `fetch` problem link to?** There is no raw-record
  viewer. The grid shows `scope_key` (the upstream id) as text; when
  the record later renders, the parse-stage row that replaces it links
  to the document. Good enough until someone asks for a raw viewer.
- **The Manage count for a source with no render step** (`fsindex`,
  `media`, `lightroom`, `apple_photos` are ingest-only). Their row
  shows the ingest step's metric, which is only non-empty after PR 5.
