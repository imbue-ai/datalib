import { test, expect } from "@playwright/test";

// A right-click on the data root's size in the status bar compacts the
// library's history: the rendered documents and the grid index each keep
// one commit, holding what they held. It rewrites the stores, so the spec
// has a root of its own (config-mutating.ts).

const GRID = "unified_document_index/grid_index";

test("a right-click on the root's size compacts the history", async ({ page, request }) => {
  const log = async (): Promise<string[]> => {
    const r = await request.get(`/api/pipeline/history?tree=${encodeURIComponent(GRID)}`);
    const h = (await r.json()) as { stores: { commits: { message: string }[] }[] };
    return h.stores[0]?.commits.map((c) => c.message) ?? [];
  };
  expect((await log()).length, "the fixture's index has a history to fold").toBeGreaterThan(2);

  await page.goto("/");
  const root = page.getByTestId("root-storage");
  await expect(root.locator(".root-bar-path")).not.toHaveText("", { timeout: 10_000 });
  page.once("dialog", (d) => void d.accept());
  await root.locator(".root-bar-spark").first().click({ button: "right" });
  const asked = page.waitForResponse((r) => r.url().endsWith("/api/compact"));
  await page.getByRole("menuitem", { name: "Compact history…" }).click();
  expect([202, 204]).toContain((await asked).status());

  await expect
    .poll(log, { timeout: 60_000 })
    .toEqual([expect.stringMatching(/^compact: \d+ commits folded into one/), expect.any(String)]);
});
