# TODO: finish the step-vocabulary rename (extract/translate/load → download/render/grid_index)

Status: **not started** — this doc is the handoff. The orchestration
layer already uses the standardized names; this task renames the
older strata underneath so the codebase has exactly one vocabulary.

## Background

The pipeline's four step types are standardized (see
`pipeline_dag_architecture.md`) as:

| standard name | what it does | older names still in code |
|---|---|---|
| `download` | fetch data from a vendor into the raw store | **extract** |
| `render` | render raw data into markdown + sidecars | **translate**, **render_and_index_md** |
| `grid_index` | load every sidecar into the unified grid table (`system/backend_index`) | **load**, "index" |
| `qmd_index` | build the qmd search index (`system/qmd`) | "qmd" |

The 2026-07 rename covered the *orchestration* layer: DAG config step
tokens (`slack_api.download`, `grid_index`, …), `datalib-step`
subcommands, UI templates/scaffold/migration, fixture pipeline, docs.
What remains is the **etl crate + provider layer**, where three
generations of names coexist, plus one small http/UI vestige.

## Scope

### 1. Provider crates (all 17 under `frankweiler/backend/etl/providers/`)

Per provider:

* `src/extract/` module tree → `src/download/` (module name, `use`
  paths, `mod` decls).
* `src/render_and_index_md/` module tree → `src/render/`.
* Processor structs and ids: `SlackExtract` → `SlackDownload`,
  `SlackRender` stays; the `DataProcessor::id()` strings
  (`"slack/{name}/extract"`, `"slack/{name}/translate"`) →
  `…/download`, `…/render`. **Gotcha:** these ids appear in logs and
  `with_context` messages; grep tests for assertions on them.
* Per-wave entry points already carry the new names
  (`plan_download` / `plan_render`) — only their bodies/comments
  reference the old modules.
* `EXTRACT.md` docs per provider → `DOWNLOAD.md` (and the references
  to them in `datalib_step/src/hints.rs` and provider READMEs).

### 2. `frankweiler/backend/etl` shared crate

* `SourcePlan { extract, translate }` → `{ download, render }`
  (fields + doc comments). Callers: sync is gone; the only users are
  `datalib_step/src/{download,render}.rs` and provider `plan_*` fns.
* `RunCtx::for_extract` / `for_translate` → `for_download` /
  `for_render` (`processor.rs`).
* `ExtractControl` → `DownloadControl` (`control.rs`),
  `ExtractReport`/`ExtractMetrics`/`MetricsSink` (`extract_metrics.rs`
  → `download_metrics.rs`), `ExtractParams` (lives in
  `frankweiler/backend/source_common` — **serde field name
  `extract_params` appears in user configs' `common:` blocks; see
  "wire compatibility" below**), `extract_run.rs`, `extract_params.rs`.
* `load.rs` → `grid_index.rs`: `load_all` → `build_grid_index` (or
  similar), `LoadSummary` → `GridIndexSummary`, `apply_one` can keep
  its name. The dolt commit message it writes is snapshot-pinned —
  see "gotchas".
* Module docs throughout (`processor.rs`, `doltlite_raw.rs`,
  `raw_store.rs`, …) say "extract wave" / "translate wave" — sweep to
  "download wave" / "render wave".

### 3. `frankweiler/backend/datalib_step`

Internal comments/uses track the etl API and update mechanically with
it (`download.rs` says "extract wave", `dispatch.rs` mentions
"extract processors", etc.). No CLI-visible changes — the subcommands
already have the right names.

### 4. http + UI job kinds (small, independent)

`SyncJobKind = "download" | "ingest" | "render" | "all"`
(`frankweiler/ui/src/api.ts`, mirrored in the http enqueue handler and
`app_schema::sync_jobs`): `ingest` and `render` are dead — they mapped
to the retired `--skip-extract` — and the UI now only enqueues `"all"`
(with `source_name` for subset sync). Prune to the kinds actually
used; keep the DB column free-form so historical rows still render.

### 5. Docs

`docs/dev/data_architecture_ingestion.md` and friends are historical
records — leave them. Update living docs only:
`pipeline_dag_architecture.md`'s terminology note should flip from
"the prototype chose…" to naming the old terms as historical, and
`docs/dev/data_processor_and_config_refactor.md` if still referenced.

## Wire / on-disk compatibility — do NOT rename these

* **Disk layout**: `system/backend_index/`, `system/qmd/`,
  `<name>/raw/`, `<name>/rendered_md/` stay exactly as they are.
  (`raw` and `rendered_md` are layout names, not step names; renaming
  them breaks every existing data root.)
* **Sidecar format** (`.grid_rows.json`) and the raw-store schema
  (tables, `<table>_bookkeeping`) are on-disk contracts — unchanged.
* **Config wire names**: `common.extract_params` in user configs. If
  renamed to `download_params`, add a serde alias
  (`#[serde(alias = "extract_params")]`) so existing configs parse;
  the migration endpoint should emit the new name.
* **CLI flags**: `--reset-and-redownload` / `--refetch-blobs` already
  read correctly; `FRANKWEILER_HTTP_PLAYBACK` and the
  `FRANKWEILER_DAG_*` envs are fine.
* Provider **tracing event names** (`slack_fetch_complete`,
  `signal_snapshot_already_ingested`, …) are asserted on by tests
  (`tests/fixtures/ingested_tng_test.py` greps
  `signal_snapshot_already_ingested`) and consumed by the obs stack —
  rename only with their assertions, or leave them (they're
  provider-domain names, not step names).

## Gotchas

* `fixture_db_snapshot_test` pins the grid-index dolt commit message
  (`"datalib-step grid_index: …"`); if `load_all`'s message changes,
  re-bless `frankweiler/backend/core/tests/snapshots/…`.
* Insta snapshots and golden tests in provider crates may embed
  processor ids or module paths in error strings.
* `core::sync_phase` is a leftover consumed only by
  `qmd_indexer`'s marker lines (nothing parses them anymore) — this
  rename is a good excuse to delete the module and the marker
  emission.
* `datalib_step/src/hints.rs` points at
  `providers/<p>/EXTRACT.md` in its remediation text.
* Keep it **pure rename**: no behavior changes, so the diff reviews
  mechanically. Land as its own PR.

## Verification

1. `bazelisk test //frankweiler/... //tests/... //tools/...` — all
   suites, including the hermetic TNG pipeline
   (`//tests/fixtures:ingested_tng_test`) and the UI e2e suite.
2. `cd frankweiler/backend && cargo build -p frankweiler-dag -p
   frankweiler-etl` (+ `bash tools/repin_cargo.sh` if any Cargo.toml
   changed).
3. A live smoke against a real data root
   (`configs/dag_example.yaml` style): full run, then a second run
   confirming renders/index skip — proves the serde alias kept old
   configs parsing.
