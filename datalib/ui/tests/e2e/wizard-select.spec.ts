// A field whose backend type is a closed enum is a dropdown, not a text
// box — `kind: "select"` in `ui/src/config/catalog.ts`.
import { test, expect, type Page } from "@playwright/test";
import { expandGroup } from "./grid-helpers";

const wizard = (page: Page) => page.getByRole("dialog");
// Structural, matching manager2-name.spec.ts: each field's <label>
// wraps its help paragraph, so the accessible name is caption + prose.
const field = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}")) > .wiz-input`);

async function openManager(page: Page) {
  await page.goto("/sources2");
  await expect(page.getByRole("heading", { name: "Pipeline" })).toBeVisible();
}

let original = "";

test.beforeEach(async ({ page }) => {
  await openManager(page);
  original = await page.locator(".m2-editor").inputValue();
});

test.afterEach(async ({ page }) => {
  if (!original) return;
  await openManager(page);
  await page.getByText("Advanced — edit config.toml directly").click();
  await page.locator(".m2-editor").fill(original);
  await page.getByRole("button", { name: "Save", exact: true }).click();
  await expect(page.getByText("Saved the config.")).toBeVisible();
});

test("an enum-backed field is a dropdown of its values", async ({ page }) => {
  await page.getByRole("button", { name: "+ Data Source" }).click();
  await page.getByRole("searchbox").fill("signal");
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Decrypt and mirror an Android Signal backup" })
    .click();
  await field(page, "Name").fill("Phone Signal");
  await wizard(page).locator("input.wiz-path").fill("/Users/x/backups/SignalBackups");

  // Signal's render step has the option; it sits under the Rendering
  // heading of the same form.
  const span = field(page, "Document span");
  // A <select>, not an <input>: the whole point is that there is no
  // free text to get wrong.
  await expect(span).toHaveJSProperty("tagName", "SELECT");
  await expect(span.locator("option")).toHaveText([
    "A day",
    "A month",
    "A year",
    "The whole conversation",
  ]);
  // Seeded to the backend's own default (`Period::from_config(None)`),
  // so what the form shows and what an omitted key would do agree.
  await expect(span).toHaveValue("month");

  await span.selectOption("year");
  await wizard(page).getByText("Review the TOML this writes").click();
  await expect(wizard(page).locator(".wiz-review pre")).toContainText('period = "year"');

  const submit = wizard(page).getByRole("button", { name: "Add source" });
  await expect(submit).toBeEnabled();
  await submit.click();
  await expect(page.getByText("Added Phone Signal.")).toBeVisible();
  await expandGroup(page, "phone-signal");
  await expect(page.locator('.ag-row[row-id="phone-signal/render_markdown"]')).toBeVisible();
  await expect(page.locator(".m2-editor")).toHaveValue(/period = "year"/);
});
