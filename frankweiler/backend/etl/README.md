# frankweiler-etl

Rust ETL pipeline that mirrors per-service data into the canonical
`grid_rows` table in Dolt. Each stage is a separate binary so they can
run independently, be scheduled separately, and be monitored
uniformly.

```
┌────────────┐    ┌──────────────┐    ┌─────────────────┐
│  Extract   │───▶│   Translate  │───▶│      Load       │
│ raw store  │    │ rendered_md/ │    │  grid_rows in   │
│ (per prov) │    │ + sidecars   │    │     Dolt        │
└────────────┘    └──────────────┘    └─────────────────┘
```

## Stages

| Stage     | Binary                      | Inputs                                  | Outputs                                    |
|-----------|-----------------------------|-----------------------------------------|--------------------------------------------|
| Extract   | `<provider>-download`       | upstream API                            | `<out>/raw/<name>.doltlite_db`             |
| Translate | called in-process by sync   | `<out>/raw/<name>.doltlite_db`          | `<out>/rendered_md/<provider>/...`         |
| Load      | `grid-rows-load` (generic)  | `<out>/rendered_md/`                    | rows in Dolt + `markdowns_loaded` bookkeeping |

Every provider's Extract step writes to a single sqlite file at
`<out>/raw/<name>.doltlite_db`. Object payloads, sync-run bookkeeping,
endpoint shapes, and blob bytes all live in tables there. See
[`DOLTLITE_RAW_PORT_GUIDE.md`](DOLTLITE_RAW_PORT_GUIDE.md) for the
shape and [`src/doltlite_raw.rs`](src/doltlite_raw.rs) for the shared
helpers.

Translate runs in-process inside `frankweiler-sync` — no per-provider
translate binaries.

Each provider is its own crate at
[`providers/<name>/`](providers/), named `frankweiler-etl-<name>`. The
provider crate owns its Extract + Translate code, bins, integration
tests, and the sample fixtures the tests run against — keeping sample
data right next to the code under test serves as documentation of
"what this provider's wire format looks like." The Load step is
provider-agnostic and lives at [`src/load.rs`](src/load.rs).

## Target schema

Every translator emits rows of the codegen'd `GridRow` struct defined in
[`frankweiler-schema`](../schema/src/generated/grid_rows.rs). The schema
source-of-truth (column types, indexes, comments) lives at
[`schema/grid_rows.yaml`](../../../schema/grid_rows.yaml).

## Cross-provider contract: the sidecar

Translate emits, for every document, two co-located files:

  * `<id>.md` — human-readable, with YAML frontmatter.
  * `<id>.grid_rows.json` — the [`Sidecar`](src/sidecar.rs):

```jsonc
{
  "header": {
    "document_uuid": "…",         // primary key for the document
    "source_fingerprint": "…",    // hash of upstream payload
    "render_version": 1           // renderer-side schema stamp
  },
  "rows": [GridRow, …]
}
```

The Load step reads the sidecar tree — it never re-parses markdown.

## Incrementality

A single concept threads through every stage: **`source_fingerprint`**.
Each stage stamps it into its output and reads it from upstream before
deciding to do work.

  * **Extract** dedups by content hash; pages whose every item matches
    a prior capture are skipped.
  * **Translate** computes `source_fingerprint` from the canonical raw
    payload; if the existing `.md` already carries that fingerprint,
    the write is skipped.
  * **Load** stores `(qmd_path, source_fingerprint)` in
    `markdowns_loaded`. Re-running with no upstream changes is a no-op:
    every sidecar is short-circuited before the DELETE/INSERT.

Bump `RENDER_VERSION` in the per-provider translate module (e.g.
[`providers/slack/src/translate/render.rs`](providers/slack/src/translate/render.rs))
to force a rebake even when payloads are unchanged.

## Observability

Every binary flattens [`obs::ObsArgs`](../obs/src/lib.rs) into its
clap parser, so every stage takes the same flags:

  * On a TTY, pretty log lines on stderr.
  * Otherwise, NDJSON events on stderr.
  * Either way, log emissions are routed through an `IndicatifWriter`
    that coordinates with the shared `MultiProgress` exposed by
    `frankweiler_obs::shared_multi()`, so progress bars attached by
    callers (e.g. sync's per-source bars) don't get stomped by log
    lines.
  * `--otlp-endpoint http://host:4317` exports spans + events via
    OTLP, so a single Tempo/Jaeger collector can ingest every stage.

Each stage emits a `*_start`, `*_complete`, and per-document progress
events with a stable prefix (`slack_download_*`, `grid_rows_load_*`,
etc.). The `*Summary` structs are `Serialize`, so a web UI can consume
the final stats line without knowing which provider produced it.

## Adding a new provider

Each provider is a sibling crate under `providers/`. Copy the Slack
crate as a template:

1. `cp -r providers/slack providers/<name>` (then strip out
   slack-specific code).
2. Rename the package in its `Cargo.toml` to `frankweiler-etl-<name>`,
   lib name `frankweiler_etl_<name>`.
3. Add `etl/providers/<name>` to the workspace `members =` list in
   `frankweiler/backend/Cargo.toml` and to the `crate.from_cargo`
   manifest list in `MODULE.bazel`.
4. Implement `<name>-download` (Extract) as a standalone bin and
   `<name>::translate` as a library function called from sync. The
   translate side must emit `*.grid_rows.json` sidecars matching
   [`Sidecar`](src/sidecar.rs).
5. Drop sample wire-format data into `providers/<name>/tests/fixtures/`
   and write integration tests next to it.
6. The Load step needs no per-provider changes — `grid-rows-load` will
   pick up the new sidecars on its next run.

## Cargo vs Bazel

  * `cargo test -p frankweiler-etl-<name>` is the inner loop. Each
    provider crate's tests resolve fixtures via
    `CARGO_MANIFEST_DIR.join("tests/fixtures/...")`, so they're
    standalone.
  * `bazelisk test //frankweiler/backend/etl/...` runs both the unit
    tests (`:etl_unittests`, each `:<provider>_unittests`) and the
    fixture-backed integration tests (`:<provider>_playback_roundtrip`,
    `:<provider>_render`, `:<provider>_blob_render`, …). The only
    tests tagged `manual` are the `:<provider>_live` ones, which hit
    the real upstream API and read latchkey credentials from the host.
