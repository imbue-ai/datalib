# Watching an e2e run

`//datalib/ui:e2e_test` records itself. Every run writes a **Playwright
HTML report** — the modern form of a "screenshot movie": a per-test
video, plus a *trace*, which is a scrubbable timeline carrying a full
DOM snapshot before and after every action, along with the network log,
the console, and the source line each action came from.

`onboarding-pdf.spec.ts` is recorded **always**, passing or failing
(`test.use({ video: "on", trace: "on" })` at the top of that file). It
is the widest UI path the suite has — first run, the wizard twice, the
Pipeline table, real syncs, the Explore grid — so it doubles as a way to
*see* what onboarding looks like without building the app. Every other
spec keeps `retain-on-failure`, so its artifacts appear only when it
fails.

## Where it lands

Under `bazel test`, the report goes to `TEST_UNDECLARED_OUTPUTS_DIR`,
which bazel zips:

```
bazel-testlogs/datalib/ui/e2e_test/test.outputs/outputs.zip
```

Outside bazel (`pnpm exec playwright test`) it is `datalib/ui/playwright-report/`.

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

## From CI

The `e2e onboarding recording (non-gating)` job on every PR attaches the
same zip as a run artifact named **`e2e-onboarding-recording`** —
download it from the run's summary page and use the commands above.

That job is not a merge gate: `bazel test //...` still excludes
`//datalib/ui:e2e_test` (see the FIXME in `.github/workflows/test.yml`),
and this job exists to publish the recording, not to re-enable that gate
by the back door. It runs the onboarding specs only, with
`E2E_BROWSERS=chromium`, since those specs start no other engine.

When the invocation went to BuildBuddy, the same `outputs.zip` is on the
invocation page under Artifacts.

## Cost

About 28 MB per run: ~13 MB of trace per recorded test and ~0.4 MB of
video. The report embeds a copy of everything it references, so it is
the *only* thing published — `outputDir` is pointed at the test's
scratch directory, because shipping both put the same trace in the zip
twice.
