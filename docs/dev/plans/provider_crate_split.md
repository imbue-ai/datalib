# Design: splitting download from render

**Status: proposal, nothing built.** Written 2026-09-09 against
`a4752fb5`. Every measurement below was taken from that tree; per
[`AGENTS.md`](../../../AGENTS.md), don't cite this file as a description
of what exists. When a stage lands, rewrite the section it makes real.

This is the prerequisite for
[`data_centric_ui.md`](data_centric_ui.md), which wants to add a
column-type vocabulary without that vocabulary's next edit rebuilding
every downloader. It is worth doing on its own terms regardless.

## The problem, in one sentence

Changing how something is **rendered** rebuilds and re-runs the code
that **downloads** it, and downloading is the complicated half.

## What the measurement found

`//datalib/backend/schema` — the crate holding `GridRow`, `edges`,
`markdowns` and the other denormalized tables the UI reads — has **97
test targets** downstream of it:

```sh
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'
```

67 of those 97 live in provider packages, and a large share are
purely about downloading: `chatgpt_live`, `claude_reset_and_redownload`,
`github_child_prune`, `slack_dm_download`, `slack_history_prune`,
`slack_config_change_backfill`, `lightroom_real_catalogs`,
`notion_playback_roundtrip`, and every other `*_live` and
`*_playback_roundtrip`. None of them can be affected by a `grid_rows`
column moving. All of them rebuild and re-run when one does.

The crate itself already knows this is wrong. `schema/src/lib.rs`
opens with:

> Datalib **render schema** crate — the "universal schema" for the
> denormalized tables that back the grid / UI.

### The dependency is already one-sided

Across all 20 provider crates, **zero** files under any `src/download/`
reference `datalib_schema`. Every single reference is in `src/render/`,
`render.rs`, or a render helper. So this is not a code-untangling job.
The dependency already stops at the render boundary on its own; there
is simply no crate boundary drawn there to make Bazel notice.

### But splitting the providers alone would fix nothing

This is the part that is easy to get wrong. Every provider's download
side depends on `//datalib/backend/etl` (the shared ingest machinery),
and:

```sh
bazelisk query 'somepath(//datalib/backend/etl:datalib_etl, //datalib/backend/schema:datalib_schema)'
# //datalib/backend/etl:datalib_etl
# //datalib/backend/schema:datalib_schema
```

A direct edge. So a download binary reaches `schema` through
`datalib_etl` whatever happens to the provider crates. **Splitting the
providers without first splitting `datalib_etl` buys nothing at all**,
which is why the stages below are in the order they are.

### What `datalib_etl` actually uses it for

Four files, and they sort cleanly:

| File | Uses | Side |
| --- | --- | --- |
| `indexed_markdown.rs` | `grid_rows`, `edges`, `markdowns`, `measurements`, `render_problems` | render |
| `grid_index.rs` | the same, plus `source_cursors` | render |
| `section.rs` | `providers::Provider` | render (consumed only by three providers' `src/render/`) |
| `bulk.rs` | one line: `pub use datalib_schema::bulk::BulkUpsertable;` | **download** |

(`doltlite_raw.rs` matches a grep for `datalib_schema` but only as the
string constant `__datalib_schema_probe__`. Not a dependency.)

So exactly one thing the download side needs from `schema` — the
`BulkUpsertable` trait — and it reaches it through a re-export.

### `BulkUpsertable` is a leaf

`schema/src/bulk.rs` is about fifty lines: one trait, four associated
constants, two methods, and its only dependency is `sqlx`. It uses
nothing else from `schema`. Every provider's
`download/schema_raw.rs` derives an impl of it, and the derive in
`etl/macros/src/lib.rs` emits `::datalib_etl::bulk::BulkUpsertable`
— the re-export path, not the original.

That last detail is what makes the fix cheap: **the download side
already refers to the trait by a path that isn't in `schema`.**

## The plan

Three stages. Each is independently landable and independently
measurable, and the first one carries most of the benefit.

### Stage 1 — give `BulkUpsertable` its own crate

Move `schema/src/bulk.rs` into a new leaf crate (working name
`datalib_table`) whose only dependency is `sqlx`. This follows the
existing `//datalib/backend/runtime` pattern, which AGENTS.md
describes as having no dependencies *deliberately*, for exactly this
reason.

Then:

- `datalib_etl::bulk` re-exports from the new crate instead of from
  `schema`. **No call site changes** — the path providers use is
  unchanged.
- `datalib_schema::bulk` re-exports it too, so `PortableTable`'s
  emitted `::datalib_schema::bulk::BulkUpsertable` keeps resolving.
- The `RawTable` / `WirePayloadRow` / `CasEdgeRow` derives keep
  emitting `::datalib_etl::bulk::…`, also unchanged.

One constraint to respect: `schema/src/lib.rs` carries
`extern crate self as datalib_schema` so `PortableTable` can emit an
absolute path for structs defined inside `schema` itself. That stays
as it is; the self-alias is about resolution inside one crate, not
about where the trait lives.

### Stage 2 — move the render machinery out of `datalib_etl`

`indexed_markdown.rs`, `grid_index.rs` and `section.rs` move to a new
`datalib_etl_render` crate that depends on `datalib_etl` and
`datalib_schema`. After stage 1 those are the only things left holding
the edge, so `datalib_etl` drops its `schema` dependency here.

**This is where the blast radius actually falls.** Every download
binary now reaches neither `schema` nor the render machinery.

### Stage 3 — split the provider crates

Each provider becomes `datalib_etl_<p>` (download, no `schema`) plus
`datalib_etl_<p>_render` (render). Three groups:

- **14 providers already have `src/render/`** as its own directory —
  beeper, chatgpt, claude, contacts, email, github, gitlab, notion,
  pdf, perseus, signal, slack, whatsapp, yolink. Mechanical.
- **3 providers have a flat `src/render.rs`** — google_takeout,
  linkedin, sms_backup_restore. linkedin is the awkward one: its
  `posts.rs` and `connections.rs` sit at the top level and both use
  `schema`, so they need sorting into the render half first.
- **3 providers have no render side at all** — fsindex, media,
  lightroom. Their BUILD files already carry no `schema` dependency;
  they need nothing.

Note that each provider's `*_unittests` is a `crate = ` test over the
whole library, so it splits with the crate — which is part of the
point, since today one crate test covers both halves and re-runs for
either.

## What this does not fix

- **`datalib_etl` is still a wide crate.** 80 test targets depend on
  it, and stages 1–3 don't change that; they only stop `schema` from
  reaching them. A change to the shared ingest machinery still costs
  what it costs.
- **The render side keeps its own blast radius**, and should. A
  `grid_rows` column moving *ought* to rebuild every renderer.
- **This is not a runtime change.** No behavior moves; the shipped
  binaries do the same things. If any test's result changes, something
  is wrong.

## Verifying each stage

The query that motivated this is the query that checks it:

```sh
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'
```

97 today. After stage 2 it should drop sharply; after stage 3 it
should contain no download-only test target. Record the number in the
commit message for each stage, so a later regression is visible rather
than inferred.

And, because this is a pure refactor, `bazelisk run //:lint_repo &&
bazelisk test //...` passing unchanged is the whole correctness
argument.

## One piece of stale prose to fix along the way

`AGENTS.md`'s doc map describes
[`step_identity.md`](completed/step_identity.md) as a *proposal* of which
"nothing in it is built". **It shipped**, and the doc itself says so —
its banner reads "Status: built (2026-08-31)". It is only the doc
map's summary that is wrong. `config.rs`'s `StepEntry`
has no `outputs` field at all — the fields are `id`, `name`, `inputs`,
`command`, `params`, `env`, `code_version`; `inputs` holds step ids;
and `config.rs` synthesizes `--outputs` for the child as "the single
tree its id names, so steps written against the old contract keep
working". The shipped config examples confirm it
(`id = "claude_chats/raw"`, `id = "unified_index/grid"`).

Correct the doc-map entry as part of stage 1; the doc it points at
needs nothing. It matters beyond tidiness: **a step now has
exactly one output tree, named by its id**, and
[`data_centric_ui.md`](data_centric_ui.md) relies on that fact to
decide where a step's run log lives.
