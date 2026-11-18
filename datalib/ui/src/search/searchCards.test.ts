import { describe, expect, it } from "vitest";
import { ref } from "vue";
import { activeOf, draftAfterMove, type CardEntry, type SearchCard } from "./searchCards";

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

/// A search card landing after the box was up wrote its query over what had
/// been typed, which lost a quick person's text and failed specs at random.
describe("what the toolbar's box holds when its card moves", () => {
  const move = { draft: "warp", shown: "", query: "is:document", hadCard: false, inBox: true };

  it("keeps what was typed when a card arrives", () => {
    expect(draftAfterMove(move)).toBe("warp");
  });

  it("keeps it when a card arrives after the keyboard has left the box", () => {
    expect(draftAfterMove({ ...move, inBox: false })).toBe("warp");
  });

  it("keeps what is still being typed when the query or the card moves", () => {
    expect(draftAfterMove({ ...move, shown: "warp core", hadCard: true })).toBe("warp");
    expect(draftAfterMove({ ...move, shown: "warp core", hadCard: true, query: "" })).toBe("warp");
  });

  it("follows the card once the keyboard has left, as on a click on another tab", () => {
    const left = { ...move, shown: "warp core", hadCard: true, inBox: false };
    expect(draftAfterMove(left)).toBe("is:document");
    expect(draftAfterMove({ ...left, query: "" })).toBe("");
  });

  it("follows the card when nothing was typed, keyboard in the box or not", () => {
    const untouched = { ...move, draft: "", shown: "" };
    expect(draftAfterMove(untouched)).toBe("is:document");
    expect(draftAfterMove({ ...untouched, inBox: false })).toBe("is:document");
    expect(draftAfterMove({ ...untouched, hadCard: true })).toBe("is:document");
  });
});
