import { describe, expect, it } from "vitest";
import type { ManageRow, Place } from "@/api";
import { parseCardSource } from "./cardCall";
import {
  placeLabel,
  isNodeId,
  nodeCardSource,
  nodeCardProps,
  nodeOpts,
  treeChip,
  placeNodes,
  scopeOf,
  sourceNodes,
} from "./library";

function group(id: string, label: string, type: string | null): ManageRow {
  return {
    id,
    kind: "group",
    name: { id, label },
    type: type === null ? null : { id: type, label: type },
  } as ManageRow;
}

function place(over: Partial<Place> & { uuid: string }): Place {
  return {
    sort_key: "",
    kind: "Folder",
    name: null,
    preview: null,
    markdown_uuid: null,
    link: null,
    places_below: false,
    ...over,
  };
}

describe("sourceNodes", () => {
  /** The tree's top is the sources, in the config's order, each opening
   *  onto what it holds at its top; a group with no type is no source. */
  it("opens a source only where it holds places", () => {
    const rows = [group("notion", "Notion", "Notion"), group("slack", "Slack", "Slack")];
    const nodes = sourceNodes(rows, (id) => id === "source:slack");
    expect(nodes.map((n) => [n.id, n.hasChildren])).toEqual([
      ["source:notion", false],
      ["source:slack", true],
    ]);
  });

  /** A source is drawn as its group's chip, led by its row's mark until
   *  the server says what the source is now. */
  it("is drawn as its group's chip, with its row's mark", () => {
    const slack = {
      ...group("slack", "Work Slack", "Slack"),
      name: { id: "slack", label: "Work Slack", icon: "slack" },
    };
    const [node] = sourceNodes([slack]);
    expect(treeChip(node)).toEqual({
      kind: "group",
      uri: "datalib:group/slack",
      shown: "Work Slack",
      mark: "slack",
    });
  });

  it("lists each typed group as a source's top, in order", () => {
    const rows = [
      group("tng_email", "Picard's mail", "Email"),
      { ...group("slack/ingest", "Download", null), kind: "step" } as ManageRow,
      group("unified_grid_and_search_index", "Index", null),
      group("slack-diff", "Slack, compared", "diff"),
      group("slack", "Enterprise Slack", "Slack"),
    ];
    expect(sourceNodes(rows)).toEqual([
      { id: "source:tng_email", label: "Picard's mail", hasChildren: true },
      { id: "source:slack", label: "Enterprise Slack", hasChildren: true },
    ]);
  });
});

describe("treeChip", () => {
  /** Every node of the tree is a chip: a place by its own uuid, carrying
   *  the document it is, and nothing that is neither a uuid nor a source. */
  it("draws a place as its node's chip, and nothing for any other id", () => {
    const id = "00000000-0000-8570-90c4-e54c531f3fff";
    const [page] = placeNodes([
      place({
        uuid: id,
        kind: "Notion page",
        name: "Shift Roster",
        markdown_uuid: id,
      }),
    ]);
    expect(treeChip(page)).toEqual({
      kind: "node",
      id,
      name: "Shift Roster",
      nodeKind: "Notion page",
      doc: id,
    });
    expect(treeChip({ id: "..", label: "x", hasChildren: false })).toBeNull();
    expect(treeChip({ id: "source:..", label: "x", hasChildren: false })).toBeNull();
  });
});

describe("placeNodes", () => {
  /** Labels have no order of their own: they read by name. A place opens
   *  only where places lie below it. */
  it("orders by the source's key, then by name, and opens where places lie below", () => {
    const nodes = placeNodes([
      place({ uuid: "s", kind: "Email Label", name: "Sent" }),
      place({ uuid: "i", kind: "Email Label", name: "Inbox", places_below: true }),
      place({ uuid: "a", kind: "Email Label", name: "Archive" }),
    ]);
    expect(nodes.map((n) => [n.label, n.hasChildren])).toEqual([
      ["Archive", false],
      ["Inbox", true],
      ["Sent", false],
    ]);
  });
});

describe("a place that is a document", () => {
  /** A Notion page is a place and a document: the tree carries the
   *  document to the node card, which leads with it. A channel is no
   *  document and carries none. */
  it("carries its document from the tree", () => {
    const [channel, page] = placeNodes([
      place({
        uuid: "p",
        kind: "Notion page",
        name: "Shift Roster",
        markdown_uuid: "p",
      }),
      place({ uuid: "c", kind: "Slack Channel", name: "#bridge" }),
    ]);
    expect(nodeOpts(page)).toEqual({ node: "p", name: "Shift Roster", doc: "p" });
    expect(nodeOpts(channel)).toEqual({ node: "c", name: "#bridge" });
  });

  /** Card source comes from any link: `doc` names a document to read, so
   *  only a uuid is taken. */
  it("takes a document only by its uuid", () => {
    const node = "00000000-0000-8570-90c4-e54c531f3fff";
    expect(nodeCardProps({ node, name: "Roster", doc: node })).toEqual({
      node,
      name: "Roster",
      doc: node,
    });
    expect(nodeCardProps({ node })).toEqual({ node, name: null, doc: null });
    for (const doc of ["..", "source:notion", "x/y", 7]) {
      expect(() => nodeCardProps({ node, doc: doc as string }), String(doc)).toThrow(/uuid/);
    }
    expect(() => nodeCardProps({ node: ".." })).toThrow(/needs the node/);
  });
});

describe("placeLabel", () => {
  it("is the name, else the first line of the text, else the kind", () => {
    expect(placeLabel(place({ uuid: "1", name: "#bridge" }))).toBe("#bridge");
    expect(placeLabel(place({ uuid: "2", preview: "Red alert\nAll hands" }))).toBe("Red alert");
    expect(placeLabel(place({ uuid: "3", kind: "Slack Thread" }))).toBe("Slack Thread");
  });
});

describe("what a node opens", () => {
  it("opens its card by its id and its name", () => {
    expect(
      parseCardSource(nodeCardSource({ id: "c", label: "#bridge", hasChildren: false })),
    ).toEqual({ name: "nodeView", args: [{ node: "c", name: "#bridge" }] });
    expect(nodeCardSource({ id: "source:slack", label: "Slack", hasChildren: true })).toBe(
      'nodeView({"node":"source:slack","name":"Slack"})',
    );
  });

  /** A place holds its card's search to what is directly in it; a source,
   *  to all of it. */
  it("searches a place or a source", () => {
    const node = "00000000-0000-8570-90c4-e54c531f3fff";
    expect(scopeOf(node)).toBe(`at:${node}`);
    expect(scopeOf("source:work/slack-2.0")).toBe("source:work/slack-2.0");
  });
});

describe("isNodeId", () => {
  it("takes a uuid or a source's top, and nothing that could leave the path", () => {
    expect(isNodeId("00000000-0000-8570-90c4-e54c531f3fff")).toBe(true);
    expect(isNodeId("source:tng_email")).toBe(true);
    expect(isNodeId("source:work/slack-2.0")).toBe(true);
    for (const bad of ["..", ".", "source:..", "source:a/../b", "source:", "x?y", "", 7]) {
      expect(isNodeId(bad), String(bad)).toBe(false);
    }
  });
});
