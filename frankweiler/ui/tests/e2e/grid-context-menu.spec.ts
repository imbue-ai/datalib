import { test, expect } from "@playwright/test";

// Pin the contract: a right-click on a grid row opens the AG Grid
// Enterprise context menu (.ag-menu) — with our custom items prepended
// to AG Grid's defaults — and the underlying contextmenu event is
// `defaultPrevented` by the time it reaches `window`, so the browser's
// native menu never shows over the grid's. AG Grid handles the
// preventDefault via `preventDefaultOnContextMenu: true` set in
// GridCard.ce.vue's gridOptions.

test("right-click on a grid row suppresses the native browser menu", async ({
  page,
  request,
}) => {
  const resp = await request.get("/api/search?q=&limit=50");
  expect(resp.ok()).toBeTruthy();
  const data = (await resp.json()) as { rows: unknown[] };
  expect(data.rows.length, "fixture must have at least one row").toBeGreaterThan(0);

  await page.goto("/");
  const firstRow = page
    .locator('.ag-center-cols-container [role="row"]')
    .first();
  await firstRow.waitFor({ timeout: 10_000 });

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
    // own context menu on top of AG Grid's `.ag-menu`.
    window.addEventListener(
      "contextmenu",
      (ev) => {
        w.__ctxMenuFired = true;
        w.__ctxMenuPrevented = ev.defaultPrevented;
      },
      false,
    );
  });

  await firstRow.click({ button: "right" });

  // AG Grid Enterprise menu should appear.
  await expect(page.locator(".ag-menu")).toBeVisible({ timeout: 5_000 });

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
    "contextmenu must be defaultPrevented before reaching window — otherwise the browser shows its native menu over AG Grid's",
  ).toBe(true);
});
