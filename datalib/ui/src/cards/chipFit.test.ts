import { describe, expect, it } from "vitest";
import { elideMiddle, fitChips, type ChipMeasure } from "./chipFit";

/// One pixel a character and no chrome, so a width is a character count.
const PLAIN: ChipMeasure = { charPx: 1, chipPx: 0, gapPx: 0, minChars: 4 };

describe("elideMiddle", () => {
  it("keeps the start and the end of a label too long for its share", () => {
    expect(elideMiddle("Jean-Luc Picard", 11)).toBe("Jean-…icard");
    expect(elideMiddle("Jean-Luc Picard", 15)).toBe("Jean-Luc Picard");
    expect(elideMiddle("Data", 1)).toBe("…");
  });

  it("counts characters, not UTF-16 units", () => {
    expect(elideMiddle("Beverly ☕ Crusher", 7)).toBe("Bev…her");
  });
});

describe("fitChips", () => {
  it("draws every label whole when they fit", () => {
    expect(fitChips(["Picard", "Riker"], 11, PLAIN)).toEqual({
      shown: ["Picard", "Riker"],
      folded: 0,
    });
  });

  it("shares the width, a short label leaving the rest to a long one", () => {
    expect(fitChips(["Q", "Jean-Luc Picard"], 10, PLAIN)).toEqual({
      shown: ["Q", "Jean…card"],
      folded: 0,
    });
  });

  it("folds the trailing chips into a count before cutting below the floor", () => {
    const crew = ["Picard", "Riker", "Troi", "Crusher", "La Forge"];
    // Five chips would get two characters each and four would get three;
    // three keep four, beside a count as wide as two characters.
    expect(fitChips(crew, 14, PLAIN)).toEqual({ shown: ["Pi…d", "Ri…r", "Troi"], folded: 2 });
  });

  it("always draws the first chip, however narrow the cell", () => {
    expect(fitChips(["Jean-Luc Picard", "Riker"], 2, PLAIN)).toEqual({
      shown: ["Je…d"],
      folded: 1,
    });
  });

  it("draws nothing for nobody", () => {
    expect(fitChips([], 100, PLAIN)).toEqual({ shown: [], folded: 0 });
  });
});
