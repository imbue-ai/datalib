import { describe, expect, it } from "vitest";
import { keptSelection, newlyPicked, rowsToRestore, withoutLeftOut } from "./selection";

const keyOf = (r: { uuid: string }) => r.uuid;

describe("a selection change", () => {
  /// The regression: a refresh that moved the selected row re-selected
  /// it at its new index, which read as a new pick and opened its
  /// document again.
  it("picks nothing when the same record is selected at a new index", () => {
    const { picked } = newlyPicked(new Set(["a"]), [{ uuid: "a" }], keyOf);
    expect(picked).toEqual([]);
  });

  it("picks the records that were not selected before", () => {
    const { picked, selected } = newlyPicked(new Set(["a"]), [{ uuid: "a" }, { uuid: "b" }], keyOf);
    expect(picked).toEqual([{ uuid: "b" }]);
    expect(selected).toEqual(new Set(["a", "b"]));
  });
});

describe("the selection kept across a new answer", () => {
  const heldNow = new Set(["a", "b"]);
  const held = (id: string) => heldNow.has(id);

  /// A row a search hid must not come back after a plain click asked
  /// for one row alone.
  it("is the grid's selection after a plain click or arrow", () => {
    expect(keptSelection(new Set(["a", "far"]), ["b"], held, false)).toEqual(new Set(["b"]));
  });
  it("keeps records not held when the change adds to the selection", () => {
    expect(keptSelection(new Set(["a", "far"]), ["a", "b"], held, true)).toEqual(
      new Set(["a", "b", "far"]),
    );
  });
  it("lets a ⌘-click take a held record out", () => {
    expect(keptSelection(new Set(["a", "b", "far"]), ["b"], held, true)).toEqual(
      new Set(["b", "far"]),
    );
  });
});

describe("restoring a kept selection", () => {
  const rowOf = (id: string) => ({ a: 4, b: 1 })[id];

  it("selects the kept records the grid holds, wherever they now are", () => {
    expect(rowsToRestore(new Set(["a", "b", "far"]), rowOf, [])).toEqual([1, 4]);
  });
  it("does nothing when the grid already has them selected", () => {
    expect(rowsToRestore(new Set(["a", "b"]), rowOf, [4, 1])).toBeNull();
  });
  it("clears rows selected that are not kept", () => {
    expect(rowsToRestore(new Set(["b"]), rowOf, [1, 4])).toEqual([1]);
  });
});

describe("what a new search leaves of a selection", () => {
  it("drops the records asked about that the search does not hold", () => {
    expect(withoutLeftOut(new Set(["a", "b", "c"]), ["b", "c"], ["c"])).toEqual(
      new Set(["a", "c"]),
    );
  });
  /// A record selected after the question went out was not asked about.
  it("keeps records it was not asked about", () => {
    expect(withoutLeftOut(new Set(["a", "late"]), ["a"], [])).toEqual(new Set(["late"]));
  });
});
