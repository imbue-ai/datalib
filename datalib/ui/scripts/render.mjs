// Headless preview of a datalib card, for a coding agent that
// can't see the user's browser. Loads a card URL in headless Chromium,
// screenshots it, and reports console errors plus any in-card "card
// error:" text the host renders when a card fails to compile/run.
//
//   node datalib/ui/scripts/render.mjs '<cardUrl>' --out /tmp/card.png \
//     --token "$(cat <data_root>/system/api-token)"
//
// Prints a JSON report to stdout; writes the screenshot to --out
// (default ./card.png). Exit code is non-zero if anything errored, so a
// scripted agent loop can branch on it.
//
// The server requires its API token on every route (see
// datalib/backend/http/src/auth.rs). Pass it with --token or
// $DATALIB_TOKEN; it's sent as a header on every request this page
// makes, so it never lands in the URL. Without it the page comes back
// 401 and the report says so.
//
// Requires the source-tree node_modules (playwright). From a checkout:
//   (cd datalib/ui && pnpm install)
import { chromium } from "playwright";

const argv = process.argv.slice(2);
const url = argv.find((a) => !a.startsWith("--"));
const outIdx = argv.indexOf("--out");
const out = outIdx >= 0 ? argv[outIdx + 1] : "card.png";
const waitIdx = argv.indexOf("--wait");
const settleMs = waitIdx >= 0 ? Number(argv[waitIdx + 1]) : 600;
const tokenIdx = argv.indexOf("--token");
const token = tokenIdx >= 0 ? argv[tokenIdx + 1] : process.env.DATALIB_TOKEN;

if (!url) {
  console.error(
    "usage: render.mjs <cardUrl> [--out file.png] [--wait ms] [--token tok]",
  );
  process.exit(2);
}
if (!token) {
  console.error(
    "no API token: pass --token or set $DATALIB_TOKEN.\n" +
      "The running server's token is in <data_root>/system/api-token.",
  );
  process.exit(2);
}

const browser = await chromium.launch();
const page = await browser.newPage({
  viewport: { width: 960, height: 900 },
  extraHTTPHeaders: { authorization: `Bearer ${token}` },
});

const consoleErrors = [];
page.on("console", (m) => {
  if (m.type() === "error") consoleErrors.push(m.text());
});
page.on("pageerror", (e) => consoleErrors.push(String(e)));

const response = await page.goto(url, {
  waitUntil: "networkidle",
  timeout: 30_000,
});
if (response && response.status() === 401) {
  console.error(
    `${url} → 401: the API token is wrong or stale (it changes every time ` +
      "the server restarts). Re-read it from " +
      "<data_root>/system/api-token.",
  );
  await browser.close();
  process.exit(1);
}
await page.waitForTimeout(settleMs);

// The host renders compile/run failures as "card error: …" text inside
// each card's shadow root. Surface that — it's the single most useful
// signal for an agent iterating on a factory.
const cardErrors = await page.evaluate(() => {
  const found = [];
  for (const host of document.querySelectorAll(".shadow-card-host")) {
    const root = host.shadowRoot;
    if (!root) continue;
    const text = (root.textContent || "").trim();
    if (text.startsWith("card error:")) found.push(text.slice(0, 800));
  }
  return found;
});

await page.screenshot({ path: out });
await browser.close();

const report = { url, out, consoleErrors, cardErrors };
console.log(JSON.stringify(report, null, 2));
process.exit(consoleErrors.length || cardErrors.length ? 1 : 0);
