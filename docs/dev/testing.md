# Testing

Everything in this repo is tested through Bazel. This doc is a map of the
test surface.

## Source of truth: `bazelisk test //...`

"Build green" means `bazelisk test //...` passes — nothing less. It runs the
Rust unit + integration tests, the cross-language goldens, `//:precommit_test`
(cargo fmt / clippy / ruff / pyright / vue-tsc), and the Playwright e2e suite,
the same way CI does. Bazel's action cache makes re-runs cheap, so for a
tight inner loop narrow the *bazel* invocation to what you're touching
(e.g. `bazelisk test //frankweiler/backend/etl/...`). Bazel is the only
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
bazel run //frankweiler/backend/core:fixture_db_snapshot_test.update
bazel run //frankweiler/backend/etl/providers/slack:slack_translate.update
```

These land the new snapshots in the source tree where `git status` shows them.
Always review the diff before committing. See [`/AGENTS.md`](/AGENTS.md)
§ "Updating insta snapshots" for the full pattern, including how to declare a
`.update` for a new test.

## Manual e2e live-sync golden — retired

The `//frankweiler/backend/sync:manual_e2e_live_sync_golden` test was
retired together with the `frankweiler-sync` binary when the pipeline moved
to the DAG runner (`datalib-dag` — see
[`/docs/dev/pipeline_dag_architecture.md`](/docs/dev/pipeline_dag_architecture.md)).
Its config, file-based source data, and golden snapshots still live in the
private `data_liberation_manual_e2e_test_data` directory (outside this repo
— it holds slightly sensitive personal data), but nothing in-tree currently
runs against it.

### Caveat: old copies are still in this repo's history

This data used to live in-repo (`configs/thad_tiny.yaml` +
`frankweiler/backend/sync/tests/snapshots/`). It was moved out and deleted from
the working tree, but it's still recoverable from past commits. Before this
repo is ever made public, expunge those paths from history with `git
filter-repo` — see the note at the top of [`/TODO.md`](/TODO.md).
