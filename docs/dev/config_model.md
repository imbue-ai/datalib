# The config model: groups, steps and functions

What a `config.toml` is made of, the naming rules, how an ingest step
says where its data comes from, and which parts of the tree read
each of those. The runner's own view — what it fingerprints, what it
forwards, what a bad entry costs — is
[`datalib/backend/dag/README.md`](../../datalib/backend/dag/README.md);
the contract a step command implements is
[`step_protocol.md`](step_protocol.md).

## Three kinds of entry

```toml
[[groups]]
id = "work-slack"          # the directory; permanent
name = "Work Slack"        # free text; nothing depends on it
type = "slack"             # what is mirrored — makes this a source

[[steps]]
group = "work-slack"
function = "ingest"        # id composes to work-slack/ingest
[steps.params.api]         # the method: Slack's own API
channels = ["chat-qi"]

[[steps]]
group = "work-slack"
function = "render_markdown"
inputs = ["work-slack/ingest"]

[[groups]]
id = "unified_index"       # no type: it mirrors nothing

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["work-slack/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
```

**A group** is one thing on the Manage screen: an `id`, an optional
`name`, an optional `type`, an optional `description`. A source is a
group with a `type`; the unified index is a group without one. `name`
and `description` are display text — never forwarded to a step, never
fingerprinted — so editing either re-runs nothing.

**A step** is `(group, function)`. The loader composes its id as
`<group>/<function>`; it is never written, and nothing downstream
splits it. That id is the tree the step writes, the key its state is
kept under, and what another step's `inputs` name. A step with no
`command` runs the built-in `datalib-step`, which reads its function
and its group's `type` from the environment, so it has to be under a
group. A custom step names a `command` and any function word it likes,
or sits outside any group with a verbatim `id` — the one place a step
id is written.

**An applet** is a server the http gateway spawns on demand. Its `id`
is the mount prefix and a JavaScript identifier, unique on its own and
not composed; `group` on an applet only says which Manage row it is
filed under.

**A diff group** is a group of `type = "diff"` with a `source`: it
mirrors nothing itself and renders what changed in the `source` group's
raw store between two commits, as documents with the changes marked and
`grid_rows` with a `diff_status`. Its one step is `render_markdown`,
reading `<source>/ingest`, with the two commits under `params.diff`
(`from` and `to`, both required, and `max_documents`, the most
documents a side may render before the step fails, default 1000); the
rest of `params` is the source type's render config. The fan-ins name it like any render step. The
source's id and type reach the step as `DATALIB_DAG_SOURCE_GROUP` and
`DATALIB_DAG_SOURCE_GROUP_TYPE`, set by the loader in the step's `env`. `docs/dev/plans/completed/diff_renderer.md`
has the design; `configs/dag_example.toml` has one.

`configs/dag_example.toml` is the commented, complete version;
`docs/user/config_examples/` has the shapes people start from.

## Naming rules

- A group id and a function are each one portable filename segment:
  letters, digits, `.`, `_`, `-`, not starting with `-`, no `/`.
- `system` is reserved for the runner's and server's own state.
- Step ids are unique and never nested: two steps under one tree would
  be two writers on one doltlite file.
- The built-in functions are the directory names: `ingest`,
  `render_markdown`, `grid_index`, `qmd_index`
  (`datalib_step/src/function.rs`, tested against the layout constants
  the render and index crates use). The two index steps write
  `unified_index/grid_index` and `unified_index/qmd_index` and nothing
  else, because the applet that reads them finds them from the data
  root alone.
- A group's `type` names the thing mirrored, never the way it is
  reached — `claude` over the API or from an export, `contacts` over
  CardDAV or from `.vcf` files — and the product a person recognizes,
  not the vendor. Where a `grid_rows.provider` tag exists for a type,
  the two spell it the same way. `SourceType` in
  `datalib_step/src/source_type.rs` is the closed list.

## Where the data comes from: the method table

An `ingest` step's params hold one table per method, and the table's
name is read *under the type*: a product has one API, so its table is
just `api`, and a type that is not one product qualifies its sources
(email's `jmap`, `gmail`, `mbox`). A table that reads files carries
its own `path`. There is no global vocabulary — each provider's
`<p>_config` crate declares its own two or three:

| type | methods |
|---|---|
| chatgpt, garmin, github, gitlab, notion, slack, yolink | `api` |
| claude | `api`, `export` |
| contacts | `carddav`, `vcf` |
| email | `jmap`, `gmail`, `mbox` |
| airvisual, facebook, google_takeout | `export` |
| linkedin | `export`, plus `export.fetch_photos` |
| signal, sms_backup_restore, whatsapp | `backup` |
| fsindex, media, pdf | `fswalk` |
| beeper `texts` · apple_messages `database` · apple_photos `library` · lightroom `catalog` · claude_code `sessions` · perseus `github` | |

That table is `ui/src/config/ingestMethods.json`, the one place to
read the list from.

A method is *held* when its path is written and its value is neither
`null` nor `false`: a table counts by presence (`api = {}` is a
complete selection), a flag such as linkedin's `export.fetch_photos`
only when on. A provider with two tables refuses a step naming both
(`claude_config`'s `validate`), so a store is filled one way.

Knobs that apply whatever the method (email's `only_extract_labels`,
the `common.*` envelope) stay at the top level of `params`. Render
knobs go on the render step, whose config is strict: download-shaped
params on a `render_markdown` step are refused.

## `Reach`: origin or local

Every method a provider accepts declares itself `Origin` (fetches from
a live service) or `Local` (reads files already on this machine).
The enum is `Reach` and the declaration is `impl IngestMethods for
<P>Config` — a list of `IngestMethod { path, reach }` — both in
`datalib/backend/source_common/src/lib.rs`. `datalib_step/src/methods.rs`
maps a type to its list and applies the held rule; a step's reach is
`Origin` if anything held reaches one, else `Local`.

The UI reads the same declarations without a hand-kept copy:
`ui/src/config/ingestMethods.json` is generated by
`bazel run //datalib/backend/datalib_step:ingest_methods.update`, a
test in `methods.rs` fails when it drifts, and
`ui/src/config/ingestMethods.ts` applies the same held rule
(`methodsHeld`, `ingestReach`, `ingestLabel`). Two things read it:

- **The Manage row's label.** The server labels an ingest step
  "Ingest" (`http/src/manage/mod.rs::child_label`); the browser
  replaces that with "Download" for an origin method or "Import" for a
  local one (`cards/SourcesCard.ce.vue`), off the step's written params.
- **The wizard's Connection section.** A descriptor with a
  `credentialService` shows its latchkey controls only while the
  params the form would write reach an origin: an import has nothing
  to log in to (`SourceWizard.vue`).

`datalib-dag --reset` is *not* gated on it: it empties the step's store
whatever the method, and the next sync reads its files or its origin in
full — the only "from scratch" button a user has. The built-in ingest
driver only logs the reach.

## What the loader checks

The rules sit in `datalib/backend/dag/src/config.rs` (`accept_groups`,
`accept_steps`, `accept_applets`) and `graph.rs`; `diagnostics.rs`
explains the severities. A bad entry is dropped and the rest of the
file runs.

| the entry | is dropped when |
|---|---|
| a group | its id is not one segment, is `system`, or is a duplicate; it is a `diff` group with no `source`, or whose `source` names no group, a typeless group or another `diff` group; it carries `source` without being a `diff` group |
| a step | its function is not one segment; its group is undeclared; it has no `command` and no group; its `command` is the retired `datalib-step download\|render\|grid_index\|qmd_index …` shape; its id is under `system`, duplicated, or nested with another; an input names itself; under a `diff` group it is not `render_markdown`, or it has inputs and the first is not `<source>/ingest` |
| a step (blocked: fix elsewhere) | its group or an input was itself dropped; an input names no step; it sits on a cycle |
| an applet | its id is not a JS identifier, is `user`, or is a duplicate |

Warnings: a group with nothing filed under it; a `name` on a grouped
step (the label comes from the group and the function); an applet
filed under an undeclared group.

## What the runner forwards and what `datalib-step` refuses

Every grouped step gets `DATALIB_DAG_STEP` (the composed id),
`DATALIB_DAG_GROUP`, `DATALIB_DAG_FUNCTION`, and `DATALIB_DAG_GROUP_TYPE`
when the group has one. The group's `type` is in every child step's
fingerprint, so changing it re-runs the group.

`datalib-step` (`src/source.rs`, `main.rs`, `dispatch.rs`,
`methods.rs`) refuses, naming the fix:

- no `DATALIB_DAG_GROUP`, or a function it does not perform;
- a step id that is not `<group>/<function>` as the environment
  spells them;
- `ingest` or `render_markdown` under a group with no `type`;
- a `type` it has no provider for — a retired spelling (`*_api`,
  `*_backup`, `claude_export`, `carddav`) names `datalib-migrate-config`;
- params still carrying `sync`, `common.input_path`, `common.raw_path`,
  `gmail_api` or a top-level `fetch_photos`, likewise naming the tool;
- an `ingest` step whose params hold none of its type's methods;
- `grid_index` or `qmd_index` under any group but `unified_index`.

A render reads its raw store from its first input; one that declares
none reads its own group's `ingest` tree and logs a warning. A store
kept on another disk is a symlink at `<group>/ingest` — no params key
can point elsewhere.

## The fan-ins read exactly their `inputs`

`grid_index` and `qmd_index` take the render stores they index from
the `inputs` they declare — each is `<group>/render_markdown`, and the
group is its first segment (`qmd_index::groups_from_inputs`, shared by
`grid_index.rs`). Nothing scans the root. A source removed from the
config stops being indexed on the next run even while its rendered
tree is still on disk, and `qmd_index` retires the collections no
group claims.

So a source's render step has to be named in both fan-ins' `inputs`,
or it renders and is never searchable. The wizard maintains those
lists — `wireIntoFanIns` on create, `unwireFromFanIns` on delete and
when a render step is removed (`ui/src/config/sourceSteps.ts`). A hand
edit has to remember, and the Manage screen flags a render step
nothing consumes.

## What the Manage screen and the wizard make of it

One row per group, its steps and applets under a chevron. The group
row's rules are in `http/src/manage/group.rs`: children in pipeline
order; status running if any child is, else failed, else stopped, else
the last step's; last synced is the ingest step's instant, else the
newest child's; bytes are the group directory's own measured series,
never a sum across children; a sync of the group starts at its steps
with no inputs. A child step is labelled by its function, with the
composed id muted beside it. Phase is read off `function` and the
grid's Source column joins `source_id` to a group's `name`; nothing in
the UI splits an id.

The wizard (`SourceWizard.vue`, writers in `sourceSteps.ts`) edits a
source as one thing: the group plus its `ingest` and `render_markdown`
steps from one form, render fields under a "Rendering" heading, one
name box for the group. Editing renames the group in place and
replaces both steps in one cut-and-append. A source missing one of its
two steps gets it back on save; a render step under a provider that
renders nothing (`renderStep: false` in `ui/src/config/catalog.ts`) is
removed and unwired, and the dialog says so before Save.

## The retired shapes

`datalib-migrate-config` (`datalib/backend/migrate_config/`) is the
one place an earlier config shape is understood: `datalib-step
download …` command lines become groups and command-less steps,
retired type words become the current ones, and `sync` /
`common.input_path` / `common.raw_path` become method tables with
their own `path`. The rewrite is value-level, so comments do not
survive. The loader and `datalib-step` recognize the old shapes only
well enough to name the tool.
