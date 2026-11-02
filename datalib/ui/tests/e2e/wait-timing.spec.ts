// The waits a person sits through reach the log as lines of their own:
// a search's rows reaching the screen, and a document's body. A source's
// table took seven seconds to open for days, and only the server's
// per-request times recorded it, spread over three requests.

import { test, expect, type Page } from "@playwright/test";
import { GRID, SEARCH_ROWS, firstRowUuid, openRowByUuid, searchAndSettle } from "./grid-helpers";

type Posted = { name: string; level?: string; fields?: Record<string, unknown> };

/// Every event the page posts from now on, as the server receives it.
function postedEvents(page: Page): Posted[] {
  const seen: Posted[] = [];
  page.on("request", (r) => {
    if (r.method() === "POST" && new URL(r.url()).pathname === "/api/ui/events") {
      seen.push(...(r.postDataJSON() as { events: Posted[] }).events);
    }
  });
  return seen;
}

/// The page batches its events for up to three seconds.
const POSTED = { timeout: 15_000 };

test("a search card logs how long its rows and a document took to show", async ({ page }) => {
  const events = postedEvents(page);
  const waits = (name: string) => events.filter((e) => e.name === name).map((e) => e.fields);

  await page.goto(GRID);
  await page.locator(SEARCH_ROWS).first().waitFor();
  await expect
    .poll(() => waits("search_answered"), POSTED)
    .toContainEqual(
      expect.objectContaining({
        trigger: "open",
        outcome: "rows",
        q: "is:document",
        ms: expect.any(Number),
      }),
    );

  await searchAndSettle(page, "-is:document");
  await expect
    .poll(() => waits("search_answered"), POSTED)
    .toContainEqual(
      expect.objectContaining({ trigger: "search", outcome: "rows", q: "-is:document" }),
    );

  await searchAndSettle(page, "is:document");
  await openRowByUuid(page, await firstRowUuid(page));
  await expect
    .poll(() => waits("doc_shown"), POSTED)
    .toContainEqual(
      expect.objectContaining({
        trigger: "open",
        outcome: "shown",
        bytes: expect.any(Number),
        ms: expect.any(Number),
      }),
    );
});
