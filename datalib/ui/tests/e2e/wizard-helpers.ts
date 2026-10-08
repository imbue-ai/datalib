// Reaching into the "Add source" dialog: a row by its heading, and the
// parts that sit inside Advanced options, which starts closed.
import { expect, type Locator, type Page } from "@playwright/test";

export const wizardOf = (page: Page): Locator => page.getByRole("dialog");

/// The sign-in controls, opened if the account row had folded them
/// away behind "Connected as …". Waits for the row to settle first:
/// an account latchkey holds is checked on opening.
export async function showSignIn(page: Page) {
  const again = wizardOf(page).getByRole("button", { name: "Use a different account" });
  const controls = wizardOf(page).locator(".wiz-conn-intro");
  await expect(again.or(controls)).toBeVisible();
  if (await again.isVisible()) await again.click();
}

/// One heading of the form and everything drawn beside it.
export const row = (page: Page, heading: string): Locator =>
  wizardOf(page).locator(`.wiz-field:has(> .wiz-label:text-is("${heading}"))`);

async function open(details: Locator) {
  if (!(await details.evaluate((el) => (el as HTMLDetailsElement).open))) {
    await details.locator("> summary").click();
  }
}

export async function openAdvanced(page: Page) {
  await open(wizardOf(page).locator(".wiz-advanced"));
}

/// The TOML the dialog would write, opened if it was not.
export async function reviewToml(page: Page): Promise<Locator> {
  await openAdvanced(page);
  const review = wizardOf(page).locator(".wiz-review");
  await open(review);
  return review.locator("pre");
}
