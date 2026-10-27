import { test, expect } from "@playwright/test";
import { EVERY_ROW } from "./grid-helpers";

// The status bar's clock sets the zone and format every date is drawn in
// (docs/dev/chips.md § "A date"). The search grid's date columns are date
// chips, and a grid draws its cells itself, so it is the place a change of
// zone has to reach without a reload.

/** `YYYY-MM-DD HH:MM` on the zone's clock, as the ISO format writes it. */
function isoIn(datetime: string, zone: string): string {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: zone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hourCycle: "h23",
  }).formatToParts(Date.parse(datetime));
  const p = (t: string) => parts.find((x) => x.type === t)!.value;
  return `${p("year")}-${p("month")}-${p("day")} ${p("hour")}:${p("minute")}`;
}

test("a grid's date chips redraw in the zone and format the clock picks", async ({ page }) => {
  await page.goto(EVERY_ROW);
  const chips = page.locator(".grid-box .slick-cell time.date-chip[datetime]");
  await expect(chips.first()).toBeVisible({ timeout: 10_000 });
  const datetime = (await chips.first().getAttribute("datetime"))!;
  const cell = page.locator(`.grid-box .slick-cell time.date-chip[datetime="${datetime}"]`).first();

  const clock = page.getByTestId("time-view-clock");
  await clock.click();
  await page.getByRole("menuitemradio", { name: /^2026-04-15/ }).click();
  await expect(cell).toHaveText(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/);

  // A right-click opens the same menu; the zone search has the focus.
  await clock.click({ button: "right" });
  await page.getByLabel("Find a time zone").fill("tokyo");
  await page.keyboard.press("Enter");
  await expect(clock).toContainText("Tokyo");
  await expect(cell).toHaveText(isoIn(datetime, "Asia/Tokyo"));

  await clock.click();
  await page.getByRole("menuitemradio", { name: /^UTC/ }).click();
  await expect(cell).toHaveText(isoIn(datetime, "UTC"));
});
