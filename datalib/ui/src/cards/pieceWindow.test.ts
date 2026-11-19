import { describe, expect, it } from "vitest";
import type { BodyNode, ChatResponse } from "@/api";
import { growth, holds, withPage } from "./pieceWindow";

const piece = (uuid: string | null): BodyNode => ({ uuid, md: `${uuid}\n` });

describe("growth", () => {
  const a = piece("a");
  const b = piece("b");
  const c = piece("c");
  const d = piece("d");

  /// Pages added at either end are drawn beside what is there; anything
  /// else, or the same nodes copied (a refetch), is drawn anew.
  it("tells a window that grew at its ends from a new one", () => {
    expect(growth([b, c], [a, b, c, d])).toEqual({ kind: "grew", above: [a], below: [d] });
    expect(growth([b, c], [b, c])).toEqual({ kind: "same" });
    expect(growth([], [a])).toEqual({ kind: "new" });
    expect(growth([b, c], [a, c])).toEqual({ kind: "new" });
    expect(growth([b, c], [piece("b"), piece("c")])).toEqual({ kind: "new" });
    expect(growth([b, c], [c, b])).toEqual({ kind: "new" });
  });
});

describe("withPage", () => {
  const win = {
    pieces: [piece("b")],
    before: 5,
    after: 9,
  } as unknown as ChatResponse;

  /// A page above takes its own cursor above; one below, its own below;
  /// the window's own nodes stay the same objects, so the body sees them
  /// grow.
  it("adds a page at an end and keeps the other end's cursor", () => {
    const above = withPage(win, { pieces: [piece("a")], before: null } as ChatResponse, "above");
    expect(above.pieces.map((p) => ("uuid" in p ? p.uuid : null))).toEqual(["a", "b"]);
    expect([above.before, above.after]).toEqual([null, 9]);
    expect(above.pieces[1]).toBe(win.pieces[0]);
    const below = withPage(win, { pieces: [piece("c")], after: 12 } as ChatResponse, "below");
    expect([below.before, below.after]).toEqual([5, 12]);
  });
});

describe("holds", () => {
  /// A message inside a fold is held too.
  it("finds an item at any depth", () => {
    const nodes: BodyNode[] = [
      piece(null),
      { fold: "tool-group", summary: "🛠", pieces: [piece("t1")] },
    ];
    expect(holds(nodes, "t1")).toBe(true);
    expect(holds(nodes, "x")).toBe(false);
  });
});
