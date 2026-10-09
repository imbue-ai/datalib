// First-run onboarding against a genuinely empty data root.

import { test, expect } from "@playwright/test";
import { cardOf, tabLabels } from "./grid-helpers";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const EMPTY_URL = process.env.DATALIB_TEST_E2E_EMPTY_URL;

test("an empty folder gets an explained bootstrap, not a 502", async ({ page, request }) => {
  expect(EMPTY_URL, "playwright.config.ts should have started the empty-root backend").toBeTruthy();

  // Precondition: the root really is uninitialized, and the applet the
  // grid needs really is missing — i.e. this run reproduces the
  // reported failure rather than testing a root that got initialized
  // by an earlier run.
  const before = await request.get(`${EMPTY_URL}/api/config`);
  expect((await before.json()).exists).toBe(false);
  const search = await request.get(`${EMPTY_URL}/applet/unified_index/search?q=&limit=1`);
  expect(search.status()).toBe(502);
  expect(await search.text()).toContain("no applet");

  await page.goto(`${EMPTY_URL}/`);

  // The user is told what will happen before anything is written: the
  // heading, the exact file, and that no source is added for them.
  await expect(page.getByRole("heading", { name: "Initialize data library" })).toBeVisible();
  await expect(page.getByText("creates a bare-bones config file")).toBeVisible();
  // What the file holds is one click away, closed until asked for.
  const written = page.locator("details");
  await expect(written.getByText("no data sources")).toBeHidden();
  await written.locator("summary").click();
  await expect(written.locator("code.root")).toContainText("config.toml");
  await expect(written.getByText("no data sources")).toBeVisible();

  // …and nothing has been written yet just by looking at the screen.
  const stillEmpty = await request.get(`${EMPTY_URL}/api/config`);
  expect((await stillEmpty.json()).exists).toBe(false);

  // The top bar names the library, which in the desktop app is the way
  // back to the other libraries. It has no search box: the grid behind
  // it is the 502.
  await expect(page.getByRole("navigation", { name: "App" })).toContainText("Data Liberation");
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toHaveCount(0);

  await page.getByRole("button", { name: "Initialize data library" }).click();

  // Initializing lands on the Manage view, where a source can be added —
  // a library with no sources is not finished, so there is no
  // congratulations screen in between.
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  await expect(page.getByRole("button", { name: "Add source" })).toBeVisible();
  // The sources card alone: the config editor is a click away from it,
  // not open beside it.
  await expect(cardOf(page, "sourcesView(")).toHaveCount(1);
  await expect(cardOf(page, "configView(")).toHaveCount(0);

  // The file is on disk and valid, and it carries the applet whose
  // absence was the original error.
  const after = await (await request.get(`${EMPTY_URL}/api/config`)).json();
  expect(after.exists).toBe(true);
  expect(after.parsed_ok).toBe(true);
  expect(after.text).toContain('id = "unified_index"');

  // The gate is gone, so the search box is back…
  await expect(page.getByRole("searchbox", { name: "Search your data" })).toBeVisible();

  // …and it does not come back on reload now that the root is
  // initialized.
  await page.goto(`${EMPTY_URL}/data_sources`);
  await expect(page.getByRole("button", { name: "Sync everything" })).toBeVisible();
  await expect(page.getByRole("heading", { name: "Initialize data library" })).toHaveCount(0);

  // The Dashboard of a library with no sources says so where the
  // sources would be, with nothing to sync, and its button opens the
  // add-source form.
  // This page's tabs come back with it, so pick the Dashboard's.
  await page.goto(`${EMPTY_URL}/`);
  await tabLabels(page).filter({ hasText: "Dashboard" }).click();
  const sources = page.getByRole("region", { name: "Sources" });
  await expect(sources.getByText("No sources yet.")).toBeVisible();
  await expect(page.getByText("Nothing to sync yet")).toBeVisible();
  await sources.getByRole("button", { name: "Add source" }).click();
  await expect(page.locator(".wiz-filter")).toBeVisible();
});
