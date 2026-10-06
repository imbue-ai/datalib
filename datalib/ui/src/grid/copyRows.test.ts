import { describe, expect, it } from "vitest";
import type { Column } from "@slickgrid-universal/common";
import { copiesRows, rowsAsTsv, tsvField } from "./copyRows";

type Line = { at: string; msg: string };

const columns: Column<Line>[] = [
  { id: "at", name: "Time", field: "at" },
  { id: "msg", name: "Message", field: "msg" },
];

describe("rows as TSV", () => {
  it("writes a header line, then a line per row in the order given", () => {
    const tsv = rowsAsTsv(
      columns,
      [
        { at: "t1", msg: "one" },
        { at: "t2", msg: "two" },
      ],
      (c, r) => r[c.field as keyof Line],
    );
    expect(tsv).toBe("Time\tMessage\nt1\tone\nt2\ttwo");
  });

  /// A multi-line error message pasted into a spreadsheet stays one cell.
  it("quotes a field holding a tab, a line break or a quote", () => {
    expect(tsvField("plain")).toBe("plain");
    expect(tsvField("a\tb")).toBe('"a\tb"');
    expect(tsvField("line\nnext")).toBe('"line\nnext"');
    expect(tsvField('say "hi"')).toBe('"say ""hi"""');
  });
});

describe("what the copy key copies", () => {
  it("leaves it to the browser with no row selected", () => {
    expect(copiesRows(0, false)).toBe(false);
  });

  it("lets a text selection win over one selected row", () => {
    expect(copiesRows(1, true)).toBe(false);
    expect(copiesRows(1, false)).toBe(true);
  });

  /// A shift-click range also stretches a text selection across the rows.
  it("copies several selected rows whatever text is selected", () => {
    expect(copiesRows(3, true)).toBe(true);
  });
});
