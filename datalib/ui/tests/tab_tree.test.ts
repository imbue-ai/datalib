import { describe, expect, it } from "vitest";
import {
  closeTab,
  makeTopLevel,
  newTab,
  nextCounter,
  openChain,
  openStack,
  parseStored,
  rows,
  selectAfterClose,
  serialize,
  startingTree,
  tabForSpec,
  type Tab,
} from "@/views/tabTree";

function counter(start = 100) {
  let n = start;
  return () => `t${n++}`;
}

const grid = newTab("t1", "gridView()", null);

describe("rows", () => {
  it("lists children under their opener, indented, and hides a collapsed branch", () => {
    const a = newTab("t2", 'documentView("a")', "t1");
    const b = newTab("t3", 'documentView("b")', "t2");
    const log = newTab("t4", "logView()", null);
    const tabs = [grid, a, log, b];
    expect(rows(tabs).map((r) => [r.tab.id, r.depth])).toEqual([
      ["t1", 0],
      ["t2", 1],
      ["t3", 2],
      ["t4", 0],
    ]);
    const folded = tabs.map((t) => (t.id === "t2" ? { ...t, collapsed: true } : t));
    expect(rows(folded).map((r) => r.tab.id)).toEqual(["t1", "t2", "t4"]);
  });

  it("lists a tab whose parent is gone as a root rather than losing it", () => {
    const orphan = newTab("t9", "logView()", "missing");
    expect(rows([grid, orphan]).map((r) => [r.tab.id, r.depth])).toEqual([
      ["t1", 0],
      ["t9", 0],
    ]);
  });
});

describe("openChain", () => {
  it("opens a chain as a spine under the caller", () => {
    const { tabs, ids } = openChain([grid], "t1", ["a()", "b()"], counter());
    expect(ids).toEqual(["t100", "t101"]);
    expect(tabs.map((t) => [t.id, t.parentId])).toEqual([
      ["t1", null],
      ["t100", "t1"],
      ["t101", "t100"],
    ]);
  });

  it("replaces the caller's preview child in place, so a row click per row is one tab", () => {
    const next = counter();
    const kept = { ...newTab("t2", "kept()", "t1"), preview: false };
    const first = openChain([grid, kept], "t1", ['documentView("a")'], next).tabs;
    const second = openChain(first, "t1", ['documentView("b")'], next);
    expect(second.tabs.map((t) => t.source)).toEqual(["gridView()", "kept()", 'documentView("b")']);
  });

  it("keeps a preview child that has a visited tab under it", () => {
    const preview = { ...newTab("t2", "a()", "t1"), preview: true };
    const visited = newTab("t3", "b()", "t2");
    const { tabs } = openChain([grid, preview, visited], "t1", ["c()"], counter());
    expect(tabs.map((t) => t.id)).toEqual(["t1", "t2", "t3", "t100"]);
  });
});

describe("openStack", () => {
  it("opens a miller link as a root and a spine, each at its URL state", () => {
    const { tabs, lastId } = openStack(
      [grid],
      [
        { code: "gridView()", state: "q=a" },
        { code: 'documentView("a")', state: "" },
      ],
      counter(),
    );
    expect(lastId).toBe("t101");
    expect(tabs.map((t) => [t.id, t.parentId, t.state])).toEqual([
      ["t1", null, ""],
      ["t100", null, "q=a"],
      ["t101", "t100", ""],
    ]);
  });
});

describe("makeTopLevel", () => {
  it("detaches a tab with its subtree, listed right after the root it came from", () => {
    const a = { ...newTab("t2", "a()", "t1"), preview: true };
    const b = newTab("t3", "b()", "t2");
    const other = newTab("t4", "other()", null);
    const next = makeTopLevel([grid, a, b, other], "t2");
    expect(rows(next).map((r) => [r.tab.id, r.depth])).toEqual([
      ["t1", 0],
      ["t2", 0],
      ["t3", 1],
      ["t4", 0],
    ]);
    expect(next.find((t) => t.id === "t2")?.preview).toBe(false);
  });

  it("leaves a root where it is", () => {
    const tabs = [grid, newTab("t2", "a()", "t1")];
    expect(makeTopLevel(tabs, "t1")).toBe(tabs);
  });
});

describe("closeTab", () => {
  const a = newTab("t2", "a()", "t1");
  const b = newTab("t3", "b()", "t2");
  const c = newTab("t4", "c()", "t2");
  const tabs: Tab[] = [grid, a, b, c];

  it("hands an expanded tab's children to its parent, in its place", () => {
    const { tabs: next, closed } = closeTab(tabs, "t2");
    expect([...closed]).toEqual(["t2"]);
    expect(next.map((t) => [t.id, t.parentId])).toEqual([
      ["t1", null],
      ["t3", "t1"],
      ["t4", "t1"],
    ]);
  });

  it("closes a collapsed tab's whole subtree", () => {
    const folded = tabs.map((t) => (t.id === "t2" ? { ...t, collapsed: true } : t));
    const { tabs: next, closed } = closeTab(folded, "t2");
    expect([...closed].sort()).toEqual(["t2", "t3", "t4"]);
    expect(next.map((t) => t.id)).toEqual(["t1"]);
  });
});

describe("selectAfterClose", () => {
  it("goes back to the opener", () => {
    const a = newTab("t2", "a()", "t1");
    const before = [grid, a];
    expect(selectAfterClose(before, closeTab(before, "t2").tabs, "t2")).toBe("t1");
  });

  it("goes to the next row when a root closes, the previous one at the end", () => {
    const x = newTab("t2", "x()", null);
    const y = newTab("t3", "y()", null);
    const before = [grid, x, y];
    expect(selectAfterClose(before, closeTab(before, "t2").tabs, "t2")).toBe("t3");
    expect(selectAfterClose(before, closeTab(before, "t3").tabs, "t3")).toBe("t2");
  });
});

describe("tabForSpec", () => {
  const g1 = { ...newTab("t1", "gridView()", null), state: "q=a" };
  const g2 = { ...newTab("t2", "gridView()", null), state: "q=b" };

  it("matches source and state exactly first", () => {
    expect(tabForSpec([g1, g2], "t1", { code: "gridView()", state: "q=b" })?.id).toBe("t2");
  });

  it("takes a bare link to the selected tab of that card", () => {
    expect(tabForSpec([g1, g2], "t2", { code: "gridView()", state: "" })?.id).toBe("t2");
  });

  it("does not claim a tab whose state the URL contradicts", () => {
    expect(tabForSpec([g1], "t1", { code: "gridView()", state: "q=z" })).toBeNull();
  });
});

describe("startingTree", () => {
  const own = { tabs: [newTab("t5", "logView()", null)], selectedId: "t5" };
  const saved = { tabs: [grid], selectedId: "t1" };

  it("keeps a window's own tree across its reload", () => {
    expect(startingTree(own, saved, false)).toBe(own);
  });

  it("restores the saved tree in the main window at launch", () => {
    expect(startingTree(null, saved, true)).toBe(saved);
  });

  it("starts a popped-out window empty, so it is a stack of its own", () => {
    expect(startingTree(null, saved, false)).toBeNull();
  });
});

describe("storage", () => {
  it("round-trips", () => {
    const tabs = [grid, { ...newTab("t2", "a()", "t1"), title: "A", collapsed: true }];
    expect(parseStored(serialize({ tabs, selectedId: "t2" }))).toEqual({
      tabs,
      selectedId: "t2",
    });
  });

  it("refuses what it did not write rather than guessing", () => {
    expect(parseStored(null)).toBeNull();
    expect(parseStored("not json")).toBeNull();
    expect(parseStored(JSON.stringify({ v: 99, tabs: [] }))).toBeNull();
    expect(parseStored(JSON.stringify({ v: 1, tabs: [{ id: 3 }] }))).toBeNull();
  });

  it("numbers new tabs past every stored one", () => {
    expect(nextCounter([grid, newTab("t41", "a()", null), newTab("x", "b()", null)])).toBe(42);
  });
});
