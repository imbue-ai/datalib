import { describe, expect, it } from "vitest";
import {
  documentCopyText,
  documentMenu,
  documentTitle,
  outlinkOf,
  type DocumentAbout,
  type Host,
} from "./documentMenu";

const CHAT: DocumentAbout = {
  markdown_uuid: "4c1e7b20-0000-4000-8000-000000009a07",
  name: "Risa Survey Briefing",
  source_label: "ChatGPT",
  source_url: "https://chatgpt.com/c/4c1e7b20-0000-4000-8000-000000009a07",
  source_ref: { id: "chatgpt-api", label: "chatgpt-api" },
};
const CARD: Host = { preview: false, revealLabel: null };

describe("documentMenu", () => {
  it("names the app it opens in, then the source, then the copies and feedback", () => {
    expect(documentMenu(CHAT, CARD)).toEqual([
      { id: "open-url", label: "Open in ChatGPT" },
      { id: "open-source", label: "Open the chatgpt-api source" },
      { id: "copy-id", label: "Copy ID", separator: true },
      { id: "copy-name", label: "Copy “Risa Survey Briefing”" },
      { id: "copy-both", label: "Copy title and link" },
      { id: "copy-markdown", label: "Copy as Markdown" },
      { id: "feedback", label: "Feedback on this document…", separator: true },
    ]);
  });

  it("offers Open as card only beside a list", () => {
    const ids = (preview: boolean) => documentMenu(CHAT, { ...CARD, preview }).map((e) => e.id);
    expect(ids(true)[0]).toBe("open-card");
    expect(ids(false)).not.toContain("open-card");
  });

  /** A document with nowhere to go starts at its copies, with no line above them. */
  it("draws no line first when there is nothing to open", () => {
    const bare = { ...CHAT, source_url: null, source_ref: null };
    expect(documentMenu(bare, CARD)[0]).toEqual({ id: "copy-id", label: "Copy ID" });
  });
});

describe("outlinkOf", () => {
  it("says at the source when the source has no product name", () => {
    expect(outlinkOf({ ...CHAT, source_label: null }, CARD)?.label).toBe("Open at the source");
  });

  /** A file:// URL cannot be opened from the page; it is revealed or its path copied. */
  it("reveals a local file in the app and copies its path in a browser", () => {
    const pdf = { ...CHAT, source_url: "file:///Users/picard/Logs/Stardate%2047988.pdf" };
    expect(outlinkOf(pdf, CARD)).toEqual({
      id: "copy-path",
      label: "Copy file path",
      target: "/Users/picard/Logs/Stardate 47988.pdf",
    });
    expect(outlinkOf(pdf, { ...CARD, revealLabel: "Reveal in Finder" })?.id).toBe("reveal");
  });

  it("is null with no source URL", () => {
    expect(outlinkOf({ ...CHAT, source_url: null }, CARD)).toBeNull();
  });
});

describe("copies", () => {
  it("copies the title with the document's datalib name", () => {
    expect(documentCopyText(CHAT)).toBe(
      "Risa Survey Briefing (datalib:doc/4c1e7b20-0000-4000-8000-000000009a07)",
    );
  });

  it("titles an unnamed document", () => {
    expect(documentTitle({ ...CHAT, name: "  " })).toBe("Untitled document");
  });
});
