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
//
// ## It checks that qmd *worked*, not that the server answered
//
// This used to assert `status === 200` and nothing else, which is a
// warm-up that cannot tell a warm daemon from a dead one. The applet
// degrades on purpose: when a qmd query fails it records the reason in
// `query_echo.qmd_error` and falls back to a SQL `LIKE`, then returns
// 200 (`backend/applets/src/unified_index/mod.rs`). That is right for
// the UI — a degraded grid beats a broken one — and it made this
// assertion vacuous.
//
// It was not hypothetical. On a machine whose Node had been upgraded
// past a cached `better-sqlite3` build, every qmd query failed with
// `ERR_DLOPEN_FAILED`; this step passed in 4.3s, reported the daemon
// warm, and eight specs then failed downstream with assertions about
// scores and row counts that named nothing about the actual cause.
//
// If that is the failure you are reading — "compiled against a
// different Node.js version using NODE_MODULE_VERSION <n>" — it is a
// stale prebuilt binary, not this repo. npm caches the prebuild
// separately from the package, so clearing `_npx` alone restores the
// same wrong one:
//
//     rm -rf ~/.npm/_npx/*                       # the installed trees
//     rm -f ~/.npm/_prebuilds/*better-sqlite3*   # and the prebuilds
//
// The next qmd invocation refetches the prebuild matching the running
// Node's ABI.
//
// So there are three checks now, and they are deliberately redundant:
//
//   * `qmd_error` is null. The direct signal, and the one that carries
//     the reason — an `ERR_DLOPEN_FAILED` arrives here in full.
//   * rows came back for `grey earl`. Reversed on purpose: the literal
//     "earl grey" appears in the fixture, so `LIKE` matches it and the
//     old query would have returned rows even with qmd dead. The
//     reversed pair matches only through qmd — the same property
//     `search-qmd-routing.spec.ts` is built on.
//   * those rows carry a `score`, which only qmd-routed rows do.
//
// The last two do not trust the backend's own bookkeeping: if
// `qmd_error` ever stops being populated, they still fail.
//
// ## The blast radius is the whole suite, and that is the intent
//
// Every other project `dependencies: ["warmup"]`, so a failure here
// skips them rather than letting them fail one at a time about scores
// and row counts. One named failure carrying qmd's own message beats
// eight anonymous ones — which is exactly the trade this file was
// created to make, now that it is in a position to actually make it.
import { test as setup, expect } from "@playwright/test";

/// A qmd failure collapsed onto one line, capped.
///
/// Not `split("\n")[0]`, which was the first thing tried and threw the
/// diagnosis away: qmd's stderr opens with a node loader frame
/// (`node:internal/modules/cjs/loader:1996`) and the sentence that says
/// what is actually wrong — "compiled against a different Node.js
/// version using NODE_MODULE_VERSION 127" — is several lines down. The
/// cap has to be wide enough to reach it.
function summarize(s: string): string {
  const flat = s.replace(/\s+/g, " ").trim();
  return flat.length > 400 ? `${flat.slice(0, 400)}…` : flat;
}

/// What one row looks like to this file. `score` is the whole point:
/// `api.ts` documents it as present only on qmd-routed rows and absent
/// on the LIKE fallback.
type Row = { score?: number };

setup("warm the qmd daemon", async ({ request }) => {
  // A cold model load under `--runs_per_test=N`, where every sandbox
  // pays it at once, is minutes rather than seconds.
  setup.setTimeout(300_000);

  // Straight at the applet — no browser, no grid. Bare words are free
  // text, which is exactly what routes through qmd. `grey earl` rather
  // than `earl grey`: see the header — only qmd matches the reversed
  // pair, so the row count is itself a routing assertion.
  const query = "/applet/unified_index/search?q=grey%20earl&limit=1";

  // Two phases, because the two things being waited on have completely
  // different lifetimes and deserve different budgets.
  //
  // **Is it answering at all** — minutes. 502 means the gateway has not
  // spawned the applet yet, and the model load happens *inside* the
  // request, so `timeout` has to cover it rather than Playwright's 30s
  // per-request default.
  await expect
    .poll(async () => (await request.get(query, { timeout: 240_000 })).status(), {
      message:
        "the applet never answered (502 = still spawning; see the status it settled on)",
      timeout: 240_000,
      intervals: [1_000],
    })
    .toBe(200);

  // **Did it answer well** — seconds. A 200 means the daemon finished
  // loading, so a qmd failure now is a property of the environment
  // rather than a race, and re-asking for another four minutes only
  // delays the report. The short retry is for the one race left: the
  // gateway can restart an applet under us, and the first query after
  // that can fail while its child comes back.
  //
  // Returns a *description* rather than a boolean, so the failure names
  // which of the four things was wrong — and, for the case that
  // actually happens, quotes qmd's own error.
  await expect
    .poll(
      async () => {
        const r = await request.get(query, { timeout: 60_000 });
        if (r.status() !== 200) return `HTTP ${r.status()}`;
        const body = (await r.json()) as {
          query_echo?: { qmd_error?: string | null };
          rows?: Row[];
        };
        const failed = body.query_echo?.qmd_error;
        if (failed) {
          return `qmd failed, and the applet fell back to LIKE: ${summarize(failed)}`;
        }
        const rows = body.rows ?? [];
        if (rows.length === 0) {
          return 'qmd answered with no rows for "grey earl" — the index is empty or unqueryable';
        }
        if (typeof rows[0].score !== "number") {
          return "rows carry no score, so they came from the LIKE fallback rather than qmd";
        }
        return "warm";
      },
      {
        message: "the qmd daemon answered, but not with a working qmd query",
        timeout: 30_000,
        intervals: [1_000],
      },
    )
    .toBe("warm");
});
