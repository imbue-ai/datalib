# Splitting download from render

**Status: built (2026-09-09).** Written as a proposal against
`a4752fb5` and kept as the explanation. The numbers below were
re-measured against `0d7115c9`, where the tree had moved; where the
build disagreed with the proposal, the proposal is corrected in place
and the correction is marked.

The rules this leaves behind are in
[`AGENTS.md`](../../../AGENTS.md#download-and-render-are-separate-crates).
This file is the reasoning and the measurements.

## The problem, in one sentence

Changing how something was **rendered** rebuilt and re-ran the code
that **downloads** it, and downloading is the complicated half.

## What the measurement found

`//datalib/backend/schema` — the crate holding `GridRow`, `edges`,
`markdowns` and the other denormalized tables the UI reads — had **105
test targets** downstream of it:

```sh
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'
```

Most of those lived in provider packages, and a large share were purely
about downloading: `chatgpt_live`, `claude_reset_and_redownload`,
`github_child_prune`, `slack_dm_download`, `slack_history_prune`,
`slack_config_change_backfill`, `lightroom_real_catalogs`,
`notion_playback_roundtrip`, and every other `*_live` and
`*_playback_roundtrip`. None of them can be affected by a `grid_rows`
column moving. All of them rebuilt and re-ran when one did.

### The dependency was already one-sided

Across all 20 provider crates, **zero** files under any `src/download/`
referenced `datalib_schema`. Every single reference was in
`src/render/`, `render.rs`, or a render helper. So this was not a
code-untangling job: the dependency already stopped at the render
boundary on its own, and there was simply no crate boundary drawn there
to make Bazel — or the compiler — notice.

### Splitting the providers alone would have fixed nothing

Every provider's download side depends on `//datalib/backend/etl`, and
`datalib_etl` had an edge to `schema`. So a download binary reached
`schema` through the framework whatever happened to the provider
crates, which is why the stages below are in the order they are.

## What the build corrected

Three things the proposal got wrong, all found by measuring rather than
by reading.

### `datalib_etl` reached `schema` by three routes, not one

`somepath` showed the direct edge and stopped looking. `allpaths` shows
three:

```sh
bazelisk query 'allpaths(//datalib/backend/etl:datalib_etl, //datalib/backend/schema:datalib_schema)'
# //datalib/backend/app_schema:app_schema
# //datalib/backend/core:datalib_core
# //datalib/backend/etl:datalib_etl
# //datalib/backend/schema:datalib_schema
# //datalib/backend/unified_index:datalib_unified_index
```

Two of the three were dependencies on paper only. `datalib_etl` named
`datalib_unified_index` in `deps` and referenced it in no file under
`src/` at all. It wanted `datalib_core` for exactly one line of
`latchkey.rs` — the bundled-Node resolver, which lives in
`datalib_runtime` precisely so that callers need not take `core`.
`datalib_core` in turn named `datalib_schema` and referenced it
nowhere, reaching the render schema only through `app_schema`'s
`PortableTable` impls.

Moving the three render files without cutting those two edges would
have looked like a refactor and bought nothing.

### `processor.rs` was schema-coupled too, and the proposal did not mention it

`RunCtx` — the run context handed to *every* processor, download and
render alike — carried the render document sinks, and a
`RenderedMarkdown` holds `GridRow`, `EdgeRow` and `RenderProblemRow`.
So the shared harness put the render schema in front of every
downloader independently of the three files above.

The split turned out to be clean, because the two phases share nothing.
Measured across all 17 renderers, a render processor uses exactly
`name`, `root`, `progress`, `prior_fingerprints` and the three sinks; a
download processor uses `control`, `now`, the checkpoint sink, the
metrics and diagnostics scopes, and `open_store`. Disjoint sets. The
fused `RunCtx` had every field behind an `Option` and two accessors
that panicked on the wrong phase; splitting it removed both.

### Stage 3 was "mechanical" in the end, but not for the stated reason

The proposal called the provider split mechanical because the render
code was already in its own directory. The thing that could actually
have made it hard is a download file calling *into* render, which grep
finds in four providers. Three of those four are doc comments. The
fourth — beeper's `download/index_db.rs` — imported three uuid recipes
from `crate::render`, but those recipes already lived in
`download::schema_raw` and `render/mod.rs` merely re-exported them. So
the real cycle count was zero, and the fix was to name them where they
live.

That is worth knowing because it is now enforced. Rust crate graphs are
acyclic, so the same import would no longer compile.

## What was built

### Stage 1 — `BulkUpsertable` got its own crate

`schema/src/bulk.rs` is about fifty lines: one trait, four associated
constants, two methods, and `sqlx` as its only dependency. Every
provider's `download/schema_raw.rs` derives an impl of it. It now lives
in `datalib_table`, a leaf crate following the
`//datalib/backend/runtime` pattern, which AGENTS.md describes as
having no dependencies *deliberately*, for exactly this reason.

`PortableTable` emits `::datalib_table::BulkUpsertable`; the
`RawTable` / `WirePayloadRow` / `CasEdgeRow` derives keep emitting
`::datalib_etl::bulk::…`, which `datalib_etl` re-exports from the new
crate — so no provider call site changed.

Two things fell out. `app_schema` had named `datalib_schema` *only* for
the trait path the derive used, and now names `datalib_table`. And
`datalib_schema` dropped `extern crate self as datalib_schema`, which
existed solely so that derive path resolved inside `schema` itself.

**105 → 95.** The ten that went are `app_schema`, `core`, and all eight
of `datalib-http`'s. The server does not open the index — only the
`unified_index` applet does — and the build now agrees.

### Stage 2 — the render machinery left `datalib_etl`

`indexed_markdown.rs`, `grid_index.rs` and `section.rs` moved to
`datalib_etl_render`, which depends on `datalib_etl` and
`datalib_schema`. `grid_rows_load` moved with `grid_index`, which it
links. `datalib_etl::processor` kept `DataProcessor` and a download-only
`RunCtx`; `datalib_etl_render::processor` got `RenderProcessor`,
`RenderCtx`, `RenderPass` and the three callback types, and
`render_version()` moved to `RenderProcessor`, where every
implementation of it already was. `PlannedSource::processors` became a
`Wave` enum, since the two phases no longer share a trait object.

**95 → 94.** That is the expected shape, not a disappointment: the
provider crates still named `schema` directly, so this stage is the
prerequisite rather than the payoff.

### Stage 3 — the provider crates split

Each provider became `datalib_etl_<p>` (download, no `schema`) plus
`datalib_etl_<p>_render`. 14 already had `src/render/` as its own
directory; three had a flat `src/render.rs` (google_takeout, linkedin,
sms_backup_restore), and linkedin additionally renders from `posts.rs`
and `connections.rs`, which moved with it. Three providers have no
render side at all — fsindex, media and lightroom — and got no
`_render` crate; their stub `plan_render` is gone, replaced by
`download_only!` in `dispatch.rs`, which parses the render params (so a
typo is still rejected) and plans nothing.

Each provider's `*_unittests` is a `crate = ` test over the whole
library, so it split with the crate — which was part of the point,
since one crate test used to cover both halves and re-run for either.
The integration tests stayed in the download package and take a
dependency on the render crate only where their own sources need one.

**94 → 79**, and every download-only target is out: `chatgpt_live`,
`claude_live`, `jmap_live`, every `fsindex_*`, `lightroom_*` and
`media_*`, and every download-side `*_unittests`. The 17 targets added
are the new `<p>_render_unittests`, which *should* depend on `schema`.

## What this does not fix

- **`datalib_etl` is still a wide crate.** ~80 test targets depend on
  it, and none of this changes that; it only stops `schema` from
  reaching them. A change to the shared ingest machinery still costs
  what it costs.
- **The render side keeps its own blast radius**, and should. A
  `grid_rows` column moving *ought* to rebuild every renderer.
- **This was not a runtime change.** No behavior moved.

## Checking it

The query that motivated this is the query that checks it. **79**
today. Record the number in the commit message whenever it changes, so
a later regression is visible rather than inferred.

And, because this was a pure refactor, `bazelisk run //:lint_repo &&
bazelisk test //...` passing unchanged is the whole correctness
argument. On the hermetic subset that is 161 of 161 (144 before, plus
the 17 new render unittests).

One check to be careful with: `scripts/lint_repo.py`'s render-side
rules find their files by matching `/src/render` under
`etl/providers/`, which still matches after the move
(`<p>_render/src/render/…`). Both HEAD and the split index list 60 such
files, so the check is looking at the same set and not passing
vacuously — the failure mode AGENTS.md's "test-quality claims" warning
describes.
