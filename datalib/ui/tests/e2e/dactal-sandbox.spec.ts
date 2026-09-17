// The DACTAL card runs its page in a sandboxed iframe (issue #146) and
// the page keeps its own CSP against the vendored engine's dactal.org
// paths (issue #138, mitigation 4). Both are checked from inside the
// running app, because the sandbox is the property under test: the page
// on its own no longer does anything.

import { test, expect, type Frame, type Page } from "@playwright/test";

declare const process: { env: Record<string, string | undefined> };
const TOKEN = process.env.DATALIB_TOKEN;

const CARD = "/" + encodeURIComponent('dactalView({ q: "rows/source" })');

// Authenticate the way a browser does — `?token=` once, then the cookie
// — rather than through the suite's ambient `Authorization` header.
// Playwright puts that header on every request the page makes, and on
// a cross-origin one (the sandboxed frame loading its module script
// from origin `null`) it forces a CORS preflight the server has no
// answer for. Chromium then drops the script; WebKit does not send the
// header on that fetch. Neither is what a user's browser does.
test.use({ extraHTTPHeaders: {} });

/** The DACTAL frame once it has drawn a table — the engine ran a query
 *  over rows the host handed it. */
async function loadedFrame(page: Page): Promise<Frame> {
  expect(TOKEN, "playwright.config.ts should have pinned DATALIB_TOKEN").toBeTruthy();
  await page.goto(`${CARD}?token=${TOKEN}`);
  const frame = page.frameLocator('iframe[src="/dactal/index.html"]');
  await expect(frame.locator("#queryoutput table").first()).toBeVisible({
    timeout: 20_000,
  });
  await expect(frame.locator("#status")).toContainText(/results? for/);
  await expect(frame.locator("#queryoutput .err")).toHaveCount(0);
  const f = page
    .frames()
    .find((f) => f.url().endsWith("/dactal/index.html"));
  expect(f, "the DACTAL frame should be in the frame tree").toBeTruthy();
  return f!;
}

test("the sandboxed frame gets its rows from the host and nothing else", async ({
  page,
}) => {
  const frame = await loadedFrame(page);

  // The frame element carries the sandbox, without `allow-same-origin`
  // — with it, the frame could reach up and remove its own sandbox.
  const sandbox = await page
    .locator('iframe[src="/dactal/index.html"]')
    .getAttribute("sandbox");
  expect(sandbox).toBe("allow-scripts");

  const inside = await frame.evaluate(async () => {
    // An opaque origin serializes as "null"; a same-origin frame would
    // say http://127.0.0.1:<port>.
    const origin = window.origin;
    // No session, no API: the fetch never leaves the page (the page's
    // own `connect-src 'none'`), and even without that policy it would
    // be a cross-origin request from an origin the server never
    // answers.
    let apiReached = false;
    try {
      const r = await fetch("/applet/unified_index/search?q=&limit=1");
      apiReached = r.ok;
    } catch {
      apiReached = false;
    }
    // The renderer's store is the in-memory stand-in, not IndexedDB —
    // which an opaque origin does not have.
    const db = (window as unknown as { dactaldb: { store?: unknown } }).dactaldb;
    return { origin, apiReached, inMemoryStore: db.store instanceof Map };
  });
  expect(inside.origin, "the frame must run in an opaque origin").toBe("null");
  expect(inside.apiReached, "the frame must not reach /applet/*").toBe(false);
  expect(inside.inMemoryStore).toBe(true);
});

test("the page opened on its own does nothing", async ({ page }) => {
  // Reachable without a session (the statics are public) — and inert:
  // no host, no rows, no query evaluated from the URL.
  const resp = await page.goto("/dactal/index.html?dq=rows%2Fsource");
  expect(resp?.status()).toBe(200);
  expect(resp?.headers()["content-security-policy"]).toMatch(/sandbox/);
  await expect(page.locator("#status")).toContainText(
    "runs inside a Datalib card",
  );
  await expect(page.locator("#queryoutput table")).toHaveCount(0);
  expect(await page.evaluate(() => window.origin)).toBe("null");
});

test("the CSP blocks the dactal.org paths and keeps eval", async ({ page }) => {
  const frame = await loadedFrame(page);

  const result = await frame.evaluate(async () => {
    // The two directives the assertions below are about. Collected as
    // they fire, and — the part that matters — *waited on* rather than
    // slept through: the listener resolves `settled` as soon as both
    // have been seen.
    const want = new Set(["script-src-elem", "connect-src"]);
    const violations: string[] = [];
    let seenBoth = () => {};
    const settled = new Promise<void>((resolve) => {
      seenBoth = resolve;
    });
    document.addEventListener("securitypolicyviolation", (e) => {
      violations.push(e.violatedDirective);
      want.delete(e.violatedDirective);
      if (want.size === 0) seenBoth();
    });
    const w = window as unknown as {
      loadscript: (n: string) => Promise<unknown>;
      loadscript_namespaced: (n: string, ns: string) => Promise<unknown>;
    };
    const blocked = async (fn: () => Promise<unknown>) => {
      try {
        await fn();
        return false;
      } catch {
        return true;
      }
    };
    // Both remote-loading shapes in the vendored engine: a <script src>
    // injection, and a fetch()-then-new Function().
    const scriptTag = await blocked(() => w.loadscript("dactal_assist.js"));
    const fetched = await blocked(() =>
      w.loadscript_namespaced("anything.js", "ns"),
    );
    // …and `eval`, which must still work — it is load-bearing for the
    // query language, which is why 'unsafe-eval' stays in the policy.
    let evalWorks = false;
    try {
      evalWorks = eval("1 + 1") === 2;
    } catch {
      evalWorks = false;
    }
    // A CSP violation report is dispatched asynchronously, after the
    // load it blocked has already rejected — so the `blocked()` calls
    // above can finish before the events arrive, and something has to
    // wait for them. The race is a *deadline*, not a wait: it only
    // expires when the events never come, and then the assertion below
    // says which one was missing.
    await Promise.race([settled, new Promise((r) => setTimeout(r, 5_000))]);
    return { scriptTag, fetched, evalWorks, violations };
  });

  expect(result.scriptTag, "loadscript() must not reach dactal.org").toBe(true);
  expect(
    result.fetched,
    "loadscript_namespaced() must not fetch from dactal.org",
  ).toBe(true);
  expect(result.evalWorks, "'unsafe-eval' must stay — the engine needs it").toBe(
    true,
  );
  // The failures have to come from the policy, not from the network
  // happening to be down in CI.
  expect(result.violations).toEqual(
    expect.arrayContaining(["script-src-elem", "connect-src"]),
  );
});
