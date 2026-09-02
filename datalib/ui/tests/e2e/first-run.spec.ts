// First-run onboarding against a genuinely empty data root.
//
// The bug this guards: opening the app on an empty folder showed the
// grid, the grid asked the `unified_index` applet for rows, and that
// applet is declared *in the config* — so a new user's first screen was
// `502 {"error":"no applet \"unified_index\""}`. The fix is a gate that
// says what it is about to do to the folder and then does it.
//
// This spec drives a second backend, started by playwright.config.ts on
// its own empty `mkdtemp` root (FW_E2E_EMPTY_URL) — the onboarding
// state is unreachable from the fixture root, which has a config.
//
// The one destructive step is the point of the test, so the order
// inside the single test matters: everything asserted about the
// uninitialized root has to happen before the button is clicked.

import { test, expect } from "@playwright/test";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

const EMPTY_URL = process.env.FW_E2E_EMPTY_URL;

test("an empty folder gets an explained bootstrap, not a 502", async ({
  page,
  request,
}) => {
  expect(
    EMPTY_URL,
    "playwright.config.ts should have started the empty-root backend",
  ).toBeTruthy();

  // Precondition: the root really is uninitialized, and the applet the
  // grid needs really is missing — i.e. this run reproduces the
  // reported failure rather than testing a root that got initialized
  // by an earlier run.
  const before = await request.get(`${EMPTY_URL}/api/config`);
  expect((await before.json()).exists).toBe(false);
  const search = await request.get(
    `${EMPTY_URL}/applet/unified_index/search?q=&limit=1`,
  );
  expect(search.status()).toBe(502);
  expect(await search.text()).toContain("no applet");

  await page.goto(`${EMPTY_URL}/`);

  // The user is told what will happen before anything is written: the
  // heading, the exact file, and that no source is added for them.
  await expect(page.getByRole("heading", { name: "Set up a data library" })).toBeVisible();
  // `.first()`: getByText matches every ancestor whose text contains
  // the string, and strict mode rejects a multi-element locator.
  await expect(page.locator("code.root")).toContainText("config.toml");
  await expect(page.getByText("no data sources").first()).toBeVisible();

  // …and nothing has been written yet just by looking at the screen.
  const stillEmpty = await request.get(`${EMPTY_URL}/api/config`);
  expect((await stillEmpty.json()).exists).toBe(false);

  // The tabs are hidden while the root is uninitialized — none of them
  // can do anything, and the grid behind "Explore" is the 502.
  await expect(page.getByRole("link", { name: "Explore" })).toHaveCount(0);

  await page.getByRole("button", { name: "Initialize empty data library" }).click();

  // Initializing lands on the Manage view, where a source can be added —
  // a library with no sources is not finished, so there is no
  // congratulations screen in between.
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
  await expect(page.getByRole("button", { name: "+ Add Data Source" })).toBeVisible();
  expect(new URL(page.url()).pathname).toBe("/sources2");

  // The file is on disk and valid, and it carries the applet whose
  // absence was the original error.
  const after = await (await request.get(`${EMPTY_URL}/api/config`)).json();
  expect(after.exists).toBe(true);
  expect(after.parsed_ok).toBe(true);
  expect(after.text).toContain('id = "unified_index"');

  // The gate is gone, so the tabs are back…
  await expect(page.getByRole("link", { name: "Explore" })).toBeVisible();

  // …and it does not come back on reload now that the root is
  // initialized.
  await page.goto(`${EMPTY_URL}/sources`);
  await expect(page.getByRole("heading", { name: "Configure data sources" })).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Set up a data library" }),
  ).toHaveCount(0);
});
