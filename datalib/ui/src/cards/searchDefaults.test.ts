import { describe, expect, it } from "vitest";
import type { RowGroup, SearchRow } from "@/api";
import { searchPlaceholder } from "./searchDefaults";

const group = (
  id: string,
  count: number,
  touched: string | null,
): RowGroup<Partial<SearchRow>> => ({
  values: [id],
  count,
  sample: { source_id: id, touched_at: touched },
});

describe("searchPlaceholder", () => {
  /// The hint used to suggest `source:Slack` to a library with no Slack
  /// in it: its examples have to be filters that find something here.
  it("names the biggest source and the year of its newest row", () => {
    expect(
      searchPlaceholder([
        group("enterprise-mail", 12, "2025-03-04T09:00:00-08:00"),
        group("ten-forward-slack", 40, "2026-06-02T13:00:00-07:00"),
      ]),
    ).toBe("search…  (try: source_id:ten-forward-slack, after:2026-01-01)");
  });

  /// Datalib's storage report is a source id in the index, but not one
  /// of the person's sources.
  it("does not suggest datalib's own rows", () => {
    expect(
      searchPlaceholder([
        group("datalib", 300, "2026-09-29T11:47:01+02:00"),
        group("holodeck-pdfs", 2, null),
      ]),
    ).toBe("search…  (try: source_id:holodeck-pdfs)");
  });

  it("says only search… when the index names no source", () => {
    expect(searchPlaceholder([])).toBe("search…");
    expect(searchPlaceholder([group("datalib", 3, "2026-09-29T11:47:01+02:00")])).toBe("search…");
  });
});
