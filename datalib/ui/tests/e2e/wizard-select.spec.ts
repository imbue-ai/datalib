// A field whose backend type is a closed enum is a dropdown, not a text
// box — `kind: "select"` in `ui/src/config/catalog.ts`.
import { test, expect, type Page } from "@playwright/test";
import { openAdvanced, reviewToml } from "./wizard-helpers";
import {
  fillConfigEditor,
  expandGroup,
  MANAGE_WITH_CONFIG,
  savedConfig,
  expectSyncEverything,
  SAVE_ANSWERED,
} from "./grid-helpers";

const wizard = (page: Page) => page.getByRole("dialog");
// Structural, matching data-sources-name.spec.ts: each field's <label>
// wraps its help paragraph, so the accessible name is caption + prose.
const field = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) .wiz-input`);

async function openManager(page: Page) {
  await page.goto(MANAGE_WITH_CONFIG);
  await expectSyncEverything(page);
}

let original = "";

test.beforeEach(async ({ page, request }) => {
  await openManager(page);
  original = await savedConfig(request);
});

test.afterEach(async ({ page }) => {
  if (!original) return;
  await openManager(page);
  await fillConfigEditor(page, original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible({ timeout: SAVE_ANSWERED });
});

test("an enum-backed field is a dropdown of its values", async ({ page }) => {
  await page.getByRole("button", { name: "Add source" }).click();
  await page.locator(".wiz-filter").fill("claude");
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Ingest an unpacked Claude data export already on disk." })
    .click();
  await field(page, "Name").fill("Personal Claude");
  await wizard(page).locator("input.wiz-path").fill("/Users/x/Downloads/claude-export");

  // Under its section's heading, found by the value only it offers.
  const kept = wizard(page).locator(".wiz-input:has(option[value='keep_local_copy'])");
  // A <select>, not an <input>: the whole point is that there is no
  // free text to get wrong.
  await expect(kept).toHaveJSProperty("tagName", "SELECT");
  await expect(kept.locator("option")).toHaveText(["Delete my copy too", "Keep my copy"]);
  // Seeded to the backend's own default, so what the form shows and
  // what an omitted key would do agree.
  await expect(kept).toHaveValue("delete_local_copy");

  await kept.selectOption("keep_local_copy");
  await reviewToml(page);
  await expect(wizard(page).locator(".wiz-review pre")).toContainText(
    'when_deleted_upstream = "keep_local_copy"',
  );

  const submit = wizard(page).getByRole("button", { name: "Add source" });
  await expect(submit).toBeEnabled();
  await submit.click();
  await expect(page.getByText("Added Personal Claude.")).toBeVisible({ timeout: SAVE_ANSWERED });
  await expandGroup(page, "personal-claude");
  await expect(
    page.locator('.tg-grid .slick-row[data-key="personal-claude/ingest"]'),
  ).toBeVisible();
  await expect(page.locator(".m2-editor")).toHaveValue(/when_deleted_upstream = "keep_local_copy"/);
});
