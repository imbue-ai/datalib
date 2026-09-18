# Diff groups: a source's changes as a first-class thing in the app

**Status: built (2026-09-18) — every step of "Order of work".** Kept as
the record of what was decided and why; the reference for how a diff
group works is [`config_model.md`](../../config_model.md) and the code it
names. The contacts diff
group in the TNG fixture is the working example
(`tests/fixtures/run_sync_pipeline.py`, `ingested_tng_test`'s
`_diff_shape`). This replaces an earlier proposal of the same name
that diffed the render store's rows between two of its commits; that
design could list which documents changed but could not show *how*,
because the render store never held the old markdown.

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
[steps.params.diff]
from = "<raw commit>"          # both required: a diff is asked for,
to = "<raw commit>"            # never standing
```

The pair sits under `params.diff` so the rest of `params` is the
source type's own render config, parsed as strictly as on the source's
step (`render_diff::split_params`). `params.diff.max_documents`
(default 1000) is the most documents either side may render: a pair a
person expected to be small and is not fails the step on the document
past the cap — costing that many renders and no more. The refusal is
the driver's, not the provider's: a renderer may log a document's
failure and carry on (contact-common does), so the side remembers it
refused and fails after the processors return. The failure names the
fix — pick closer commits, or raise the cap — as the step's error
(the Manage row's status detail) and as a `Hint` event, the run log's
fix-it channel.

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

A person creates one from **"Compare two syncs…" on a source's row in
Manage** (`CompareDialog.vue`): the source's ingest tree's commit
history (`GET /api/pipeline/history`) fills two pickers, the newest
sync and the one before it by default, with a name and the document
cap beside them. Submitting writes the group and its step
(`buildDiffSource` in `ui/src/config/sourceSteps.ts`), names the step
in both fan-ins' `inputs` (`wireIntoFanIns`), and syncs the source —
the diff step is downstream of the source's ingest and runs in its
chain (`datalib-dag --sync <source>/ingest`), and "Sync now" on the
diff group does the same (`diff_group_seeds`). Removing the group
removes the tree. A diff group has no guided edit form; its commits are
changed in Advanced, or by comparing again.

A *rolling* diff — `from` advancing to the last consumed commit on every
sync, so the group is a live "what changed in the last sync" view — is
one flag away, because `from` would simply be `RenderCtx.raw_cursor`,
the render step's own mechanism. It is deliberately not built and would
never be a default: a diff that appears without being asked for is
noise in the sources list, and a first run with no cursor has nothing
to compare against.

## How the step computes it

The render driver ([`render.rs`](../../../../datalib/backend/datalib_step/src/render.rs),
`render_source`) already does everything but the subtraction. For a
`diff` group it runs the source type's render processors — the same
`plan_render` the source's own render step uses — with a sink that
collects instead of stores:

1. **One pass per side, each scanning from the other.** `RawRange {
   cursor: from, pin: to, stale: Some(∅) }` renders, at `to`, the
   buckets the provider's forward scan names — the rows the raw
   `dolt_diff` says moved, mapped to their buckets through the rows
   loaded at `to`. Then `RawRange { cursor: to, pin: from, stale:
   Some(∅) }` does the same at `from`. The passes are symmetric because
   a *deleted* row has no bucket at `to` — nothing there to load it
   into — and is named only by the pass at `from`; an added one only by
   the pass at `to`. The stale set is empty rather than absent so the
   scan decides alone (`None` means "render everything"), and the
   driver's `render_inputs` reverse lookup is not used: a diff store
   has none on its first run, and the pair is fixed, so every run is a
   full walk of the same delta. Two sequential read-only pinned opens
   of the raw store per pass, each closed before the next.
2. **Collect rather than store.** The renderer still writes its `.md`
   and blobs to their real paths; the sink keeps each emitted
   `RenderedMarkdown` and its sections in memory (reading the file back
   as one block for a renderer that declares no sections, and saying
   so once per source at `warn`), and the subtraction's result
   overwrites the file.
3. **Subtract.** For each `markdown_uuid` in the union of the two
   sides: rows keyed by `uuid` — only in `to` is *added*, only in
   `from` is *removed*, in both with any cell different is *modified*
   with the list of columns that differ, otherwise *unchanged*.
   Sections keyed by section uuid (see "Sections" below) the same way;
   a modified section's body gets a word-level inline diff. A document
   only on the `to` side is one whose every section and row is added;
   only on the `from` side, all removed.
4. **Re-key, then write** through the ordinary store path
   (`put_document`, `put_inputs`, seal, one commit), one document per
   `markdown_uuid` in the union: rows are the union with `diff_status`
   set, the `.md` is the highlighted document, the bucket's
   `render_inputs` are the `to` side's declarations. A document the
   scan named whose rows and sections came out identical is not a
   document of the diff. Every run is a full walk, so the sweep at the
   end removes whatever a previous pair produced that this pair did not.

**A diff row has its own id.** It is about the source's entity but is
not it, and the unified index refuses two sources claiming one uuid
(`IdClaims`). So every uuid a diff document carries — its rows', its
`markdown_uuid`, its `conversation_uuid` — is minted again by the one
recipe: `entity_id_str(IdNamespace::Datalib, Scope::SourceInstance(<diff
group>), "diff", <the source's uuid>)`, the same shape as the storage
rows. The anchors in the markdown (`id="m-…"`, `data-section-uuid`,
`data-page-title-uuid`) and the frontmatter's `markdown_uuid:` /
`chat_uuid:` follow, so a row still scrolls to its section; a path
never does, since the file and its blobs are where the renderer put
them; `upstream_id` stays the source's, since it is the backpointer to
the real thing. `render_diff::rekeyed`.

**What the scan cannot see.** A provider's forward scan diffs its
content tables — Slack's messages and attachments, contacts' cards and
address books. A change only in a lookup table (a user renamed in
`users`, with no message touched) names no bucket, and the diff shows
nothing for it; the normal render catches that case through the
reverse lookup in `render_inputs`, which the diff does not use. The
three providers whose render decides by `RawRange::is_stale` alone
(airvisual, garmin, yolink — one page of plots each) render nothing
under a diff group. A source that renders nothing (`ingest_only!`) is
refused by `datalib-step`.

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
[`grid_rows.md`](../../grid_rows.md) §"Adding a column":

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
<div class="diff-added">    …a whole section that is new…      </div>
<div class="diff-removed">  …a whole section that is gone…     </div>
<div class="diff-modified"> …a section on both sides, with
                             <del>old words</del><ins>new words</ins> inside… </div>
```

Inside a modified section the diff is lines first, then the words of a
line that changed (`diff::inline_diff`): a whole inserted or deleted
line has its content marked, a changed line only the words that moved.
A marker never crosses a table cell boundary or a line end, never
contains an HTML tag, and leaves a line's markdown prefix and a table's
delimiter row alone — so a diffed heading is still a heading and a
diffed table is still a table. Unkeyed sections (frontmatter, a
`<details>` wrapper) come through from the `to` side unmarked; a
document with no `to` side keeps `from`'s.

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
([`render.rs`](../../../../datalib/backend/etl/chat-common/src/render.rs),
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

- **Sources list / Manage.** A `diff` group shows with its own icon and
  the label "Diff" (`source_catalog.rs`, `icons.ts`); the wizard never
  opens on one (`groupEditBlocked` says where its commits are edited),
  so its step-repair rule cannot add an `ingest` step to it. "Compare
  two syncs…" is on every source's row menu (`rowMenu.ts`), and says
  why not on a step, the index, or a diff group itself.
- **Grid.** `rowClassRules` on `diff_status` (added → green band,
  removed → red band, struck through) and `cellClassRules` on
  `diff_changed_columns` (yellow, with `text` mapped to the `snippet`
  column), in `GridCard`. Real sources have NULL and are unaffected.
- **Changed only.** `change:` is a search filter on `diff_status`
  (`Field::Change`), and a diff group's Browse opens on
  `source_id:<group> -change:unchanged` with the two diff columns
  leading (`browsePresets.ts`) — every row that moved, not one per
  document. Negation keeps NULL, so the same filter over the whole grid
  keeps every real row.
- **Preview.** The highlighted `.md` renders through the same
  `ChatBody`; `.diff-added` / `.diff-removed` / `.diff-modified` are
  tinted bands with a coloured left border, `ins` / `del` tinted
  inline, all over the card background so they read in either colour
  scheme. The whole document is shown with context, like a code diff at
  full context — the opposite default from the grid.

## Loader, runner, `datalib-step`

- **Loader** (`dag/src/config.rs`, `DIFF_GROUP_TYPE`,
  `diff_source_problem`): a `diff` group must carry `source`, and
  `source` must name a declared group with a `type` that is not `diff`;
  a group of any other type must not carry `source`. A `diff` group's
  only step is `render_markdown`, and its first input, when it has any,
  must be `<source>/ingest` — a root with no ingest steps at all (the
  materialized fixture root) declares it with none, and the step reads
  `<source>/ingest`. Violations drop the entry with a diagnostic naming
  the rule, like any other bad entry.
- **The source's id and type reach the step as `DATALIB_DAG_SOURCE_GROUP`
  and `DATALIB_DAG_SOURCE_GROUP_TYPE`**, which the loader puts in the
  step's own `env` — so they are forwarded like any env entry and
  fingerprinted with it, and changing the source re-runs the diff. The
  runner itself knows nothing about diff groups.
- **`datalib-step`** (`source.rs`, `main.rs`, `render_diff.rs`): under
  `type = diff` the render function splits `params.diff` off, plans the
  source type's render wave with the rest, and runs the two-pass
  driver. The `SourceType` list stays closed; `diff` is a word the
  loader knows, not a provider.
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

- **Contacts** (built): a `carddav_tng_v2/` sibling of `carddav_tng/`
  with one card added, one removed, and one edited (a phone number and
  the `ORG`). The pipeline copies `carddav_tng` into the workspace,
  runs the DAG, lays v2 over the copy and syncs the contacts chain, then
  writes a `tng_contacts-diff` group with the two commits (read off
  `system/dag_state.json`) and syncs the chain once more. The `.vcf`
  ingest had to learn to delete a card gone from a re-read file for the
  removal to exist at all — it was upsert-only. Contacts first because
  one document is one contact, so a field edit is one yellow cell and
  the shape of every rule is visible in a screen of output.
- **Slack** (built; HTTP playback): `slack_api_v2/` is the captured
  API as the second sync sees it — the incremental
  `conversations.history` at the `oldest` the resume scan computes,
  carrying a new message, an edited one with a reaction, and the
  "status report" thread root with `reply_count` advanced so the thread
  is re-walked through a `conversations.replies` tape with one more
  reply. A tape is keyed by its request, so the second capture is
  synthesized into its own playback tree (`playback_v2/`) and the
  second sync run with `DATALIB_HTTP_PLAYBACK` pointed there. What a
  re-sync cannot carry is a deletion — an incremental history returns
  only what is newer than `oldest` — so that fate is the contacts
  fixture's. Through Slack, `chat-common` under a diff: a grown thread
  keeps its sections verbatim with the new reply in an added band, an
  edited message gets the word diff inside its band, and a reaction
  added to it is a marked line.

Goldens: both diff trees' `.md` files are in the render-preview golden
and their rows in the fixture-DB snapshot and the grid's Playwright
golden, so the highlighting and the status columns are pinned;
`ingested_tng_test` asserts the contacts diff's three fates and changed
columns (`_diff_shape`), the Slack diff's counts by fate
(`_diff_fates`), and the markup of the grown thread and the edited
message. The id round-trip check there skips diff rows: their uuid is
minted under the diff group, their `upstream_id` is the source's. A unit test on the subtraction
covers the table above (added / removed / modified with the right
column list / unchanged, and a document present on one side only).
`schema_inventory` regenerates for the two columns. The step opens the
raw store read-only twice in sequence; no new statement runs from a
read-only connection while a writer is open, so
`doltlite_two_process_test` is unaffected — say so in the PR, and run
it anyway.

## Order of work

1. *(built)* `Section` on `RenderedMarkdown`; `chat-common` and
   `contact-common` emit sections; the `.md` bytes are unchanged.
2. *(built)* The two `grid_rows` columns, end to end through the
   checklist, NULL everywhere.
3. *(built)* The subtraction (`datalib_etl_render::diff`): rows,
   sections, line-then-word inline diff (the `similar` crate).
4. *(built)* Loader + `datalib-step`: the `diff` group, `source`,
   `DATALIB_DAG_SOURCE_GROUP_TYPE`, the two-pass driver, the re-keying.
   The contacts fixture's second commit and diff group; goldens.
5. *(built)* UI: colouring rules, the `change:` filter and the diff
   Browse preset, diff CSS, the icon and label, "Compare two syncs…",
   the sanitizer test.
6. *(built)* Slack's second capture and diff group. `chat-common`
   needed nothing that contacts did not show.

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
