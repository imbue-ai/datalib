// Pay the qmd cold start once, in the open, before any spec runs.
import { test as setup, expect } from "@playwright/test";

/// A qmd failure collapsed onto one line, capped.
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
