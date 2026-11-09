import { test, expect, type Page } from "@playwright/test";

// A right-click on the data root's size in the status bar frees up disk.
// Left as it opens, the dialog deletes only what no commit reaches; with
// "drop old versions" ticked, each source's rendered documents also keep
// one commit, holding what they held. It rewrites the
// stores, so the spec has a root of its own (config-mutating.ts).

const RENDER = "slack/render_markdown";

async function openDialog(page: Page) {
  const root = page.getByTestId("root-storage");
  await expect(root.locator(".root-bar-path")).not.toHaveText("", { timeout: 10_000 });
  await root.locator(".root-bar-spark").first().click({ button: "right" });
  await page.getByRole("menuitem", { name: "Free up disk space…" }).click();
  return page.getByRole("dialog", { name: "Free up disk space" });
}

test("freeing disk collects, and folds history only when asked", async ({ page, request }) => {
  const log = async (): Promise<string[]> => {
    const r = await request.get(`/api/pipeline/history?tree=${encodeURIComponent(RENDER)}`);
    const h = (await r.json()) as { stores: { commits: { message: string }[] }[] };
    return h.stores[0]?.commits.map((c) => c.message) ?? [];
  };
  const before = await log();
  expect(before.length, "the fixture's render store has a history to fold").toBeGreaterThan(2);

  await page.goto("/");
  let dialog = await openDialog(page);
  await expect(dialog.getByTestId("free-collect")).toBeChecked();
  await expect(dialog.getByTestId("free-drop-old")).not.toBeChecked();
  let asked = page.waitForResponse((r) => r.url().endsWith("/api/free_disk_space"));
  await dialog.getByRole("button", { name: "Free up space" }).click();
  let answer = await asked;
  expect([202, 204]).toContain(answer.status());
  expect(answer.request().postDataJSON()).toEqual({ drop_old_versions_on_derived_data: false });

  // The toolbar's pill says how it went, source by source, until dismissed.
  const pill = page.getByTestId("free-space-pill").getByRole("button");
  await expect(pill).toHaveText(/Freed \d/, { timeout: 20_000 });
  await pill.click();
  const rows = page.getByTestId("free-space-pill").locator(".pass-list li");
  await expect(rows.first()).toBeVisible();
  await expect(rows.filter({ hasNotText: "Done" })).toHaveCount(0);
  await page.getByTestId("free-space-pill").getByRole("button", { name: "Dismiss" }).click();
  await expect(page.getByTestId("free-space-pill")).toHaveCount(0);

  // Dropping old versions holds the first box ticked: it collects too.
  dialog = await openDialog(page);
  await dialog.getByTestId("free-drop-old").check();
  await expect(dialog.getByTestId("free-collect")).toBeChecked();
  await expect(dialog.getByTestId("free-collect")).toBeDisabled();
  asked = page.waitForResponse((r) => r.url().endsWith("/api/free_disk_space"));
  await dialog.getByRole("button", { name: "Free up space" }).click();
  answer = await asked;
  expect([202, 204]).toContain(answer.status());
  expect(answer.request().postDataJSON()).toEqual({ drop_old_versions_on_derived_data: true });

  await expect
    .poll(log, { timeout: 60_000 })
    .toEqual([expect.stringMatching(/^compact: \d+ commits folded into one/), expect.any(String)]);
});
