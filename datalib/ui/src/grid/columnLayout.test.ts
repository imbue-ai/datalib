import { describe, expect, it } from "vitest";
import { carryLayout } from "./columnLayout";

const col = (id: string, width = 100) => ({ id, width, name: id.toUpperCase() });

describe("carryLayout", () => {
  it("keeps a dragged width and a moved column across a re-declaration", () => {
    // The Manage grid re-declares its columns on every poll; a drag made
    // between two polls used to snap back on the second.
    const fresh = [col("a"), col("b"), col("c")];
    const current = [
      { id: "c", width: 100 },
      { id: "a", width: 55 },
      { id: "b", width: 100 },
    ];
    expect(carryLayout(fresh, current).map((c) => [c.id, c.width])).toEqual([
      ["c", 100],
      ["a", 55],
      ["b", 100],
    ]);
  });

  it("takes everything else from the fresh definition", () => {
    const [a] = carryLayout([{ ...col("a"), name: "Renamed" }], [{ id: "a", width: 55 }]);
    expect(a.name).toBe("Renamed");
  });

  it("puts a new column after the carried ones, at its declared width", () => {
    const fresh = [col("new", 90), col("a"), col("b")];
    const current = [
      { id: "b", width: 70 },
      { id: "a", width: 60 },
    ];
    expect(carryLayout(fresh, current).map((c) => [c.id, c.width])).toEqual([
      ["b", 70],
      ["a", 60],
      ["new", 90],
    ]);
  });

  it("drops a column the fresh definitions no longer have", () => {
    const out = carryLayout([col("a")], [{ id: "gone" }, { id: "a", width: 60 }]);
    expect(out.map((c) => c.id)).toEqual(["a"]);
  });
});
