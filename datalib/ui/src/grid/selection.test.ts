import { describe, expect, it } from "vitest";
import { newlyPicked } from "./selection";

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
