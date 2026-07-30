import { describe, expect, it } from "vitest";
import {
  addSibling,
  appendChild,
  deleteNode,
  dropOntoLeaf,
  findNode,
  findTile,
  listTiles,
  makeTile,
  makeRoot,
  moveNodeToContainer,
  oppositeDir,
  parentOf,
  type Dir,
  type TileNode,
  type TileSplit,
} from "@/views/tilingTree";

const ids = (node: TileNode): string[] => listTiles(node).map((p) => p.id);

const split = (
  id: string,
  dir: Dir,
  children: TileNode[],
  weight = 1,
): TileSplit => ({ kind: "split", id, dir, weight, children });

// A horizontal root container, like the live tree (the one split that
// is allowed a single child and never collapses).
const root = (children: TileNode[]): TileSplit => split("root", "h", children);

const blank = () => makeTile("blank", "");

describe("addSibling", () => {
  it("inserts the new tile right after the target in its parent", () => {
    const tree = addSibling(
      root([makeTile("a", "x"), makeTile("b", "y")]),
      "a",
      makeTile("c", "z"),
    );
    expect(ids(tree)).toEqual(["a", "c", "b"]);
  });

  it("copies the target's weight onto the new sibling", () => {
    const tree = addSibling(
      split("s0", "v", [makeTile("a", "x", 2), makeTile("b", "y", 1)]),
      "a",
      makeTile("c", "z"),
    ) as TileSplit;
    expect(tree.children[1].weight).toBe(2);
  });
});

describe("deleteNode + collapse rule", () => {
  it("collapses a now-single-child non-root split, promoting the lone child", () => {
    const tree = root([
      split("s1", "v", [makeTile("a", "x"), makeTile("b", "y")], 5),
      makeTile("z", "w"),
    ]);
    const out = deleteNode(tree, "a", blank) as TileSplit;
    expect(ids(out)).toEqual(["b", "z"]);
    const promoted = out.children[0];
    expect(promoted.kind).toBe("leaf");
    expect(promoted.id).toBe("b");
    // The promoted child inherits the collapsed split's weight.
    expect(promoted.weight).toBe(5);
  });

  it("never collapses the root, even down to a single child", () => {
    const out = deleteNode(
      root([makeTile("a", "x"), makeTile("b", "y")]),
      "b",
      blank,
    ) as TileSplit;
    expect(out.kind).toBe("split");
    expect(out.dir).toBe("h");
    expect(ids(out)).toEqual(["a"]);
  });

  it("keeps a split with three children as a split", () => {
    const tree = root([
      split("s1", "h", [makeTile("a", "x"), makeTile("b", "y"), makeTile("c", "z")]),
    ]);
    const out = deleteNode(tree, "b", blank);
    const s1 = (out as TileSplit).children[0] as TileSplit;
    expect(s1.kind).toBe("split");
    expect(ids(s1)).toEqual(["a", "c"]);
  });

  it("promotes a nested split when its parent collapses", () => {
    const inner = split("s1", "v", [makeTile("b", "y"), makeTile("c", "z")]);
    const tree = root([split("s0b", "h", [makeTile("a", "x"), inner], 4)]);
    const promoted = (deleteNode(tree, "a", blank) as TileSplit)
      .children[0] as TileSplit;
    expect(promoted.id).toBe("s1");
    expect(promoted.dir).toBe("v");
    expect(promoted.weight).toBe(4);
    expect(ids(promoted)).toEqual(["b", "c"]);
  });

  it("yields a fresh blank tile when the last card is deleted", () => {
    const out = deleteNode(makeRoot("root", makeTile("a", "g()")), "a", blank) as TileSplit;
    expect(out.kind).toBe("split");
    expect(out.dir).toBe("h");
    expect(out.children).toEqual([makeTile("blank", "")]);
  });
});

describe("appendChild (the add button)", () => {
  it("appends a child at the end of the container, weight reset to 1", () => {
    const out = appendChild(
      root([makeTile("a", "x", 4)]),
      "root",
      makeTile("b", "", 9),
    ) as TileSplit;
    expect(ids(out)).toEqual(["a", "b"]);
    expect(out.children[1].weight).toBe(1);
  });

  it("focuses the appended tab in a tab container", () => {
    const tabs = split("t", "tab", [makeTile("a", "x")]);
    const out = appendChild(root([tabs]), "t", makeTile("b", ""));
    const t = (out as TileSplit).children[0] as TileSplit;
    expect(t.active).toBe(1);
  });
});

describe("moveNodeToContainer (drop on an add area)", () => {
  it("reorders to the end within the same parent", () => {
    const out = moveNodeToContainer(
      root([makeTile("a", "x"), makeTile("b", "y")]),
      "a",
      "root",
    );
    expect(ids(out)).toEqual(["b", "a"]);
  });

  it("moves across parents, collapsing the emptied old parent", () => {
    const tree = root([
      split("s1", "v", [makeTile("a", "x"), makeTile("b", "y")]),
      makeTile("c", "z"),
    ]);
    const out = moveNodeToContainer(tree, "a", "root");
    // s1 collapses to b; a lands at the root's end.
    expect(ids(out)).toEqual(["b", "c", "a"]);
  });

  it("is a no-op when moving a node into its own subtree", () => {
    const tree = root([split("s1", "v", [makeTile("a", "x"), makeTile("b", "y")])]);
    expect(moveNodeToContainer(tree, "s1", "s1")).toBe(tree);
  });
});

describe("dropOntoLeaf (drop on a card)", () => {
  it("replaces the target with a split perpendicular to its parent, target first", () => {
    const out = dropOntoLeaf(
      root([makeTile("a", "x"), makeTile("b", "y")]),
      "a",
      "b",
      "ns",
    );
    const ns = (out as TileSplit).children[0] as TileSplit;
    // parent of b is the horizontal root → the new split is vertical.
    expect(ns.id).toBe("ns");
    expect(ns.dir).toBe("v");
    expect(ids(ns)).toEqual(["b", "a"]);
  });

  it("is a no-op dropping a node onto itself", () => {
    const tree = root([makeTile("a", "x"), makeTile("b", "y")]);
    expect(dropOntoLeaf(tree, "a", "a", "ns")).toBe(tree);
  });

  it("is a no-op dropping a container onto a leaf inside it", () => {
    const tree = root([
      split("s1", "v", [makeTile("a", "x"), makeTile("b", "y")]),
      makeTile("c", "z"),
    ]);
    expect(dropOntoLeaf(tree, "s1", "b", "ns")).toBe(tree);
  });
});

describe("tab splits", () => {
  it("keeps active pointing at the same surviving tab", () => {
    const tabs = split("t", "tab", [
      makeTile("a", "x"),
      makeTile("b", "y"),
      makeTile("c", "z"),
    ]);
    tabs.active = 2; // "c"
    const out = deleteNode(root([tabs]), "a", blank);
    const t = (out as TileSplit).children[0] as TileSplit;
    expect(ids(t)).toEqual(["b", "c"]);
    expect(t.active).toBe(1); // "c" shifted down by one
  });

  it("removes a whole grouped tab (a split child) by id, collapsing the tabs", () => {
    const group = split("g", "h", [makeTile("b", "y"), makeTile("c", "z")]);
    const tabs = split("t", "tab", [makeTile("a", "x"), group]);
    tabs.active = 1;
    const out = deleteNode(root([tabs]), "g", blank);
    const promoted = (out as TileSplit).children[0];
    expect(promoted.kind).toBe("leaf");
    expect(promoted.id).toBe("a");
  });
});

describe("helpers", () => {
  it("oppositeDir flips h/v and defaults tab to h", () => {
    expect(oppositeDir("h")).toBe("v");
    expect(oppositeDir("v")).toBe("h");
    expect(oppositeDir("tab")).toBe("h");
  });

  it("findNode / parentOf / findTile", () => {
    const inner = split("s1", "v", [makeTile("b", "y"), makeTile("c", "z")]);
    const tree = root([makeTile("a", "x"), inner]);
    expect(findNode(tree, "s1")).toBe(inner);
    expect(parentOf(tree, "s1")?.id).toBe("root");
    expect(parentOf(tree, "c")?.id).toBe("s1");
    expect(findTile(tree, "c")?.source).toBe("z");
    expect(findTile(tree, "s1")).toBeNull(); // a split, not a tile
  });
});
