// Garmin reaches latchkey through a plugin datalib ships, so the first
// sign-in also installs it. These run the real plugin: its bearer
// exchange and its profile request both reach the fake Garmin.
import { TNG } from "./fake_sites.mjs";
import { expect, pickTile, subcommand, test, TILE, wizard } from "./world";
import { reviewToml } from "../e2e/wizard-helpers";

const garthFolder = (world: { writeFile: (rel: string, text: string) => string }) => {
  const file = world.writeFile(
    "garth/oauth1_token.json",
    JSON.stringify({
      oauth_token: TNG.garminOauthToken,
      oauth_token_secret: TNG.garminOauthSecret,
      domain: "garmin.com",
    }),
  );
  return file.replace(/\/oauth1_token\.json$/, "");
};

async function importTokens(page: import("@playwright/test").Page, folder: string) {
  await wizard(page).getByRole("tab", { name: "Import tokens" }).click();
  await wizard(page).getByRole("combobox", { name: "Garmin account" }).fill("picard");
  const form = wizard(page).locator(".wiz-paste");
  await form.getByLabel("Token folder").fill(folder);
  await form.getByRole("button", { name: "Store in latchkey" }).click();
  await expect(form).toContainText("Stored in latchkey.");
}

test("importing a garth folder installs the plugin, then Check connection reaches Garmin", async ({
  page,
  world,
  internet,
}) => {
  await pickTile(page, TILE.garmin);
  // Before anything is written, the dialog says where the plugin goes.
  await expect(wizard(page).locator(".wiz-plugin-note")).toContainText(
    `${world.pluginsDir}/garmin`,
  );
  expect(world.readFile(`${world.pluginsDir}/garmin/package.json`)).toBeNull();

  await importTokens(page, garthFolder(world));
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText("picard@enterprise.test");

  expect(world.readFile(`${world.pluginsDir}/garmin/.datalib-installed`)).toMatch(/^[0-9a-f]{40}$/);
  expect(world.latchkeyRuns().map(subcommand)).toContain("auth set-nocurl");
  // The plugin minted the bearer from the imported token and sent it.
  expect(
    internet.to("connectapi.garmin.com", "/oauth-service/oauth/exchange/user/2.0"),
  ).toHaveLength(1);
  for (const r of internet.to("connectapi.garmin.com", "/userprofile-service/socialProfile")) {
    expect(r.headers.authorization).toBe(`Bearer ${TNG.garminBearer}`);
  }
  // Once latchkey knows the service the note is gone, and the account
  // is one it holds.
  await expect(wizard(page).locator(".wiz-plugin-note")).toHaveCount(0);
  await expect(await reviewToml(page)).toContainText('account = "picard"');
});

/// Guards a person's own copy of the plugin: a sign-in from the wizard
/// must neither overwrite it nor stamp it as datalib's.
test("a Garmin plugin installed by hand is used and left exactly as it was", async ({
  page,
  world,
}) => {
  const own = world.installOwnGarminPlugin();
  world.writeFile(`latchkey/plugins/garmin/MINE`, "a clone I keep up to date");
  const before = world.readFile(`${own}/dist/garmin.js`);

  await pickTile(page, TILE.garmin);
  await expect(wizard(page).locator(".wiz-plugin-note")).toHaveCount(0);
  await importTokens(page, garthFolder(world));
  await expect(wizard(page).locator(".wiz-probe-ok")).toContainText("picard@enterprise.test");

  expect(world.readFile(`${own}/.datalib-installed`)).toBeNull();
  expect(world.readFile(`${own}/MINE`)).toBe("a clone I keep up to date");
  expect(world.readFile(`${own}/dist/garmin.js`)).toBe(before);
});
