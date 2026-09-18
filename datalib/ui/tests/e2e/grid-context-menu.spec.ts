import { test, expect } from "@playwright/test";
import { contextMenuRowByUuid } from "./grid-helpers";

// Pin the contract: a right-click on a grid row opens the grid's
// context menu — with our own items ahead of the grid's — and the
// underlying contextmenu event is `defaultPrevented` by the time it
// reaches `window`, so the browser's native menu never shows over the
// grid's.

test("right-click on a grid row suppresses the native browser menu", async ({
  page,
  request,
}) => {
  const resp = await request.get("/applet/unified_index/search?q=&limit=50");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: unknown[] };
  expect(data.rows.length, "fixture must have at least one row").toBeGreaterThan(0);

  await page.goto("/");
  await page.locator(".grid-box .slick-row").first().waitFor({ timeout: 10_000 });

  // Capture-phase listener at window: this is the last point at which the
  // browser checks `defaultPrevented` before deciding whether to render
  // the native menu. (Bubble-phase at window is equivalent for events
  // that originate inside the document.)
  await page.evaluate(() => {
    const w = window as unknown as {
      __ctxMenuPrevented?: boolean;
      __ctxMenuFired?: boolean;
    };
    w.__ctxMenuPrevented = false;
    w.__ctxMenuFired = false;
    // Bubble-phase listener on window fires *last* in the event flow —
    // same point at which the UA decides whether to render the native
    // menu. If `defaultPrevented` is false here, the browser shows its
    // own context menu on top of the grid's.
    window.addEventListener(
      "contextmenu",
      (ev) => {
        w.__ctxMenuFired = true;
        w.__ctxMenuPrevented = ev.defaultPrevented;
      },
      false,
    );
  });

  // A row brought into view first: the grid renders a few rows beyond
  // the viewport, and a click that has to scroll one of those in lands
  // on a node the grid has since replaced.
  const target = (data.rows[0] as { uuid: string }).uuid;
  await contextMenuRowByUuid(page, target);

  const { fired, prevented } = await page.evaluate(() => {
    const w = window as unknown as {
      __ctxMenuPrevented?: boolean;
      __ctxMenuFired?: boolean;
    };
    return { fired: !!w.__ctxMenuFired, prevented: !!w.__ctxMenuPrevented };
  });
  expect(fired, "contextmenu event must have fired on window").toBe(true);
  expect(
    prevented,
    "contextmenu must be defaultPrevented before reaching window — otherwise the browser shows its native menu over the grid's",
  ).toBe(true);
});
