import { describe, expect, it } from "vitest";
import { ref } from "vue";
import { activeOf, draftAfter, type CardEntry, type SearchCard } from "./searchCards";

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

describe("what the toolbar's box holds", () => {
  it("is the query of the card it speaks for, and nothing with no card", () => {
    expect(draftAfter("old", false, "warp")).toBe("warp");
    expect(draftAfter("old", false, undefined)).toBe("");
  });

  /// A card comes on screen a moment after the page does, and once replaced
  /// what had been typed by then.
  it("keeps what is being typed when a card comes on screen", () => {
    expect(draftAfter("source:", true, "is:document")).toBe("source:");
  });

  it("follows the card when the caret is there but nothing is typed", () => {
    expect(draftAfter("", true, "is:document")).toBe("is:document");
  });
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
