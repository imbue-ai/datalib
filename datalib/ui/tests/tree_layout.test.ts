import { describe, expect, it } from "vitest";
import { layoutTree, type LayoutNode } from "@/views/treeLayout";

function node(
  id: string,
  parentId: string | null,
  width = 100,
  height = 50,
): LayoutNode {
  return { id, parentId, width, height };
}

const OPTS = { hGap: 20, vGap: 10 };

describe("layoutTree", () => {
  it("places a single root at the origin", () => {
    const rects = layoutTree([node("a", null)], OPTS);
    expect(rects.get("a")).toEqual({ x: 0, y: 0, width: 100, height: 50 });
  });

  it("places a child to the right of its parent, vertically aligned", () => {
    const rects = layoutTree([node("a", null), node("b", "a")], OPTS);
    expect(rects.get("a")).toEqual({ x: 0, y: 0, width: 100, height: 50 });
    // x = parent right edge + hGap; equal heights → same y.
    expect(rects.get("b")).toEqual({ x: 120, y: 0, width: 100, height: 50 });
  });

  it("stacks siblings with vGap and centers the parent on their span", () => {
    const rects = layoutTree(
      [node("a", null), node("b", "a"), node("c", "a")],
      OPTS,
    );
    // children span = 50 + 10 + 50 = 110
    expect(rects.get("b")).toEqual({ x: 120, y: 0, width: 100, height: 50 });
    expect(rects.get("c")).toEqual({ x: 120, y: 60, width: 100, height: 50 });
    expect(rects.get("a")!.y).toBe((110 - 50) / 2);
  });

  it("centers children on a parent taller than their span", () => {
    const rects = layoutTree(
      [node("a", null, 100, 200), node("b", "a")],
      OPTS,
    );
    expect(rects.get("a")!.y).toBe(0);
    expect(rects.get("b")!.y).toBe((200 - 50) / 2);
  });

  it("gives sibling subtrees disjoint vertical bands", () => {
    // b has two children (band 110 tall), its sibling c must start
    // below b's whole band, not just below b.
    const rects = layoutTree(
      [
        node("a", null),
        node("b", "a"),
        node("c", "a"),
        node("b1", "b"),
        node("b2", "b"),
      ],
      OPTS,
    );
    expect(rects.get("c")!.y).toBe(110 + 10);
    // b centered on its own children's span
    expect(rects.get("b")!.y).toBe((110 - 50) / 2);
  });

  it("offsets a child's x by its own parent's width", () => {
    const rects = layoutTree(
      [node("a", null, 300), node("b", "a", 100), node("c", "b", 100)],
      OPTS,
    );
    expect(rects.get("b")!.x).toBe(320);
    expect(rects.get("c")!.x).toBe(440);
  });

  it("stacks multiple roots vertically", () => {
    const rects = layoutTree([node("a", null), node("b", null)], OPTS);
    expect(rects.get("a")!.y).toBe(0);
    expect(rects.get("b")!.y).toBe(60);
  });

  it("treats a node with a missing parent as a root", () => {
    const rects = layoutTree([node("a", null), node("b", "gone")], OPTS);
    expect(rects.get("b")).toEqual({ x: 0, y: 60, width: 100, height: 50 });
  });
});
