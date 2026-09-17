# Diff groups: a source's changes as a first-class thing in the app

**Status: proposal (2026-09-17). Nothing here is built.** This replaces
an earlier proposal of the same name that diffed the render store's rows
between two of its commits; that design could list which documents
changed but could not show *how*, because the render store never held
the old markdown. Every "already exists" claim below names the file it
was checked against.

## What we want

Understanding a delta is as important as understanding a state, and
almost nothing outside version control renders one. A person should be
able to look at any source in the app and see what changed in it between
two points in time, at the level they think in — this contact gained a
phone number, that Slack message was edited, this reaction is gone — in
both of the forms the app already shows data in:

- **the document**, as markdown, with the changes highlighted the way a
  code diff is: added sections on green, removed ones on red, an edited
  section with the changed words marked inside it;
- **the grid**, as `grid_rows`, with added rows on green, removed rows
  on red, and the altered cells of an edited row on yellow.

Adds, deletes and edits, for every source, without a second renderer
that could drift from the first.

## The idea

**A diff is a group in the config, and its step is the render step with
a different sink.**

```toml
[[groups]]
id = "work-slack-diff"
type = "diff"
source = "work-slack"          # the group whose raw store is diffed

[[steps]]
group = "work-slack-diff"
function = "render_markdown"
inputs = ["work-slack/ingest"]
[steps.params]
from = "<raw commit>"          # both required: a diff is asked for,
to = "<raw commit>"            # never standing
```

The step renders the source's raw store at two commits, subtracts one
render from the other, and writes the result as an ordinary render tree
under `work-slack-diff/render_markdown/` — the standard
`indexed_markdown.doltlite_db` schema plus two nullable `grid_rows`
columns, and one highlighted `.md` per changed document beside it.
Because the tree has the standard shape, everything already built for a
source takes over: `grid_index` and `qmd_index` index it when the
fan-ins name it, the applet serves it, the grid shows it under its own
`source_id`, `source_id:work-slack-diff` filters to it, the preview
pane opens its documents, and the Manage screen lists it with its run
history. The UI's additions are a row/cell colouring rule and an icon.

Three properties follow from making it a group rather than a view:

- **It cannot drift from the real render.** There is one renderer per
  provider; the diff step runs it twice and subtracts. A layout change
  changes both.
- **Its cost is the sync step's cost, not the store's.** Pass one is
  exactly the incremental render the sync already does (the raw
  `dolt_diff` from `from` to `to` names the buckets that moved; only
  those render). Pass two renders the same buckets at `from`. A store
  with a million rows and ten changed threads renders twenty threads.
- **"Ephemeral" is a lifecycle, not a format.** A diff group is added
  from a source's Manage row and removed like any source; its directory
  goes with it. Nothing new to clean up, no scratch dirs, no JSON.

## A diff is asked for, never standing

`from` and `to` are both required raw commits of the source's store.
They sit in the step's params and so in its fingerprint: the DAG runs
the step once, and again only if someone changes the pair. A re-run
that produces the same rows commits nothing (doltlite's
content-addressed tables), so a diff group at rest costs a directory
and nothing else. There is no default pair and no diff that maintains
itself as the source syncs — a person names the two points, and what
they get is exactly that comparison until they ask for another.

A person creates one from **"Compare…" on a source's row in Manage**: a
picker lists the raw store's commits (date, and the sync run that made
each, from `datalib_history`) and they choose two. The wizard writes the
group, its one step, and names the step in both fan-ins' `inputs`,
exactly as it does for a source (`wireIntoFanIns` in
`ui/src/config/sourceSteps.ts`). Removing the group removes the tree.

A *rolling* diff — `from` advancing to the last consumed commit on every
sync, so the group is a live "what changed in the last sync" view — is
one flag away, because `from` would simply be `RenderCtx.raw_cursor`,
the render step's own mechanism. It is deliberately not built and would
never be a default: a diff that appears without being asked for is
noise in the sources list, and a first run with no cursor has nothing
to compare against.

## How the step computes it

The render driver ([`render.rs`](../../../datalib/backend/datalib_step/src/render.rs),
`render_source`) already does everything but the subtraction. For a
`diff` group it runs the source type's render processors — the same
`plan_render` the source's own render step uses — with a sink that
collects instead of stores:

1. **Pass one, at `to`.** `RawRange { cursor: from, pin: to, stale: None }`.
   The provider's forward scan — the raw `dolt_diff` from `from` to
   `to` — names the buckets that moved; the provider renders them at
   `to` and declares each bucket's inputs. Collect the emitted
   `RenderedMarkdown`s and the declared bucket set *S*. (The driver's
   reverse lookup through `render_inputs` is not needed: the pair is
   fixed, so every run is a full walk of the same delta.)
2. **Pass two, at `from`.** `RawRange { cursor: from, pin: from,
   stale: S }`. The forward scan from `from` to `from` is empty, so the
   provider renders exactly *S* — at `from`. A bucket in *S* whose raw
   row does not exist at `from` comes back through `Narrowed::gone` and
   produces no document; that is how an added document looks. Two
   sequential read-only pinned opens of the raw store, each closed
   before the next, so the one-open-per-file rule holds.
3. **Subtract.** For each `markdown_uuid` in the union of the two
   sides: rows keyed by `uuid` — only in `to` is *added*, only in
   `from` is *removed*, in both with any cell different is *modified*
   with the list of columns that differ, otherwise *unchanged*.
   Sections keyed by section uuid (see "Sections" below) the same way;
   a modified section's body gets a word-level inline diff. A document
   only on the `to` side is one whose every section and row is added;
   only on the `from` side, all removed.
4. **Write**, through the ordinary store path (`put_document`,
   `put_inputs`, checkpoint, seal), one document per `markdown_uuid` in
   the union: rows are the union with `diff_status` set, the `.md` is the
   highlighted document, the bucket's `render_inputs` are pass one's
   declarations. Every run is a full walk, so the sweep at the end
   removes whatever a previous pair produced that this pair did not.

The prerequisites for "render twice, subtract" to mean anything are
already rules of the tree, checked here against it: a render is a pure
function of the raw rows at the pin (`markdowns_carries_no_per_run_stamp`
in `datalib_schema`; the run's `now` reaches only `render_problems`,
`indexed_markdown.rs::insert_problems`); the raw diff names content
changes only, because bookkeeping and volatile fields live in sidecar
tables (`datalib/backend/etl/README.md` §"Volatile fields") so
`changed_keys` (`doltlite_raw.rs`) never names a row that was merely
re-fetched.

## What the diff tree holds

**`grid_rows` gains two nullable columns**, following the checklist in
[`grid_rows.md`](../grid_rows.md) §"Adding a column":

| column | values |
|---|---|
| `diff_status` | `added` / `removed` / `modified` / `unchanged`; **NULL on every real source's rows** |
| `diff_changed_columns` | for `modified`: the differing column names, sorted, `|`-joined; else NULL |

`diff_status` non-null is what says "this row is from a diff tree". A
removed row is the `from`-side row, verbatim, so the grid can still show
what left. `provider` stays the underlying provider, so per-provider
CSS and icons apply; `source_id` is the diff group's id. Both are
`strum` enums on the Rust side and string unions in `api.ts`, per
AGENTS.md §"Name a closed set of strings".

**The `.md`** is the `to`-side document with the highlight vocabulary
added, and nothing else new:

```html
<div class="diff-added">   …a whole section that is new…      </div>
<div class="diff-removed"> …a whole section that is gone…     </div>
…inside a modified section: <ins>new words</ins> <del>old words</del>…
```

The `<div id="m-{uuid}" data-section-uuid="{uuid}">` wrappers stay
intact inside the diff wrappers, so row-click-to-section still works.
`ins`, `del` and `class` are in DOMPurify's default allowlist; a test
in `ui/tests/sanitize.test.ts` pins that they survive, the way the
rest of the vocabulary is pinned.

**Attachments** are diffed by what the rows and sections say about them
(path, name, size), not by their bytes — the same limit the earlier
proposal noted. A blob swapped under the same path is invisible here.

## Sections: the one renderer change

Subtracting documents needs both sides as sections keyed by uuid.
Splitting the finished `.md` on the wrapper contract would work, but it
makes the diff a markdown parser, which the tree forbids for good
reason (AGENTS.md §"QMDs are write-only"). Instead the renderer says
what its sections are:

```rust
pub struct Section { pub uuid: Option<String>, pub md: String }
// RenderedMarkdown gains:
pub sections: Vec<Section>,   // concatenated, they are the .md
```

In `chat-common` this is `render_markdown` collecting the per-item
strings it already builds one at a time
([`render.rs`](../../../datalib/backend/etl/chat-common/src/render.rs),
`render_item`) into a `Vec` and joining at the end; `contact-common`
the same. The frontmatter and title are one uuid-less section, diffed
as text. Those two crates cover most providers in one edit each. A
renderer that has not been taught sections yet emits one section for
the whole body; its documents diff as one block with inline `<ins>`/
`<del>` and no green/red section bands. That degradation is logged
once per source when a diff step meets it, so nobody mistakes it for
the finished behaviour.

This is also the first step toward generating the `.md` on read rather
than storing it — the sections are the natural unit — but that is a
separate decision and nothing here depends on it.

## The UI

- **Sources list / Manage.** A `diff` group shows the underlying
  provider's icon with a delta badge, and "Compare…" on a source's row
  creates one. The wizard's rule "a source missing one of its two steps
  gets it back on save" must not add an `ingest` step to a `diff`
  group; its catalog descriptor has no ingest and no connection section.
- **Grid.** `rowClassRules` on `diff_status` (added → green band,
  removed → red band) and `cellClassRules` on `diff_changed_columns`
  (yellow), in `GridCard`. A "changed only" toggle, on by default
  inside a diff source, filters `diff_status != 'unchanged'`. Real
  sources have NULL and are unaffected.
- **Preview.** The highlighted `.md` renders through the same
  `ChatBody`; the diff classes get CSS. The whole document is shown
  with context, like a code diff at full context — the opposite default
  from the grid.
- **Search.** `source_id:<diff group>` and every other filter already
  work; `diff_status:added` is one more column filter through the
  existing typed-column path.

## Loader, runner, `datalib-step`

- **Loader** (`dag/src/config.rs`): a `diff` group must carry `source`,
  and `source` must name a group that has a `type` other than `diff`
  and an `ingest` step. A `diff` group's only permitted grouped
  function is `render_markdown`. Violations drop the group with a
  diagnostic, like any other bad entry.
- **Runner**: forwards the source group's type to the diff group's
  steps as `DATALIB_DAG_SOURCE_GROUP_TYPE`, and puts it in the step's
  fingerprint beside `DATALIB_DAG_GROUP_TYPE`.
- **`datalib-step`**: `type = diff` dispatches to the source type's
  `plan_render` (the `SourceType` list stays closed; `diff` is not a
  `SourceType` but a second word the group loader knows), then to the
  render driver in diff mode with `from`/`to` from params. Refuses a
  `diff` group with no `DATALIB_DAG_SOURCE_GROUP_TYPE`, or one whose
  source type renders nothing (`ingest_only!`).
- **The raw store** comes from the step's first input, exactly as a
  render step's does — `source` on the group is for the loader's
  validation, the Manage screen's label and the wizard, not for
  locating data.

Rejected alternative: a `diff` group typed like its source
(`type = "slack"`) with `params.diff = { from, to }` on its render step.
It reuses one line more but lies about what the group is — a source is
a group with a `type`, and every rule written for sources (the wizard's
step repair, the Manage row's "Download" label, the connection section)
would then need a `diff`-shaped exception. A group that mirrors nothing
should not say it mirrors Slack.

## Fixture and tests

The TNG fixture ingests once, so no raw store in it has a second
commit. Extend `tests/fixtures/run_sync_pipeline.py`:

- **Contacts** (`.vcf` files read from disk): a `carddav_tng_v2/`
  sibling of `carddav_tng/` with one card added, one removed, and one
  edited (a phone number and the `ORG`). The pipeline runs the DAG,
  repoints the `tng_contacts` group's `vcf.path` at v2, and runs it
  again; the raw store then has two commits, and a `tng_contacts-diff`
  group in the config renders the delta. Contacts first because one
  document is one contact, so a field edit is one yellow cell and the
  shape of every rule is visible in a screen of output.
- **Slack** (HTTP playback): a second playback tape from a
  `slack_api_v2/` fixture dir — one new message, one deleted, one
  edited, one reaction added — so `chat-common`'s aside runs,
  reactions and the `##` header all get exercised. Slack second, and
  through it every `chat-common` provider.

Goldens: the two diff trees' `.md` files and their `grid_rows` join the
`render_contract_test` / render-preview goldens, so the highlighting
and the status columns are pinned. A unit test on the subtraction
covers the table above (added / removed / modified with the right
column list / unchanged, and a document present on one side only).
`schema_inventory` regenerates for the two columns. The step opens the
raw store read-only twice in sequence; no new statement runs from a
read-only connection while a writer is open, so
`doltlite_two_process_test` is unaffected — say so in the PR, and run
it anyway.

## Order of work

1. `Section` on `RenderedMarkdown`; `chat-common` and `contact-common`
   emit sections; the `.md` bytes are unchanged (a golden proves it).
2. The two `grid_rows` columns, end to end through the checklist, NULL
   everywhere; `schema_inventory` and the fixture rebuilt.
3. The subtraction (`datalib_etl_render::diff`): rows, sections,
   inline word diff (the `similar` crate — a new third-party dep), unit
   tests. Nothing wired yet.
4. Loader + runner + `datalib-step`: the `diff` group, `source`,
   `DATALIB_DAG_SOURCE_GROUP_TYPE`, the driver's two passes and the
   collecting sink. The contacts fixture's second commit and diff group;
   goldens.
5. UI: colouring rules, the changed-only toggle, diff CSS, the icon,
   "Compare…" and the wizard rules, the sanitizer test.
6. Slack's second tape and diff group; whatever `chat-common` needs
   that contacts did not show.

Each is a PR that leaves the tree green on its own.

## Open questions

- **Full document or changed sections only in the `.md`?** Full, with
  highlights, is proposed — it reads like a code diff at full context
  and keeps every anchor. For a very long document a "collapse
  unchanged" control belongs in the frontend, beside the existing
  "Show more" clamp, not in the markdown.
- **Changing the pair in place, or a new group per pair?** Editing
  `from`/`to` re-runs the step and the sweep replaces the old delta;
  the diff store's own commit log then holds every pair ever asked for,
  readable through the existing pinned-reader path. Whether "Compare…"
  edits an existing diff group or always makes a new one is a wizard
  question, not a pipeline one.
- **Cost of pass two on a lookup-heavy provider.** Slack loads `users`
  and `channels` whole before it renders anything; pass two loads them
  again at `from`. Measure on a real store before deciding whether the
  two passes should share one open with two pins.
- **Type-level or vocabulary-level edits.** The word diff treats a
  section body as text. A renamed author, a changed timestamp and an
  edited body all look alike inside `<ins>`/`<del>`; the row's
  `diff_changed_columns` is where the kind of change is legible. Good
  enough to start; revisit once people have used it.
