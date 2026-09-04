# Testing

Everything in this repo is tested through Bazel. This doc is a map of the
test surface.

## Source of truth: `bazelisk test //...`

"Build green" means `bazelisk test //...` passes — nothing less. It runs the
Rust unit + integration tests, the cross-language goldens, the `//:lint`
gate (ruff / pyright / vue-tsc, all sandboxed), and the Playwright e2e suite,
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
bazel run //datalib/backend/unified_index:fixture_db_snapshot_test.update
bazel run //datalib/backend/etl/providers/slack:slack_translate.update
```

These land the new snapshots in the source tree where `git status` shows them.
Always review the diff before committing. See [`/AGENTS.md`](/AGENTS.md)
§ "Updating insta snapshots" for the full pattern, including how to declare a
`.update` for a new test.

## The Playwright suite runs in two engines

`//datalib/ui:e2e_test` has two projects (see
[`/datalib/ui/playwright.config.ts`](/datalib/ui/playwright.config.ts)):

* **`chromium`** — every spec.
* **`webkit`** — the grid-bearing specs only, listed by `testMatch`.

The second one exists because the Tauri desktop app renders in a
**WKWebView**, not Chromium, and the two engines disagree about layout in a
way that has shipped twice. WebKit resolves a child's percentage `height`
against the parent's *specified* height, so `height: 100%` under a
flex-sized parent that declares no height of its own computes to `auto` and
an AG Grid root collapses — to 2px of border in the
[`Manager2View`](/datalib/ui/src/views/Manager2View.vue) case. Chromium
resolves against the flexed height and looks perfect.

**What this means for how you assert.** Every row and header stays in the
DOM through that collapse, so `.ag-row` locators match, `toHaveCount`
passes, and the user sees nothing. A test only catches it if it measures
geometry — `expectGridPainted` in
[`/datalib/ui/tests/e2e/grid-helpers.ts`](/datalib/ui/tests/e2e/grid-helpers.ts)
is that assertion (bounding-box height > 100px). Reach for it whenever a
spec's real subject is "this is on screen".

Adding a spec that renders a grid? Add its filename to the `webkit`
project's `testMatch`, or it runs in Chromium only. Note that WebKit is
also stricter about `loading="lazy"` iframes (it will not load one far
below the fold — scroll it into view first;
[`yolink-plots.spec.ts`](/datalib/ui/tests/e2e/yolink-plots.spec.ts) shows
the shape).

Browser binaries are **not** Bazel inputs — chromium and webkit both come
from the host's `~/Library/Caches/ms-playwright` via `env_inherit = HOME`,
and `run_e2e.sh` runs `playwright install chromium webkit` first so a cold
cache self-heals. That network reach is what the target's
`requires-network` tag is for. Making the browsers real Bazel inputs is a
separate project.

### It IS a CI merge gate — and what that cost

`.github/workflows/test.yml` runs a bare `bazel test ... //...`, so this
suite gates merges like everything else. It spent a long time excluded
behind a FIXME, and the story of why is worth keeping, because the note
went stale in the direction that bites: it said the last thing missing
was a published image carrying `rsync` and both browsers, and **that had
been true since `v0.30.1`** (WebKit landed five days after `v0.29.0`,
and `v0.30.0`'s release run failed, so `v0.30.1` is the first published
image carrying it). Anyone acting on it would have dropped the
exclusion and gotten a red gate, because the actual blockers were two
things the note never mentioned.

* **The qmd GGUFs are not in the image.** The published devcontainer is
  built on the `-slim` prod image (`QMD_PREFETCH_MODELS=false`), which
  creates `/root/.cache/qmd/models` empty, and
  `materialize_tng_root.sh` used to require that directory to hold them
  — `exit 3` if not, deliberately, so a multi-GB HuggingFace download
  could not masquerade as a hang. CI filled it with a `qmd pull` behind
  an `actions/cache`. Both halves are gone now: the three GGUFs are
  pinned in `MODULE.bazel` as `@qmd_model_*` and reach the materializer
  (and the fixture's index genrule) as bazel inputs, and `.bazelrc`'s
  `buildbuddy` config fetches them through BuildBuddy's remote
  downloader rather than from HuggingFace.
* **`HOME=/github/home`.** GitHub forces that for container steps, while
  the image bakes its caches under `/root`, so every lookup landed in an
  empty directory. One `--test_env` flag still redirects the lookup that
  matters: `PLAYWRIGHT_BROWSERS_PATH=/root/.cache/ms-playwright`
  (without it, `run_e2e.sh`'s `playwright install` re-downloads ~400 MB
  of chromium + webkit every run instead of using the baked cache). The
  other one, `CLAUDE_MIRROR_HOST_HOME=/root`, went away with the model
  cache — nothing reads that variable any more.

The cost is honest and worth naming: the suite is `no-sandbox` +
`requires-network` and takes ~4 minutes, so unlike the rest of a warm
`main` run it is real work on the critical path rather than a cache
replay.

It buys back more than it costs. CI had never run this suite, which is
easy to miss precisely because a local `bazelisk test //...` does — so
for its whole life the only thing standing between a UI regression and
`main` was whoever remembered to run it. [#252](https://github.com/imbue-ai/datalib/pull/252)
is the worked example: AG Grid 36 restructured the row DOM and 39 tests
across 18 spec files failed *while the grid rendered perfectly*, and a
Vite 8 `outDir` change let the `dist` action succeed with an empty
declared output, which 60 e2e tests reported as "UI bundle not embedded
in this binary". CI was green through both.

### It needs a `long` timeout, and that is not slack

The target sets `timeout = "long"` (900s). Bazel's default for a test
with no `size` or `timeout` is `medium` — **300s** — and this suite does
not fit in that: 66 tests across two engines behind nine backend
processes, plus a qmd cold model load that grew to 1.2-1.5 min in qmd
2.8.3. Measured wall clock is ~70s warm and 200-270s on a loaded machine,
so the default budget made `bazelisk test //...` flaky in a way that
pointed at nothing. Bazel enforces the ceiling but does not wait for it,
so the larger budget costs nothing.

## Bazel-fetched test data (`lightroom`)

`//datalib/backend/etl/providers/lightroom:real_catalogs` ingests four real
Lightroom catalogs and asserts the incremental diffs between them. The
catalogs are **fetched, not vendored**: `http_file` entries in
`MODULE.bazel`, pinned by upstream commit sha *and* sha256, sourced from
[`thadd3us/lightroom_db_diff`](https://github.com/thadd3us/lightroom_db_diff).
~7 MB that would otherwise sit in this repo's history forever.

This is the pattern to copy when a test needs real binary input that is
too big to check in: Bazel's repository cache makes the download one-time
per machine per pin, so it stays an ordinary `bazelisk test //...` target
rather than a manual script. Tag it `requires-network` — once fetched the
test is hermetic, but a cold cache has to reach the network, and the tag
is what makes that honest.

Regenerate the checksums after a re-pin with:

```bash
curl -sL <url> | shasum -a 256
```

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
