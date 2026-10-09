import { describe, expect, it } from "vitest";
import {
  find,
  instantiate,
  isSolidified,
  landing,
  makeBox,
  makeCard,
  openFrom,
  move,
  parseTree,
  pinnedTabs,
  predatesPins,
  remove,
  rename,
  resetTo,
  setBasis,
  setDirection,
  setPinned,
  setCard,
  setSolidified,
  tabRows,
  tabShowing,
  unwrap,
  withPins,
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
  it("leaves a solidified composite for a tab of its own under it, and shows that tab", () => {
    const root = openFrom(fixture(), "d2", [makeCard("new", "x()")]) as BoxNode;
    expect(ids(root)).toEqual(["dash", "sandbox", "new"]);
    expect(find(root, "new")?.openedBy).toBe("dash");
    expect(root.selected).toBe("new");
    // The composite kept its shape.
    expect(ids(find(root, "dash")!)).toEqual(["d1", "d2"]);
  });

  it("opens a chain into tabs as one tab per card, each under the one before", () => {
    const root = openFrom(fixture(), "d1", [makeCard("a", "x()"), makeCard("b", "y()")]) as BoxNode;
    expect(tabRows(root).map((r) => [r.node.id, r.depth])).toEqual([
      ["dash", 0],
      ["a", 1],
      ["b", 2],
      ["sandbox", 0],
    ]);
    expect(root.selected).toBe("b");
  });

  it("stays inside an unsolidified container: columns drop what was right of the opener", () => {
    const root = openFrom(fixture(), "s1", [makeCard("n1", "x()"), makeCard("n2", "y()")]);
    expect(ids(find(root, "sandbox")!)).toEqual(["s1", "n1", "n2"]);
    expect(find(root, "n2")?.openedBy).toBe("n1");
  });

  it("goes from a solidified container inside a sandbox to the sandbox, right of it", () => {
    expect(landing(fixture(), "i1")).toEqual({ boxId: "sandbox", branchId: "inner" });
    const root = openFrom(fixture(), "i1", [makeCard("n", "x()")]);
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
    // A Columns container "ta" holding "a", opened from the composite.
    let root = openFrom(fixture(), "d1", [makeBox("ta", "columns", [makeCard("a", "x()")])]);
    root = openFrom(root, "a", [makeCard("b", "y()")]);
    root = openFrom(setSolidified(root, "ta", true), "b", [makeCard("c", "z()")]) as BoxNode;
    expect(ids(find(root, "ta")!)).toEqual(["a", "b"]);
    expect(tabRows(root).map((r) => [r.node.id, r.depth])).toEqual([
      ["dash", 0],
      ["ta", 1],
      ["c", 2],
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

describe("pinned tabs", () => {
  // The fixture with its composite pinned, and a card "a" opened from the sandbox.
  function pinnedFixture(): BoxNode {
    const root = openFrom(setSolidified(fixture(), "sandbox", true), "s1", [makeCard("a", "x()")]);
    return setPinned(root, "dash", true) as BoxNode;
  }

  it("a card opened from a pinned tab is a tab after the rest, under nothing", () => {
    const root = openFrom(pinnedFixture(), "d1", [
      makeCard("n1", "y()"),
      makeCard("n2", "z()"),
    ]) as BoxNode;
    expect(pinnedTabs(root).map((t) => t.id)).toEqual(["dash"]);
    expect(tabRows(root).map((r) => [r.node.id, r.depth])).toEqual([
      ["sandbox", 0],
      ["a", 1],
      ["n1", 0],
      ["n2", 1],
    ]);
    expect(find(root, "n1")?.openedBy).toBeNull();
    // So closing the pinned tab's own opens never reaches back to it.
    expect(ids(remove(root, "n1"))).toEqual(["dash", "sandbox", "a"]);
  });

  it("pinning lifts a tab out of the tree, and what it opened goes under its opener", () => {
    let root = openFrom(pinnedFixture(), "a", [makeCard("b", "y()")]) as BoxNode;
    root = setPinned(root, "a", true) as BoxNode;
    expect(ids(root)).toEqual(["dash", "a", "sandbox", "b"]);
    expect(find(root, "a")?.openedBy).toBeNull();
    expect(tabRows(root).map((r) => [r.node.id, r.depth])).toEqual([
      ["sandbox", 0],
      ["b", 1],
    ]);
  });

  it("unpinning puts a tab first among the rest", () => {
    const root = setPinned(pinnedFixture(), "dash", false) as BoxNode;
    expect(pinnedTabs(root)).toEqual([]);
    expect(ids(root)).toEqual(["dash", "sandbox", "a"]);
    expect(setPinned(root, "d1", true)).toBe(root);
  });

  it("a tab does not move across the line between pinned and the rest", () => {
    const root = setPinned(pinnedFixture(), "a", true) as BoxNode;
    expect(ids(root)).toEqual(["dash", "a", "sandbox"]);
    expect(move(root, "a", 1)).toBe(root);
    expect(move(root, "sandbox", -1)).toBe(root);
    expect(ids(move(root, "a", -1))).toEqual(["a", "dash", "sandbox"]);
  });

  it("a container put around a pinned tab is the pinned tab, and its cards are when taken out", () => {
    const wrapped = wrap(setPinned(pinnedFixture(), "a", true), "a", "columns", "w") as BoxNode;
    expect(pinnedTabs(wrapped).map((t) => t.id)).toEqual(["dash", "w"]);
    expect(find(wrapped, "a")?.pinned).toBe(false);
    const out = unwrap(wrapped, "w") as BoxNode;
    expect(pinnedTabs(out).map((t) => t.id)).toEqual(["dash", "a"]);
  });

  it("an open that would make a tab shows the pinned tab that has that card", () => {
    const root = setPinned(
      { ...fixture(), children: [...fixture().children, makeCard("src", "sourcesView()")] },
      "src",
      true,
    );
    expect(tabShowing(root, "d1", "sourcesView()")).toBe("src");
    expect(tabShowing(root, "d1", 'sourcesView({"add":true})')).toBeNull();
    // Inside an unsolidified container the card lands beside its opener.
    expect(tabShowing(root, "s1", "sourcesView()")).toBeNull();
    // An unpinned tab is not a destination.
    expect(tabShowing(setPinned(root, "src", false), "d1", "sourcesView()")).toBeNull();
  });

  it("an open of a card a tab opened from the same tab already shows goes to that tab", () => {
    // "sandbox" solidified, so its cards open tabs under it: "a" from s1.
    let root = openFrom(setSolidified(fixture(), "sandbox", true), "s1", [
      makeCard("a", "x()"),
    ]) as BoxNode;
    expect(tabShowing(root, "s1", "x()")).toBe("a");
    // From any card of the same tab, and only for the same source.
    expect(tabShowing(root, "s3", "x()")).toBe("a");
    expect(tabShowing(root, "s1", "y()")).toBeNull();
    // Not a tab opened from another tab, nor one further down.
    expect(tabShowing(root, "d1", "x()")).toBeNull();
    root = openFrom(root, "a", [makeCard("b", "y()")]) as BoxNode;
    expect(tabShowing(root, "s1", "y()")).toBeNull();
    expect(tabShowing(root, "a", "y()")).toBe("b");
    // A pinned tab that shows the card comes first.
    const src = makeCard("src", "x()");
    const pinned = setPinned({ ...root, children: [...root.children, src] }, "src", true);
    expect(tabShowing(pinned, "s1", "x()")).toBe("src");
  });

  it("withPins pins the tab that already shows a pin, and adds the ones missing", () => {
    const dash = { ...fixture().children[0], template: "Dashboard" } as BoxNode;
    const kept = makeBox("root", "tabs", [
      makeCard("mine", "x()"),
      dash,
      makeCard("src", "sourcesView()"),
    ]);
    const pins = [
      makeBox("p-dash", "page", [], { template: "Dashboard" }),
      makeCard("p-search", "searchView()"),
      makeCard("p-src", "sourcesView()"),
    ].map((p) => ({ ...p, pinned: true }));
    const root = withPins(kept, pins);
    expect(ids(root)).toEqual(["dash", "p-search", "src", "mine"]);
    expect(pinnedTabs(root).map((t) => t.id)).toEqual(["dash", "p-search", "src"]);
    expect(root.selected).toBe("mine");
  });

  it("only a stored tree that says nothing of pins predates them", () => {
    const stored = JSON.parse(JSON.stringify(fixture())) as { children: { pinned?: boolean }[] };
    expect(predatesPins(stored)).toBe(false);
    expect(parseTree(stored)?.children.map((c) => c.pinned)).toEqual([false, false]);
    for (const c of stored.children) delete c.pinned;
    expect(predatesPins(stored)).toBe(true);
    expect(predatesPins(null)).toBe(false);
  });

  it("a stored pin is read back", () => {
    const stored: unknown = JSON.parse(JSON.stringify(pinnedFixture()));
    expect(pinnedTabs(parseTree(stored)!).map((t) => t.id)).toEqual(["dash"]);
  });
});
