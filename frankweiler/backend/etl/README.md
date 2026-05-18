# frankweiler-etl

Rust ETL pipeline that mirrors per-service data into the canonical
`grid_rows` table in Dolt. Each stage is a separate binary so they can
run independently, be scheduled separately, and be monitored
uniformly.

```
┌────────────┐    ┌──────────────┐    ┌─────────────────┐
│  Extract   │───▶│   Translate  │───▶│      Load       │
│ raw_api/   │    │ rendered_md/ │    │  grid_rows in   │
│ (per prov) │    │ + sidecars   │    │     Dolt        │
└────────────┘    └──────────────┘    └─────────────────┘
```

## Stages

| Stage     | Binary                      | Inputs               | Outputs                                    |
|-----------|-----------------------------|----------------------|--------------------------------------------|
| Extract   | `<provider>-download`       | upstream API         | `<out>/raw_api/<method>/events.jsonl`      |
| Translate | `<provider>-translate`      | `<out>/raw_api/`     | `<out>/rendered_md/<provider>/...`         |
| Load      | `grid-rows-load` (generic)  | `<out>/rendered_md/` | rows in Dolt + `documents_loaded` bookkeeping |

Per-provider code lives under [`src/providers/<name>/`](src/providers/);
each provider has an `extract/` and a `translate/` submodule. The Load
step is provider-agnostic and lives at [`src/load.rs`](src/load.rs).

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
    `documents_loaded`. Re-running with no upstream changes is a no-op:
    every sidecar is short-circuited before the DELETE/INSERT.

Bump [`RENDER_VERSION`](src/providers/slack/translate/render.rs) (or
the per-provider equivalent) to force a rebake even when payloads are
unchanged.

## Observability

All three binaries flatten [`obs::ObsArgs`](src/obs.rs) into their
clap parser, so every stage takes the same flags:

  * On a TTY, `tracing-indicatif` renders progress bars.
  * Otherwise, NDJSON events go to stderr.
  * `--otlp-endpoint http://host:4317` exports spans + events via
    OTLP, so a single Tempo/Jaeger collector can ingest every stage.

Each stage emits a `*_start`, `*_complete`, and per-document progress
events with a stable prefix (`slack_download_*`, `slack_translate_*`,
`grid_rows_load_*`). The `*Summary` structs are `Serialize`, so a web
UI can consume the final stats line without knowing which provider
produced it.

## Adding a new provider

1. Create `src/providers/<name>/{extract,translate}/`.
2. Add a `<name>-download` bin in `src/bin/` and `[[bin]]` /
   `rust_binary` entries in `Cargo.toml` + `BUILD.bazel`.
3. Add a `<name>-translate` bin that emits sidecars matching
   [`Sidecar`](src/sidecar.rs).
4. The Load step needs no per-provider changes — `grid-rows-load` will
   pick up the new sidecars on its next run.

The Slack provider is the worked example; copy it as a template.

## Cargo vs Bazel

  * `cargo test -p frankweiler-etl` is the inner loop. It picks up the
    repo-root `tests/fixtures/slack_api` fixtures via
    `CARGO_MANIFEST_DIR.ancestors()`.
  * `bazelisk test //frankweiler/backend/etl/...` runs the unit tests
    (`:etl_unittests`). The fixture-backed integration tests are tagged
    `manual` because the fixture tree isn't in the bazel sandbox runfiles.
