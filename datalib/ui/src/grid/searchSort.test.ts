import { describe, expect, it } from "vitest";
import {
  chosenSort,
  sortChoices,
  sortersOf,
  sortOfSorters,
  sortsBySize,
  startsDescending,
} from "./searchSort";

describe("the order control", () => {
  it("offers best match only where there are words to match", () => {
    expect(sortChoices(null, true).map((c) => c.label)).toEqual([
      "Best match",
      "Newest first",
      "Oldest first",
      "Largest first",
      "Smallest first",
      "Author A to Z",
      "Title A to Z",
    ]);
    expect(sortChoices(null, false)[0]).toEqual({ label: "Newest first", sort: "" });
    expect(sortChoices(null, false).map((c) => c.label)).not.toContain("Best match");
  });

  it("reads a header's sort as the choice that means the same", () => {
    expect(chosenSort("touched_at:desc", false)).toBe("");
    expect(chosenSort("touched_at:desc", true)).toBe("touched_at:desc");
    expect(chosenSort("score:desc", true)).toBe("");
    expect(chosenSort(null, true)).toBe("");
    expect(chosenSort("byte_size:asc", false)).toBe("byte_size:asc");
  });

  it("offers an order only the headers can ask for, rather than claim another", () => {
    const choices = sortChoices("channel:asc", false);
    expect(choices.at(-1)).toEqual({ label: "By the table's columns", sort: "channel:asc" });
    expect(sortChoices("byte_size:asc", false).map((c) => c.sort)).not.toContain("channel:asc");
  });
});

describe("a sort as the headers hold it", () => {
  it("reads back as it was written, most significant first", () => {
    const sort = "author_ref:desc,touched_at:asc";
    expect(sortersOf(sort)).toEqual([
      { columnId: "author_ref", direction: "DESC" },
      { columnId: "touched_at", direction: "ASC" },
    ]);
    expect(sortOfSorters(sortersOf(sort))).toBe(sort);
  });

  it("is nothing when nothing is sorted", () => {
    expect(sortersOf(null)).toEqual([]);
    expect(sortOfSorters([])).toBeNull();
  });

  it("takes a bare column as ascending, as the server does", () => {
    expect(sortersOf("kind")).toEqual([{ columnId: "kind", direction: "ASC" }]);
  });
});

it("knows when a list has to show each row's size", () => {
  expect(sortsBySize("byte_size:desc,author_ref")).toBe(true);
  expect(sortsBySize("author_ref,byte_size:desc")).toBe(false);
  expect(sortsBySize(null)).toBe(false);
});

it("starts a date, a size or a score at its largest, and text at A", () => {
  for (const type of ["datetime", "timestamp", "bytes", "count", "number"] as const) {
    expect(startsDescending(type)).toBe(true);
  }
  for (const type of ["text", "identity", "identities"] as const) {
    expect(startsDescending(type)).toBe(false);
  }
});
