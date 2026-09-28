# Testing

Everything in this repo is tested through Bazel. This doc is a map of the
test surface.

## Source of truth: `bazelisk test //...`

"Build green" means `bazelisk test //...` passes — nothing less. It runs the
Rust unit + integration tests, the cross-language goldens, the `//:lint`
gate (ruff / pyright / vue-tsc / prettier, all sandboxed), and the Playwright
e2e suite, the same way CI does. Formatting is checked in every language,
never fixed by the gate: `bazelisk run //:rustfmt -- <file or directory>...`
for Rust (it runs the aspect's rustfmt with the aspect's flags, and works
from a checkout whose path has spaces, which `@rules_rust//:rustfmt` does not),
`uv run ruff format .` for Python, and `pnpm exec prettier --write .` from
`datalib/ui` for TypeScript and Vue — `.prettierrc` sets 100 columns and
`.prettierignore` keeps it to what `//datalib/ui:format_test` checks. Bazel's action cache makes re-runs cheap, so for a
tight inner loop narrow the *bazel* invocation to what you're touching
(e.g. `bazelisk test //datalib/backend/etl/...`). Bazel is the only
supported build/test driver — don't shell out to `cargo` / `pnpm`, which
bypass (and never warm) the cache and can disagree with CI.

The complete local gate is `bazelisk run //:precommit` (the hygiene lint,
`//:lint`, a `build //...` for the fmt and clippy aspects, and every
hermetic test). Never put `--test_tag_filters=-manual,-external` on the
full run: `-external` silently drops `//datalib/ui:e2e_test`. Coverage:
[`/docs/dev/coverage.md`](/docs/dev/coverage.md); why a run was slow:
[`/docs/dev/ci.md`](/docs/dev/ci.md).

## Updating insta goldens (`.update` targets)

`bazel test` runs in a sandbox, so `INSTA_UPDATE=always` would write new
`*.snap`s where you can't review them. Every insta-using `rust_test` has a
sibling `.update` target (via the `insta_update` macro in
[`/tools/insta.bzl`](/tools/insta.bzl)) that you invoke with `bazel run`:

```bash
bazel run //datalib/backend/unified_index:unified_index_tests.update
bazel run //datalib/backend/etl/providers/slack:slack_tests.update
```

The wrapper sets `INSTA_WORKSPACE_ROOT=$BUILD_WORKSPACE_DIRECTORY`, which
only exists under `bazel run` and resolves to the source tree, so new
`.snap` files land where `git status` shows them. Always review the diff
before committing. The same wrapper regenerates a golden that is not an
insta snapshot: a test that writes its file when `INSTA_UPDATE=always` is
set and compares otherwise (`//datalib/backend/datalib_step:ingest_methods.update`
is one). The live tests need `LATCHKEY_CURL` pointed at the router curl,
never the impersonator (`docs/dev/curl_impersonate.md`).

A package's integration tests share one binary (below), so its
snapshots are named for that binary and the whole package has one
`.update`: `slack_tests.update` refreshes the goldens of every module
in `tests/slack_tests/`. Where a package also has a `live` module, its
goldens come from the network, so the two are separate runs:
`<p>_tests.update` carries `test_args = ["--skip", "live::"]` and
`<p>_live.update` carries `test_args = ["live::"]`.

When adding an insta-using test, declare a sibling `.update`:

```python
load("//tools:insta.bzl", "insta_update")

rust_test(
    name = "my_render_test",
    data = [":tng_fixture"],
    env = {"MY_FIXTURE_DIR": "datalib/.../fixtures/my_api"},
    ...
)

insta_update(
    name = "my_render_test.update",
    test = ":my_render_test",
    test_args = ["--ignored"],  # only if the test is #[ignore]'d
    # `data` and `env` on rust_test do NOT propagate through the sibling
    # sh_binary wrapper — mirror every fixture / env-var dep here.
    extra_data = [":tng_fixture"],
    extra_env = {"MY_FIXTURE_DIR": "datalib/.../fixtures/my_api"},
)
```

## A package's integration tests are one binary

`tests/<name>/main.rs` with a `mod` line per file, one `rust_test`
named `<name>` over `tests/<name>/*.rs`. Not one target per file: a
`rust_test` is a whole opt-mode link of everything the crate reaches,
which is most of what a test here costs, and a shared-crate edit pays
it once per target ([`ci.md`](ci.md) § "Reading a run" has the
measurement).

Two things follow from sharing a process:

* **Anything process-global is now shared.** The playback transport is
  chosen by an environment variable each test points at its own fixture
  tree, so the provider binaries set `RUST_TEST_THREADS = "1"` and say
  so in `main.rs`. A tracing subscriber is the same story from the
  other side: four of `datalib/backend/http`'s modules each install the
  process's only one, so each runs as a slice (below).
* **insta names a snapshot after the module path.** The goldens live in
  `tests/<name>/snapshots/` and are called
  `<name>__<module>__<snapshot>.snap`. Keep the target name equal to
  the directory name, because cargo discovers `tests/<dir>/main.rs`
  under that name too and the two must agree on the path.

A test that cannot share — a different `tags` (`no-sandbox`,
`requires-network`), or a process-global it must own — still compiles
into the binary, as a module run by a slice.

### Slices: one binary, a process per module

`tools/test_slice.bzl`. The package's `rust_test` skips the module by
name, and `rust_test_slice` is a test target of its own that runs
exactly that module from the same binary, in its own process, with its
own `tags`, `data` and `env`. One dict in the BUILD file feeds both
halves, so the skip and the slice cannot drift apart:

```starlark
_SLICES = {"server_log_test": "server_log::"}

rust_test(name = "http_tests", args = skip_slices(_SLICES), ...)

rust_test_slice(
    name = "server_log_test",
    filter = _SLICES["server_log_test"],
    test = ":http_tests",
)
```

A slice fails when its filter matches no test, so renaming the module
cannot leave it green and empty. `datalib/backend/http` is the example:
five slices over one binary — four that own the tracing subscriber and
`applet_tests`, which is `no-sandbox` and `requires-network`. Only a test
that needs a different `manual` or `external` tag, or a binary of its
own on purpose, stays a separate `rust_test` (`http`'s `shutdown_test`
runs the real server binary).

### The `live` module

A provider's live test — the one that talks to the real service through
`latchkey` — is a module named `live` in the same binary, not a target
of its own. Its tests are therefore named `live::<fn>`, the `rust_test`
carries `args = ["--skip", "live::"]`, and a sibling `live_run` target
(`tools/live.bzl`) runs exactly what that skips:

```bash
bazel run //datalib/backend/etl/providers/claude:claude_live
```

`bazel run`, because a `bazel test` of the same target would still apply
its `--skip`, and because these tests need the invoking shell's
environment — the host keyring, and `LATCHKEY_CURL` pointed at the
router curl. The gain is that the live code compiles with the rest of
the package's tests instead of in a link of its own, and still cannot
rot.

**Not `#[ignore]`.** That is one flag for the whole binary and
`insta_update`'s `test_args = ["--ignored"]` already spends it on tests
that are ignored for the ordinary reason
(`//datalib/backend/dag:manual_e2e_live_sync_golden`). Two meanings in
one binary would be indistinguishable.

## The Playwright suite runs in two engines

`//datalib/ui:e2e_test`'s projects (see
[`/datalib/ui/playwright.config.ts`](/datalib/ui/playwright.config.ts)):

* **`chromium`** — every spec that leaves the config alone, against
  the one shared fixture root.
* **`chromium-<spec>`** — one project per spec that rewrites its
  config (`CONFIG_MUTATING` in `tests/e2e/config-mutating.ts`: the
  `data-sources-*` specs and a few more), each against a backend and
  root of its own, so no two specs ever share a queue.
* **`webkit`** — the grid-bearing specs only, listed by `testMatch`.
* **`warmup`** — qmd's cold model load, paid once before `chromium`
  and `webkit` start.

The `webkit` project exists because the Tauri desktop app renders in a
**WKWebView**, not Chromium, and the two engines disagree about layout in a
way that has shipped twice. WebKit resolves a child's percentage `height`
against the parent's *specified* height, so `height: 100%` under a
flex-sized parent that declares no height of its own computes to `auto` and
a grid root collapses — to 2px of border in the sources card's case
(`ui/src/cards/SourcesCard.ce.vue`). Chromium resolves against the flexed
height and looks perfect.

**What this means for how you assert.** Every row and header stays in the
DOM through that collapse, so row locators match, `toHaveCount`
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
the shape). And do not drive a native HTML5 drag with `page.mouse` in a
spec the `webkit` project runs: Playwright's WebKit on macOS turns it into
a native drag session that it sometimes loses under load — the page gets
`dragstart` and one `dragenter`, then nothing, not even `dragend` on the
release — while CI's Linux WebKit drives it fine, so the failure is
mac-only. Dispatch the drag events by hand around a real press and
release; [`run-log.spec.ts`](/datalib/ui/tests/e2e/run-log.spec.ts)
shows the shape.

Browser binaries are **not** Bazel inputs — chromium and webkit both come
from the host's Playwright cache (`~/Library/Caches/ms-playwright` on a
mac) via `env_inherit = HOME`, and `run_e2e.sh` runs `playwright install`
for both first so a cold cache self-heals. That network reach is what the target's
`requires-network` tag is for. Making the browsers real Bazel inputs is a
separate project.

### Running one spec, and running it several times

`bazelisk run //datalib/ui:e2e -- <playwright args>` runs the suite
from the source tree with every backend the config spawns; `--project
<name>` and `--grep <pattern>` narrow it. To hunt a flake, repeat it:

```bash
bazelisk run //datalib/ui:e2e -- --project chromium-data-sources-sync --repeat-each 3 --workers 1
```

**`--repeat-each` needs `--workers 1`.** Without it Playwright spreads
the copies across workers, and a config-mutating spec then runs beside
a copy of itself on the same backend and root: the copies rewrite each
other's config and start each other's syncs, and the failures read as
the spec's own, not as a collision.

### It is a CI merge gate

`.github/workflows/test.yml` runs a bare `bazel test ... //...`, so this
suite gates merges like everything else. Two things make it work in the
CI image:

* **The qmd GGUFs are bazel inputs.** A build action
  (`//third-party/qmd_models`) fetches them into the remote cache, and
  they reach the materializer and the fixture's index genrule as
  runfiles; the image has no `/root/.cache/qmd/models`. The e2e suite
  runs on the runner itself, so its runfiles *are* downloaded before it
  starts — which is why it uses `materialize_tng_root_embed_only`: the
  embedding model is all it loads, and the other two are 1.8 GB. The
  suite sets `DATALIB_QMD_MODELS_NO_FETCH` so the applet reports them
  absent instead of fetching them into the fixture root.
* **`--test_env=PLAYWRIGHT_BROWSERS_PATH=/root/.cache/ms-playwright`.**
  GitHub forces `HOME=/github/home` for container steps, while the image
  bakes its browsers under `/root`; without the flag `run_e2e.sh`'s
  `playwright install` re-downloads ~400 MB of chromium + webkit every
  run.

The suite is `no-sandbox` + `requires-network` + `cpu:4` and takes ~4
minutes, so unlike the rest of a warm `main` run it is real work on the
critical path rather than a cache replay. It catches what a build
cannot: in #252 a Vite 8 `outDir` change let the `dist` action succeed
with an empty output, and this suite was what said so (60 tests
reporting "UI bundle not embedded in this binary").

### It needs a `long` timeout, and that is not slack

The target sets `timeout = "long"` (900s). Bazel's default for a test
with no `size` or `timeout` is `medium` — **300s** — and this suite does
not fit in that: the suite runs across two engines behind nine backend
processes, plus a qmd cold model load of 1.2-1.5 min (qmd 2.8.3).
Measured wall clock is ~70s warm and 200-270s on a loaded machine,
so the default budget made `bazelisk test //...` flaky in a way that
pointed at nothing. Bazel enforces the ceiling but does not wait for it,
so the larger budget costs nothing.

### Writing a spec that does not flake

The page under test keeps changing on its own. The Manage grid is drawn
again after every remount, whenever the loop's record moves, and on a
clock, and a real `datalib-dag` runs the syncs. A month of CI
(September 2026) had about a hundred flaky e2e failures, and nearly all
of them were one of two mistakes: the test looked at the page between
two drawings, or it expected to catch a state the backend was free to
leave. The rules below are how to avoid both.

**Assert with a retrying expectation.** `await expect(locator).toHaveText(…)`,
`toBeVisible()` and `toHaveAttribute()` keep retrying until they match or
time out, and so does `expect.poll(fn)`. `expect(await locator.textContent())`
reads the page once. Keep the one-shot form for values that cannot move
under you: an API response, a number the test computed, or a reading a
helper has already waited for.

**Read a Manage row through the row helpers.** `statusOf`, `stampOf`,
`lastSuccessOf` and `readRow` in
[`grid-helpers.ts`](/datalib/ui/tests/e2e/grid-helpers.ts) wait for the
row to be drawn, then return what that one drawing showed. A `null` from
`stampOf` therefore means the step never ran, and never that the row was
not drawn yet; a reader that returned `null` for both would let a test
that expects `null` pass without looking at anything. Inside a
poll that has to keep going through a redraw, use `sampleRow`, which
returns `null` for "not drawn". A step under a closed group is never
drawn, so call `expandGroup` first; `readRow` says so when it gives up.
The rule for any new helper: **a reader must not return the same value
for "absent" and "not there yet".**

**Take related values from one drawing.** Two separate reads can
straddle a redraw. For example, the status from before a sync can sit
beside the stamp that sync just wrote, which reads as the sync having
stopped. `sampleRow` reads every cell in one pass. Likewise, once a poll
has matched, use the value the poll saw rather than reading again:
`readRow` and `settleRow` both do this.

**Hold a state before asserting it.** `Running`, `Queued` and a toast are
states the page passes through, and a fast step on a fast runner can
finish between two samples. CI failed about two dozen times on "expected
Running, got Succeeded". Either hold the step in that state until the
test lets go — the taped sources replay recorded HTTP, and `hold()` /
`release()` in the `data-sources-*` specs park every replayed request
behind a file — or assert the end state instead. A banner is a passing
state too: "Queued a sync for …" comes down when its request closes, and
a sync nothing holds (the local PDF folder) can close before the check
for the banner first looks. When a banner only stands for something
else, wait for that thing instead: `start()` in
`data-sources-control.spec.ts` waits for the `POST /api/requests`
response rather than the banner.

**Hold what the page is still loading when the next step can lose to
it.** A page keeps working after it first paints: the search grid loads
further pages of rows and scrolls to keep its top row in place when one
lands. On a fast runner that is over before the test's next step; on a
loaded one it lands in the middle of it. A menu that closed on every
grid scroll then closed before the test could click in it.
`page.route` can hold the request until the step is set up and release
it after, which turns "usually first" into "always this order" and is
how `grid-search-bar.spec.ts` checks the menu survives the page.

**Drive the app, not a gesture, when the gesture is not what is under
test.** A hand-driven header drag failed 23 times in one day; the
run-log spec groups through the panel's API instead. When
a click is the subject, retry the click and its visible effect as a pair
(`pickRowMenu`, `run-log.spec.ts`'s `rightClick`), because the row the
click landed on can be replaced by a redraw.

**Compare like with like.** The run log's line counter counts the lines
*loaded*, not the lines in the log: after a load it holds the newest
page plus whatever has arrived since. Two counts taken either side of a
reload therefore compared how long each had been open, and failed as
538 < 569 once the log outgrew a page (#835). Before comparing a reading
from before an action with one from after it, check that both measure
the same thing.

The suite runs with no retries (`playwright.config.ts` sets none), on
purpose: a flake turns the run red and gets fixed rather than hidden. To
hunt one, repeat the spec (§"Running one spec, and running it several
times") or `bazelisk test //datalib/ui:e2e_test --runs_per_test=N
--local_test_jobs=1`. `scripts/flaky_tests.py` lists the targets that
went red and then green on the same commit.

## Watching a sync stream

[`data-sources-streaming.spec.ts`](/datalib/ui/tests/e2e/data-sources-streaming.spec.ts)
is the one spec that watches a sync *while it runs*, and the place to
look when the question is "does streaming actually reach the screen".
Two API-backed sources (`chatgpt`, `claude`) replay playback tapes
behind an after-seal hold: each download runs to its first checkpoint
and parks there until the spec releases it. While they are parked the
spec waits for the Explore grid — opened before the sync and never
touched again — to show rows, and for the Pipeline table to show every
download, the render behind it and the index behind both reading Running
in one reading. Nothing upstream can finish while the hold is in place,
so both are states to wait for rather than frames to catch. A delay on
every request cannot promise that: on a busy runner a download can end
before its first rows reach the grid.

```bash
bazelisk run //datalib/ui:e2e -- --project chromium-data-sources-streaming
```

Three pieces make that possible, and each is small:

* `DATALIB_HTTP_PLAYBACK_DELAY_MS` beside `DATALIB_HTTP_PLAYBACK`
  ([`http.rs`](/datalib/backend/etl/src/http.rs)): a replayed request
  waits that long before it answers. Playback only; a fixture that
  answers instantly hides everything that depends on a download taking
  time. Its sibling `DATALIB_HTTP_PLAYBACK_HOLD` names a file: while it
  exists no replayed request is answered at all, and removing it lets
  the download run on. That is what a spec uses when it has to *act* on
  a download in flight (`data-sources-control.spec.ts` adds and stops
  sources beside one) — a hold is released when the spec is done, where
  a delay is a window that a slow runner can miss.
  `DATALIB_HTTP_PLAYBACK_HOLD_SEALED` is the same, except a request waits
  only once its process has sealed a checkpoint, so a download publishes
  something before it parks; the streaming spec uses it.
* The tapes come from `datalib-step synthesize`, run by `run_e2e.sh` at
  startup over the checked-in `chatgpt_api` / `claude_export` fixtures —
  the same call `tests/fixtures/run_sync_pipeline.py` makes.
* The spec's root is **not** the materialized TNG fixture: those tapes
  hold the same conversations the fixture already indexed under
  `chatgpt-api` / `claude-api`, and entity ids are provider-global, so
  against that root the index sees every one as already indexed and
  skips it. `playwright.config.ts` writes it a bare root instead — the
  index group and the applet, no data.

To watch the same thing by hand, against any root, run the runner
directly with the two variables set and a short cadence in the config:

```bash
bazelisk build //datalib/backend:bin //datalib/backend/datalib_step:datalib_step
step=bazel-bin/datalib/backend/datalib_step/datalib_step
echo '{"fixture_path": "'$PWD'/datalib/backend/etl/providers/chatgpt/tests/fixtures/chatgpt_api"}' > /tmp/synth.json
$step synthesize chatgpt --name chatgpt --params-file /tmp/synth.json --out /tmp/tapes
DATALIB_HTTP_PLAYBACK=/tmp/tapes DATALIB_HTTP_PLAYBACK_DELAY_MS=1500 \
  bazel-bin/datalib/backend/bin/datalib-dag <root>/config.toml
```

with `[checkpoint_cadence] at_most_every_secs = 2` at the top of that
config and a `chatgpt` group whose ingest step's params are
`[steps.params.api]`. The NDJSON on stderr shows the chain: a
`checkpoint` from the ingest, a `step_start` for its render within
milliseconds, and one for `unified_index/grid_index` right after.

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
datalib/backend/dag/manual_e2e_run.sh            # bake: run the pipeline, write new goldens
```

**There is no compare mode, on purpose.** These snapshots are a diff to
read, not a gate to pass. They record what real upstreams looked like at
the last bake, and upstream moves whether or not our code does — so a
comparison run spends three full pipeline passes and real API quota to
report a diff that was always going to be accepted. The test refuses to
start without `INSTA_UPDATE` set to a writing mode, before it fetches
anything, so the waste can't happen by accident.

The snapshots are the eyeball half. The other half does fail the run,
and is why the exit code still means something: every step must succeed,
the `data_root` layout is asserted, and run 3's content-stability check
is a plain `assert!`. Those hold in update mode exactly as they would in
compare mode.

Read the bake with `git diff` in `$DATALIB_MANUAL_E2E_DIR` — it is a git
repo, and the commit message is where the triage goes (deliberate /
accidental / noise, per cluster).

Start with `--config`. It parses the config, builds the graph, and round-trips
every step's params against the provider schemas in seconds, without touching
the network. Every provider's config schema is `deny_unknown_fields`, so a
misplaced knob fails here rather than during the live run.

The test makes three pipeline runs, each asserting something different:

1. **Cold** — snapshots the produced data tree, one `.snap` per file, plus a
   manifest and the layout invariants.
2. **Incremental** — re-runs against the now-populated `data_root` and
   snapshots each source's `sync_runs.summary`, whose `deltas` prove the run
   didn't re-fetch the world. A broken-incrementality regression shows up as
   `deltas.<table>.added` back at first-run scale. Only the API-backed
   providers stamp `sync_runs`; file-backed sources record an explicit
   "no rows" marker, since there is no upstream to be incremental about.
3. **`--reset`, then sync** — empties the store and re-downloads it, then
   asserts the content tables come back byte-identical. This is what catches a
   per-fetch field leaking into a content payload (it belongs in the
   `volatile_payload` sidecar instead).

The bake leaves its data root behind under `$TMPDIR/datalib-e2e-runs/run-<millis>/data`
(the newest three runs are kept; the test prints the path as `[test]
data_root = …`). The config is written inside it, so it is a complete
root the app can serve as it stands:

```bash
bazel-bin/datalib/backend/bin/datalib-http "$TMPDIR/datalib-e2e-runs/run-<millis>/data"
```

Run it from there, not from a copy: `:bin` stages a `git-hash` beside
the binaries, which is what gives the log card's source links their
commit (`logging.md` § "Every line has an author").

The three runs' NDJSON event streams sit beside it in `run-<millis>/`.
Free-text search works there: the golden config keyword-indexes every
source and embeds two small ones (whatsapp, google_calendar_window), so
semantic search reaches those two alone. The qmd index is not
snapshotted.

The normalization machinery — the volatile keys the snapshots redact, each
commented with why — is in the test's source.

