import { describe, expect, it } from "vitest";
import { revealScrollLeft } from "@/views/millerReveal";

// The row is 800 wide and currently scrolled to 0 unless a case says
// otherwise; columns are 640 wide (the default) unless a case says
// otherwise.
const view = (start = 0, width = 800) => ({ start, width });
const col = (start: number, width = 640) => ({ start, width });

describe("revealScrollLeft", () => {
  it("leaves a column that is already fully visible where it is", () => {
    expect(revealScrollLeft(view(), col(0))).toBe(0);
    expect(revealScrollLeft(view(500), col(600))).toBe(500);
  });

  it("scrolls just far enough to show a column past the right edge", () => {
    // The second column of a default stack: its right edge lands on
    // the row's right edge, keeping as much of the first as fits.
    expect(revealScrollLeft(view(), col(640))).toBe(640 + 640 - 800);
  });

  it("scrolls back to the left edge of a column past the left edge", () => {
    expect(revealScrollLeft(view(1000), col(0))).toBe(0);
    expect(revealScrollLeft(view(1000), col(640))).toBe(640);
  });

  it("favours the left edge of a column wider than the row", () => {
    expect(revealScrollLeft(view(), col(640, 1200))).toBe(640);
    // Even when it is already partly on screen — the scroll that
    // would show its right edge would hide its header.
    expect(revealScrollLeft(view(700), col(640, 1200))).toBe(640);
  });
});
