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

Nothing extra is needed, but nothing arrives yet either: `bazel test
//...` still excludes `//datalib/ui:e2e_test` (see the FIXME in
`.github/workflows/test.yml`), so no CI job runs this suite today.

When that exclusion comes off, the report rides along for free — the
gate job builds and runs `e2e_test` like any other target, and
`--zip_undeclared_test_outputs` puts `outputs.zip` on the BuildBuddy
invocation page under Artifacts. If a GitHub-Actions artifact is wanted
on top of that, it is an `actions/upload-artifact` step on the existing
job pointing at
`$(bazel info bazel-testlogs)/datalib/ui/e2e_test/test.outputs/outputs.zip`.

A dedicated job to run the suite early was tried and removed. It is not
worth its own bazel invocation: a second invocation only shares the
remote cache if its configuration matches the gate's exactly (`-c opt
--config=release --config=ci`, `--action_env=LIBCLANG_PATH=…`, the qmd
mount pair), and one that does match is redundant with the gate the
moment e2e rejoins it. The version that did not match rebuilt 2664
actions with zero cache hits, took 999s against the gate's 143s, and
then failed building `boring-sys2` for want of `LIBCLANG_PATH`.

## Cost

About 28 MB per run: ~13 MB of trace per recorded test and ~0.4 MB of
video. The report embeds a copy of everything it references, so it is
the *only* thing published — `outputDir` is pointed at the test's
scratch directory, because shipping both put the same trace in the zip
twice.
