import { describe, expect, it } from "vitest";
import type { ChildrenPage, TreeChild } from "@/api";
import {
  ROOT,
  addFolder,
  canMoveInto,
  decodeState,
  emptyLayout,
  encodeState,
  joinPage,
  layoutChildren,
  moveInto,
  pageSize,
  removeFolder,
  shownChildren,
  sourceTopPage,
  sourceTopParams,
} from "./dataTree";

const sources = [
  { id: "slack", label: "slack" },
  { id: "email", label: "email" },
  { id: "signal", label: "signal" },
];

const child = (uuid: string): TreeChild => ({
  uuid,
  sort_key: "",
  container: false,
  kind: "Message",
  name: null,
  author: null,
  created_at: null,
  preview: null,
  markdown_uuid: null,
});

const page = (ids: string[], before: string | null, after: string | null): ChildrenPage => ({
  children: ids.map(child),
  before,
  after,
});

describe("layoutChildren", () => {
  it("puts every source no folder names at the root, by label", () => {
    expect(layoutChildren(emptyLayout(), sources)[ROOT]).toEqual([
      "source:email",
      "source:signal",
      "source:slack",
    ]);
  });

  it("keeps what a folder holds there, and drops a source the library no longer has", () => {
    let layout = addFolder(emptyLayout(), "a", "Chat");
    layout = { ...layout, lists: { ...layout.lists, "folder:a": ["source:slack", "source:gone"] } };
    const shown = layoutChildren(layout, sources);
    expect(shown["folder:a"]).toEqual(["source:slack"]);
    expect(shown[ROOT]).toEqual(["folder:a", "source:email", "source:signal"]);
  });
});

describe("moveInto", () => {
  it("counts the index among the target's children as shown, the moved ones included", () => {
    const layout = emptyLayout();
    const shown = layoutChildren(layout, sources);
    // email, signal, slack: drop email between signal and slack.
    const moved = moveInto(layout, shown, ["source:email"], ROOT, 2);
    expect(layoutChildren(moved, sources)[ROOT]).toEqual([
      "source:signal",
      "source:email",
      "source:slack",
    ]);
  });

  it("takes a source out of the root when it goes into a folder", () => {
    const layout = addFolder(emptyLayout(), "a", "Chat");
    const shown = layoutChildren(layout, sources);
    const moved = layoutChildren(moveInto(layout, shown, ["source:slack"], "folder:a"), sources);
    expect(moved["folder:a"]).toEqual(["source:slack"]);
    expect(moved[ROOT]).not.toContain("source:slack");
  });
});

describe("canMoveInto", () => {
  it("refuses a folder into itself or a folder under it, and anything into a node", () => {
    let layout = addFolder(addFolder(emptyLayout(), "outer", "Outer"), "inner", "Inner");
    layout = moveInto(layout, layoutChildren(layout, sources), ["folder:inner"], "folder:outer");
    expect(canMoveInto(layout, ["folder:outer"], "folder:outer")).toBe(false);
    expect(canMoveInto(layout, ["folder:outer"], "folder:inner")).toBe(false);
    expect(canMoveInto(layout, ["folder:inner"], ROOT)).toBe(true);
    expect(canMoveInto(layout, ["source:slack"], "some-thread-uuid")).toBe(false);
    expect(canMoveInto(layout, ["some-thread-uuid"], ROOT)).toBe(false);
  });
});

describe("removeFolder", () => {
  it("puts what the folder held where the folder was", () => {
    let layout = addFolder(emptyLayout(), "a", "Chat");
    layout = moveInto(layout, layoutChildren(layout, sources), ["source:slack"], "folder:a");
    const shown = layoutChildren(removeFolder(layout, "folder:a"), sources);
    expect(shown[ROOT][0]).toBe("source:slack");
    expect(shown["folder:a"]).toBeUndefined();
  });
});

describe("joinPage", () => {
  it("adds a page on the side it was asked for, keeps the other cursor, lists a child once", () => {
    const first = joinPage(null, page(["b", "c"], "cb", "cc"), null);
    expect(shownChildren("p", first)).toEqual(["more:before:p", "b", "c", "more:after:p"]);
    const later = joinPage(first, page(["c", "d"], "x", null), "after");
    expect(later).toEqual({ ids: ["b", "c", "d"], before: "cb", after: null });
    const earlier = joinPage(later, page(["a"], null, "y"), "before");
    expect(earlier).toEqual({ ids: ["a", "b", "c", "d"], before: null, after: null });
    expect(shownChildren("p", earlier)).toEqual(["a", "b", "c", "d"]);
  });
});

describe("the top of a source", () => {
  const columns = [
    "uuid",
    "sort_key",
    "kind",
    "name",
    "container",
    "author",
    "created_at",
    "preview",
    "markdown_uuid",
  ];
  const row = (uuid: string, key: string) => [uuid, key, "Chat", "n", 1, null, null, "", uuid];

  it("reads one more row than a page, so a full page says there is more", () => {
    const p = sourceTopPage(columns, [row("a", "1"), row("b", "2"), row("c", "3")], 2);
    expect(p.children.map((c) => c.uuid)).toEqual(["a", "b"]);
    expect(p.children[0].container).toBe(true);
    expect(p.after).not.toBeNull();
    expect(sourceTopParams("chat", p.after, 2)).toEqual({
      source: "chat",
      sort_key: "2",
      uuid: "b",
      limit: 3,
    });
  });

  it("has nothing after a short page, and starts before every key", () => {
    expect(sourceTopPage(columns, [row("a", "1")], 2).after).toBeNull();
    expect(sourceTopParams("chat", null, 2)).toEqual({
      source: "chat",
      sort_key: "",
      uuid: "",
      limit: 3,
    });
  });
});

describe("the card's state", () => {
  it("round-trips, and anything unreadable is an empty tree", () => {
    const s = {
      expanded: ["source:slack"],
      selected: [],
      focused: "source:slack",
      layout: addFolder(emptyLayout(), "a", "Chat"),
    };
    expect(decodeState(encodeState(s))).toEqual(s);
    expect(decodeState("not json").expanded).toEqual([]);
    expect(decodeState('{"v":2,"expanded":["x"]}').expanded).toEqual([]);
    expect(
      decodeState('{"v":1,"layout":{"names":{"source:x":"not a folder"}}}').layout.names,
    ).toEqual({});
  });

  it("asks for a page /children will take", () => {
    expect(pageSize(undefined)).toBe(100);
    expect(pageSize(3)).toBe(3);
    expect(pageSize(0)).toBe(1);
    expect(pageSize(5000)).toBe(1000);
    expect(pageSize(2.5)).toBe(100);
  });
});
