import { describe, expect, it } from "vitest";
import type { Child, ManageRow } from "@/api";
import { parseCardSource } from "./cardCall";
import {
  childCardSource,
  childLabel,
  isNodeId,
  nodeCardSource,
  kindsDiffer,
  nodeCardProps,
  nodeOpts,
  rowParts,
  rowTitle,
  treeChip,
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
    author_handle: null,
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
      child({
        uuid: id,
        kind: "Notion page",
        name: "Shift Roster",
        container: true,
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

describe("a place that is a document", () => {
  /** A Notion page is a place and a document: the tree carries the
   *  document to the node card, which shows it, and a page opened from a
   *  node card carries it too. A channel is no document and carries none. */
  it("carries its document from the tree, and from a node card's list", () => {
    const [channel, page] = placeNodes([
      child({
        uuid: "p",
        kind: "Notion page",
        name: "Shift Roster",
        container: true,
        markdown_uuid: "p",
      }),
      child({ uuid: "c", kind: "Slack Channel", name: "#bridge", container: true }),
    ]);
    expect(nodeOpts(page)).toEqual({ node: "p", name: "Shift Roster", doc: "p" });
    expect(nodeOpts(channel)).toEqual({ node: "c", name: "#bridge" });
    const listed = child({ uuid: "p", name: "Shift Roster", container: true, markdown_uuid: "p" });
    expect(parseCardSource(childCardSource(listed)!).args).toEqual([
      { node: "p", name: "Shift Roster", doc: "p" },
    ]);
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

describe("rowTitle", () => {
  const ID = "00000000-0000-8570-90c4-e54c531f3fff";

  /** A node card's rows are chips where they name something: a thread by
   *  its node, a message by its author, a page by itself. */
  it("titles a row by the chip it names", () => {
    const thread = child({
      uuid: ID,
      container: true,
      kind: "Slack Thread",
      name: "#bridge",
      preview: "Shields up.",
      markdown_uuid: ID,
    });
    expect(rowTitle(thread, "#bridge")).toEqual({
      kind: "node",
      id: ID,
      name: "Shields up.",
      nodeKind: "Slack Thread",
      doc: ID,
    });
    const message = child({
      uuid: "m",
      author: "Worf",
      author_handle: "slack:T1/U_WORF",
      preview: "Today",
    });
    expect(rowTitle(message)).toEqual({ kind: "person", handle: "slack:T1/U_WORF", shown: "Worf" });
    const pdf = child({ uuid: ID, kind: "PDF Document", name: "Hull Survey", markdown_uuid: ID });
    expect(rowTitle(pdf)).toEqual({ kind: "document", page: ID, title: "Hull Survey" });
  });

  /** A name that leads, or an author with no handle, stays words: a chip
   *  for the author would title the row by someone the row is not named
   *  for. */
  it("keeps words where no chip names the title", () => {
    const named = child({
      uuid: "p",
      name: "Handbook",
      author: "Picard",
      author_handle: "email:picard@x",
    });
    expect(rowTitle(named)).toEqual({ kind: "text", text: "Handbook" });
    expect(rowTitle(child({ uuid: "m", author: "Worf" }))).toEqual({ kind: "text", text: "Worf" });
    expect(rowTitle(child({ uuid: "..", container: true, name: "x" }))).toEqual({
      kind: "text",
      text: "x",
    });
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
    expect(nodeCardSource({ id: "source:slack", label: "Slack", hasChildren: true })).toBe(
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
