import { describe, expect, it } from "vitest";
import { ref } from "vue";
import { activeOf, type CardEntry, type SearchCard } from "./searchCards";

const card = (id: string): SearchCard => ({
  id,
  query: ref(""),
  settled: ref(""),
  run: () => {},
  openFeedback: () => {},
});
const entry = (id: string, shown: boolean, usedAt: number): CardEntry => ({
  card: card(id),
  shown,
  usedAt,
});

describe("the card the toolbar's box speaks for", () => {
  it("is none with no search card on screen", () => {
    expect(activeOf([])).toBeNull();
    expect(activeOf([entry("hidden", false, 9)])).toBeNull();
  });

  it("is the last used of those on screen, however recently a hidden one was used", () => {
    const active = activeOf([
      entry("older", true, 1),
      entry("hidden", false, 9),
      entry("newer", true, 2),
    ]);
    expect(active?.id).toBe("newer");
  });
});
