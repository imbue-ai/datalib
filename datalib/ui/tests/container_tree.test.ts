import { describe, expect, it } from "vitest";
import {
  find,
  instantiate,
  isSolidified,
  landing,
  makeBox,
  makeCard,
  openFrom,
  parseTree,
  remove,
  rename,
  resetTo,
  setBasis,
  setDirection,
  setCard,
  setSolidified,
  tabRows,
  unwrap,
  wrap,
  type BoxNode,
  type TreeNode,
} from "@/views/containerTree";

// The tabs root, holding a Dashboard-like solidified composite and a
// sandbox of columns that itself holds a solidified stack.
function fixture(): BoxNode {
  const dash = makeBox("dash", "page", [makeCard("d1", "a()"), makeCard("d2", "b()")], {
    solidified: true,
  });
  const inner = makeBox("inner", "split", [makeCard("i1", "c()")], {
    solidified: true,
    direction: "column",
  });
  const sandbox = makeBox("sandbox", "columns", [
    makeCard("s1", "d()"),
    inner,
    makeCard("s3", "e()"),
  ]);
  return makeBox("root", "tabs", [dash, sandbox]);
}

function ids(n: TreeNode): string[] {
  return n.kind === "box" ? n.children.map((c) => c.id) : [];
}

describe("where an opened card lands", () => {
  it("leaves a solidified composite for a new tab beside it, and shows that tab", () => {
    const root = openFrom(fixture(), "d2", [makeCard("new", "x()")], "tab") as BoxNode;
    // A tab of its own: a Columns container holding the card, so what
    // the card opens lands beside it.
    expect(ids(root)).toEqual(["dash", "sandbox", "tab"]);
    expect(find(root, "tab")?.openedBy).toBe("dash");
    expect(ids(find(root, "tab")!)).toEqual(["new"]);
    expect(root.selected).toBe("tab");
    // The composite kept its shape.
    expect(ids(find(root, "dash")!)).toEqual(["d1", "d2"]);
  });

  it("stays inside an unsolidified container: columns drop what was right of the opener", () => {
    const root = openFrom(fixture(), "s1", [makeCard("n1", "x()"), makeCard("n2", "y()")], "tab");
    expect(ids(find(root, "sandbox")!)).toEqual(["s1", "n1", "n2"]);
    expect(find(root, "n2")?.openedBy).toBe("n1");
  });

  it("goes from a solidified container inside a sandbox to the sandbox, right of it", () => {
    expect(landing(fixture(), "i1")).toEqual({ boxId: "sandbox", branchId: "inner" });
    const root = openFrom(fixture(), "i1", [makeCard("n", "x()")], "tab");
    expect(ids(find(root, "sandbox")!)).toEqual(["s1", "inner", "n"]);
  });
});

describe("solidifying", () => {
  it("covers the container and everything in it, and a flag further in counts again once it is off", () => {
    let root: TreeNode = fixture();
    expect(isSolidified(root, "sandbox")).toBe(false);
    expect(isSolidified(root, "i1")).toBe(true);
    root = setSolidified(root, "sandbox", true);
    expect(isSolidified(root, "s1")).toBe(true);
    // Nothing inside takes an open now: it goes out to the root's tabs.
    expect(landing(root, "s1")).toEqual({ boxId: "root", branchId: "sandbox" });
    root = setSolidified(root, "sandbox", false);
    expect(isSolidified(root, "inner")).toBe(true);
    expect(isSolidified(root, "s1")).toBe(false);
  });

  it("never solidifies the outermost container, so an open always lands", () => {
    const root = setSolidified(fixture(), "root", true);
    expect(isSolidified(root, "root")).toBe(false);
    expect(landing(root, "d1")?.boxId).toBe("root");
  });
});

describe("changing the tree", () => {
  it("closing a tab closes what was opened from it, and selects the one before", () => {
    let root = openFrom(fixture(), "d1", [makeCard("a", "x()")], "ta") as BoxNode;
    root = openFrom(root, "a", [makeCard("b", "y()")], "unused") as BoxNode;
    root = openFrom(setSolidified(root, "ta", true), "b", [makeCard("c", "z()")], "tc") as BoxNode;
    expect(tabRows(root).map((r) => [r.node.id, r.depth])).toEqual([
      ["dash", 0],
      ["ta", 1],
      ["tc", 2],
      ["sandbox", 0],
    ]);
    root = remove(root, "ta") as BoxNode;
    expect(ids(root)).toEqual(["dash", "sandbox"]);
    expect(root.selected).toBe("sandbox");
  });

  it("a container its last card leaves goes too", () => {
    const root = remove(fixture(), "i1");
    expect(find(root, "inner")).toBeUndefined();
    expect(ids(find(root, "sandbox")!)).toEqual(["s1", "s3"]);
  });

  // A nested tabs container showed nothing after either: it still
  // pointed at the id the replaced node had.
  it("wrapping or resetting the shown tab keeps it shown", () => {
    const tabsBox = makeBox("t", "tabs", [
      makeCard("a", "x()"),
      { ...makeCard("b", "y()"), openedBy: "a" },
    ]);
    const root = makeBox("root", "tabs", [{ ...tabsBox, selected: "a" }]);

    const wrapped = wrap(root, "a", "split", "w");
    const t = find(wrapped, "t") as BoxNode;
    expect(t.selected).toBe("w");
    expect(t.children[1].openedBy).toBe("w");

    let n = 0;
    const reset = resetTo(
      wrapped,
      "w",
      makeBox("tpl", "split", [makeCard("z", "z()")]),
      () => `n${n++}`,
    );
    expect((find(reset, "t") as BoxNode).selected).toBe("n0");
    expect(ids(find(reset, "n0")!)).toEqual(["n1"]);
  });

  it("turning a split the other way forgets sizes set along the old axis", () => {
    let root = setBasis(fixture(), "i1", 200);
    root = setDirection(root, "inner", "row");
    const inner = find(root, "inner") as BoxNode;
    expect(inner.direction).toBe("row");
    expect(inner.children[0].basis).toBeNull();
  });

  it("wrapping a card and taking it out again puts it back as it was", () => {
    const wrapped = wrap(fixture(), "s3", "tabs", "w");
    const w = find(wrapped, "w") as BoxNode;
    expect(w.children.map((c) => c.id)).toEqual(["s3"]);
    expect(w.selected).toBe("s3");
    expect(ids(find(unwrap(wrapped, "w"), "sandbox")!)).toEqual(["s1", "inner", "s3"]);
  });
});

describe("names", () => {
  it("a name the person gave a card outlasts the title the card gives itself", () => {
    let root = rename(fixture(), "s1", "My search");
    root = setCard(root, "s1", { title: "Search: launch plan" });
    const card = find(root, "s1");
    expect(card?.name).toBe("My search");
    expect(card?.kind === "card" && card.title).toBe("Search: launch plan");
  });
});

describe("composites", () => {
  it("a copy takes fresh ids, with its openers and selection re-pointed", () => {
    const tabs = makeBox("t", "tabs", [
      makeCard("a", "x()"),
      { ...makeCard("b", "y()"), openedBy: "a" },
    ]);
    let n = 0;
    const copy = instantiate({ ...tabs, selected: "b" }, () => `n${n++}`) as BoxNode;
    expect(copy.id).toBe("n0");
    expect(copy.children.map((c) => c.id)).toEqual(["n1", "n2"]);
    expect(copy.children[1].openedBy).toBe("n1");
    expect(copy.selected).toBe("n2");
  });

  it("a stored tree is read with what it leaves out filled in", () => {
    const stored = {
      kind: "box",
      id: "r",
      layout: "tabs",
      selected: "gone",
      children: [{ kind: "card", id: "c", source: "x()" }],
    };
    const tree = parseTree(stored)!;
    expect(tree.selected).toBe("c");
    expect(tree.solidified).toBe(false);
    expect(tree.children[0]).toEqual(makeCard("c", "x()"));
  });

  it("a stored tree this build cannot read is dropped, not half-used", () => {
    expect(parseTree(fixture())).not.toBeNull();
    expect(parseTree(null)).toBeNull();
    expect(parseTree({ kind: "box", id: "r", layout: "grid", children: [] })).toBeNull();
    expect(parseTree(makeCard("c", "x()"))).toBeNull();
    // One unreadable card anywhere drops the whole tree.
    expect(
      parseTree(makeBox("r", "tabs", [{ ...makeCard("c", "x()"), source: 1 } as never])),
    ).toBeNull();
  });
});
