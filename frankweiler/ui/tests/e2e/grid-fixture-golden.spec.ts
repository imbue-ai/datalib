import { test, expect } from "@playwright/test";

/**
 * Golden snapshot of every row in the TNG fixture, projected to a
 * compact identifying tuple. Catches:
 *
 *   * Backend-side ingest regressions — a provider stops emitting
 *     rows, a new provider lands without being wired into the test
 *     pipeline, a row's `kind` / `channel` / `author` shifts.
 *   * UI-facing search-API drift — a column gets renamed, an
 *     enabled-by-default filter starts hiding rows.
 *
 * Does NOT catch UI-render bugs (column formatters, cell
 * components). The existing `grid-populated.spec.ts` covers
 * "rows actually bind to the grid"; this one covers "the right
 * rows make it through".
 *
 * Snapshot maintenance: when the TNG fixture changes intentionally
 * (new provider, new character, schema bump), regenerate via
 *
 *   bazelisk run //frankweiler/ui:e2e -- --update-snapshots
 *
 * and commit the resulting `.txt` under
 * `tests/e2e/grid-fixture-golden.spec.ts-snapshots/`.
 */

// Compact tuple per row — enough to identify which entry shifted
// without dragging the full SearchRow shape into the snapshot.
//
// `entire_chat` / `snippet` / `when` / `markdown_uuid` deliberately
// left out: they churn with cosmetic doc-route refactors, content
// edits, and clock changes. The five fields below uniquely identify
// a row and shift only on a real ingest behavior change.
//
// Mirrors the visible AG Grid columns the user actually sees
// (Source / Kind / Channel / Author).
type RowTuple = [
  uuid: string,
  source: string,
  kind: string,
  channel: string,
  author: string,
];

interface SearchRow {
  uuid: string;
  source: string;
  kind: string;
  channel: string;
  author: string;
  // Other SearchRow fields exist but are intentionally not part of
  // the snapshot projection.
}

test("grid fixture row set matches the golden", async ({ request }) => {
  // limit=10000 is bigger than any plausible TNG fixture row count
  // (the snapshot test currently sits at 79); the backend caps at
  // 100k internally, so this won't truncate silently.
  const resp = await request.get("/api/search?q=&limit=10000");
  expect(resp.ok(), `search API: HTTP ${resp.status()}`).toBeTruthy();
  const data = (await resp.json()) as { rows: SearchRow[] };

  const tuples: RowTuple[] = data.rows
    .map(
      (r): RowTuple => [r.uuid, r.source, r.kind, r.channel, r.author],
    )
    // Sort by UUID for a stable, locale-independent ordering — the
    // backend's default `score` sort is request-time-relevance and
    // would flap on every fixture tweak that perturbs scoring.
    .sort((a, b) => a[0].localeCompare(b[0], "en"));

  // Stringify as JSON Lines so a `git diff` on the snapshot reads
  // one row per line. JSON.stringify(arr, null, 2) would balloon
  // each tuple to 7 lines and turn the diff into noise.
  const snapshot = tuples.map((t) => JSON.stringify(t)).join("\n") + "\n";
  expect(snapshot).toMatchSnapshot("grid-fixture-golden.txt");
});
