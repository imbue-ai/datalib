import { describe, expect, it } from "vitest";
import { nodeSource } from "./cardSources";
import { nodeCardSource, nodeCell, nodeLook, nodeMenu } from "./nodeChip";

const ID = "0b75b842-ba60-88dd-b090-baf44d88db59";
const URI = `datalib:node/${ID}`;

describe("a node chip", () => {
  it("is its name, with its kind and its link on hover", () => {
    const look = nodeLook(URI, "#bridge", "Slack Channel");
    expect(look.text).toBe("#bridge");
    expect(look.ariaLabel).toBe("#bridge, Slack Channel");
    expect(look.title).toBe(`#bridge\nSlack Channel\n${URI}`);
  });

  /** A node that is a document (a Notion page) opens on it. */
  it("opens the node's card, on the document it is when it is one", () => {
    expect(nodeCardSource(URI, "#bridge", null)).toBe(nodeSource({ node: ID, name: "#bridge" }));
    expect(nodeCardSource(URI, "Shift Roster", ID)).toBe(
      nodeSource({ node: ID, name: "Shift Roster", doc: ID }),
    );
    expect(nodeCardSource("datalib:doc/" + ID, "x", null)).toBeNull();
  });

  it("offers to open it, then its copies; nothing for a link that is no node", () => {
    expect(nodeMenu(URI, "#bridge").map((e) => e.id)).toEqual([
      "open",
      "copy-id",
      "copy-name",
      "copy-both",
    ]);
    expect(nodeMenu("datalib:node/..", "x")).toEqual([]);
  });

  /** The id goes into a card's source: anything but a uuid draws no chip. */
  it("draws a chip only for a uuid, and keeps a document only when it is one", () => {
    const a = nodeCell(ID, "#bridge", "Slack Channel", "not-a-uuid");
    expect(a?.dataset.node).toBe(URI);
    expect(a?.dataset.nodeDoc).toBeUndefined();
    expect(a?.getAttribute("href")).toBe(URI);
    expect(a?.textContent).toBe("#bridge");
    expect(nodeCell(ID, "Roster", "Notion page", ID)?.dataset.nodeDoc).toBe(ID);
    expect(nodeCell("source:slack", "Slack", "Source", null)).toBeNull();
  });
});
