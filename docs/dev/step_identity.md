# Step identity: the id *is* the path

**Status: built (2026-08-31).** A step's `id` is the path it writes,
`inputs` name step ids instead of artifact paths, and the places that
recovered an identity by splitting a string are gone. Phase 1 (the
`name` / `id` split in the wizard and the config) shipped separately —
see [`source_wizard.md`](source_wizard.md#two-names-id-is-the-identity-name-is-what-you-type).

This file was written as the design and kept as the explanation. Where
it says "proposal" below, read "what was done"; the two places the build
diverged from the plan are marked.

## Where we are

A step has an `id` (`slack.download`) and declares `outputs`
(`slack/raw`). Those two strings are related only by convention — the
runner never parses the id, and `datalib-step` never reads it. The
identity a person cares about, "which source is this", is the *first
path segment of the outputs*, and six separate places recover it by
splitting a string:

| | Where | How |
|---|---|---|
| 1 | `datalib_step::source::name_from_outputs` | `outputs[0].split('/')` — to rebuild the path it was already handed |
| 2 | `grid_index::build_grid_index` | `entry.file_name()` per directory — sets `markdowns.source_name` |
| 3 | `dolt_repo::source_name_from_qmd_path` | `qmd_path.split_once('/')` — the grid's Source column |
| 4 | `db::build_where` | `INSTR(qmd_path, ?) = 1` — a prefix test standing in for an equality |
| 5 | `sourceSteps.ts` | `out.split("/")` — grouping steps into sources |
| 6 | `Manager2View.jobFor` | `id.startsWith(name + ".")` — parses the step id to attribute a job |

Two of those (#3, #4) were added by phase 1 and are the reason this
document exists: the pattern reproduces itself every time something new
needs to know which source a row came from.

Site #2 deserves a note. `build_grid_index`'s doc comment claims it is
"off the hot path now", but `datalib-step grid_index` calls it directly,
so the directory walk *is* how every `markdowns.source_name` is set.
Stale prose of the kind AGENTS.md warns about; the code is the truth.

## The change

```toml
[[steps]]
id      = "work-slack/raw"          # identity AND the tree it writes
name    = "Work Slack"              # what a person sees
command = "datalib-step download slack_api"

[[steps]]
id      = "work-slack/rendered_md"
name    = "Work Slack (render markdown)"
command = "datalib-step render slack_api"
inputs  = ["work-slack/raw"]        # a step id — which is also a path

[[steps]]
id      = "unified_index/grid"
name    = "Search index"
command = "datalib-step grid_index"
inputs  = ["work-slack/rendered_md"]
```

**`outputs` is gone.** A step writes exactly one tree, `<id>/`, and
nothing else. That is the whole idea: `inputs = ["work-slack/raw"]` is
simultaneously a step id and a path, so "reference a step" and "declare
an artifact" stop being two mechanisms. Nothing is ever parsed — whole
strings are compared.

### Rules

- An id is one or more `/`-separated segments, each matching
  `[A-Za-z0-9._-]+`. No `.`, `..`, no leading `-`, no empty segment.
- Ids are unique. That is already enforced (`validate_steps`, again in
  `Graph::build`).
- A step's tree is `<data_root>/<id>/`. **Single-writer becomes true by
  construction** — two steps cannot write one tree without sharing an
  id, which is already refused.
- No step's id may start with `system/`, which is the runner's and the
  server's own state.
- `inputs` is a list of step ids. An input naming no declared step is
  an error, not a staged path (see below).

### The disk layout does not move

`work-slack/raw` and `work-slack/rendered_md` are exactly the
directories that exist today, and `unified_index/grid` /
`unified_index/qmd` are exactly where the index steps already write.
Path-shaped ids describe the current layout rather than replacing it —
`markdowns.md_path` and `grid_rows.qmd_path` stay valid, and no
rendered file moves.

`raw` and `rendered_md` stay as the leaf names. They describe what is
*in* the directory rather than what made it, which is what you want when
browsing on disk — and they avoid "download", a word this codebase
overloads for local ingestion that downloads nothing.

The reserved-name list mostly evaporates: `unified_index` cannot be
claimed by a source because the index steps already hold
`unified_index/grid` and `unified_index/qmd`, and id uniqueness does the
rest. Only the `system/` prohibition is still a rule of its own.

## What gets deleted

- `name_from_outputs` and its tests — `datalib-step` reads its id from
  `DATALIB_DAG_STEP` (which the runner already sets) and forms its own
  paths. **No step-protocol change**: the step already receives
  everything it needs.
- `PHASE_BY_SUFFIX`, `STANZA_OUTPUT_SUFFIXES`, `RESERVED_STANZA_NAMES` —
  the `raw` / `rendered_md` suffix convention and the reserved-directory
  rule exist only because two steps share one tree today.
- The source-grouping logic in `sourceSteps.ts`, and `jobFor`'s
  `id.startsWith(name + ".")`.
- `ArtifactPat` and its wildcard machinery: `overlaps`,
  `conflicts_with`, `is_concrete`, the segment recursion. Edges become
  what is written, and ownership conflicts become id collisions.
- `synthesize_staged_sources` — see below.
- `source_name_from_qmd_path` and the `INSTR` filter, replaced by a real
  `step_id` column and an equality test.

## Fan-ins: explicit lists, maintained by the wizard

`grid_index` and `qmd_index` declare `inputs = ["**/rendered_md"]`
today, and that glob is what makes a new source feed the index
automatically. With step-id inputs it becomes an explicit list, and the
wizard appends to it when a render step is created.

The cost is that a hand-editor must remember to wire the fan-in. The UI
covers it cheaply: **a step whose id appears in no other step's
`inputs`, and which isn't a leaf by design, gets a warning badge in the
Pipeline table.** That is better than a glob, because the config then
says what the DAG does rather than implying it.

If maintaining the list turns out to be annoying in practice, the
fallback is a glob over *step ids* (`work-slack/*`, `*/rendered_md`) —
still id-space, not path-parsing. The proper answer is artifact kinds
(a step declares what it `produces`, a fan-in what it `consumes`),
which is deferred until something needs it.

## Staged inputs

`synthesize_staged_sources` invents a source step for any concrete input
no step writes, so a hand-staged Takeout export or Signal backup can
still drive change propagation.

**No shipped config uses it.** Every `inputs` entry in
`all_sources.toml` names another step's tree; file-backed sources point
`common.input_path` (a *param*, usually outside the data root) at the
staged data instead. So the machinery serves a shape nothing ships.

Deleted, as proposed. An input naming no declared step is now an error
whose message points at `common.input_path`. If the capability is wanted
back later it returns as an explicit `staged_inputs = [path]` key, which
is honest about being a path rather than a step reference.

**Divergence from the plan.** Three scheduler tests covered this. The
first and third — a staged tree's change propagating, and subset-sync
skipping a step with an external input — tested only the removed
capability and were deleted. The second tested a property that still
holds (an input that *disappears* versions as `absent`, which differs
from a hash, so the consumer re-runs and can drop stale output) and was
rewritten against a declared producer whose tree the user deletes.

**No behavior change for any shipped shape.** An earlier draft of this
section claimed input-less render steps started running every sync.
They already were: `claude-export` in `all_sources.toml` had declared
no `inputs` since before this change, with a comment saying exactly
that ("With no `inputs` this is a fringe step … so it runs every
sync"). The `inputs = ["<path>"]` shape the staged-source machinery
served was used by nothing that ships.

(As of #207 `claude-export` is no longer that example: it ingests its
export into a raw store, so it is an ordinary `raw` → `rendered_md`
pair like every other file-backed source. The point above still holds
for the change this document describes; it just no longer has a
shipped illustration.)

The one config that did use the staged shape was the TNG fixture, which
gave `yolink` — whose raw store the harness pre-seeds — an `inputs =
["yolink/raw"]` naming a tree nothing writes, so the runner synthesized
a source step to hash a directory that never existed. That is now
`inputs` omitted. The fixture got more correct, not less.

## Migration

Ids change (`slack.download` → `slack/raw`), and that is the whole
migration. Directories do not move.

- **`system/dag_state.json`** is keyed by step id. Simplest is to drop
  it and let every step re-run once. That is nearly free: downloads are
  incremental against a raw store that hasn't moved, renders skip on
  `source_fingerprint`, and `grid_index` skips on the same. Remapping
  the keys is possible — the converter knows old → new — but not worth
  the code.
- **`config.toml`** is rewritten by `datalib-migrate-config`, which
  already exists for exactly this and already holds every retired
  schema.
- **The index** needs nothing. `markdowns.md_path` and
  `grid_rows.qmd_path` still name files that are still there.
- **Applet `params.tree`** values (`slack/rendered_md`) are unchanged,
  since the trees are unchanged.

Worth stating because it was the scary part earlier: `grid_index` skips
a document whose `source_fingerprint` matches, and that fingerprint
excludes the output path — so if directories *did* move, every
`qmd_path` would silently strand. They don't move, so this hazard is
avoided rather than handled.

## UI: ungrouping, and the chained wizard

Sources stop being a grouping. Every `[[steps]]` entry is one row, so a
render step is independently editable, runnable, and visible in the
disk-usage column — which is a better picture of where bytes go than one
row covering both trees.

The wizard becomes two dialogs:

1. Configure the fetch step. Name → id, as phase 1 already does, except
   the id is now `<stem>/raw` and **the suffixing happens on the stem**:
   a second "Work Slack" is `work-slack-2/raw`, since the whole
   `work-slack/` tree is what must be unique.
2. On finishing, a button: *also render this to markdown?* The render
   dialog opens pre-filled with name `"<name> (render markdown)"` and id
   `<stem>/rendered_md`, and `inputs` pointing at the step just created.

**Both the chained and the standalone flow mint that id the same way**:
split the input step's id on its first `/` and append `rendered_md`.
The chained flow could carry the stem in memory instead, but two code
paths minting one string is how they drift — one `stemOf(inputId)`
helper, used in both.

That split is a deliberate exception to "no parsing", and worth naming
as one: it is a *UI proposing a default* from a string the user just
selected, never a program resolving identity. Nothing downstream depends
on the shape of an id.

Names drift once the steps are independent — rename the fetch step and
the render step keeps its old name. That is correct, since they are
separate steps, and the muted id beside each keeps the table legible.

## What this reverses

`pipeline_dag_architecture.md` and the config docs state that edges are
derived from artifact-path overlap and "never written by hand". That was
the right call when a step's outputs were free-form paths; it stops
being right once a step owns exactly one tree named by its id, because
then the overlap test can only ever tell you what the ids already say.
Those docs get rewritten as part of this change, not footnoted.

## Deferred

- **Artifact kinds** (`produces` / `consumes`), which would let a fan-in
  say "every markdown tree" without a list or a glob.
- **Telling `grid_index` a document's source as data.** It still learns
  it by walking one directory per source, which now reads the id off the
  directory name — the same string, since the directory *is* the id. A
  `SidecarHeader` field or a per-tree marker would make it explicit;
  deferred along with the `grid_rows.source_id` column it would feed,
  since the grid's Source column works today off `qmd_path`.
- **`step_runs`** — a per-step run record so the table can show what is
  running, queued, and waiting on what. Today `sync_jobs` is per *run*,
  which is why the table shows `~`. Separate from this change; see the
  notes on `sync_runs` in the PR discussion.
- **Renaming an id for real**, which remains a migration for the same
  five reasons phase 1 documented.
