// What happens before the person has asked for anything. On a mac,
// every latchkey run outside a gateway reads the keychain at startup —
// `services info` included — so what runs here is kept to the one read
// that fills the dialog.
import { TNG } from "./fake_sites.mjs";
import { expect, expectGlanceable, pickTile, subcommand, test, TILE, wizard } from "./world";

/// Picking a tile reads latchkey to fill the dialog — which sign-in
/// ways there are, which accounts it holds — and nothing more. Storing,
/// registering, opening a browser or reaching the service waits for a
/// click.
test("picking a tile only reads", async ({ page, world, internet }) => {
  await pickTile(page, TILE.slack);
  await expect(wizard(page).getByRole("button", { name: "Check connection" })).toBeVisible();
  await expect(wizard(page).getByRole("tab", { name: "Paste a key" })).toBeVisible();
  expect(world.latchkeyRuns().map(subcommand)).toEqual(["services info"]);
  expect(internet.to("slack.com")).toHaveLength(0);
});

/// latchkey's answer is what the Connection section is built from, so
/// while it is asked the section says so rather than sitting half
/// empty. A stored credential makes `services info` check it with
/// Slack; holding that check keeps the question open.
test("while latchkey is asked, the section says so", async ({ page, world, internet }) => {
  world.latchkey("auth", "set", "slack", "-H", `Authorization: Bearer ${TNG.slackToken}`);
  const release = internet.hold(
    (r: { host: string; path: string }) => r.host === "slack.com" && r.path === "/api/auth.test",
  );
  await pickTile(page, TILE.slack);
  const asking = wizard(page).getByText("Asking latchkey how you can sign in…");
  await expect(asking).toBeVisible();
  release();
  await expect(wizard(page).getByRole("tab", { name: "Paste a key" })).toBeVisible();
  await expect(asking).toHaveCount(0);
});

test.describe("with no bundled runtime", () => {
  test.use({ worldOptions: { runtime: false } });

  /// Slack has no account picker, which is where this note used to
  /// live — so it showed no way to sign in and said nothing at all.
  test("the Connection section says latchkey is missing, in a sentence", async ({ page }) => {
    await pickTile(page, TILE.slack);
    const note = wizard(page).locator(".wiz-accounts-failed");
    await expect(note).toHaveAttribute("data-issue", "no_runtime");
    expectGlanceable(await note.locator(".issue-headline").textContent(), "the note");
  });
});
