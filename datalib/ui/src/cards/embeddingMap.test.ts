import { describe, expect, it } from "vitest";
import type { MapPoint } from "@/api";
import {
  OTHER,
  PointGrid,
  categoryOf,
  decodeState,
  encodeState,
  fitView,
  legendFor,
  previewText,
  slotOf,
  toData,
  toScreen,
  zoomAt,
} from "./embeddingMap";

const point = (over: Partial<MapPoint> = {}): MapPoint => ({
  markdown_uuid: "u",
  x: 0,
  y: 0,
  title: "t",
  provider: "Slack",
  source: "Work Slack",
  source_id: "slack",
  kind: "Slack Thread",
  created_at: "2025-03-04T05:06:07Z",
  account: "",
  channel: "",
  ...over,
});

describe("legendFor", () => {
  it("names up to eight categories, largest first", () => {
    const pts = [
      ...Array.from({ length: 3 }, () => point({ provider: "Gmail" })),
      point({ provider: "Slack" }),
    ];
    expect(legendFor(pts, "provider")).toEqual([
      { key: "Gmail", count: 3, slot: 0 },
      { key: "Slack", count: 1, slot: 1 },
    ]);
  });

  /// A ninth hue would be a generated one, and neighbours on the map
  /// could no longer be told apart; past seven the rest share Other.
  it("folds the tail into Other rather than inventing a ninth colour", () => {
    const pts = Array.from({ length: 10 }, (_, i) =>
      Array.from({ length: 10 - i }, () => point({ kind: `k${i}` })),
    ).flat();
    const legend = legendFor(pts, "kind");
    expect(legend).toHaveLength(8);
    expect(legend[7]).toEqual({ key: OTHER, count: 3 + 2 + 1, slot: null });
    expect(slotOf(legend)("k9")).toBeNull();
    expect(slotOf(legend)("k0")).toBe(0);
  });

  it("orders years newest first and names a missing one", () => {
    const pts = [
      point({ created_at: "2023-01-01" }),
      point({ created_at: "2025-01-01" }),
      point({ created_at: "2025-06-01" }),
      point({ created_at: null }),
    ];
    expect(legendFor(pts, "year").map((e) => e.key)).toEqual(["2025", "2023", "(none)"]);
    expect(categoryOf(point({ account: "" }), "account")).toBe("(none)");
  });
});

describe("the view", () => {
  it("fits every point inside the padded box, centred", () => {
    const pts = [
      { x: -10, y: 0 },
      { x: 10, y: 5 },
    ];
    const v = fitView(pts, 400, 300, 20);
    const [ax, ay] = toScreen(v, -10, 0);
    const [bx, by] = toScreen(v, 10, 5);
    expect(ax).toBeCloseTo(20);
    expect(bx).toBeCloseTo(380);
    expect((ay + by) / 2).toBeCloseTo(150);
    expect(by).toBeLessThan(ay); // up is up
  });

  it("zooms about the pointer, which stays over the same point", () => {
    const v = fitView(
      [
        { x: 0, y: 0 },
        { x: 4, y: 4 },
      ],
      200,
      200,
    );
    const before = toData(v, 50, 70);
    const after = toData(zoomAt(v, 50, 70, 3), 50, 70);
    expect(after[0]).toBeCloseTo(before[0]);
    expect(after[1]).toBeCloseTo(before[1]);
  });
});

describe("PointGrid", () => {
  it("finds the nearest point in reach, and none out of it", () => {
    const pts = Array.from({ length: 100 }, (_, i) => ({ x: i % 10, y: Math.floor(i / 10) }));
    const grid = PointGrid.over(pts);
    expect(grid.nearest(3.2, 4.1, 0.5)).toBe(43);
    expect(grid.nearest(3.5, 4.5, 0.1)).toBe(-1);
    expect(grid.nearest(20, 20, 1)).toBe(-1);
  });

  /// A hidden category must not be hovered through: the nearest point
  /// the filter admits wins even when a hidden one is closer.
  it("skips the points the caller leaves out", () => {
    const grid = PointGrid.over([
      { x: 0, y: 0 },
      { x: 0.3, y: 0 },
    ]);
    expect(grid.nearest(0.05, 0, 1)).toBe(0);
    expect(grid.nearest(0.05, 0, 1, (i) => i !== 0)).toBe(1);
  });
});

describe("previewText", () => {
  it("drops markup and folds whitespace", () => {
    const body =
      '<div id="m-1" data-section-uuid="1" class="msg">\n## Hello\n\n**Bold** and [a link](http://x) &amp; more</div>';
    expect(previewText(body)).toBe("Hello Bold and a link & more");
  });

  it("runs a table's cells together and drops its rule row", () => {
    const body = "| When | Thu 3 Oct |\n| --- | --- |\n| Calendar | Team |";
    expect(previewText(body)).toBe("When · Thu 3 Oct · Calendar · Team");
  });

  it("cuts a long body at a word", () => {
    const out = previewText("word ".repeat(200), 50);
    expect(out.endsWith("word…")).toBe(true);
    expect(out.length).toBeLessThanOrEqual(51);
  });
});

describe("state", () => {
  it("round-trips, and a pristine card writes nothing", () => {
    const st = { q: "source_id:slack", by: "kind" as const, sel: "abc" };
    expect(decodeState(encodeState(st))).toEqual(st);
    expect(encodeState({ q: "", by: "provider", sel: null })).toBe("");
    expect(decodeState("by=bogus").by).toBe("provider");
  });
});
