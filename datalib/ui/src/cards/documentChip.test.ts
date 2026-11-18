import { describe, expect, it } from "vitest";
import { documentSource } from "./cardSources";
import {
  documentCardSource,
  documentCell,
  documentCopyHtml,
  documentCopyText,
  documentId,
  documentLook,
  documentMenu,
} from "./documentChip";

const PAGE = "6c8f2f43-1d3c-4a5e-9b5e-0d3f6a1b2c3d";
const SECTION = "0b1d2e3f-4a5b-4c6d-8e7f-901a2b3c4d5e";
const URI = `datalib:doc/${PAGE}`;

describe("a document chip", () => {
  it("is its title, led by its source's mark, with where it came from on hover", () => {
    const look = documentLook(URI, "Captain's log", "slack", "Work Slack");
    expect(look.text).toBe("Captain's log");
    expect(look.icon).toBe("slack");
    expect(look.ariaLabel).toBe("Captain's log, a document from Work Slack");
    expect(look.title).toBe(`Captain's log\nFrom Work Slack\n${URI}`);
  });

  it("opens the document's card, at a section when it names one", () => {
    expect(documentCardSource(URI)).toBe(documentSource(PAGE));
    expect(documentCardSource(`${URI}#${SECTION}`)).toBe(documentSource(PAGE, SECTION));
    expect(documentCardSource("datalib:group/slack")).toBeNull();
  });

  it("copies its id, and its title with its name", () => {
    expect(documentId(`${URI}#${SECTION}`)).toBe(PAGE);
    expect(documentCopyText(URI, "Captain's log")).toBe(`Captain's log (${URI})`);
  });

  /** A paste into anything that takes rich text is a link: the title,
   *  pointing at the document by its uuid. */
  it("copies as a link named by its title", () => {
    expect(documentCopyHtml(URI, "Captain's log")).toBe(`<a href="${URI}">Captain&#39;s log</a>`);
    const a = new DOMParser()
      .parseFromString(documentCopyHtml(URI, `<b>"Q" & me</b>`), "text/html")
      .querySelector("a");
    expect(a?.getAttribute("href")).toBe(URI);
    expect(a?.textContent).toBe(`<b>"Q" & me</b>`);
    expect(a?.children.length).toBe(0);
  });

  it("offers to open it, then its copies", () => {
    expect(documentMenu(URI, "Captain's log").map((e) => e.id)).toEqual([
      "open",
      "copy-id",
      "copy-name",
      "copy-both",
    ]);
    expect(documentMenu(URI, "Captain's log")[0].label).toBe("Open “Captain's log”");
    expect(documentMenu("datalib:doc/../etc", "x")).toEqual([]);
  });

  /** A cell's chip is drawn only for a name the reader itself would
   *  write: anything else is not a document chip. */
  it("draws a chip only for a document's name", () => {
    const a = documentCell(URI, "Captain's log", "slack", "Work Slack");
    expect(a?.dataset.doc).toBe(URI);
    expect(a?.getAttribute("href")).toBe(URI);
    expect(a?.textContent).toContain("Captain's log");
    expect(documentCell("datalib:group/slack", "Slack", null, null)).toBeNull();
    expect(documentCell("javascript:alert(1)", "x", null, null)).toBeNull();
  });
});
