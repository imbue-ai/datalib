// The DACTAL card's canned Slack queries, run on the fixture's Slack
// source: each chip loads that source's rows and its query draws a table
// that names what the TNG workspace holds.

import { test, expect } from "@playwright/test";

declare const process: { env: Record<string, string | undefined> };
const TOKEN = process.env.DATALIB_TOKEN;

// As in dactal-sandbox.spec.ts: the suite's ambient `Authorization`
// header would make the sandboxed frame's module load a CORS preflight
// the server does not answer, so sign in the way a browser does.
test.use({ extraHTTPHeaders: {} });

const PRESETS: { label: string; dq: string; shows: string }[] = [
  {
    label: "Messages per channel",
    dq: "rows:kind=[Slack Message]/conversation_name#-count",
    shows: "#bridge",
  },
  {
    label: "Who posts most",
    dq: "rows:kind=[Slack Message]/author#-count",
    shows: "Jean-Luc Picard",
  },
  {
    label: "Threads, newest first",
    dq: "rows:kind=[Slack Thread]#-touched_at",
    shows: "Shields up. Red alert.",
  },
  { label: "Reactions by emoji", dq: "rows:kind=[Slack Reaction]/name#-count", shows: "👍" },
  {
    label: "Who reacts most",
    dq: "rows:kind=[Slack Reaction]/author#-count",
    shows: "Jean-Luc Picard",
  },
];

test("each canned Slack query loads the Slack source and draws a table", async ({ page }) => {
  expect(TOKEN, "playwright.config.ts should have pinned DATALIB_TOKEN").toBeTruthy();
  await page.goto(`/${encodeURIComponent("dactalView()")}?token=${TOKEN}`);
  const frame = page.frameLocator('iframe[src="/dactal/index.html"]');
  const status = frame.locator("#status");
  await expect(status).toContainText(/results? for/, { timeout: 20_000 });

  const row = frame.locator(".preset-row", { hasText: "Slack" });
  await expect(row.locator(".preset")).toHaveText(PRESETS.map((p) => p.label));

  for (const p of PRESETS) {
    await row.getByRole("button", { name: p.label }).click();
    await expect(frame.locator("#datalib")).toHaveValue("source:slack");
    await expect(frame.locator("#dq")).toHaveValue(p.dq);
    await expect(status).toHaveText(new RegExp(`^[1-9]\\d* results? for “${escape(p.dq)}”$`));
    await expect(frame.locator("#queryoutput .err")).toHaveCount(0);
    await expect(frame.locator("#queryoutput table").first()).toContainText(p.shows);
  }
});

function escape(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
