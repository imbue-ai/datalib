# Watching an e2e run

`//datalib/ui:e2e_test` records itself. Every run writes a **Playwright
HTML report** (`reporter` in `datalib/ui/playwright.config.ts`). For a
recorded test it holds a *trace* — a scrubbable timeline carrying a full
DOM snapshot before and after every action, along with the network log,
the console, and the source line each action came from — and, where
the spec asks for one, a video.

`onboarding-pdf.spec.ts` is recorded **always**, video and trace,
passing or failing (`test.use({ video: "on", trace: "on" })`). It is the
widest UI path the suite has — first run, the wizard twice, the sources
table, real syncs, the grid, search — so it doubles as a way to *see*
what onboarding looks like without building the app. Every other spec
takes the config's `trace: "retain-on-failure"` and no video, so its
trace appears only when it fails.

## Where it lands

Under `bazel test`, the report goes to `TEST_UNDECLARED_OUTPUTS_DIR`,
which bazel zips:

```
bazel-testlogs/datalib/ui/e2e_test/test.outputs/outputs.zip
```

Outside bazel (`pnpm exec playwright test`) it is `datalib/ui/playwright-report/`.

On CI the suite runs in the `bazel test //...` gate
(`.github/workflows/test.yml`). A red run uploads that zip as the
`e2e-playwright-report` artifact (kept a week); a green one uploads no
report. The zip also reaches the BuildBuddy invocation page under
Artifacts (`--zip_undeclared_test_outputs` in `.bazelrc`). Green or red, the job uploads the suite's console log as
the `e2e-test-log` artifact (two weeks): Playwright's list of every
test with its duration, and the specs' `[e2e]` lines. That is where to
look when the question is what the suite spends its minutes on, not
what broke — bazel prints one number for the whole target. If the job's
test step says `(cached) PASSED` for `//datalib/ui:e2e_test`, the suite
did not run in that job and the log is from the run that did.

```bash
gh run download <run-id> -n e2e-test-log -D /tmp/e2e
grep -E '^\s+(✓|✘)' /tmp/e2e/test.log | grep -oE '\[[a-z0-9-]+\] › [^›]+ › .*\([0-9.]+m?s\)$'
```

## Opening it

The report is self-contained — the trace viewer is bundled, so nothing
is fetched from the network. But **it has to be served over http**: the
viewer runs in a service worker, which browsers refuse to register on
`file://`. Opening `index.html` directly shows the run and plays the
videos, and then fails on "View Trace".

So:

```bash
unzip -d /tmp/e2e bazel-testlogs/datalib/ui/e2e_test/test.outputs/outputs.zip
npx playwright show-report /tmp/e2e/playwright-report
```

A single trace, without the report around it:

```bash
npx playwright show-trace /tmp/e2e/playwright-report/data/<hash>.zip
```

## Why the suite has no CI job of its own

Don't give it one. A second bazel invocation shares the gate's remote
cache only if its configuration matches the gate's exactly (`-c opt --config=release --config=ci`, the qmd mount
pair), and one that matches is redundant with the gate. The attempt
that did not match rebuilt 2664 actions with no cache hits and took
999s against the gate's 143s.

## Cost

About 28 MB per run: ~13 MB of trace per recorded test and ~0.4 MB of
video. The report embeds a copy of everything it references, so it is
the *only* thing published — `outputDir` points at the test's scratch
directory (`TEST_TMPDIR`), so the zip does not carry each trace twice.
