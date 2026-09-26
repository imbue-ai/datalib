import { describe, expect, it } from "vitest";
import { markdownsToAsk, widen } from "./qmdAsk";

describe("widen", () => {
  it("adds the margin on both sides", () => {
    expect(widen({ top: 100, bottom: 120 }, 1000, 10)).toEqual({ top: 90, bottom: 130 });
  });

  /// A range near either end must stay inside the rows the grid holds,
  /// or the caller reads past the data view's last item.
  it("stops at the first and last row", () => {
    expect(widen({ top: 3, bottom: 8 }, 10, 50)).toEqual({ top: 0, bottom: 9 });
  });
});

describe("markdownsToAsk", () => {
  const row = (markdown_uuid: string | null) => ({ markdown_uuid });

  /// One thread's messages share its document: asking once per row
  /// would multiply the applet's file reads by the thread length.
  it("asks about each document once", () => {
    expect(markdownsToAsk([row("a"), row("a"), row("b")], new Map(), new Set())).toEqual([
      "a",
      "b",
    ]);
  });

  /// Scrolling back over rows already answered, or still in flight,
  /// must not send them again.
  it("skips what is answered or already asked", () => {
    expect(
      markdownsToAsk([row("a"), row("b"), row("c")], new Map([["a", {}]]), new Set(["b"])),
    ).toEqual(["c"]);
  });

  /// Group header rows come back from the data view as items with no
  /// document, and a row may have none of its own.
  it("ignores rows with no document", () => {
    expect(markdownsToAsk([row(null), undefined, null, row("a")], new Map(), new Set())).toEqual([
      "a",
    ]);
  });
});
