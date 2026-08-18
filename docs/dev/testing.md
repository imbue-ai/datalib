# Testing

Everything in this repo is tested through Bazel. This doc is a map of the
test surface.

## Source of truth: `bazelisk test //...`

"Build green" means `bazelisk test //...` passes — nothing less. It runs the
Rust unit + integration tests, the cross-language goldens, `//:precommit_test`
(cargo fmt / clippy / ruff / pyright / vue-tsc), and the Playwright e2e suite,
the same way CI does. Bazel's action cache makes re-runs cheap, so for a
tight inner loop narrow the *bazel* invocation to what you're touching
(e.g. `bazelisk test //datalib/backend/etl/...`). Bazel is the only
supported build/test driver — don't shell out to `cargo` / `pnpm`, which
bypass (and never warm) the cache and can disagree with CI.

See [`/AGENTS.md`](/AGENTS.md) § "Running tests" for the details (don't filter
on `-manual,-external` — it silently drops fmt/UI checks) and
[`/docs/dev/coverage.md`](/docs/dev/coverage.md) for coverage.

## Updating insta goldens (`.update` targets)

`bazel test` runs in a sandbox, so `INSTA_UPDATE=always` would write new
`*.snap`s where you can't review them. Every insta-using `rust_test` has a
sibling `.update` target (via the `insta_update` macro in
[`/tools/insta.bzl`](/tools/insta.bzl)) that you invoke with `bazel run`:

```bash
bazel run //datalib/backend/core:fixture_db_snapshot_test.update
bazel run //datalib/backend/etl/providers/slack:slack_translate.update
```

These land the new snapshots in the source tree where `git status` shows them.
Always review the diff before committing. See [`/AGENTS.md`](/AGENTS.md)
§ "Updating insta snapshots" for the full pattern, including how to declare a
`.update` for a new test.

## Manual e2e live-sync golden

`//datalib/backend/dag:manual_e2e_live_sync_golden` runs the whole pipeline
against **real** provider APIs and snapshots what it produces. It is the only
test that catches render-side drift against real payloads — upstream shape
changes, schema-projection bugs, timestamp fabrication, attachment-handling
gaps — with a human-reviewable diff.

Manual and host-bound: it needs latchkey credentials for Thad's accounts, so
only that host can run it. Never runs on CI (`manual` + `external` tags).

Its config, file-based source data, and golden snapshots live in the private
`data_liberation_manual_e2e_test_data` directory, outside this repo — it holds
slightly sensitive personal data. Point `DATALIB_MANUAL_E2E_DIR` at it (the
runner defaults to `~/data_liberation_manual_e2e_test_data`).

```bash
datalib/backend/dag/manual_e2e_run.sh --config   # validate config only: offline, no creds
datalib/backend/dag/manual_e2e_run.sh            # run + diff against goldens
datalib/backend/dag/manual_e2e_run.sh --update   # accept new goldens
```

Start with `--config`. It parses the config, builds the graph, and round-trips
every step's params against the provider schemas in seconds, without touching
the network. It is not a complete guard, though: render params are
`deny_unknown_fields` and so are most download configs, but `email`, `fsindex`,
`linkedin`, and `sms_backup_restore` are permissive, so a misplaced knob on
those parses clean and only fails during the live run.

The test makes three pipeline runs, each asserting something different:

1. **Cold** — snapshots the produced data tree, one `.snap` per file, plus a
   manifest and the layout invariants.
2. **Incremental** — re-runs against the now-populated `data_root` and
   snapshots each source's `sync_runs.summary`, whose `deltas` prove the run
   didn't re-fetch the world. A broken-incrementality regression shows up as
   `deltas.<table>.added` back at first-run scale. Only the API-backed
   providers stamp `sync_runs`; file-backed sources record an explicit
   "no rows" marker, since there is no upstream to be incremental about.
3. **`--reset-and-redownload`** — wipes and re-downloads everything, then
   asserts the content tables come back byte-identical. This is what catches a
   per-fetch field leaking into a content payload (it belongs in the
   `volatile_payload` sidecar instead).

This test was ported from the pre-DAG `frankweiler/backend/sync` crate, which
was deleted in e905d252. The normalization machinery — roughly fifty volatile
keys, each commented with why it's redacted — carried over verbatim, because it
operates on the produced data tree and the DAG migration didn't change that
layout. See the module header of the test for what genuinely had to change.

### Note: the old in-repo copies have been purged from history

This data used to live in-repo (`configs/thad_tiny.yaml` +
`datalib/backend/sync/tests/snapshots/`). It left the working tree in
26412853 and was later expunged from git history with `git filter-repo` —
no reachable commit on `main` or `origin/main` contains either path. What
remains before the repo is made public is server-side: GitHub still holds
the pre-rewrite blobs as unreachable objects, and any collaborator who
never re-cloned still has them locally. See the note at the top of
[`/TODO.md`](/TODO.md).
