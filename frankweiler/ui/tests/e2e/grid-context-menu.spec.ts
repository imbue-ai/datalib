import { test, expect } from "@playwright/test";

// Bug: right-clicking on an AG Grid row pops the *browser's* native context
// menu on top of our custom `.ctx-menu`. The handler in SearchView.vue
// (`onCellContextMenu`) does call `me.preventDefault()` on the original
// MouseEvent, but AG Grid dispatches `cellContextMenu` from inside its own
// listener — by the time our handler runs, the contextmenu event has
// already bubbled past `document` / `window` without `defaultPrevented`,
// so the UA goes ahead and shows its menu.
//
// This test pins the contract: when a user right-clicks a row, the
// contextmenu event must be `defaultPrevented` by the time it reaches
// `window` (otherwise the native menu wins). We do *not* fix the bug
// here — only reproduce it.

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
    // own context menu on top of our custom `.ctx-menu`.
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

  // Custom menu should appear.
  await expect(page.locator(".ctx-menu")).toBeVisible({ timeout: 5_000 });

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
    "contextmenu must be defaultPrevented before reaching window — otherwise the browser shows its native menu over our custom one",
  ).toBe(true);
});
