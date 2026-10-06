// Every source against every way things go wrong, through the real
// latchkey, `datalib-step probe` and wizard: each failure must come
// back as its kind, said in one short line, and quickly — a check is
// something a person is waiting on.
import { cloudflareChallenge, malformed, rateLimited, rejected, serverError } from "./faults.mjs";
import { TNG } from "./fake_sites.mjs";
import { expect, expectGlanceable, pickTile, test, TILE, wizard, type World } from "./world";

/// What a fault should be called, and how long a check may take to
/// say so: never the two minutes a sync's retries would take.
const SAID_WITHIN = { timeout: 20_000 };

type Source = {
  tile: string;
  host: string;
  /// What a person set up on the command line before opening the app.
  setUp: (world: World) => void;
};

const SOURCES: Record<string, Source> = {
  Slack: {
    tile: TILE.slack,
    host: "slack.com",
    setUp: (w) =>
      w.latchkey("auth", "set", "slack", "-H", `Authorization: Bearer ${TNG.slackToken}`),
  },
  Claude: {
    tile: TILE.claude,
    host: "claude.ai",
    setUp: (w) => {
      w.latchkey(
        "services",
        "register",
        "claude-ai",
        "--base-api-url=https://claude.ai/",
        "--login-url=https://claude.ai/login",
        "--login-flow=cookie-capture",
        '--login-flow-params={"cookieKeys":["sessionKey"]}',
      );
      w.latchkey("auth", "set", "claude-ai", "-H", `Cookie: sessionKey=${TNG.claudeSessionKey}`);
    },
  },
  ChatGPT: {
    tile: TILE.chatgpt,
    host: "chatgpt.com",
    setUp: (w) => {
      w.latchkey(
        "services",
        "register",
        "chatgpt",
        "--base-api-url=https://chatgpt.com/",
        "--login-url=https://chatgpt.com/auth/login",
        "--login-flow=token-capture",
        '--login-flow-params={"tokenUrl":"https://chatgpt.com/api/auth/session","tokenField":"accessToken"}',
      );
      w.latchkey("auth", "set", "chatgpt", "-H", `Authorization: Bearer ${TNG.chatgptAccessToken}`);
    },
  },
};

const SITE_FAULTS = [
  { name: "the credential is refused", fault: rejected, issue: "rejected" },
  { name: "Cloudflare blocks the request", fault: cloudflareChallenge, issue: "blocked" },
  { name: "the service rate-limits", fault: rateLimited, issue: "rate_limited" },
  { name: "the service is down", fault: serverError, issue: "service_error" },
  { name: "the answer is not JSON", fault: malformed, issue: "unexpected_response" },
] as const;

async function checkFails(page: import("@playwright/test").Page, issue: string) {
  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  const failed = wizard(page).locator(".wiz-probe-failed");
  await expect(failed).toHaveAttribute("data-issue", issue, SAID_WITHIN);
  expectGlanceable(await failed.locator(".issue-headline").textContent(), "the headline");
}

for (const [name, source] of Object.entries(SOURCES)) {
  test.describe(name, () => {
    for (const { name: what, fault, issue } of SITE_FAULTS) {
      test(`${what}: said as ${issue}`, async ({ page, world, internet }) => {
        source.setUp(world);
        internet.override(source.host, fault);
        await pickTile(page, source.tile);
        await checkFails(page, issue);
      });
    }

    test("nothing stored yet: said as no_credential", async ({ page }) => {
      await pickTile(page, source.tile);
      await checkFails(page, "no_credential");
    });

    test.describe("with no network", () => {
      test.use({ worldOptions: { offline: true } });
      test("said as unreachable", async ({ page, world }) => {
        source.setUp(world);
        await pickTile(page, source.tile);
        await checkFails(page, "unreachable");
      });
    });

    test.describe("with the gateway down", () => {
      test.use({ worldOptions: { gatewayDown: true } });
      test("said as gateway_unreachable", async ({ page }) => {
        await pickTile(page, source.tile);
        // latchkey itself cannot be asked, and the section says so…
        await expect(wizard(page).locator(".wiz-accounts-failed")).toHaveAttribute(
          "data-issue",
          "gateway_unreachable",
        );
        // …and so does the check.
        await checkFails(page, "gateway_unreachable");
      });
    });
  });
}
