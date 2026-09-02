// Pay the qmd cold start once, in the open, before any spec runs.
//
// The applet answers free-text queries through a long-lived `qmd mcp`
// child (backend/unified_index/src/qmd/daemon.rs). The first such query
// after the applet starts loads the embedding model; every query after
// that is sub-second. The model file itself is not downloaded here —
// tests/fixtures/materialize_tng_root.sh symlinks a shared
// ~/.cache/qmd/models into each data root and refuses to run if that
// cache is empty, precisely so a silent multi-minute download can't
// masquerade as a hang. What is paid here is the npx resolve plus the
// load.
//
// Nothing used to own that cost. It landed on whichever spec issued the
// first free-text query, which is decided by *alphabetical filename
// order*: `grid-source-name.spec.ts` sorts before `score-sort-order`
// and `search-qmd-routing`, so a stray unquoted word in the former
// (`source_name:Work Slack` parses the trailing `Slack` as free text)
// was silently paying the warm-up on their behalf. Quoting that value
// moved the cost onto them and made them fail — a spec getting slower
// because an unrelated spec's typo was fixed. This file exists so that
// can't happen again: the cost is declared, owned, and reported under a
// step named for it.
//
// It also absorbs the applet spawn race. The `webServer` readiness
// probe hits /api/health, which the http server answers before the
// gateway has spawned any applet, so the first request through
// /applet/... can land on a 502. That used to be every spec's problem.
//
// What this does NOT cover: the gateway restarts an applet whenever a
// config change touches its entry, and a restart tears the child down.
// `SEARCH_SETTLE` in grid-helpers.ts stays generous for that reason.
import { test as setup, expect } from "@playwright/test";

setup("warm the qmd daemon", async ({ request }) => {
  // A cold model load under `--runs_per_test=N`, where every sandbox
  // pays it at once, is minutes rather than seconds.
  setup.setTimeout(300_000);

  // Straight at the applet — no browser, no grid. A bare word is free
  // text, which is exactly what routes through qmd.
  const query = "/applet/unified_index/search?q=earl%20grey&limit=1";

  // Polled, because the applet may still be spawning (502) — and
  // because the query itself is the thing being waited on, `timeout`
  // has to cover the model load rather than Playwright's 30s
  // per-request default.
  await expect
    .poll(
      async () => (await request.get(query, { timeout: 240_000 })).status(),
      {
        message:
          "applet never served a qmd query (502 = still spawning; see the status it settled on)",
        timeout: 240_000,
        intervals: [1_000],
      },
    )
    .toBe(200);
});
