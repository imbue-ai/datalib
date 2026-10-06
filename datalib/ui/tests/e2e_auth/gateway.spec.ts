// Under a latchkey gateway — how Minds runs datalib — the credentials
// live in the gateway's store, not the backend's. The backend's latchkey
// has no key and no store; everything it asks goes to the gateway.
import { TNG } from "./fake_sites.mjs";
import { expect, pickTile, test, TILE, wizard } from "./world";

test.use({
  worldOptions: {
    gatewaySeed: [["auth", "set", "slack", "-H", `Authorization: Bearer ${TNG.slackToken}`]],
  },
});

test("Check connection goes through the gateway, and nothing offers to sign in here", async ({
  page,
  world,
}) => {
  await pickTile(page, TILE.slack);

  await expect(wizard(page)).toContainText(`held by a latchkey gateway (${world.gatewayUrl})`);
  await expect(wizard(page).getByRole("tab", { name: "Paste a key" })).toHaveCount(0);
  await expect(wizard(page).getByRole("button", { name: "Sign in with browser" })).toHaveCount(0);

  await wizard(page).getByRole("button", { name: "Check connection" }).click();
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText("Connected to");
  // Every latchkey the backend ran was a gateway client.
  expect(world.latchkeyRuns().every((r) => r.gateway)).toBe(true);
});
