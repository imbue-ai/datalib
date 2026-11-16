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

  /// An email subject `=HYPERLINK("https://…","Click")` pasted into
  /// Sheets or Excel as a live formula.
  it("keeps a field that starts like a formula as text", () => {
    expect(tsvField("=1+1")).toBe("'=1+1");
    expect(tsvField("+1 555 0100")).toBe("'+1 555 0100");
    expect(tsvField("@SUM(A1:A9)")).toBe("'@SUM(A1:A9)");
    expect(tsvField("-2+3")).toBe("'-2+3");
    expect(tsvField('=HYPERLINK("https://x.example","Click")')).toBe(
      `"'=HYPERLINK(""https://x.example"",""Click"")"`,
    );
    expect(tsvField("\t=1+1")).toBe('"\'\t=1+1"');
    expect(tsvField("\r=1+1")).toBe('"\'\r=1+1"');
  });

  /// A negative number is a number to the spreadsheet, and nothing more.
  it("leaves numbers and ordinary text as they are", () => {
    for (const v of ["-3", "-3.25", "-.5", "42", "3.14", "a=b", "x-1", "", "'quoted'"]) {
      expect(tsvField(v), v).toBe(v);
    }
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
