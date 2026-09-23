import { describe, expect, it } from "vitest";
import { handedOf, isEmpty, patchRows } from "./rowPatch";

type Row = { uuid: string; text: string };
const keyOf = (r: Row) => r.uuid;

describe("a refresh of the same rows", () => {
  /// The regression: a refresh used to replace the whole dataset, so
  /// every row was redrawn even when the index had not touched it.
  it("names nothing when nothing moved", () => {
    const rows = [
      { uuid: "a", text: "one" },
      { uuid: "b", text: "two" },
    ];
    const { patch } = patchRows(
      handedOf(rows, keyOf),
      rows.map((r) => ({ ...r })),
      keyOf,
    );
    expect(isEmpty(patch)).toBe(true);
  });

  it("names what went, what changed and what is new", () => {
    const before = handedOf(
      [
        { uuid: "a", text: "one" },
        { uuid: "b", text: "two" },
        { uuid: "c", text: "three" },
      ],
      keyOf,
    );
    const after = [
      { uuid: "a", text: "one" },
      { uuid: "c", text: "THREE" },
      { uuid: "d", text: "four" },
    ];
    const { patch, handed } = patchRows(before, after, keyOf);
    expect(patch).toEqual({
      removed: ["b"],
      changed: [{ uuid: "c", text: "THREE" }],
      added: [{ uuid: "d", text: "four" }],
    });
    expect(handed).toEqual(handedOf(after, keyOf));
  });
});
