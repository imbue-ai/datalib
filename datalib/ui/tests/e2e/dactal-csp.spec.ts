// The CSP on the DACTAL page (issue #138, mitigation 4).
//
// The vendored engine can load code from dactal.org at runtime — dormant
// today, one call away always. `public/dactal/index.html` pins that shut
// with `script-src 'self' 'unsafe-eval'; connect-src 'self'`.
//
// A CSP is exactly the kind of thing that rots silently: it only fails at
// runtime, in a browser, and only on the paths it forbids — which nothing
// exercises. Both halves need guarding, because both regress invisibly:
//
//   * too tight and DACTAL breaks (the engine needs `eval`, the renderer
//     needs inline styles). You'd only notice by opening a DACTAL card.
//   * too loose — someone adds `'unsafe-inline'` to fix an inline
//     `<script>`, say, or moves main.js back into the page — and the
//     remote-load paths quietly open again with nothing failing.

import { test, expect } from "@playwright/test";

const PAGE = "/dactal/index.html?dq=rows%2Fsource";

test("the CSP leaves DACTAL fully working", async ({ page }) => {
  await page.goto(PAGE);

  // The engine ran a query and the renderer drew its table. That covers
  // the three things the policy has to keep allowing: `eval` for the
  // query language, `main.js` + `vendor/*.js` as same-origin scripts,
  // and the inline styles the renderer emits.
  await expect(page.locator("#queryoutput table").first()).toBeVisible({
    timeout: 20_000,
  });
  await expect(page.locator("#status")).toContainText(/results? for/);
  await expect(page.locator("#queryoutput .err")).toHaveCount(0);

  // `/api/search` is same-origin, so `connect-src 'self'` must not have
  // blocked the working-set load.
  await expect(page.locator("#status")).not.toContainText(
    "could not reach /api/search",
  );
});

test("the CSP blocks the dactal.org paths and keeps eval", async ({ page }) => {
  await page.goto(PAGE);
  await expect(page.locator("#queryoutput table").first()).toBeVisible({
    timeout: 20_000,
  });

  const result = await page.evaluate(async () => {
    const violations: string[] = [];
    document.addEventListener("securitypolicyviolation", (e) =>
      violations.push(e.violatedDirective),
    );
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
    await new Promise((r) => setTimeout(r, 300));
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
