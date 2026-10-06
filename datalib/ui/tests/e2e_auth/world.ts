// The fixtures every sign-in spec uses: a fake internet and a world of
// its own per test (harness.mjs), and the wizard steps they share.
import { test as base, expect, type Page, type TestInfo } from "@playwright/test";
import { startFakeInternet, startGateway, startWorld } from "./harness.mjs";

export type Internet = Awaited<ReturnType<typeof startFakeInternet>>;
export type World = Awaited<ReturnType<typeof startWorld>>;
/// `startWorld`'s options (harness.mjs), except that a gateway is asked
/// for by what to seed its store with (`latchkey` argv lists) and
/// started here.
export type WorldOptions = {
  browser?: "found" | "download" | "none";
  runtime?: boolean;
  offline?: boolean;
  gatewaySeed?: string[][];
  /// Configure the gateway, then stop it before the test runs.
  gatewayDown?: boolean;
};

export const test = base.extend<{ internet: Internet; world: World; worldOptions: WorldOptions }>({
  worldOptions: [{}, { option: true }],
  internet: async ({}, use) => {
    const internet = await startFakeInternet();
    await use(internet);
    await internet.close();
  },
  world: async ({ internet, worldOptions }, use, testInfo) => {
    const { gatewaySeed, gatewayDown, ...options } = worldOptions;
    const gateway =
      gatewaySeed || gatewayDown ? await startGateway(internet, { seed: gatewaySeed ?? [] }) : null;
    const world = await startWorld(internet, { ...options, gateway });
    if (gatewayDown) await gateway?.stop();
    await use(world);
    await world.stop();
    await gateway?.stop();
    await attachWhatHappened(testInfo, world, internet);
    if (testInfo.status === testInfo.expectedStatus) world.cleanup();
  },
  baseURL: async ({ world }, use) => use(world.origin),
});

export { expect };

/// The backend log, every latchkey run and every request the fake
/// internet saw — the three things a red run is read from.
export async function attachWhatHappened(testInfo: TestInfo, world: World, internet: Internet) {
  if (testInfo.status === testInfo.expectedStatus) return;
  await testInfo.attach("latchkey runs", {
    body: world
      .latchkeyRuns()
      .map((r) => JSON.stringify(r))
      .join("\n"),
    contentType: "text/plain",
  });
  await testInfo.attach("fake internet", {
    body: internet.requests.map((r) => `${r.method} ${r.host}${r.path}${r.query}`).join("\n"),
    contentType: "text/plain",
  });
  await testInfo.attach("world dir", { body: world.dir, contentType: "text/plain" });
}

export const wizard = (page: Page) => page.getByRole("dialog");

/// One field of the wizard's form, by its caption.
export const wizField = (page: Page, caption: string) =>
  wizard(page).locator(`.wiz-field:has(> .wiz-label:text-is("${caption}"))`);

/// Open "Add source" and pick the tile whose blurb is `blurb`.
export async function pickTile(page: Page, blurb: string) {
  await page.goto("/data_sources");
  await page.getByRole("button", { name: "Add source" }).click();
  await wizard(page).locator(".wiz-tile", { hasText: blurb }).click();
}

export const TILE = {
  slack: "Mirror channels and DMs from one Slack workspace.",
  claude: "Mirror your claude.ai conversations",
  chatgpt: "Mirror your ChatGPT conversations.",
  garmin: "Weight, sleep, heart rate, activities and FIT files from Garmin Connect.",
};

/// The latchkey subcommand of one logged run: `services info`,
/// `auth browser`, … — global flags such as `--account` dropped.
export function subcommand(run: { args: string[] }): string {
  const words: string[] = [];
  for (let i = 0; i < run.args.length && words.length < 2; i++) {
    const arg = run.args[i];
    if (!arg.startsWith("-")) words.push(arg);
    else if (words.length > 0) break;
    else if (arg === "--account") i++;
  }
  return words.join(" ");
}

/// What a person can take in at a glance: one line, short, and nothing
/// that reads like a log record or a stack.
export function expectGlanceable(text: string | null, what: string) {
  const t = (text ?? "").trim();
  expect(t, `${what} should say something`).not.toBe("");
  expect(t.length, `${what} is ${t.length} characters: ${t}`).toBeLessThanOrEqual(200);
  expect(t, `${what} should be one line`).not.toContain("\n");
  expect(t, `${what} should not carry a log record`).not.toMatch(/"level"|"timestamp"|^\{/);
  expect(t, `${what} should not carry a stack`).not.toMatch(/\n\s+at |Traceback/);
}

/// A request that left through the router's impersonating curl: the
/// router drops the caller's User-Agent and curl-impersonate sends
/// Chrome's. A plain curl would say `curl/…`, and Cloudflare would 403 it.
export function expectImpersonated(request: { host: string; headers: Record<string, unknown> }) {
  expect(String(request.headers["user-agent"]), `${request.host} via curl-impersonate`).toMatch(
    /Chrome\//,
  );
}
