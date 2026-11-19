import { describe, expect, it } from "vitest";
import type { Child, ManageRow } from "@/api";
import { parseCardSource } from "./cardCall";
import {
  childCardSource,
  childLabel,
  isNodeId,
  nodeCardSource,
  kindsDiffer,
  rowParts,
  placeNodes,
  previewArgs,
  sourceNodes,
  wantsPreview,
} from "./library";

function group(id: string, label: string, type: string | null): ManageRow {
  return {
    id,
    kind: "group",
    name: { id, label },
    type: type === null ? null : { id: type, label: type },
  } as ManageRow;
}

function child(over: Partial<Child> & { uuid: string }): Child {
  return {
    sort_key: "",
    container: false,
    kind: "Message",
    name: null,
    author: null,
    created_at: null,
    preview: null,
    markdown_uuid: null,
    link: null,
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

  it("lists each typed group as a source's top, in order", () => {
    const rows = [
      group("tng_email", "Picard's mail", "Email"),
      { ...group("slack/ingest", "Download", null), kind: "step" } as ManageRow,
      group("unified_grid_and_search_index", "Index", null),
      group("slack-diff", "Slack, compared", "diff"),
      group("slack", "Enterprise Slack", "Slack"),
    ];
    expect(sourceNodes(rows)).toEqual([
      { id: "source:tng_email", label: "Picard's mail", kind: "Email", hasChildren: true },
      { id: "source:slack", label: "Enterprise Slack", kind: "Slack", hasChildren: true },
    ]);
  });
});

describe("placeNodes", () => {
  /** Labels have no order of their own: they read by name. A place opens
   *  only where places lie below it. */
  it("orders by the source's key, then by name, and opens where places lie below", () => {
    const nodes = placeNodes([
      child({ uuid: "s", kind: "Email Label", name: "Sent", container: true, places_below: false }),
      child({ uuid: "i", kind: "Email Label", name: "Inbox", container: true, places_below: true }),
      child({ uuid: "a", kind: "Email Label", name: "Archive", container: true }),
    ]);
    expect(nodes.map((n) => [n.label, n.hasChildren])).toEqual([
      ["Archive", false],
      ["Inbox", true],
      ["Sent", false],
    ]);
  });
});

describe("childLabel", () => {
  it("is the name, else the first line of the text, else the kind", () => {
    expect(childLabel(child({ uuid: "1", name: "#bridge" }))).toBe("#bridge");
    expect(childLabel(child({ uuid: "2", preview: "Red alert\nAll hands" }))).toBe("Red alert");
    expect(childLabel(child({ uuid: "3", kind: "Slack Thread" }))).toBe("Slack Thread");
  });
});

describe("rowParts", () => {
  /** A message reads as its author over its text; a page as its name. */
  it("is the name, else the author, else the text, else the kind", () => {
    expect(rowParts(child({ uuid: "1", name: "#bridge", author: "Data" })).title).toBe("#bridge");
    expect(rowParts(child({ uuid: "2", author: "Worf", preview: "Today" }))).toEqual({
      title: "Worf",
      snippet: "Today",
    });
    expect(rowParts(child({ uuid: "3", kind: "Email Thread" })).title).toBe("Email Thread");
  });

  /** A channel's threads are each named for the channel, and a thread's
   *  row names no author: listed in it, each reads as its first line,
   *  which the text under it then leaves out. */
  it("leads with the text where the name only repeats the node", () => {
    const thread = child({ uuid: "t", name: "#bridge", preview: "Shields up.\nRed alert." });
    expect(rowParts(thread, "#bridge")).toEqual({ title: "Shields up.", snippet: "Red alert." });
    expect(rowParts(thread, "#engineering").title).toBe("#bridge");
    expect(rowParts(child({ uuid: "u", name: "#bridge" }), "#bridge").title).toBe("#bridge");
  });

  /** A page whose preview is its title says it once. */
  it("leaves out text that only repeats the title", () => {
    const page = child({ uuid: "p", name: "Handbook", preview: "Handbook" });
    expect(rowParts(page)).toEqual({ title: "Handbook", snippet: null });
  });
});

describe("kindsDiffer", () => {
  it("is whether a list holds more than one kind", () => {
    expect(kindsDiffer([child({ uuid: "a", kind: "T" }), child({ uuid: "b", kind: "T" })])).toBe(
      false,
    );
    expect(kindsDiffer([child({ uuid: "a", kind: "T" }), child({ uuid: "b", kind: "M" })])).toBe(
      true,
    );
  });
});

describe("what a node opens", () => {
  /** A container opens its own children; a row opens its document, at
   *  the row unless the row is the document's own. */
  it("opens a container's children and a row's document", () => {
    const channel = child({ uuid: "c", container: true, kind: "Slack Channel", name: "#bridge" });
    expect(parseCardSource(childCardSource(channel)!)).toEqual({
      name: "nodeView",
      args: [{ node: "c", name: "#bridge" }],
    });
    const message = child({ uuid: "m", markdown_uuid: "d" });
    expect(previewArgs(message)).toEqual(["d", "m"]);
    expect(childCardSource(message)).toBe('documentView("d", "m")');
    const doc = child({ uuid: "d", markdown_uuid: "d" });
    expect(previewArgs(doc)).toEqual(["d", null]);
    expect(childCardSource(child({ uuid: "x" }))).toBeNull();
    expect(nodeCardSource({ id: "source:slack", label: "Slack" })).toBe(
      'nodeView({"node":"source:slack","name":"Slack"})',
    );
  });

  /** An account's labels are a list alone; a channel's threads read beside
   *  a preview. */
  it("previews only a level that holds documents", () => {
    expect(wantsPreview([child({ uuid: "l", container: true })])).toBe(false);
    expect(wantsPreview([child({ uuid: "t", container: true, markdown_uuid: "t" })])).toBe(true);
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
