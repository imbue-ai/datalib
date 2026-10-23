import { describe, expect, it } from "vitest";

import { isolated, seriesColor, toggled, tokenOf, withAlpha } from "./plot";

describe("the legend", () => {
  it("turns one line on or off with a click", () => {
    expect(toggled([true, true, true], 1)).toEqual([true, false, true]);
    expect(toggled([true, false, true], 1)).toEqual([true, true, true]);
  });

  /// Plotly's double-click, which the old chart pages had: alone, then
  /// every line again.
  it("shows one line alone on a double-click, and all again on the next", () => {
    const alone = isolated([true, true, false], 2);
    expect(alone).toEqual([false, false, true]);
    expect(isolated(alone, 2)).toEqual([true, true, true]);
    expect(isolated(alone, 0)).toEqual([true, false, false]);
  });
});

describe("colours", () => {
  it("number the palette from one and wrap after six", () => {
    expect(seriesColor(0)).toBe("var(--viz-series-1)");
    expect(seriesColor(6)).toBe("var(--viz-series-1)");
    expect(tokenOf(seriesColor(2))).toBe("--viz-series-3");
    expect(tokenOf("--datalib-bg")).toBe("--datalib-bg");
    expect(tokenOf("#2a78d6")).toBeNull();
  });

  /// A canvas cannot take `color-mix()` or `var()`, so a band's fill is
  /// made from the resolved colour.
  it("fade a resolved colour, and refuse one a canvas cannot", () => {
    expect(withAlpha("#2a78d6", 0.14)).toBe("rgba(42, 120, 214, 0.14)");
    expect(withAlpha("#fff", 0.5)).toBe("rgba(255, 255, 255, 0.5)");
    expect(withAlpha("rgb(1, 2, 3)", 0.2)).toBe("rgba(1, 2, 3, 0.2)");
    expect(() => withAlpha("", 0.2)).toThrow();
  });
});
