# Testing

Everything in this repo is tested through Bazel. This doc is a map of the
test surface and the one piece of setup that isn't self-contained: the
**manual e2e live-sync golden**, whose data lives outside the repo.

## Source of truth: `bazelisk test //...`

"Build green" means `bazelisk test //...` passes — nothing less. It runs the
Rust unit + integration tests, the cross-language goldens, `//:precommit_test`
(cargo fmt / clippy / ruff / pyright / vue-tsc), and the Playwright e2e suite,
the same way CI does. Bazel's action cache makes re-runs cheap. Use
`cargo test` / `pnpm test` only for tight inner-loop iteration, then confirm
with the full line.

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

## Manual e2e live-sync golden

[`//frankweiler/backend/sync:manual_e2e_live_sync_golden`](/frankweiler/backend/sync/tests/manual_e2e_live_sync_golden.rs)
runs the **full sync pipeline against the real world** — every source, hitting
live provider APIs through host-side latchkey credentials and reading the
file-based exports (LinkedIn, Google Takeout, SMS Backup & Restore, CardDAV
contacts) — then snapshots the produced `raw/` + `rendered_md/` trees and the
`sync_summary` JSON. It's the only test that catches render-side drift against
real payloads. Tagged `manual` + `external` + `no-sandbox`, so it is **not**
part of `bazelisk test //...`; you run it explicitly. See
[`/docs/dev/data_architecture_ingestion_practices.md`](/docs/dev/data_architecture_ingestion_practices.md)
for what it snapshots and why.

### The test data lives OUTSIDE this repo

The config, the file-based source data, and the golden snapshots are **not** in
this repo — they hold slightly sensitive personal data (contacts, LinkedIn,
SMS, Takeout) that must not be shared when the repo is open-sourced. They live
in a private, separately-versioned directory pointed to by the
`FRANKWEILER_MANUAL_E2E_DIR` env var (Thad's is `~/data_liberation_manual_e2e_test_data`):

```
$FRANKWEILER_MANUAL_E2E_DIR/
  config.yaml     # the sync config; file sources point at sources/
  sources/        # LinkedIn / Takeout / SMS / contacts export data
  snapshots/      # the golden .snap tree the test checks against
  run.sh          # sets the env var + runs the test / .update
```

The test resolves both its config (`$FRANKWEILER_MANUAL_E2E_DIR/config.yaml`)
and its snapshot dir (`$FRANKWEILER_MANUAL_E2E_DIR/snapshots`) from that one
env var. (You can override the config alone with `FRANKWEILER_TEST_CONFIG`.)

### Running it

The easiest path is the `run.sh` in that directory — it exports
`FRANKWEILER_MANUAL_E2E_DIR` (to its own location) and invokes the right Bazel
target:

```bash
~/data_liberation_manual_e2e_test_data/run.sh            # run + diff against snapshots/
~/data_liberation_manual_e2e_test_data/run.sh --update   # accept new output into snapshots/
```

- **Check** (`run.sh`) runs `bazel test` with `--test_arg=--ignored` (the test
  is `#[ignore]` in cargo) and forwards the env var; a mismatch leaves
  `.snap.new` files in `snapshots/` and fails.
- **Update** (`run.sh --update`) runs the `.update` target with
  `INSTA_UPDATE=always`; insta writes the refreshed `.snap`s straight into
  `$FRANKWEILER_MANUAL_E2E_DIR/snapshots`. Review and commit them **in that
  private repo**, not this one.

Prereqs: latchkey creds configured for the API-backed sources
(`latchkey auth set …`). The Cloudflare-impersonating curl shim is
auto-resolved from the sync binary's Bazel runfiles; export `LATCHKEY_CURL`
only to override it. The API sources can flake (rate limits, an inaccessible
conversation); re-run if a pass fails on a transient upstream error.

If you don't have the data directory, you can't run this test — that's by
design. Set up your own `FRANKWEILER_MANUAL_E2E_DIR` with a `config.yaml`,
`sources/`, and an empty `snapshots/`, then `run.sh --update` to seed the
goldens.

### Caveat: old copies are still in this repo's history

This data used to live in-repo (`configs/thad_tiny.yaml` +
`frankweiler/backend/sync/tests/snapshots/`). It was moved out and deleted from
the working tree, but it's still recoverable from past commits. Before this
repo is ever made public, expunge those paths from history with `git
filter-repo` — see the note at the top of [`/TODO.md`](/TODO.md).
