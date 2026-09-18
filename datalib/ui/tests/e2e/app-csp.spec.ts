// The app page carries a Content-Security-Policy (datalib/backend/http/
// src/embed.rs) as the second layer behind DOMPurify. A policy that
// blocks something the app needs fails silently — a missing image, a
// dead style, a component that never mounts — so this walks the main
// screens with a violation listener installed and expects it to stay
// empty. `unsafe-eval` for card source, inline styles for the shadow
// roots, and the applet proxy for every fetch are the things most
// likely to regress.

import { test, expect, type Page } from "@playwright/test";
import { SEARCH_ROWS, TABLE_ROWS, firstRowUuid, selectRowByUuid } from "./grid-helpers";

declare global {
  interface Window {
    __cspViolations: string[];
  }
}

async function violations(page: Page): Promise<string[]> {
  return page.evaluate(() => window.__cspViolations ?? []);
}

test("no screen violates the page's CSP", async ({ page, context }) => {
  await context.addInitScript(() => {
    window.__cspViolations = [];
    document.addEventListener("securitypolicyviolation", (e) => {
      window.__cspViolations.push(
        `${e.violatedDirective}: ${e.blockedURI || "inline"} (${e.sourceFile}:${e.lineNumber})`,
      );
    });
  });
  // Chromium also reports each violation on the console; keep those as
  // a second witness for anything the event misses.
  const consoleCsp: string[] = [];
  page.on("console", (m) => {
    if (/Content Security Policy/.test(m.text())) consoleCsp.push(m.text());
  });

  // The grid, then a document opened from it: card source evaluated,
  // rendered markdown sanitized and mounted in a shadow root.
  await page.goto("/");
  await expect(page.locator(SEARCH_ROWS).first()).toBeVisible({ timeout: 15_000 });
  await selectRowByUuid(page, await firstRowUuid(page));
  await expect(page.locator(".chat-preview")).toBeVisible();
  expect(await violations(page)).toEqual([]);

  // The Sources screens: the config editor and the pipeline table.
  await page.goto("/sources");
  await expect(
    page.getByRole("heading", { name: "Configure data sources" }),
  ).toBeVisible();
  expect(await violations(page)).toEqual([]);

  await page.goto("/sources2");
  await expect(page.locator(TABLE_ROWS).first()).toBeVisible({ timeout: 15_000 });
  expect(await violations(page)).toEqual([]);

  expect(consoleCsp).toEqual([]);

  // And the policy is live, which is what makes the empty lists above
  // evidence rather than an unenforced header: an inline script that a
  // sanitizer bypass would inject is refused, and the refusal is what
  // the listener records.
  const injected = await page.evaluate(async () => {
    const seen = new Promise<string>((resolve) =>
      document.addEventListener(
        "securitypolicyviolation",
        (e) => resolve(e.violatedDirective),
        { once: true },
      ),
    );
    const s = document.createElement("script");
    s.textContent = "window.__cspBypassed = true";
    document.body.appendChild(s);
    const directive = await Promise.race([
      seen,
      new Promise<string>((r) => setTimeout(() => r("no violation"), 3_000)),
    ]);
    return { directive, ran: (window as { __cspBypassed?: boolean }).__cspBypassed === true };
  });
  expect(injected.ran, "an inline script must not run on the app page").toBe(false);
  expect(injected.directive).toMatch(/^script-src/);
});
