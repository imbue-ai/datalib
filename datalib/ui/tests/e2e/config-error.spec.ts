// What a broken `config.toml` does to the app — and, just as much,
// what it must *not* do.
//
// The bug (#209): `config.toml` declares the `unified_index` applet,
// and that applet is the grid, the search and the document view. The
// loader was all-or-nothing, so one stray key anywhere in the file cost
// every one of them, and each screen discovered its own
// `502 no applet "unified_index"` — a symptom a whole screen away from
// its cause. 00633dd5 is that happening: a leftover `title =` took the
// e2e suite from 25 passing to 5.
//
// So there are two behaviors here and the line between them is the
// point:
//
//   * a config with a **broken entry** loads. The app works, the other
//     sources still sync, and the dropped entry is explained on its own
//     row. Nothing blocks.
//   * a config that is **not a config** blocks, with a screen that says
//     so, instead of letting every view fail separately.
//
// Both are driven by writing the file directly rather than through
// `PUT /api/config`, and that is deliberate on two counts. The PUT
// refuses a bad config on purpose, so it cannot reach either state.
// And a hand-edit — an agent, `$EDITOR`, a `git checkout` — is exactly
// how a running install gets here, which is the case the gate has to
// survive: it must appear *and disappear* while the app is open, with
// no reload. `watch.rs` reports the write as `config_changed`; `App.vue`
// re-checks on it.
import { test, expect, type Page } from "@playwright/test";
import { readFileSync, writeFileSync } from "node:fs";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

/// This spec's own data root. It is in `CONFIG_MUTATING`, so
/// `playwright.config.ts` materialized a root for it and pointed this
/// project's `baseURL` at the backend on it — which is what lets the
/// spec write the file underneath a running server without any other
/// spec noticing.
function dataRoot(): string {
  const sandboxes = JSON.parse(process.env.FW_E2E_SANDBOXES ?? "[]") as {
    spec: string;
    root: string;
  }[];
  const mine = sandboxes.find((s) => s.spec === "config-error");
  if (!mine) throw new Error("config-error is not in CONFIG_MUTATING");
  return mine.root;
}

const configPath = () => `${dataRoot()}/config.toml`;
const readConfig = () => readFileSync(configPath(), "utf8");
const writeConfig = (text: string) => writeFileSync(configPath(), text);

const tabs = (page: Page) => page.getByRole("link", { name: "Explore" });
const gate = (page: Page) => page.locator(".cfg-error");

let original = "";

test.beforeEach(() => {
  original = readConfig();
});

// Restored on failure too — this spec breaks the file on purpose, and a
// root left broken would fail every later test in this project with the
// blocking screen instead of the real reason.
test.afterEach(async ({ page }) => {
  if (original) writeConfig(original);
  await page.goto("/sources2");
  await expect(gate(page)).toHaveCount(0);
});

test("a broken entry costs that entry, and nothing else", async ({
  page,
  request,
}) => {
  // A step the loader must reject, appended to a config that is
  // otherwise entirely fine. `title` is the exact key from 00633dd5.
  writeConfig(
    `${original}\n[[steps]]\nid = "broken/raw"\n` +
      `command = "datalib-step download pdf"\ntitle = "nope"\n`,
  );

  // The server still reads a usable config: it is a config (`parsed_ok`),
  // the app can serve its views (`app_ready`), and it says precisely
  // what it dropped.
  const cfg = await (await request.get("/api/config")).json();
  expect(cfg.parsed_ok).toBe(true);
  expect(cfg.app_ready).toBe(true);
  expect(cfg.diagnostics).toHaveLength(1);
  expect(cfg.diagnostics[0].severity).toBe("rejected");
  expect(cfg.diagnostics[0].entry.id).toBe("broken/raw");
  expect(cfg.diagnostics[0].message).toContain("title");

  // The applet the whole app runs on is untouched — the assertion that
  // would have failed before #209, and the one that matters most.
  const search = await request.get("/applet/unified_index/search?q=&limit=1");
  expect(search.status()).toBe(200);

  await page.goto("/sources2");
  // No gate, and the app is fully navigable.
  await expect(gate(page)).toHaveCount(0);
  await expect(tabs(page)).toBeVisible();

  // The dropped entry is on its own row, saying why — not missing, and
  // not wearing a status from some earlier run.
  const row = page.locator('.ag-row[row-id="broken/raw"]');
  await expect(row).toBeVisible();
  await expect(row.locator('[col-id="status"] .m2-status')).toHaveAttribute(
    "title",
    /Not loaded.*title/,
  );
  // And the banner above the table says how many, so a dropped row
  // cannot be scrolled past unnoticed.
  await expect(page.getByText("entry isn’t in the pipeline")).toBeVisible();
});

test("a file that is not a config blocks the app, and unblocks it live", async ({
  page,
  request,
}) => {
  await page.goto("/sources2");
  await expect(tabs(page)).toBeVisible();
  await expect(gate(page)).toHaveCount(0);

  // Break it under the running app, the way an editor or an agent
  // would. No reload below this line: the gate has to arrive on the
  // `config_changed` the write produces.
  writeConfig("[[steps]]\nid = = \n");

  await expect(gate(page)).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "This config file can’t be read" }),
  ).toBeVisible();
  // The tabs go with it: none of them can do anything now.
  await expect(tabs(page)).toHaveCount(0);
  // And the screen names the problem and where it is, rather than
  // leaving the user to find it.
  await expect(page.locator(".diags")).toContainText("line 2");

  const cfg = await (await request.get("/api/config")).json();
  expect(cfg.parsed_ok).toBe(false);
  expect(cfg.app_ready).toBe(false);
  expect(cfg.diagnostics[0].severity).toBe("fatal");

  // Fix it in the screen's own editor. This is the whole reason the
  // editor is on the blocking screen: the Manage tab that would
  // otherwise hold it is behind the gate.
  await page.locator("#cfg-editor").fill(original);
  await page.getByRole("button", { name: "Save config" }).click();

  // The gate lifts by itself — no reload. This direction is the one
  // that is easy to get wrong, and the one an agent fixing the config
  // depends on.
  await expect(gate(page)).toHaveCount(0);
  await expect(tabs(page)).toBeVisible();
});

test("a config with no unified_index applet blocks too", async ({
  page,
  request,
}) => {
  await page.goto("/sources2");
  await expect(tabs(page)).toBeVisible();

  // Valid TOML, a valid config, and useless: every view in the app is
  // served by the applet this drops. `PUT /api/config` would accept
  // this text, which is why the check cannot live at that door.
  const withoutApplet = original.replace(/\n\[\[applets\]\][\s\S]*$/, "\n");
  expect(withoutApplet).not.toContain("[[applets]]");
  writeConfig(withoutApplet);

  await expect(gate(page)).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "This config declares no Unified Index" }),
  ).toBeVisible();
  await expect(tabs(page)).toHaveCount(0);

  // No diagnostics at all: nothing in the file is wrong. What is wrong
  // is what the file does not say, which is why `app_ready` is its own
  // answer rather than something derivable from the diagnostics list.
  const cfg = await (await request.get("/api/config")).json();
  expect(cfg.parsed_ok).toBe(true);
  expect(cfg.diagnostics).toEqual([]);
  expect(cfg.app_ready).toBe(false);
});
