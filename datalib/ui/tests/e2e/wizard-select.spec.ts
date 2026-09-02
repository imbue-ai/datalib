// A field whose backend type is a closed enum is a dropdown, not a text
// box — `kind: "select"` in `ui/src/config/catalog.ts`.
//
// Signal's "Document span" is the one such field today: it fills
// `SignalRenderConfig::period`, which `Period::from_config` parses
// against exactly four spellings. As a text box it was a place to
// mistype `weekly` and find out at sync time; as a dropdown the four
// options are both the input and the documentation, which is why the
// help text underneath is one clause shorter than it was.
//
// The fixture root's config.toml is shared by every spec in the run
// (workers: 1), so it is restored in afterEach — including on failure.
import { test, expect, type Page } from "@playwright/test";

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
  await page.getByRole("button", { name: "+ Add Data Source" }).click();
  await page.getByRole("searchbox").fill("signal");
  await wizard(page)
    .locator(".wiz-tile", { hasText: "Decrypt and mirror an Android Signal backup" })
    .click();
  await field(page, "Name").fill("Phone Signal");
  await wizard(page).locator("input.wiz-path").fill("/Users/x/backups/SignalBackups");

  // Signal's render step has options, so it is offered as a second
  // dialog rather than the checkbox. Decline it here and reach the same
  // form through the row action, so this spec doesn't also depend on
  // the confirm() that carries the offer.
  page.once("dialog", (d) => void d.dismiss());
  await wizard(page).getByRole("button", { name: "Add source" }).click();
  await expect(page.getByText("Added Phone Signal.")).toBeVisible();

  await page
    .locator('.ag-row[row-id="phone-signal/raw"]')
    .getByRole("button", { name: "Render to markdown" })
    .click();

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

  // And the form can actually be submitted. `missingRequired` used to
  // read the whole descriptor rather than the fields on screen, so this
  // button sat disabled on Signal's required *download* field — leaving
  // the render step, and therefore this dropdown, unreachable.
  const submit = wizard(page).getByRole("button", { name: "Add render step" });
  await expect(submit).toBeEnabled();
  await submit.click();
  await expect(page.locator('.ag-row[row-id="phone-signal/rendered_md"]')).toBeVisible();
  await expect(page.locator(".m2-editor")).toHaveValue(/period = "year"/);
});
